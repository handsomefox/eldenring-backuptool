//! Per-SteamID snapshot retention. Only ever deletes finalized snapshot
//! directories that live directly under `<dest>/snapshots/`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::snapshot::{self, Reason, Snapshot};

/// Create a snapshot of `source_files` and, only once it is finalized, prune
/// the account's snapshots down to `keep`. Returns the new snapshot and the
/// ones removed, or `Ok(None)` when the save is unchanged and nothing was
/// created or pruned.
///
/// # Errors
///
/// Returns an error if [`snapshot::create`] fails; nothing is pruned then.
pub fn create_and_prune(
    dest: &Path,
    steamid: &str,
    source_files: &[PathBuf],
    reason: Reason,
    keep: usize,
) -> Result<Option<(Snapshot, Vec<Snapshot>)>> {
    let Some(snapshot) = snapshot::create(dest, steamid, source_files, reason)? else {
        return Ok(None);
    };
    let removed = apply(dest, steamid, keep);
    Ok(Some((snapshot, removed)))
}

/// Delete oldest snapshots beyond `keep`. Returns the directories removed.
/// Retention is applied only after a new snapshot is finalized, so a live
/// snapshot is never deleted before its replacement exists. Retention uses the
/// same bounded structural listing as [`snapshot::list`] and does not hash
/// archive payloads. A snapshot that cannot be removed is logged and left in
/// place.
pub fn apply(dest: &Path, steamid: &str, keep: usize) -> Vec<Snapshot> {
    let keep = keep.max(1);
    let snaps = snapshot::list(dest, steamid); // oldest → newest, temp dirs excluded
    if snaps.len() <= keep {
        return Vec::new();
    }
    let snaps_dir = snapshot::snapshots_dir(dest);
    let remove_count = snaps.len() - keep;
    let mut removed = Vec::new();
    for snap in snaps.into_iter().take(remove_count) {
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
        let sources = std::slice::from_ref(&src);

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
        apply(&d, "111", 1);
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
