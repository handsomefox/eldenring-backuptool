//! Content-verified, atomically-finalized save snapshots.
//!
//! A snapshot lives at `<dest>/snapshots/<UTC-timestamp>-<shorthash>/` and
//! holds `save.zip` (the `.sl2` and optional `.sl2.bak`, deflate-compressed so
//! Windows Explorer can open it directly for a manual restore) plus a plain
//! `metadata.json`. Creation never overwrites a finalized snapshot, never
//! finalizes a partial copy, and dedups on file *content*, not modification
//! time. Hashes recorded in metadata are of the **original** save bytes.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::APP_VERSION;

pub const METADATA_VERSION: u32 = 2;
pub const METADATA_FILE: &str = "metadata.json";
pub const ARCHIVE_FILE: &str = "save.zip";
const TMP_PREFIX: &str = ".tmp-";
const MAX_COPY_ATTEMPTS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    PreLaunch,
    Periodic,
    PostExit,
    Manual,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::PreLaunch => "pre-launch",
            Reason::Periodic => "periodic",
            Reason::PostExit => "post-exit",
            Reason::Manual => "manual",
        }
    }
}

/// One save file recorded in a snapshot. `size`/`sha256` describe the
/// **original** (uncompressed) bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHash {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub format_version: u32,
    pub steamid: String,
    pub created_utc: DateTime<Utc>,
    pub source_modified_utc: Option<DateTime<Utc>>,
    pub reason: Reason,
    pub app_version: String,
    pub files: Vec<FileHash>,
    /// Archive holding the copied saves, relative to the snapshot dir.
    pub archive: String,
    /// On-disk size of the compressed archive.
    pub stored_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub metadata: Metadata,
}

impl Snapshot {
    /// Total original (uncompressed) bytes of the saved files.
    pub fn original_size(&self) -> u64 {
        self.metadata.files.iter().map(|f| f.size).sum()
    }

    /// Bytes this snapshot actually occupies on disk (compressed archive).
    pub fn stored_size(&self) -> u64 {
        self.metadata.stored_bytes
    }

    fn fingerprint(&self) -> Vec<(String, String)> {
        fingerprint(&self.metadata.files)
    }
}

fn fingerprint(files: &[FileHash]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = files
        .iter()
        .map(|f| (f.name.clone(), f.sha256.clone()))
        .collect();
    v.sort();
    v
}

pub fn snapshots_dir(dest: &Path) -> PathBuf {
    dest.join("snapshots")
}

fn hash_bytes(name: &str, bytes: &[u8]) -> FileHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    FileHash {
        name: name.to_string(),
        sha256: format!("{:x}", hasher.finalize()),
        size: bytes.len() as u64,
    }
}

/// Read a source file, returning its recorded hash and its bytes.
fn read_source(path: &Path) -> Result<(FileHash, Vec<u8>)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("source file has no valid name")?;
    Ok((hash_bytes(name, &bytes), bytes))
}

fn hash_source(path: &Path) -> Result<FileHash> {
    Ok(read_source(path)?.0)
}

fn hash_sources(files: &[PathBuf]) -> Result<Vec<FileHash>> {
    files.iter().map(|p| hash_source(p)).collect()
}

/// List finalized snapshots (temp dirs ignored), sorted oldest → newest.
/// Directories with unreadable/invalid metadata are skipped.
pub fn list(dest: &Path) -> Vec<Snapshot> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(snapshots_dir(dest)) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with(TMP_PREFIX) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(dir.join(METADATA_FILE))
            && let Ok(metadata) = serde_json::from_slice::<Metadata>(&bytes)
        {
            out.push(Snapshot { dir, metadata });
        }
    }
    out.sort_by_key(|s| s.metadata.created_utc);
    out
}

pub fn newest(dest: &Path) -> Option<Snapshot> {
    list(dest).pop()
}

fn system_time_to_utc(t: std::time::SystemTime) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(
        t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64,
        0,
    )
}

/// Create a snapshot of `source_files` (the `.sl2` and optional `.sl2.bak`).
/// Returns `Ok(None)` when content is identical to the newest snapshot (dedup).
pub fn create(
    dest: &Path,
    steamid: &str,
    source_files: &[PathBuf],
    reason: Reason,
) -> Result<Option<Snapshot>> {
    create_with_hook(dest, steamid, source_files, reason, |_| {})
}

