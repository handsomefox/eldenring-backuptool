//! Puts a snapshot back over the live save, from the dashboard.
//!
//! A restore backs up the save it replaces before it changes anything:
//!
//! 1. It refuses while the game runs, checking by process name only.
//! 2. It holds the backup folder's lock throughout, so no backup copies the
//!    save while files move.
//! 3. It hashes the chosen snapshot's archive, then backs up the current save
//!    as a manual snapshot. If that backup fails, nothing is restored.
//! 4. It extracts the snapshot into a temp folder beside the save, so the
//!    final moves are renames on one volume, and hashes the extracted files
//!    again.
//! 5. It moves the current files into a second folder beside the save, then
//!    renames the extracted files into place. If a rename fails, it moves
//!    everything back. The folder of replaced files is deleted only once its
//!    files match the backup from step 3; otherwise it stays, and its path is
//!    reported.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::discovery;
use crate::snapshot::{self, FileHash, Reason, Snapshot};

/// Prefix of the temp folder a restore extracts into, inside the save folder.
const STAGING_PREFIX: &str = ".save-guard-restore-";
/// Prefix of the folder that holds the replaced files, inside the save folder.
const REPLACED_PREFIX: &str = ".save-guard-replaced-";

/// What to restore, and where.
pub struct Request<'a> {
    /// The backup destination that holds `snapshot_dir`.
    pub dest: &'a Path,
    pub steamid: &'a str,
    /// A snapshot folder directly under `<dest>/snapshots/`.
    pub snapshot_dir: &'a Path,
    /// `%APPDATA%\EldenRing`, which holds the account's save folder.
    pub elden_root: &'a Path,
}

/// A finished restore.
#[derive(Debug)]
pub struct Restored {
    /// The snapshot now in the save folder.
    pub snapshot: Snapshot,
    /// The backup of the save it replaced, or `None` when that save matched
    /// the newest snapshot already.
    pub backup: Option<Snapshot>,
    /// The folder still holding the replaced files, when they did not match
    /// the backup and so were not deleted.
    pub kept_aside: Option<PathBuf>,
}

/// Restore the snapshot `request` names over the account's save.
/// `game_running` reports whether Elden Ring is running; it is asked before
/// anything starts and again just before the files move.
///
/// # Errors
///
/// Returns an error, with the save as it was, if the game is running, the
/// snapshot fails its check, the current save cannot be backed up, or a file
/// cannot be moved. If moving files back also fails, the error names the
/// folder that holds the previous save.
pub fn restore(request: &Request<'_>, game_running: impl Fn() -> bool) -> Result<Restored> {
    restore_with(request, game_running, |from, to| std::fs::rename(from, to))
}

