//! Per-SteamID snapshot retention. Only ever deletes finalized snapshot
//! directories that live directly under `<dest>/snapshots/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::snapshot::{self, Checked, Reason, Snapshot};

/// How many of the newest pre-launch snapshots, and separately of the newest
/// post-exit snapshots, retention keeps on top of the count the user sets. A
/// burst of periodic or manual backups then cannot push out the saves from
/// the start and the end of recent sessions.
pub const KEEP_PER_SESSION_STAGE: usize = 10;

/// Wait for any other backup to `dest`, then snapshot the files
/// `source_files` returns and, only once the snapshot is finalized, prune the
/// account's snapshots down to `keep`. Returns the new snapshot and the ones
/// removed, or `Ok(None)` when the save is unchanged and nothing was created
/// or pruned.
///
/// The files are collected only once the wait is over, because the game may
/// write or remove its `.bak` in the meantime.
///
/// # Errors
///
/// Returns an error if `source_files` or [`snapshot::create`] fails; nothing
/// is pruned then.
pub fn create_and_prune(
    dest: &Path,
    steamid: &str,
    source_files: impl FnOnce() -> Result<Vec<PathBuf>>,
    reason: Reason,
    keep: usize,
) -> Result<Option<(Snapshot, Vec<Snapshot>)>> {
    let _lock = lock(dest, Wait::Yes)?;
    create_and_prune_locked(dest, steamid, &source_files()?, reason, keep)
}

/// [`create_and_prune`], unless another backup holds the lock, which is an
/// error at once. The background monitor uses it: waiting before the game
/// starts would hold up the launch.
///
/// # Errors
///
/// Returns an error if another backup is running or [`snapshot::create`]
/// fails; nothing is pruned then.
pub fn try_create_and_prune(
    dest: &Path,
    steamid: &str,
    source_files: &[PathBuf],
    reason: Reason,
    keep: usize,
) -> Result<Option<(Snapshot, Vec<Snapshot>)>> {
    let _lock = lock(dest, Wait::No)?;
    create_and_prune_locked(dest, steamid, source_files, reason, keep)
}

fn create_and_prune_locked(
    dest: &Path,
    steamid: &str,
    source_files: &[PathBuf],
    reason: Reason,
    keep: usize,
) -> Result<Option<(Snapshot, Vec<Snapshot>)>> {
    // The lock is held, so no backup is writing into a temp folder now.
    snapshot::remove_temp_dirs(dest);
    let Some(snapshot) = snapshot::create(dest, steamid, source_files, reason)? else {
        return Ok(None);
    };
    let removed = prune(dest, steamid, keep, Some(&snapshot.dir));
    Ok(Some((snapshot, removed)))
}

/// Delete the snapshots retention does not keep. Returns the ones removed.
///
/// Retention keeps the newest `keep` snapshots, plus the newest
/// [`KEEP_PER_SESSION_STAGE`] pre-launch and the newest
/// [`KEEP_PER_SESSION_STAGE`] post-exit snapshots. It is applied only after a
/// new snapshot is finalized, so a live snapshot is never deleted before its
/// replacement exists. Only snapshots that pass their check in
/// [`snapshot::list_all_rehashed`] are kept by any of these rules. A damaged
/// one is removed once it is older than every snapshot retention keeps, and
/// never while no good snapshot is kept. A snapshot that cannot be removed is
/// logged and left in place.
#[must_use]
pub fn apply(dest: &Path, steamid: &str, keep: usize) -> Vec<Snapshot> {
    let Ok(_lock) =
        lock(dest, Wait::Yes).inspect_err(|e| tracing::warn!("retention skipped: {e:#}"))
    else {
        return Vec::new();
    };
    prune(dest, steamid, keep, None)
}

/// Whether [`lock`] waits for another backup to finish.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Yes,
    No,
}

/// Wait for, then hold, the lock every backup and prune of `dest` takes. A
/// restore holds it from its own backup to the last file it moves.
///
/// # Errors
///
/// Returns an error if the lock file cannot be created or locked.
pub(crate) fn lock_waiting(dest: &Path) -> Result<std::fs::File> {
    lock(dest, Wait::Yes)
}