/// Testable form of [`create`]: `after_copy(attempt)` runs after the archive is
/// written but before the source is re-hashed, letting tests simulate a save
/// that changes mid-copy.
pub fn create_with_hook(
    dest: &Path,
    steamid: &str,
    source_files: &[PathBuf],
    reason: Reason,
    mut after_copy: impl FnMut(u32),
) -> Result<Option<Snapshot>> {
    if source_files.is_empty() {
        bail!("no source save files to back up");
    }
    for f in source_files {
        if !f.is_file() {
            bail!("source save file missing: {}", f.display());
        }
    }

    let snaps_dir = snapshots_dir(dest);
    std::fs::create_dir_all(&snaps_dir)
        .with_context(|| format!("creating {}", snaps_dir.display()))?;

    // Fast path: skip the copy if the source already matches the newest
    // snapshot. Authoritative hashes still come from the stable copy below.
    let prev = newest(dest);
    if let Some(p) = &prev {
        let quick = hash_sources(source_files)?;
        if p.fingerprint() == fingerprint(&quick) {
            return Ok(None);
        }
    }

    for attempt in 1..=MAX_COPY_ATTEMPTS {
        let tmp = snaps_dir.join(format!(
            "{TMP_PREFIX}{}-{}",
            std::process::id(),
            now_nanos()
        ));
        match try_archive(&tmp, source_files, &mut after_copy, attempt) {
            Ok(Some((hashes, stored_bytes))) => {
                if let Some(p) = &prev
                    && p.fingerprint() == fingerprint(&hashes)
                {
                    let _ = std::fs::remove_dir_all(&tmp);
                    return Ok(None);
                }
                let source_modified_utc = std::fs::metadata(&source_files[0])
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(system_time_to_utc);
                let created = Utc::now();
                let metadata = Metadata {
                    format_version: METADATA_VERSION,
                    steamid: steamid.to_string(),
                    created_utc: created,
                    source_modified_utc,
                    reason,
                    app_version: APP_VERSION.to_string(),
                    files: hashes.clone(),
                    archive: ARCHIVE_FILE.to_string(),
                    stored_bytes,
                };
                let json = serde_json::to_vec_pretty(&metadata)?;
                std::fs::write(tmp.join(METADATA_FILE), &json)?;

                let final_dir = unique_final_dir(&snaps_dir, created, &hashes);
                std::fs::rename(&tmp, &final_dir)
                    .with_context(|| format!("finalizing snapshot {}", final_dir.display()))?;
                return Ok(Some(Snapshot {
                    dir: final_dir,
                    metadata,
                }));
            }
            Ok(None) => {
                let _ = std::fs::remove_dir_all(&tmp);
                if attempt < MAX_COPY_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e);
            }
        }
    }
    bail!("save kept changing; no stable copy after {MAX_COPY_ATTEMPTS} attempts")
}

