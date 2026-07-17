//! Filesystem-only discovery of vanilla Elden Ring save directories.
//! No Steam APIs, registry, or `loginusers.vdf` — a valid candidate is simply
//! a numeric `SteamID64` folder containing a `.sl2` save.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct SaveCandidate {
    pub steamid: String,
    pub dir: PathBuf,
    /// Primary vanilla save file (`ER0000.sl2` when present).
    pub save_file: PathBuf,
    /// Sibling `.sl2.bak` if it exists.
    pub bak_file: Option<PathBuf>,
    pub modified: Option<SystemTime>,
    pub size: u64,
}

fn is_steamid_dir_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

fn primary_sl2(dir: &Path) -> Option<PathBuf> {
    let mut sl2: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        // vanilla saves only: `.sl2`, never `.co2` (Seamless Co-op) or `.sl2.bak`.
        if p.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("sl2"))
        {
            sl2.push(p);
        }
    }
    if sl2.is_empty() {
        return None;
    }
    // Prefer the canonical ER0000.sl2, else the first found (deterministic).
    sl2.sort();
    sl2.iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("ER0000.sl2"))
        })
        .cloned()
        .or_else(|| sl2.into_iter().next())
}

fn candidate_for(dir: &Path, steamid: &str) -> Option<SaveCandidate> {
    let save_file = primary_sl2(dir)?;
    let bak = {
        let mut b = save_file.clone().into_os_string();
        b.push(".bak");
        let b = PathBuf::from(b);
        b.is_file().then_some(b)
    };
    let meta = std::fs::metadata(&save_file).ok();
    Some(SaveCandidate {
        steamid: steamid.to_string(),
        dir: dir.to_path_buf(),
        save_file,
        bak_file: bak,
        modified: meta.as_ref().and_then(|m| m.modified().ok()),
        size: meta.map_or(0, |m| m.len()),
    })
}

/// Enumerate valid save candidates under `elden_root` (`%APPDATA%\EldenRing`),
/// newest save first. Returns empty if the root is missing.
#[must_use]
pub fn discover(elden_root: &Path) -> Vec<SaveCandidate> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(elden_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_steamid_dir_name(name) {
            continue;
        }
        if let Some(c) = candidate_for(&path, name) {
            out.push(c);
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.modified));
    out
}

/// Find the candidate whose folder name matches `steamid`.
#[must_use]
pub fn find(elden_root: &Path, steamid: &str) -> Option<SaveCandidate> {
    discover(elden_root)
        .into_iter()
        .find(|c| c.steamid == steamid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "erbt-disc-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mkdir_save(root: &Path, name: &str, save: Option<&str>, bak: bool) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        if let Some(s) = save {
            std::fs::write(d.join(s), b"save").unwrap();
            if bak {
                std::fs::write(d.join(format!("{s}.bak")), b"bak").unwrap();
            }
        }
    }

    #[test]
    fn single_valid_dir() {
        let r = root();
        mkdir_save(&r, "76561198000000001", Some("ER0000.sl2"), true);
        let c = discover(&r);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].steamid, "76561198000000001");
        assert!(c[0].bak_file.is_some());
    }

    #[test]
    fn ignores_junk() {
        let r = root();
        mkdir_save(&r, "76561198000000001", Some("ER0000.sl2"), false);
        mkdir_save(&r, "Copies", Some("ER0000.sl2"), false); // non-numeric
        mkdir_save(&r, "76561198000000002", None, false); // numeric but no .sl2
        std::fs::write(r.join("GraphicsConfig.xml"), b"<xml/>").unwrap(); // a file
        mkdir_save(&r, "999", Some("ER0000.co2"), false); // co-op save only
        let c = discover(&r);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].steamid, "76561198000000001");
    }

    #[test]
    fn multiple_dirs() {
        let r = root();
        mkdir_save(&r, "111", Some("ER0000.sl2"), false);
        mkdir_save(&r, "222", Some("ER0000.sl2"), false);
        let c = discover(&r);
        assert_eq!(c.len(), 2);
        assert!(find(&r, "222").is_some());
        assert!(find(&r, "333").is_none());
    }

    #[test]
    fn missing_root() {
        assert!(discover(Path::new("/no/such/eldenring/root")).is_empty());
    }
}