fn restore_with(
    request: &Request<'_>,
    game_running: impl Fn() -> bool,
    rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<Restored> {
    let Request {
        dest,
        steamid,
        snapshot_dir,
        elden_root,
    } = *request;
    if game_running() {
        bail!("Elden Ring is running. Close it, then restore.");
    }
    let find = || {
        discovery::find(elden_root, steamid).with_context(|| {
            format!(
                "no save for account {steamid} under {}. Save Guard restores only over an \
                 existing save, because it backs that save up first",
                elden_root.display()
            )
        })
    };
    crate::paths::validate_backup_dest(&find()?.dir, dest)?;

    let _lock = crate::retention::lock_waiting(dest)?;
    snapshot::remove_temp_dirs(dest);
    let target = snapshot::open_for_restore(dest, snapshot_dir, steamid)
        .context("the chosen backup failed its check, so nothing was restored")?;

    // Found again under the lock: the game may have written its .bak since.
    let candidate = find()?;
    let save_dir = candidate.dir.clone();
    let backup = snapshot::create(dest, steamid, &candidate.source_files(), Reason::Manual)
        .context("could not back up the current save first, so nothing was restored")?;
    // With no new backup, the save matched the newest snapshot.
    let backed_up: Vec<FileHash> = match &backup {
        Some(backup) => backup.metadata.files.clone(),
        None => snapshot::newest(dest, steamid).map_or_default(|newest| newest.metadata.files),
    };

    remove_leftover_staging(&save_dir);
    let staging = save_dir.join(unique_name(STAGING_PREFIX));
    let result = stage_and_swap(&target, &candidate, &staging, &game_running, rename);
    let _ = std::fs::remove_dir_all(&staging);
    let aside = result?;

    let kept_aside = if replaced_match(&aside, &backed_up) {
        std::fs::remove_dir_all(&aside)
            .inspect_err(|e| {
                tracing::warn!("could not remove {}: {e}", aside.display());
            })
            .err()
            .map(|_| aside)
    } else {
        tracing::warn!(
            "the replaced save files do not match the backup; keeping them in {}",
            aside.display()
        );
        Some(aside)
    };
    tracing::info!(
        "restored {} into {}",
        target.dir.display(),
        save_dir.display()
    );
    Ok(Restored {
        snapshot: target,
        backup,
        kept_aside,
    })
}

/// Extract `target` into `staging`, check it, then swap it in. Returns the
/// folder holding the replaced files.
fn stage_and_swap(
    target: &Snapshot,
    candidate: &discovery::SaveCandidate,
    staging: &Path,
    game_running: &impl Fn() -> bool,
    rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf> {
    snapshot::extract(&target.dir, staging).context("could not extract the backup")?;
    snapshot::check_files(staging, &target.metadata.files)
        .context("the extracted backup failed its check")?;
    if game_running() {
        bail!("Elden Ring started during the restore. Nothing was changed.");
    }

    let new_names: Vec<String> = target
        .metadata
        .files
        .iter()
        .map(|f| f.name.clone())
        .collect();
    // Every current save file goes aside, including a .bak the backup lacks,
    // so the game never pairs the restored save with a stale .bak.
    let mut old_names: BTreeSet<String> = new_names.iter().cloned().collect();
    old_names.extend(
        candidate
            .source_files()
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_string)),
    );
    let aside = candidate.dir.join(unique_name(REPLACED_PREFIX));
    std::fs::create_dir(&aside).with_context(|| format!("creating {}", aside.display()))?;
    let old_names: Vec<String> = old_names.into_iter().collect();
    if let Err(error) = swap_files(
        &candidate.dir,
        staging,
        &aside,
        &new_names,
        &old_names,
        rename,
    ) {
        // An empty folder means everything went back.
        let _ = std::fs::remove_dir(&aside);
        return Err(error);
    }
    Ok(aside)
}

/// Move each of `old_names` present in `save_dir` into `aside`, then each of
/// `new_names` from `staging` into `save_dir`. On a failure, undo every move
/// made, newest first.
fn swap_files(
    save_dir: &Path,
    staging: &Path,
    aside: &Path,
    new_names: &[String],
    old_names: &[String],
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let mut moved_aside: Vec<&str> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();
    let mut failure = None;
    for name in old_names {
        let live = save_dir.join(name);
        if live.symlink_metadata().is_err() {
            continue;
        }
        if let Err(e) = rename(&live, &aside.join(name)) {
            failure =
                Some(anyhow::Error::new(e).context(format!("moving {} aside", live.display())));
            break;
        }
        moved_aside.push(name);
    }
    if failure.is_none() {
        for name in new_names {
            let live = save_dir.join(name);
            if let Err(e) = rename(&staging.join(name), &live) {
                failure = Some(
                    anyhow::Error::new(e).context(format!("putting {} in place", live.display())),
                );
                break;
            }
            placed.push(name);
        }
    }
    let Some(error) = failure else {
        return Ok(());
    };

    let mut undo_failed = false;
    for name in placed.iter().rev() {
        undo_failed |= rename(&save_dir.join(name), &staging.join(name)).is_err();
    }
    for name in moved_aside.iter().rev() {
        undo_failed |= rename(&aside.join(name), &save_dir.join(name)).is_err();
    }
    if undo_failed {
        bail!(
            "{error:#}. Putting the previous save back also failed: its files are in {}. \
             Copy them back into {} before you start the game",
            aside.display(),
            save_dir.display()
        );
    }
    Err(error.context("the save was put back as it was"))
}

