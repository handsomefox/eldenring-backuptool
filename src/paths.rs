//! Path resolution and the safety guards that keep backups out of the save
//! tree (and vice-versa). Known folders come from environment variables
//! (reliable, inherited by the Steam-launched monitor); Documents uses the
//! `directories` crate for OneDrive-redirect handling.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::APP_NAME;

fn env_dir(var: &str) -> Result<PathBuf> {
    let v =
        std::env::var_os(var).with_context(|| format!("environment variable {var} is not set"))?;
    if v.is_empty() {
        bail!("environment variable {var} is empty");
    }
    Ok(PathBuf::from(v))
}

/// `%APPDATA%` (Roaming).
pub fn appdata_roaming() -> Result<PathBuf> {
    env_dir("APPDATA")
}

/// `%LOCALAPPDATA%`.
pub fn local_appdata() -> Result<PathBuf> {
    env_dir("LOCALAPPDATA")
}

/// `%APPDATA%\EldenRing` — the save root that holds numeric SteamID64 dirs.
pub fn elden_ring_root() -> Result<PathBuf> {
    Ok(appdata_roaming()?.join("EldenRing"))
}

/// `%LOCALAPPDATA%\EldenRingSaveGuard` — config and logs.
pub fn app_data_dir() -> Result<PathBuf> {
    Ok(local_appdata()?.join(APP_NAME))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("config.json"))
}

pub fn log_dir() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("logs"))
}

/// Documents folder, honoring redirection/OneDrive where possible.
pub fn documents_dir() -> Result<PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(Path::to_path_buf))
        .context("could not resolve the Documents folder")
}

/// Default backup destination for a given SteamID64.
pub fn default_backup_dest(steamid: &str) -> Result<PathBuf> {
    Ok(default_backup_dest_in(&documents_dir()?, steamid))
}

/// Pure form of [`default_backup_dest`] for testing.
pub fn default_backup_dest_in(documents: &Path, steamid: &str) -> PathBuf {
    documents
        .join("Game Save Backups")
        .join("Elden Ring")
        .join(steamid)
}

/// Lexical normalization: resolves `.`/`..` textually without touching the
/// filesystem, so it works for paths that do not exist yet.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn cmp_components(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().into_owned();
            if cfg!(windows) { s.to_lowercase() } else { s }
        })
        .collect()
}

/// True if `child` is equal to or nested under `ancestor` (lexical, case-
/// insensitive on Windows).
pub fn is_within(child: &Path, ancestor: &Path) -> bool {
    let c = cmp_components(&normalize_lexical(child));
    let a = cmp_components(&normalize_lexical(ancestor));
    a.len() <= c.len() && c[..a.len()] == a[..]
}

/// Reject a backup destination that overlaps the live save directory in
/// either direction.
pub fn validate_backup_dest(save_dir: &Path, dest: &Path) -> Result<()> {
    if is_within(dest, save_dir) {
        bail!("the backup destination is inside the Elden Ring save folder");
    }
    if is_within(save_dir, dest) {
        bail!("the Elden Ring save folder is inside the backup destination");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_basic() {
        assert!(is_within(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(is_within(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!is_within(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(!is_within(Path::new("/a/x"), Path::new("/a/b")));
    }

    #[test]
    fn within_resolves_dotdot() {
        assert!(is_within(Path::new("/a/b/../b/c"), Path::new("/a/b")));
        assert!(!is_within(Path::new("/a/b/../x"), Path::new("/a/b")));
    }

    #[test]
    fn guard_rejects_overlap() {
        let save = Path::new("/save/76561198000000000");
        assert!(validate_backup_dest(save, Path::new("/save/76561198000000000/backups")).is_err());
        assert!(validate_backup_dest(save, Path::new("/documents/backups")).is_ok());
        // save inside dest
        assert!(
            validate_backup_dest(
                Path::new("/documents/backups/save"),
                Path::new("/documents/backups")
            )
            .is_err()
        );
    }

    #[test]
    fn default_dest_layout() {
        let d = default_backup_dest_in(Path::new("/home/u/Documents"), "76561198000000000");
        assert!(d.ends_with("Game Save Backups/Elden Ring/76561198000000000"));
    }
}