/// Holds the lock on `<dest>/snapshots` that every backup and prune takes,
/// across processes, waiting for it or not. Without it the window and the
/// monitor could each make a snapshot, then each prune the other's, leaving
/// none.
fn lock(dest: &Path, wait: Wait) -> Result<std::fs::File> {
    let dir = snapshot::ensure_snapshots_dir(dest)?;
    let path = dir.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match wait {
        Wait::Yes => file
            .lock()
            .with_context(|| format!("locking {}", path.display()))?,
        Wait::No => file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                anyhow::anyhow!("another backup of this destination is running")
            }
            std::fs::TryLockError::Error(error) => {
                anyhow::Error::new(error).context(format!("locking {}", path.display()))
            }
        })?,
    }
    Ok(file)
}

/// [`apply`], never removing the snapshot in `newest`.
///
/// Snapshots sort by the time recorded when each was made. After the clock
/// is set back, the snapshot just made can sort before older ones, and plain
/// [`apply`] would delete the one it was called to make room for. The one in
/// `newest` takes one of the `keep` slots and the rest go to the others,
/// newest first.
fn prune(dest: &Path, steamid: &str, keep: usize, newest: Option<&Path>) -> Vec<Snapshot> {
    let snaps = snapshot::list_all_rehashed(dest, steamid); // oldest → newest, temp dirs excluded
    let remove = to_remove(&snaps, keep, newest);
    let snaps_dir = snapshot::snapshots_dir(dest);
    let mut removed = Vec::new();
    for (index, checked) in snaps.into_iter().enumerate() {
        if !remove[index] {
            continue;
        }
        let snap = checked.snapshot;
        // Defense in depth: never delete anything outside the managed tree.
        if snap.dir.parent() != Some(snaps_dir.as_path()) {
            continue;
        }
        match std::fs::remove_dir_all(&snap.dir) {
            Ok(()) => removed.push(snap),
            Err(e) => tracing::warn!("could not remove old snapshot {}: {e}", snap.dir.display()),
        }
    }
    removed
}

