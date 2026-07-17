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
///
/// # Errors
///
/// Returns an error if the environment variable is missing or empty.
pub fn appdata_roaming() -> Result<PathBuf> {
    env_dir("APPDATA")
}

/// `%LOCALAPPDATA%`.
///
/// # Errors
///
/// Returns an error if the environment variable is missing or empty.
pub fn local_appdata() -> Result<PathBuf> {
    env_dir("LOCALAPPDATA")
}

/// `%APPDATA%\EldenRing` — the save root that holds numeric `SteamID64` dirs.
///
/// # Errors
///
/// Returns an error if `%APPDATA%` cannot be resolved.
pub fn elden_ring_root() -> Result<PathBuf> {
    Ok(appdata_roaming()?.join("EldenRing"))
}

/// `%LOCALAPPDATA%\EldenRingSaveGuard` — config and logs.
///
/// # Errors
///
/// Returns an error if `%LOCALAPPDATA%` cannot be resolved.
pub fn app_data_dir() -> Result<PathBuf> {
    Ok(local_appdata()?.join(APP_NAME))
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

/// Resolve every existing ancestor through the filesystem, then append any
/// not-yet-created tail lexically. This catches symlink/junction overlap while
/// still allowing a new destination folder.
fn resolve_for_comparison(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("the backup destination must be an absolute path");
    }
    let normalized = normalize_lexical(path);
    let mut current = normalized.as_path();
    let mut tail = Vec::new();
    loop {
        match std::fs::canonicalize(current) {
            Ok(mut resolved) => {
                for component in tail.iter().rev() {
                    resolved.push(component);
                }
                return Ok(normalize_lexical(&resolved));
            }
            Err(error) => {
                let name = current.file_name().with_context(|| {
                    format!(
                        "could not resolve any existing ancestor of {}: {error}",
                        path.display()
                    )
                })?;
                tail.push(name.to_os_string());
                current = current.parent().with_context(|| {
                    format!(
                        "could not resolve any existing ancestor of {}",
                        path.display()
                    )
                })?;
            }
        }
    }
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
#[must_use]
pub fn is_within(child: &Path, ancestor: &Path) -> bool {
    let c = cmp_components(&normalize_lexical(child));
    let a = cmp_components(&normalize_lexical(ancestor));
    a.len() <= c.len() && c[..a.len()] == a[..]
}

/// Validate that a backup destination is absolute and separate from the save tree.
///
/// # Errors
///
/// Returns an error when a path cannot be resolved safely or the paths overlap.
pub fn validate_backup_dest(save_dir: &Path, dest: &Path) -> Result<()> {
    let save_dir = resolve_for_comparison(save_dir)
        .with_context(|| format!("resolving save folder {}", save_dir.display()))?;
    let dest = resolve_for_comparison(dest)
        .with_context(|| format!("resolving backup destination {}", dest.display()))?;
    if is_within(&dest, &save_dir) {
        bail!("the backup destination is inside the Elden Ring save folder");
    }
    if is_within(&save_dir, &dest) {
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

    #[test]
    fn guard_rejects_relative_destination() {
        assert!(validate_backup_dest(Path::new("/save/account"), Path::new("backups")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn guard_resolves_symlinked_destination() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "erbt-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let save = root.join("save");
        std::fs::create_dir_all(&save).unwrap();
        let disguised = root.join("disguised");
        symlink(&save, &disguised).unwrap();

        assert!(validate_backup_dest(&save, &disguised.join("backups")).is_err());
    }
}
