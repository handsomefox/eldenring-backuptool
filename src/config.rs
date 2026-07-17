//! Versioned JSON config with atomic writes and malformed-file recovery.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 1;
pub const MIN_INTERVAL_SECS: u64 = 60;
pub const DEFAULT_INTERVAL_SECS: u64 = 300;
pub const MAX_RETENTION: usize = 10_000;
pub const DEFAULT_RETENTION: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Numeric SteamID64 folder the user chose to protect.
    pub selected_steamid: Option<String>,
    pub backup_dest: Option<PathBuf>,
    pub interval_secs: u64,
    pub retention: usize,
    pub pre_launch: bool,
    pub periodic: bool,
    pub post_exit: bool,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            selected_steamid: None,
            backup_dest: None,
            interval_secs: DEFAULT_INTERVAL_SECS,
            retention: DEFAULT_RETENTION,
            pre_launch: true,
            periodic: true,
            post_exit: true,
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    /// Clamp values to safe ranges. Applied on load and before every save so a
    /// hand-edited file can never create a tight backup loop or absurd retention.
    pub fn sanitized(mut self) -> Self {
        self.version = CONFIG_VERSION;
        self.interval_secs = self.interval_secs.max(MIN_INTERVAL_SECS);
        self.retention = self.retention.clamp(1, MAX_RETENTION);
        if self.log_level.trim().is_empty() {
            self.log_level = "info".to_string();
        }
        self
    }
}

/// Result of loading config: the effective config plus, if the file on disk
/// was malformed, the path it was preserved to.
pub struct LoadResult {
    pub config: Config,
    pub recovered_from: Option<PathBuf>,
}

/// Load config from `path`. Missing file → defaults. Malformed file → the
/// broken file is preserved next to it and defaults are returned (the account
/// selection is never silently discarded on a *readable* file, only on an
/// unparsable one, and even then the original bytes are kept for inspection).
pub fn load(path: &Path) -> LoadResult {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            return LoadResult {
                config: Config::default(),
                recovered_from: None,
            };
        }
    };
    match serde_json::from_slice::<Config>(&bytes) {
        Ok(cfg) => LoadResult {
            config: cfg.sanitized(),
            recovered_from: None,
        },
        Err(_) => {
            let preserved = preserve_broken(path);
            LoadResult {
                config: Config::default(),
                recovered_from: preserved,
            }
        }
    }
}

fn preserve_broken(path: &Path) -> Option<PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let mut bad = path.as_os_str().to_owned();
    bad.push(format!(".bad-{ts}"));
    let bad = PathBuf::from(bad);
    std::fs::rename(path, &bad).ok().map(|_| bad)
}

/// Atomically write config: serialize, write to a temp file in the same
/// directory, then rename over the target.
pub fn save(path: &Path, config: &Config) -> Result<()> {
    let config = config.clone().sanitized();
    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    let json = serde_json::to_vec_pretty(&config)?;

    let tmp = dir.join(format!(".config.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("finalizing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "erbt-cfg-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tmpdir();
        let r = load(&dir.join("config.json"));
        assert_eq!(r.config, Config::default());
        assert!(r.recovered_from.is_none());
    }

    #[test]
    fn round_trip() {
        let dir = tmpdir();
        let path = dir.join("config.json");
        let cfg = Config {
            selected_steamid: Some("76561198000000000".into()),
            backup_dest: Some(PathBuf::from("/backups")),
            ..Default::default()
        };
        save(&path, &cfg).unwrap();
        let r = load(&path);
        assert_eq!(r.config, cfg);
        assert!(r.recovered_from.is_none());
    }

    #[test]
    fn malformed_preserved_and_defaults_returned() {
        let dir = tmpdir();
        let path = dir.join("config.json");
        std::fs::write(&path, b"{ not valid json").unwrap();
        let r = load(&path);
        assert_eq!(r.config, Config::default());
        let preserved = r.recovered_from.expect("broken file preserved");
        assert!(preserved.exists());
        assert!(!path.exists(), "original replaced by defaults on next save");
    }

    #[test]
    fn sanitize_clamps() {
        let cfg = Config {
            interval_secs: 1,
            retention: 0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(cfg.interval_secs, MIN_INTERVAL_SECS);
        assert_eq!(cfg.retention, 1);
    }

    #[test]
    fn unknown_fields_and_partial_ok() {
        // serde(default) tolerates missing fields; extra fields are ignored.
        let dir = tmpdir();
        let path = dir.join("config.json");
        std::fs::write(&path, br#"{"selected_steamid":"123","extra":true}"#).unwrap();
        let r = load(&path);
        assert_eq!(r.config.selected_steamid.as_deref(), Some("123"));
        assert_eq!(r.config.interval_secs, DEFAULT_INTERVAL_SECS);
        assert!(r.recovered_from.is_none());
    }
}