/// For each of `snaps`, sorted oldest → newest, whether retention removes it.
/// See [`apply`] for the rules.
fn to_remove(snaps: &[Checked], keep: usize, newest: Option<&Path>) -> Vec<bool> {
    let good: Vec<usize> = (0..snaps.len())
        .filter(|&index| !snaps[index].is_damaged())
        .collect();
    let mut kept = vec![false; snaps.len()];

    let mut slots = keep.max(1);
    if let Some(newest) = newest
        && let Some(&index) = good.iter().find(|&&i| snaps[i].snapshot.dir == newest)
    {
        kept[index] = true;
        slots -= 1;
    }
    for &index in good.iter().rev() {
        if slots == 0 {
            break;
        }
        if !kept[index] {
            kept[index] = true;
            slots -= 1;
        }
    }
    for stage in [Reason::PreLaunch, Reason::PostExit] {
        for &index in good
            .iter()
            .rev()
            .filter(|&&i| snaps[i].snapshot.metadata.reason == stage)
            .take(KEEP_PER_SESSION_STAGE)
        {
            kept[index] = true;
        }
    }

    let oldest_kept = good
        .iter()
        .filter(|&&i| kept[i])
        .map(|&i| snaps[i].snapshot.metadata.created_utc)
        .min();
    snaps
        .iter()
        .enumerate()
        .map(|(index, checked)| {
            if checked.is_damaged() {
                oldest_kept.is_some_and(|oldest| checked.snapshot.metadata.created_utc < oldest)
            } else {
                !kept[index]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::create;

    fn make_snapshots(dest: &Path, n: usize) {
        let src = dest.join("ER0000.sl2");
        for i in 0..n {
            std::fs::write(&src, format!("content-{i}").as_bytes()).unwrap();
            create(dest, "111", std::slice::from_ref(&src), Reason::Periodic)
                .unwrap()
                .expect("distinct content each time");
            // Ensure distinct finalize-dir timestamps / ordering.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn keeps_newest_n() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 5);
        let removed = apply(&d, "111", 3);
        assert_eq!(removed.len(), 2);
        let remaining = snapshot::list(&d, "111");
        assert_eq!(remaining.len(), 3);
        // The survivors are the newest three.
        let newest = remaining.last().unwrap();
        let out = newest.dir.join("extracted");
        snapshot::extract(&newest.dir, &out).unwrap();
        assert_eq!(std::fs::read(out.join("ER0000.sl2")).unwrap(), b"content-4");
    }

    #[test]
    fn create_and_prune_prunes_only_after_a_new_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 3);
        let src = d.join("ER0000.sl2");
        let sources = || Ok(vec![src.clone()]);

        assert!(
            create_and_prune(&d, "111", sources, Reason::Periodic, 1)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            snapshot::list(&d, "111").len(),
            3,
            "unchanged save prunes nothing"
        );

        std::fs::write(&src, b"content-new").unwrap();
        let (created, removed) = create_and_prune(&d, "111", sources, Reason::Periodic, 1)
            .unwrap()
            .unwrap();
        assert_eq!(removed.len(), 3);
        let remaining = snapshot::list(&d, "111");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].dir, created.dir);
    }

    #[test]
    fn the_new_snapshot_survives_a_clock_set_back() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 1);
        // The earlier snapshot was taken while the clock ran a year ahead, so
        // the next one sorts before it.
        let earlier = snapshot::list(&d, "111").remove(0);
        let metadata = earlier.dir.join(snapshot::METADATA_FILE);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&metadata).unwrap()).unwrap();
        let ahead = earlier.metadata.created_utc + chrono::Duration::days(365);
        json["created_utc"] = serde_json::json!(ahead);
        std::fs::write(&metadata, serde_json::to_vec(&json).unwrap()).unwrap();

        let src = d.join("ER0000.sl2");
        std::fs::write(&src, b"after the clock was fixed").unwrap();
        let (created, removed) =
            create_and_prune(&d, "111", || Ok(vec![src.clone()]), Reason::Periodic, 1)
                .unwrap()
                .unwrap();
        assert!(created.dir.exists(), "retention deleted the new snapshot");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].dir, earlier.dir);
        assert_eq!(snapshot::list(&d, "111").len(), 1);
    }

    #[test]
    fn backups_hold_a_lock_that_shuts_out_another_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 1);
        let held = lock(&d, Wait::Yes).unwrap();
        // The monitor's backup does not wait: it fails at once and makes nothing.
        let src = d.join("ER0000.sl2");
        std::fs::write(&src, b"changed while another backup runs").unwrap();
        let busy = try_create_and_prune(&d, "111", std::slice::from_ref(&src), Reason::Periodic, 5);
        assert!(busy.unwrap_err().to_string().contains("another backup"));
        assert_eq!(snapshot::list(&d, "111").len(), 1);
        let other = std::fs::OpenOptions::new()
            .write(true)
            .open(snapshot::snapshots_dir(&d).join(".lock"))
            .unwrap();
        assert!(other.try_lock().is_err(), "a second backup must wait");
        drop(held);
        assert!(other.try_lock().is_ok());
        // The lock file is not a snapshot.
        assert_eq!(snapshot::list(&d, "111").len(), 1);
    }

    #[test]
    fn a_waiting_backup_collects_the_files_once_the_other_is_done() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 1);
        let src = d.join("ER0000.sl2");
        std::fs::write(&src, b"changed while another backup runs").unwrap();
        let held = lock(&d, Wait::Yes).unwrap();
        let released = AtomicBool::new(false);

        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                create_and_prune(
                    &d,
                    "111",
                    || {
                        assert!(released.load(Ordering::SeqCst), "collected while waiting");
                        Ok(vec![src.clone()])
                    },
                    Reason::Manual,
                    5,
                )
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            released.store(true, Ordering::SeqCst);
            drop(held);
            assert!(waiting.join().unwrap().unwrap().is_some());
        });
        assert_eq!(snapshot::list(&d, "111").len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_snapshots_folder_is_neither_written_nor_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        make_snapshots(&live, 2);
        // The destination's snapshots folder points at another tree.
        let d = tmp.path().join("dest");
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(snapshot::snapshots_dir(&live), snapshot::snapshots_dir(&d))
            .unwrap();
        let src = d.join("ER0000.sl2");
        std::fs::write(&src, b"new").unwrap();

        assert!(create_and_prune(&d, "111", || Ok(vec![src.clone()]), Reason::Manual, 1).is_err());
        assert!(snapshot::list(&d, "111").is_empty());
        assert!(apply(&d, "111", 1).is_empty());
        assert_eq!(snapshot::list(&live, "111").len(), 2);
    }

    fn corrupt(snapshot: &Snapshot) {
        let path = snapshot.dir.join(snapshot::ARCHIVE_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
    }

    #[test]
    fn a_damaged_snapshot_newer_than_every_kept_one_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 3);
        let snaps = snapshot::list(&d, "111");
        // Flip one byte of the newest archive without changing its size.
        corrupt(&snaps[2]);

        let removed = apply(&d, "111", 1);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].dir, snaps[0].dir);
        assert!(snaps[2].dir.exists(), "it is newer than the kept snapshot");
        assert!(
            snaps[1].dir.exists(),
            "the damaged snapshot does not take the one kept slot"
        );
    }

    #[test]
    fn a_damaged_snapshot_older_than_every_kept_one_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 3);
        let snaps = snapshot::list(&d, "111");
        corrupt(&snaps[0]);
        // A missing archive is damage too.
        std::fs::remove_file(snaps[1].dir.join(snapshot::ARCHIVE_FILE)).unwrap();

        let removed: Vec<_> = apply(&d, "111", 1).into_iter().map(|s| s.dir).collect();
        assert_eq!(removed, vec![snaps[0].dir.clone(), snaps[1].dir.clone()]);
        assert!(snaps[2].dir.exists());
    }

    #[test]
    fn damaged_snapshots_stay_while_no_good_one_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 2);
        let snaps = snapshot::list(&d, "111");
        corrupt(&snaps[0]);
        corrupt(&snaps[1]);
        assert!(apply(&d, "111", 1).is_empty());
        assert!(snaps.iter().all(|s| s.dir.exists()));
    }

    /// A listed snapshot made at `minute`, without touching the disk.
    fn listed(minute: i64, reason: Reason, damaged: bool) -> Checked {
        let created = chrono::DateTime::from_timestamp(1_700_000_000 + minute * 60, 0).unwrap();
        Checked {
            snapshot: Snapshot {
                dir: PathBuf::from(format!("/snapshots/{minute}")),
                metadata: snapshot::Metadata {
                    format_version: snapshot::METADATA_VERSION,
                    steamid: "111".into(),
                    created_utc: created,
                    source_modified_utc: None,
                    reason,
                    app_version: String::new(),
                    files: Vec::new(),
                    archive: snapshot::ARCHIVE_FILE.into(),
                    stored_bytes: 0,
                    archive_sha256: String::new(),
                },
            },
            damage: damaged.then(|| "damaged".to_string()),
        }
    }

    fn removed_minutes(snaps: &[Checked], keep: usize) -> Vec<i64> {
        to_remove(snaps, keep, None)
            .into_iter()
            .zip(snaps)
            .filter(|(remove, _)| *remove)
            .map(|(_, checked)| {
                (checked.snapshot.metadata.created_utc.timestamp() - 1_700_000_000) / 60
            })
            .collect()
    }

    #[test]
    fn the_newest_pre_launch_and_post_exit_snapshots_are_kept_on_top_of_the_count() {
        // 12 sessions, each a pre-launch, two periodic, and a post-exit
        // snapshot, then 30 manual ones.
        let mut snaps = Vec::new();
        let mut minute = 0;
        for _ in 0..12 {
            for reason in [
                Reason::PreLaunch,
                Reason::Periodic,
                Reason::Periodic,
                Reason::PostExit,
            ] {
                snaps.push(listed(minute, reason, false));
                minute += 1;
            }
        }
        for _ in 0..30 {
            snaps.push(listed(minute, Reason::Manual, false));
            minute += 1;
        }

        let remove = to_remove(&snaps, 5, None);
        let kept: Vec<&Checked> = snaps
            .iter()
            .zip(&remove)
            .filter(|(_, remove)| !**remove)
            .map(|(checked, _)| checked)
            .collect();
        let count = |reason| {
            kept.iter()
                .filter(|c| c.snapshot.metadata.reason == reason)
                .count()
        };
        assert_eq!(count(Reason::Manual), 5, "the count keeps the newest five");
        assert_eq!(count(Reason::PreLaunch), KEEP_PER_SESSION_STAGE);
        assert_eq!(count(Reason::PostExit), KEEP_PER_SESSION_STAGE);
        assert_eq!(count(Reason::Periodic), 0);
        // The two oldest sessions' pre-launch and post-exit snapshots go.
        assert!(remove[0] && remove[3] && remove[4] && remove[7]);
        assert!(!remove[8] && !remove[11]);
    }

    #[test]
    fn damaged_snapshots_count_for_no_rule() {
        let snaps = vec![
            listed(0, Reason::PreLaunch, false),
            listed(1, Reason::PreLaunch, true),
            listed(2, Reason::PostExit, true),
            listed(3, Reason::Manual, false),
            listed(4, Reason::Manual, true),
        ];
        // Keep 1: the manual at 3 by count, the pre-launch at 0 by stage.
        // Nothing damaged is older than 0, so all three damaged ones stay.
        assert_eq!(removed_minutes(&snaps, 1), Vec::<i64>::new());

        let snaps = vec![
            listed(0, Reason::PostExit, true),
            listed(1, Reason::Periodic, false),
            listed(2, Reason::Periodic, false),
        ];
        assert_eq!(removed_minutes(&snaps, 1), vec![0, 1]);
    }

    #[test]
    fn create_and_prune_removes_leftover_temp_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 1);
        let snaps_dir = snapshot::snapshots_dir(&d);
        // What a backup cut off by a power cut leaves behind.
        let stray = snaps_dir.join(".tmp-999-999");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join(snapshot::ARCHIVE_FILE), b"partial").unwrap();
        // A file with the prefix is not a temp folder.
        let file = snaps_dir.join(".tmp-file");
        std::fs::write(&file, b"keep").unwrap();
        #[cfg(unix)]
        let link = {
            let outside = d.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("keep"), b"keep").unwrap();
            let link = snaps_dir.join(".tmp-link");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            link
        };

        let src = d.join("ER0000.sl2");
        std::fs::write(&src, b"new content").unwrap();
        create_and_prune(&d, "111", || Ok(vec![src.clone()]), Reason::Manual, 5)
            .unwrap()
            .unwrap();
        assert!(!stray.exists());
        assert!(file.exists());
        #[cfg(unix)]
        {
            assert!(link.symlink_metadata().is_ok(), "a link is left alone");
            assert!(d.join("outside").join("keep").exists());
        }
        assert_eq!(snapshot::list(&d, "111").len(), 2);
    }

    #[test]
    fn under_limit_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 2);
        assert!(apply(&d, "111", 60).is_empty());
        assert_eq!(snapshot::list(&d, "111").len(), 2);
    }

    #[test]
    fn ignores_temp_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        make_snapshots(&d, 2);
        // A stray temp dir must be neither counted nor deleted by retention.
        let stray = snapshot::snapshots_dir(&d).join(".tmp-999-999");
        std::fs::create_dir_all(&stray).unwrap();
        let _ = apply(&d, "111", 1);
        assert!(stray.exists());
        assert_eq!(snapshot::list(&d, "111").len(), 1);
    }

    #[test]
    fn retention_never_removes_another_accounts_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().to_path_buf();
        let src = d.join("ER0000.sl2");
        for i in 0..2 {
            std::fs::write(&src, format!("account-111-{i}")).unwrap();
            create(&d, "111", std::slice::from_ref(&src), Reason::Periodic)
                .unwrap()
                .unwrap();
        }
        for i in 0..2 {
            std::fs::write(&src, format!("account-222-{i}")).unwrap();
            create(&d, "222", std::slice::from_ref(&src), Reason::Periodic)
                .unwrap()
                .unwrap();
        }

        assert_eq!(apply(&d, "111", 1).len(), 1);
        assert_eq!(snapshot::list(&d, "111").len(), 1);
        assert_eq!(snapshot::list(&d, "222").len(), 2);
    }
}
