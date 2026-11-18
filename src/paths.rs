//! Path resolution and the safety guards that keep backups out of the save
//! tree (and vice-versa). Known folders come from `app-folders`, which asks
//! the Windows shell and falls back to the environment; Documents uses the
//! `directories` crate for OneDrive-redirect handling.

use std::path::{Path, PathBuf};

use app_paths::Overlap;

use anyhow::{Context, Result, bail};

use crate::APP_NAME;

/// `%APPDATA%\EldenRing` — the save root that holds numeric `SteamID64` dirs.
///
/// # Errors
///
/// Returns an error if `%APPDATA%` cannot be resolved.
pub fn elden_ring_root() -> Result<PathBuf> {
    Ok(app_folders::roaming_app_data()
        .context("could not resolve the %APPDATA% folder")?
        .join("EldenRing"))
}

/// `%LOCALAPPDATA%\EldenRingSaveGuard` — config and logs. Off Windows,
/// `EldenRingSaveGuard` under the XDG data folder.
///
/// # Errors
///
/// Returns an error if `%LOCALAPPDATA%` cannot be resolved.
pub fn app_data_dir() -> Result<PathBuf> {
    app_folders::app_data_dir(APP_NAME).context("could not resolve the %LOCALAPPDATA% folder")
}

/// Return the application configuration path.
///
/// # Errors
///
/// Returns an error if the application data directory cannot be resolved.
pub fn config_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("config.json"))
}

/// Return the application log directory.
///
/// # Errors
///
/// Returns an error if the application data directory cannot be resolved.
pub fn log_dir() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("logs"))
}

/// Documents folder, honoring redirection/OneDrive where possible.
///
/// # Errors
///
/// Returns an error if the Documents known folder cannot be resolved.
pub fn documents_dir() -> Result<PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(Path::to_path_buf))
        .context("could not resolve the Documents folder")
}

/// Default backup destination for a given `SteamID64`.
///
/// # Errors
///
/// Returns an error if the Documents known folder cannot be resolved.
pub fn default_backup_dest(steamid: &str) -> Result<PathBuf> {
    Ok(default_backup_dest_in(&documents_dir()?, steamid))
}

/// Pure form of [`default_backup_dest`] for testing.
#[must_use]
pub fn default_backup_dest_in(documents: &Path, steamid: &str) -> PathBuf {
    documents
        .join("Game Save Backups")
        .join("Elden Ring")
        .join(steamid)
}

/// True if `child` is equal to or nested under `ancestor` (lexical, case-
/// insensitive on Windows).
#[must_use]
pub fn is_within(child: &Path, ancestor: &Path) -> bool {
    app_paths::is_within(child, ancestor)
}

/// Validate that a backup destination is absolute and separate from the save tree.
///
/// Both paths are compared after following every link in the part that
/// exists, so a junction or symlink cannot disguise an overlap, and a
/// destination folder that does not exist yet is still allowed.
///
/// # Errors
///
/// Returns an error when a path cannot be resolved safely or the paths overlap.
pub fn validate_backup_dest(save_dir: &Path, dest: &Path) -> Result<()> {
    if !dest.is_absolute() {
        bail!("the backup destination must be an absolute path");
    }
    let overlap = app_paths::overlap(dest, save_dir).with_context(|| {
        format!(
            "could not compare the backup destination {} with the save folder {}",
            dest.display(),
            save_dir.display()
        )
    })?;
    match overlap {
        Overlap::Apart => Ok(()),
        Overlap::Same | Overlap::FirstInsideSecond => {
            bail!("the backup destination is inside the Elden Ring save folder")
        }
        Overlap::SecondInsideFirst => {
            bail!("the Elden Ring save folder is inside the backup destination")
        }
    }
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
        let root = std::env::temp_dir().join("eldenring-backuptool-path-guard-test");
        let save = root.join("save/76561198000000000");
        let backups = root.join("documents/backups");
        assert!(validate_backup_dest(&save, &save.join("backups")).is_err());
        assert!(validate_backup_dest(&save, &backups).is_ok());
        // save inside dest
        assert!(validate_backup_dest(&backups.join("save"), &backups).is_err());
    }

    #[test]
    fn default_dest_layout() {
        let d = default_backup_dest_in(Path::new("/home/u/Documents"), "76561198000000000");
        assert!(d.ends_with("Game Save Backups/Elden Ring/76561198000000000"));
    }

    #[test]
    fn guard_rejects_relative_destination() {
        assert!(validate_backup_dest(Path::new("/save/account"), Path::new("backups")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn guard_resolves_symlinked_destination() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let save = root.join("save");
        std::fs::create_dir_all(&save).unwrap();
        let disguised = root.join("disguised");
        symlink(&save, &disguised).unwrap();

        assert!(validate_backup_dest(&save, &disguised.join("backups")).is_err());
    }
}