/// Whether `aside` holds exactly the files `backed_up` records, byte for byte.
fn replaced_match(aside: &Path, backed_up: &[FileHash]) -> bool {
    let Ok(entries) = std::fs::read_dir(aside) else {
        return false;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut expected: Vec<String> = backed_up.iter().map(|f| f.name.clone()).collect();
    expected.sort();
    !backed_up.is_empty() && names == expected && snapshot::check_files(aside, backed_up).is_ok()
}

/// Remove extraction folders a cut-off restore left in `save_dir`. They hold
/// copies of a snapshot, never the only copy of anything. Folders of replaced
/// files are never removed here.
fn remove_leftover_staging(save_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(save_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_staging = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(STAGING_PREFIX));
        let is_dir = entry
            .file_type()
            .is_ok_and(|t| t.is_dir() && !t.is_symlink());
        if is_staging && is_dir {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn unique_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{prefix}{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "76561190000000001";

    struct Env {
        _root: tempfile::TempDir,
        elden_root: PathBuf,
        save_dir: PathBuf,
        dest: PathBuf,
    }

    fn env() -> Env {
        let root = tempfile::tempdir().unwrap();
        let elden_root = root.path().join("EldenRing");
        let save_dir = elden_root.join(ID);
        let dest = root.path().join("backups");
        std::fs::create_dir_all(&save_dir).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        Env {
            _root: root,
            elden_root,
            save_dir,
            dest,
        }
    }

    impl Env {
        fn write(&self, name: &str, bytes: &[u8]) {
            std::fs::write(self.save_dir.join(name), bytes).unwrap();
        }

        fn read(&self, name: &str) -> Option<Vec<u8>> {
            std::fs::read(self.save_dir.join(name)).ok()
        }

        fn backup(&self) -> Snapshot {
            let candidate = discovery::find(&self.elden_root, ID).unwrap();
            snapshot::create(&self.dest, ID, &candidate.source_files(), Reason::Manual)
                .unwrap()
                .unwrap()
        }

        fn request<'a>(&'a self, snapshot: &'a Snapshot) -> Request<'a> {
            Request {
                dest: &self.dest,
                steamid: ID,
                snapshot_dir: &snapshot.dir,
                elden_root: &self.elden_root,
            }
        }

        /// Names in the save folder other than the save files.
        fn extra_entries(&self) -> Vec<String> {
            std::fs::read_dir(&self.save_dir)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with("ER0000"))
                .collect()
        }
    }

    #[test]
    fn restores_after_backing_up_the_current_save() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        e.write("ER0000.sl2.bak", b"good-bak");
        let good = e.backup();
        e.write("ER0000.sl2", b"ruined");
        e.write("ER0000.sl2.bak", b"ruined-bak");

        let restored = restore(&e.request(&good), || false).unwrap();
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"good");
        assert_eq!(e.read("ER0000.sl2.bak").unwrap(), b"good-bak");
        assert_eq!(restored.snapshot.dir, good.dir);
        assert!(restored.kept_aside.is_none());
        assert_eq!(e.extra_entries(), Vec::<String>::new());

        // The ruined save was backed up first, and restores in turn.
        let backup = restored.backup.unwrap();
        assert_eq!(backup.metadata.reason, Reason::Manual);
        restore(&e.request(&backup), || false).unwrap();
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"ruined");
    }

    #[test]
    fn restoring_the_save_already_in_place_makes_no_new_backup() {
        let e = env();
        e.write("ER0000.sl2", b"same");
        let snap = e.backup();
        let restored = restore(&e.request(&snap), || false).unwrap();
        assert!(restored.backup.is_none());
        assert!(restored.kept_aside.is_none());
        assert_eq!(snapshot::list(&e.dest, ID).len(), 1);
        assert_eq!(e.extra_entries(), Vec::<String>::new());
    }

    #[test]
    fn a_current_bak_the_backup_lacks_is_moved_aside() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let good = e.backup();
        e.write("ER0000.sl2", b"ruined");
        e.write("ER0000.sl2.bak", b"ruined-bak");

        let restored = restore(&e.request(&good), || false).unwrap();
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"good");
        assert!(e.read("ER0000.sl2.bak").is_none());
        assert_eq!(restored.backup.unwrap().metadata.files.len(), 2);
        assert_eq!(e.extra_entries(), Vec::<String>::new());
    }

    #[test]
    fn refuses_while_the_game_runs() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let good = e.backup();
        e.write("ER0000.sl2", b"ruined");
        let error = restore(&e.request(&good), || true).unwrap_err();
        assert!(error.to_string().contains("running"), "{error:#}");
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"ruined");
        assert_eq!(snapshot::list(&e.dest, ID).len(), 1, "no backup was made");
    }

    #[test]
    fn stops_when_the_game_starts_before_the_files_move() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let good = e.backup();
        e.write("ER0000.sl2", b"ruined");
        let checks = std::cell::Cell::new(0);
        let error = restore(&e.request(&good), || {
            checks.set(checks.get() + 1);
            checks.get() > 1
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("started"), "{error:#}");
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"ruined");
        assert_eq!(e.extra_entries(), Vec::<String>::new());
    }

    #[test]
    fn refuses_a_damaged_backup() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let good = e.backup();
        let archive = good.dir.join(snapshot::ARCHIVE_FILE);
        let mut bytes = std::fs::read(&archive).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        std::fs::write(&archive, bytes).unwrap();
        e.write("ER0000.sl2", b"ruined");

        let error = restore(&e.request(&good), || false).unwrap_err();
        assert!(
            format!("{error:#}").contains("failed its check"),
            "{error:#}"
        );
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"ruined");
        assert_eq!(e.extra_entries(), Vec::<String>::new());
    }

    #[test]
    fn refuses_another_accounts_backup_and_a_folder_outside_the_destination() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let other = snapshot::create(
            &e.dest,
            "222",
            &[e.save_dir.join("ER0000.sl2")],
            Reason::Manual,
        )
        .unwrap()
        .unwrap();
        e.write("ER0000.sl2", b"ruined");
        assert!(restore(&e.request(&other), || false).is_err());

        let copy = e.dest.join("copy");
        std::fs::create_dir_all(&copy).unwrap();
        for name in [snapshot::ARCHIVE_FILE, snapshot::METADATA_FILE] {
            std::fs::copy(other.dir.join(name), copy.join(name)).unwrap();
        }
        let outside = Request {
            snapshot_dir: &copy,
            ..e.request(&other)
        };
        assert!(restore(&outside, || false).is_err());
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"ruined");
    }

    #[test]
    fn nothing_is_restored_when_the_current_save_cannot_be_backed_up() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let good = e.backup();
        // Over the size limit, so the backup refuses it.
        let file = std::fs::File::create(e.save_dir.join("ER0000.sl2")).unwrap();
        file.set_len(snapshot::MAX_SOURCE_FILE_BYTES + 1).unwrap();
        drop(file);

        let error = restore(&e.request(&good), || false).unwrap_err();
        assert!(
            format!("{error:#}").contains("could not back up"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::metadata(e.save_dir.join("ER0000.sl2"))
                .unwrap()
                .len(),
            snapshot::MAX_SOURCE_FILE_BYTES + 1
        );
        assert_eq!(e.extra_entries(), Vec::<String>::new());
    }

    /// Lay out a save folder mid-restore: live files, staged files, an empty
    /// folder for the replaced ones.
    fn swap_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let save = root.path().join("save");
        let staging = save.join(".staging");
        let aside = save.join(".aside");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&aside).unwrap();
        std::fs::write(save.join("ER0000.sl2"), b"old").unwrap();
        std::fs::write(save.join("ER0000.sl2.bak"), b"old-bak").unwrap();
        std::fs::write(staging.join("ER0000.sl2"), b"new").unwrap();
        std::fs::write(staging.join("ER0000.sl2.bak"), b"new-bak").unwrap();
        (root, save, staging, aside)
    }

    fn names() -> Vec<String> {
        vec!["ER0000.sl2".into(), "ER0000.sl2.bak".into()]
    }

    #[test]
    fn a_failed_rename_puts_the_save_back() {
        // Fail each rename in turn: the two moves aside, then the two moves in.
        for fail_at in 1..=4 {
            let (_root, save, staging, aside) = swap_fixture();
            let mut calls = 0;
            let error = swap_files(&save, &staging, &aside, &names(), &names(), |from, to| {
                calls += 1;
                if calls == fail_at {
                    return Err(std::io::Error::other("disk said no"));
                }
                std::fs::rename(from, to)
            })
            .unwrap_err();
            assert!(format!("{error:#}").contains("put back"), "{error:#}");
            assert_eq!(std::fs::read(save.join("ER0000.sl2")).unwrap(), b"old");
            assert_eq!(
                std::fs::read(save.join("ER0000.sl2.bak")).unwrap(),
                b"old-bak"
            );
            assert_eq!(std::fs::read_dir(&aside).unwrap().count(), 0);
        }
    }

    #[test]
    fn a_failed_undo_names_the_folder_that_holds_the_previous_save() {
        let (_root, save, staging, aside) = swap_fixture();
        let mut calls = 0;
        // Both files go aside, the first new one fails, and so does every undo.
        let error = swap_files(&save, &staging, &aside, &names(), &names(), |from, to| {
            calls += 1;
            if calls >= 3 {
                return Err(std::io::Error::other("disk said no"));
            }
            std::fs::rename(from, to)
        })
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&aside.display().to_string()), "{message}");
        assert_eq!(std::fs::read(aside.join("ER0000.sl2")).unwrap(), b"old");
        assert_eq!(
            std::fs::read(aside.join("ER0000.sl2.bak")).unwrap(),
            b"old-bak"
        );
    }

    #[test]
    fn replaced_files_that_match_no_backup_are_kept() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let good = e.backup();
        e.write("ER0000.sl2", b"ruined");
        // A rename that also changes the file it moves aside, as if the game
        // wrote it at that moment.
        let restored = restore_with(
            &e.request(&good),
            || false,
            |from, to| {
                std::fs::rename(from, to)?;
                if to
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n.to_string_lossy().starts_with(REPLACED_PREFIX))
                {
                    std::fs::write(to, b"changed")?;
                }
                Ok(())
            },
        )
        .unwrap();
        let kept = restored.kept_aside.unwrap();
        assert_eq!(std::fs::read(kept.join("ER0000.sl2")).unwrap(), b"changed");
        assert_eq!(e.read("ER0000.sl2").unwrap(), b"good");
    }

    #[test]
    fn leftover_extraction_folders_are_removed_and_replaced_ones_kept() {
        let e = env();
        e.write("ER0000.sl2", b"good");
        let good = e.backup();
        let staging = e.save_dir.join(format!("{STAGING_PREFIX}1-1"));
        let replaced = e.save_dir.join(format!("{REPLACED_PREFIX}1-1"));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&replaced).unwrap();
        std::fs::write(replaced.join("ER0000.sl2"), b"keep").unwrap();
        restore(&e.request(&good), || false).unwrap();
        assert!(!staging.exists());
        assert!(replaced.join("ER0000.sl2").exists());
    }
}
