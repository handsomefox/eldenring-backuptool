//! Per-SteamID snapshot retention. Only ever deletes finalized snapshot
//! directories that live directly under `<dest>/snapshots/`.

use std::path::Path;

use anyhow::Result;

use crate::snapshot::{self, Snapshot};

/// Delete oldest snapshots beyond `keep`. Returns the directories removed.
/// Retention is applied only after a new snapshot is finalized, so a live
/// snapshot is never deleted before its replacement exists.
pub fn apply(dest: &Path, keep: usize) -> Result<Vec<Snapshot>> {
    let keep = keep.max(1);
    let snaps = snapshot::list(dest); // oldest → newest, temp dirs excluded
    if snaps.len() <= keep {
        return Ok(Vec::new());
    }
    let snaps_dir = snapshot::snapshots_dir(dest);
    let remove_count = snaps.len() - keep;
    let mut removed = Vec::new();
    for snap in snaps.into_iter().take(remove_count) {
        // Defense in depth: never delete anything outside the managed tree.
        if snap.dir.parent() != Some(snaps_dir.as_path()) {
            continue;
        }
        if std::fs::remove_dir_all(&snap.dir).is_ok() {
            removed.push(snap);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Reason, create};
    use std::path::PathBuf;

    fn dest() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "erbt-ret-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

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
        let d = dest();
        make_snapshots(&d, 5);
        let removed = apply(&d, 3).unwrap();
        assert_eq!(removed.len(), 2);
        let remaining = snapshot::list(&d);
        assert_eq!(remaining.len(), 3);
        // The survivors are the newest three.
        let newest = remaining.last().unwrap();
        let out = newest.dir.join("extracted");
        snapshot::extract(&newest.dir, &out).unwrap();
        assert_eq!(std::fs::read(out.join("ER0000.sl2")).unwrap(), b"content-4");
    }

    #[test]
    fn under_limit_deletes_nothing() {
        let d = dest();
        make_snapshots(&d, 2);
        assert!(apply(&d, 60).unwrap().is_empty());
        assert_eq!(snapshot::list(&d).len(), 2);
    }

    #[test]
    fn ignores_temp_dirs() {
        let d = dest();
        make_snapshots(&d, 2);
        // A stray temp dir must be neither counted nor deleted by retention.
        let stray = snapshot::snapshots_dir(&d).join(".tmp-999-999");
        std::fs::create_dir_all(&stray).unwrap();
        apply(&d, 1).unwrap();
        assert!(stray.exists());
        assert_eq!(snapshot::list(&d).len(), 1);
    }
}