/// One archive attempt. Reads + hashes sources, writes them into a deflate
/// `save.zip`, verifies the archive decompresses to the same bytes, then
/// confirms the source did not change. Returns `Ok(Some((hashes, zip_size)))`
/// on a stable copy, `Ok(None)` if the source changed mid-copy (caller retries).
fn try_archive(
    tmp: &Path,
    source_files: &[PathBuf],
    after_copy: &mut impl FnMut(u32),
    attempt: u32,
) -> Result<Option<(Vec<FileHash>, u64)>> {
    // Read every source once (hash + bytes for compression).
    let mut loaded: Vec<(String, Vec<u8>, FileHash)> = Vec::new();
    for src in source_files {
        let (hash, bytes) = read_source(src)?;
        loaded.push((hash.name.clone(), bytes, hash));
    }
    let before: Vec<FileHash> = loaded.iter().map(|(_, _, h)| h.clone()).collect();

    std::fs::create_dir_all(tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let zip_path = tmp.join(ARCHIVE_FILE);
    write_archive(&zip_path, &loaded)?;

    after_copy(attempt);

    // The archive must decompress back to exactly the bytes we hashed.
    verify_archive(&zip_path, &before)?;

    // The source must be unchanged since we read it.
    if hash_sources(source_files)? != before {
        return Ok(None);
    }
    let stored_bytes = std::fs::metadata(&zip_path)?.len();
    Ok(Some((before, stored_bytes)))
}

fn write_archive(zip_path: &Path, files: &[(String, Vec<u8>, FileHash)]) -> Result<()> {
    let file = std::fs::File::create(zip_path)
        .with_context(|| format!("creating {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes, _) in files {
        zip.start_file(name, opts)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(())
}

fn verify_archive(zip_path: &Path, expected: &[FileHash]) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for exp in expected {
        let mut entry = archive
            .by_name(&exp.name)
            .with_context(|| format!("archive missing {}", exp.name))?;
        let mut buf = Vec::with_capacity(exp.size as usize);
        entry.read_to_end(&mut buf)?;
        if &hash_bytes(&exp.name, &buf) != exp {
            bail!("archive verification failed for {}", exp.name);
        }
    }
    Ok(())
}

/// Extract a snapshot's saved files into `out_dir` (used by tests and any
/// future automated restore). Entry names are reduced to their file name to
/// resist path traversal.
pub fn extract(snapshot_dir: &Path, out_dir: &Path) -> Result<()> {
    let file = std::fs::File::open(snapshot_dir.join(ARCHIVE_FILE))?;
    let mut archive = zip::ZipArchive::new(file)?;
    std::fs::create_dir_all(out_dir)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        let safe = Path::new(&name)
            .file_name()
            .with_context(|| format!("unsafe archive entry {name}"))?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        std::fs::write(out_dir.join(safe), buf)?;
    }
    Ok(())
}

fn unique_final_dir(snaps_dir: &Path, created: DateTime<Utc>, files: &[FileHash]) -> PathBuf {
    let short = files
        .first()
        .map(|f| f.sha256[..8.min(f.sha256.len())].to_string())
        .unwrap_or_else(|| "00000000".to_string());
    let base = format!("{}-{}", created.format("%Y%m%d-%H%M%S"), short);
    let mut dir = snaps_dir.join(&base);
    let mut n = 1;
    while dir.exists() {
        dir = snaps_dir.join(format!("{base}-{n}"));
        n += 1;
    }
    dir
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _root: PathBuf,
        save_dir: PathBuf,
        dest: PathBuf,
    }

    fn env() -> Env {
        let root =
            std::env::temp_dir().join(format!("erbt-snap-{}-{}", std::process::id(), now_nanos()));
        let save_dir = root.join("save");
        let dest = root.join("dest");
        std::fs::create_dir_all(&save_dir).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        Env {
            _root: root,
            save_dir,
            dest,
        }
    }

    fn write(p: &Path, bytes: &[u8]) {
        std::fs::write(p, bytes).unwrap();
    }

    fn sources(e: &Env, with_bak: bool) -> Vec<PathBuf> {
        let sl2 = e.save_dir.join("ER0000.sl2");
        write(&sl2, b"save-v1");
        let mut v = vec![sl2.clone()];
        if with_bak {
            let bak = e.save_dir.join("ER0000.sl2.bak");
            write(&bak, b"bak-v1");
            v.push(bak);
        }
        v
    }

    fn extracted(snap: &Snapshot, name: &str) -> Vec<u8> {
        let out = snap.dir.join("extracted");
        extract(&snap.dir, &out).unwrap();
        std::fs::read(out.join(name)).unwrap()
    }

    #[test]
    fn first_snapshot_with_bak() {
        let e = env();
        let src = sources(&e, true);
        let snap = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .expect("created");
        assert_eq!(snap.metadata.files.len(), 2);
        assert!(snap.dir.join(ARCHIVE_FILE).exists());
        assert!(snap.dir.join(METADATA_FILE).exists());
        assert!(
            !snap.dir.join("ER0000.sl2").exists(),
            "saves live inside the zip"
        );
        assert_eq!(extracted(&snap, "ER0000.sl2"), b"save-v1");
        assert_eq!(extracted(&snap, "ER0000.sl2.bak"), b"bak-v1");
    }

    #[test]
    fn compresses_zero_heavy_saves() {
        let e = env();
        let sl2 = e.save_dir.join("ER0000.sl2");
        write(&sl2, &vec![0u8; 4 * 1024 * 1024]); // 4 MiB of zeros
        let snap = create(&e.dest, "111", &[sl2], Reason::Manual)
            .unwrap()
            .unwrap();
        assert_eq!(snap.original_size(), 4 * 1024 * 1024);
        assert!(
            snap.stored_size() < snap.original_size() / 10,
            "zeros should compress hugely"
        );
    }

    #[test]
    fn without_bak() {
        let e = env();
        let src = sources(&e, false);
        let snap = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        assert_eq!(snap.metadata.files.len(), 1);
    }

    #[test]
    fn dedup_identical_content() {
        let e = env();
        let src = sources(&e, true);
        assert!(
            create(&e.dest, "111", &src, Reason::Manual)
                .unwrap()
                .is_some()
        );
        assert!(
            create(&e.dest, "111", &src, Reason::Periodic)
                .unwrap()
                .is_none()
        );
        assert_eq!(list(&e.dest).len(), 1);
    }

    #[test]
    fn changed_content_creates_new() {
        let e = env();
        let src = sources(&e, true);
        create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        write(&src[0], b"save-v2-different");
        assert!(
            create(&e.dest, "111", &src, Reason::Periodic)
                .unwrap()
                .is_some()
        );
        assert_eq!(list(&e.dest).len(), 2);
    }

    #[test]
    fn source_changes_during_copy_then_stabilizes() {
        let e = env();
        let src = sources(&e, false);
        let path = src[0].clone();
        let snap = create_with_hook(&e.dest, "111", &src, Reason::Manual, move |attempt| {
            if attempt == 1 {
                std::fs::write(&path, b"changed-mid-copy").unwrap();
            }
        })
        .unwrap()
        .expect("eventually stable");
        assert_eq!(extracted(&snap, "ER0000.sl2"), b"changed-mid-copy");
        assert_eq!(list(&e.dest).len(), 1);
    }

    #[test]
    fn no_temp_dir_left_behind() {
        let e = env();
        let src = sources(&e, true);
        create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        let leftover = std::fs::read_dir(snapshots_dir(&e.dest))
            .unwrap()
            .flatten()
            .any(|d| d.file_name().to_string_lossy().starts_with(TMP_PREFIX));
        assert!(!leftover, "temp dir must not survive finalization");
    }

    #[test]
    fn missing_source_errors() {
        let e = env();
        let missing = e.save_dir.join("nope.sl2");
        assert!(create(&e.dest, "111", &[missing], Reason::Manual).is_err());
    }
}
