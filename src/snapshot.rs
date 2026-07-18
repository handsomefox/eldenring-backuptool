//! Content-verified, atomically-finalized save snapshots.
//!
//! A snapshot lives at `<dest>/snapshots/<UTC-timestamp>-<shorthash>/` and
//! holds `save.zip` (the `.sl2` and optional `.sl2.bak`, deflate-compressed so
//! Windows Explorer can open it directly for a manual restore) plus a plain
//! `metadata.json`. Creation never overwrites a finalized snapshot, never
//! finalizes a partial copy, and dedups on file *content*, not modification
//! time. Hashes recorded in metadata are of the **original** save bytes.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::APP_VERSION;

pub const METADATA_VERSION: u32 = 1;
pub const METADATA_FILE: &str = "metadata.json";
pub const ARCHIVE_FILE: &str = "save.zip";
pub const MAX_SOURCE_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_TOTAL_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = MAX_TOTAL_SOURCE_BYTES + 16 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 8;
const IO_BUFFER_BYTES: usize = 64 * 1024;
const TMP_PREFIX: &str = ".tmp-";
const MAX_COPY_ATTEMPTS: u32 = 4;

#[cfg(test)]
thread_local! {
    static ARCHIVE_HASH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    PreLaunch,
    Periodic,
    PostExit,
    Manual,
}

impl Reason {
    #[must_use]
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
    /// SHA-256 of the complete compressed archive.
    pub archive_sha256: String,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub metadata: Metadata,
}

impl Snapshot {
    /// Total original (uncompressed) bytes of the saved files.
    #[must_use]
    pub fn original_size(&self) -> u64 {
        self.metadata.files.iter().map(|f| f.size).sum()
    }

    /// Bytes this snapshot actually occupies on disk (compressed archive).
    #[must_use]
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

#[must_use]
pub fn snapshots_dir(dest: &Path) -> PathBuf {
    dest.join("snapshots")
}

fn source_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("source file has no valid name")?;
    if name.contains(['/', '\\']) {
        bail!("source file has an unsafe name: {name}");
    }
    Ok(name.to_string())
}

fn hash_reader(name: &str, reader: &mut impl Read, limit: u64) -> Result<FileHash> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; IO_BUFFER_BYTES];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("file size overflow while hashing")?;
        if size > limit {
            bail!("{name} exceeds the {limit}-byte safety limit");
        }
        hasher.update(&buf[..n]);
    }
    Ok(FileHash {
        name: name.to_string(),
        sha256: finish_sha256(hasher),
        size,
    })
}

fn finish_sha256(hasher: Sha256) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hash_source(path: &Path) -> Result<FileHash> {
    let name = source_name(path)?;
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    hash_reader(&name, &mut file, MAX_SOURCE_FILE_BYTES)
}

fn hash_sources(files: &[PathBuf]) -> Result<Vec<FileHash>> {
    let hashes: Vec<FileHash> = files
        .iter()
        .map(|p| hash_source(p))
        .collect::<Result<_>>()?;
    validate_file_hashes(&hashes)?;
    Ok(hashes)
}

fn validate_file_hashes(files: &[FileHash]) -> Result<()> {
    if files.is_empty() || files.len() > MAX_ARCHIVE_ENTRIES {
        bail!("snapshot must contain 1..={MAX_ARCHIVE_ENTRIES} files");
    }
    let mut names = HashSet::new();
    let mut total = 0u64;
    for file in files {
        if file.name.is_empty()
            || file.name.contains(['/', '\\'])
            || Path::new(&file.name).file_name().and_then(|n| n.to_str())
                != Some(file.name.as_str())
        {
            bail!("unsafe snapshot file name: {}", file.name);
        }
        if !names.insert(file.name.as_str()) {
            bail!("duplicate snapshot file name: {}", file.name);
        }
        if file.size > MAX_SOURCE_FILE_BYTES {
            bail!("snapshot file {} exceeds the safety limit", file.name);
        }
        if file.sha256.len() != 64 || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid SHA-256 for {}", file.name);
        }
        total = total
            .checked_add(file.size)
            .context("snapshot size overflow")?;
    }
    if total > MAX_TOTAL_SOURCE_BYTES {
        bail!("snapshot exceeds the total-size safety limit");
    }
    Ok(())
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("{} exceeds the {limit}-byte safety limit", path.display());
    }
    Ok(bytes)
}

fn hash_file(path: &Path, name: &str, limit: u64) -> Result<FileHash> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    hash_reader(name, &mut file, limit)
}

fn load_snapshot_metadata(dir: &Path, expected_steamid: Option<&str>) -> Result<Snapshot> {
    let bytes = read_limited(&dir.join(METADATA_FILE), MAX_METADATA_BYTES)?;
    let metadata: Metadata = serde_json::from_slice(&bytes)?;
    if metadata.format_version != METADATA_VERSION {
        bail!("unsupported snapshot metadata version");
    }
    if let Some(expected) = expected_steamid
        && metadata.steamid != expected
    {
        bail!("snapshot belongs to a different Steam account");
    }
    if metadata.archive != ARCHIVE_FILE {
        bail!("unexpected snapshot archive name");
    }
    validate_file_hashes(&metadata.files)?;

    let archive_path = dir.join(ARCHIVE_FILE);
    let archive_meta = std::fs::metadata(&archive_path)?;
    if !archive_meta.is_file() || archive_meta.len() != metadata.stored_bytes {
        bail!("snapshot archive is missing or has the wrong size");
    }
    if metadata.stored_bytes > MAX_ARCHIVE_BYTES {
        bail!("snapshot archive exceeds the safety limit");
    }
    Ok(Snapshot {
        dir: dir.to_path_buf(),
        metadata,
    })
}

fn verify_snapshot_archive(snapshot: Snapshot) -> Result<Snapshot> {
    #[cfg(test)]
    ARCHIVE_HASH_COUNT.with(|count| count.set(count.get() + 1));
    let archive_path = snapshot.dir.join(ARCHIVE_FILE);
    let actual = hash_file(&archive_path, ARCHIVE_FILE, MAX_ARCHIVE_BYTES)?;
    if actual.sha256 != snapshot.metadata.archive_sha256 {
        bail!("snapshot archive hash does not match metadata");
    }
    Ok(snapshot)
}

fn load_verified_snapshot(dir: &Path, expected_steamid: Option<&str>) -> Result<Snapshot> {
    verify_snapshot_archive(load_snapshot_metadata(dir, expected_steamid)?)
}

fn is_real_directory(entry: &std::fs::DirEntry) -> bool {
    let Ok(file_type) = entry.file_type() else {
        return false;
    };
    if !file_type.is_dir() || file_type.is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

/// Structurally list snapshots for one Steam account, sorted oldest → newest.
///
/// Metadata is bounded and validated, and the recorded archive name, existence,
/// and size must match. Archive payloads are deliberately not read here;
/// callers that restore data must use [`extract`], while dedup uses [`newest`].
/// Temp directories and reparse points are ignored.
#[must_use]
pub fn list(dest: &Path, steamid: &str) -> Vec<Snapshot> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(snapshots_dir(dest)) else {
        return out;
    };
    for entry in entries.flatten() {
        if !is_real_directory(&entry) {
            continue;
        }
        let dir = entry.path();
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with(TMP_PREFIX) {
            continue;
        }
        if let Ok(snapshot) = load_snapshot_metadata(&dir, Some(steamid)) {
            out.push(snapshot);
        }
    }
    out.sort_by_key(|s| s.metadata.created_utc);
    out
}

/// Return the newest integrity-validated snapshot for one Steam account.
///
/// Candidates are checked newest-first and archive hashing stops as soon as a
/// valid snapshot is found. A corrupt newest entry therefore falls back to the
/// next valid snapshot without requiring the complete history to be rehashed.
#[must_use]
pub fn newest(dest: &Path, steamid: &str) -> Option<Snapshot> {
    let mut snapshots = list(dest, steamid);
    while let Some(snapshot) = snapshots.pop() {
        if let Ok(snapshot) = verify_snapshot_archive(snapshot) {
            return Some(snapshot);
        }
    }
    None
}

fn system_time_to_utc(t: std::time::SystemTime) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(
        t.duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs()
            .cast_signed(),
        0,
    )
}

/// Create a snapshot of `source_files` (the `.sl2` and optional `.sl2.bak`).
/// Returns `Ok(None)` when content is identical to the newest snapshot (dedup).
///
/// # Errors
///
/// Returns an error if inputs are unsafe, copying fails, or verification fails.
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
///
/// # Errors
///
/// Returns an error if inputs are unsafe, copying fails, or verification fails.
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
    let prev = newest(dest, steamid);
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
            Ok(Some((hashes, stored_bytes, archive_sha256))) => {
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
                    archive_sha256,
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
) -> Result<Option<(Vec<FileHash>, u64, String)>> {
    std::fs::create_dir_all(tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let zip_path = tmp.join(ARCHIVE_FILE);
    let archived = write_archive(&zip_path, source_files)?;

    after_copy(attempt);

    // The archive must decompress back to exactly the bytes we hashed.
    verify_archive(&zip_path, &archived)?;

    // The source must be unchanged since we read it.
    if hash_sources(source_files)? != archived {
        return Ok(None);
    }
    let stored_bytes = std::fs::metadata(&zip_path)?.len();
    if stored_bytes > MAX_ARCHIVE_BYTES {
        bail!("snapshot archive exceeds the safety limit");
    }
    let archive_sha256 = hash_file(&zip_path, ARCHIVE_FILE, MAX_ARCHIVE_BYTES)?.sha256;
    Ok(Some((archived, stored_bytes, archive_sha256)))
}

fn write_archive(zip_path: &Path, source_files: &[PathBuf]) -> Result<Vec<FileHash>> {
    if source_files.len() > MAX_ARCHIVE_ENTRIES {
        bail!("too many source files");
    }
    let file =
        File::create(zip_path).with_context(|| format!("creating {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut hashes = Vec::with_capacity(source_files.len());
    let mut names = HashSet::new();
    for path in source_files {
        let name = source_name(path)?;
        if !names.insert(name.clone()) {
            bail!("duplicate source file name: {name}");
        }
        if std::fs::metadata(path)?.len() > MAX_SOURCE_FILE_BYTES {
            bail!("{name} exceeds the source-file safety limit");
        }
        zip.start_file(&name, opts)?;
        let mut source = File::open(path)?;
        let mut file_hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; IO_BUFFER_BYTES];
        loop {
            let n = source.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size = size.checked_add(n as u64).context("source size overflow")?;
            if size > MAX_SOURCE_FILE_BYTES {
                bail!("{name} exceeds the source-file safety limit");
            }
            file_hasher.update(&buf[..n]);
            zip.write_all(&buf[..n])?;
        }
        hashes.push(FileHash {
            name,
            sha256: finish_sha256(file_hasher),
            size,
        });
    }
    zip.finish()?;
    validate_file_hashes(&hashes)?;
    Ok(hashes)
}

fn verify_archive(zip_path: &Path, expected: &[FileHash]) -> Result<()> {
    validate_file_hashes(expected)?;
    let file = File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    if archive.len() != expected.len() {
        bail!("archive entry count does not match metadata");
    }
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() || name.contains(['/', '\\']) || !seen.insert(name.clone()) {
            bail!("unsafe or duplicate archive entry: {name}");
        }
        let exp = expected
            .iter()
            .find(|file| file.name == name)
            .with_context(|| format!("unexpected archive entry {name}"))?;
        let actual = hash_reader(&name, &mut entry, MAX_SOURCE_FILE_BYTES)?;
        total = total
            .checked_add(actual.size)
            .context("archive size overflow")?;
        if total > MAX_TOTAL_SOURCE_BYTES || &actual != exp {
            bail!("archive verification failed for {name}");
        }
    }
    Ok(())
}

/// Fully verify and extract a snapshot into a new output directory.
///
/// The complete archive hash and every ZIP entry's name, size, and content hash
/// are checked before the restore is accepted. Unsafe names, duplicate entries,
/// oversized content, and existing output files are rejected.
///
/// # Errors
///
/// Returns an error if validation, extraction, or any filesystem operation fails.
pub fn extract(snapshot_dir: &Path, out_dir: &Path) -> Result<()> {
    let snapshot = load_verified_snapshot(snapshot_dir, None)?;
    let file = File::open(snapshot_dir.join(ARCHIVE_FILE))?;
    let mut archive = zip::ZipArchive::new(file)?;
    std::fs::create_dir_all(out_dir)?;
    if archive.len() != snapshot.metadata.files.len() {
        bail!("archive entry count does not match metadata");
    }
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir()
            || name.contains(['/', '\\'])
            || Path::new(&name).file_name().and_then(|n| n.to_str()) != Some(name.as_str())
            || !seen.insert(name.clone())
        {
            bail!("unsafe or duplicate archive entry: {name}");
        }
        let expected = snapshot
            .metadata
            .files
            .iter()
            .find(|file| file.name == name)
            .with_context(|| format!("unexpected archive entry {name}"))?;
        let output_path = out_dir.join(&name);
        let mut output = File::options()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .with_context(|| format!("creating {}", output_path.display()))?;
        let result = (|| -> Result<FileHash> {
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            let mut buf = vec![0u8; IO_BUFFER_BYTES];
            loop {
                let n = entry.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                size = size
                    .checked_add(n as u64)
                    .context("archive size overflow")?;
                if size > MAX_SOURCE_FILE_BYTES {
                    bail!("archive entry {name} exceeds the safety limit");
                }
                hasher.update(&buf[..n]);
                output.write_all(&buf[..n])?;
            }
            output.sync_all()?;
            Ok(FileHash {
                name: name.clone(),
                sha256: finish_sha256(hasher),
                size,
            })
        })();
        let actual = match result {
            Ok(actual) => actual,
            Err(error) => {
                drop(output);
                let _ = std::fs::remove_file(&output_path);
                return Err(error);
            }
        };
        total = total
            .checked_add(actual.size)
            .context("archive size overflow")?;
        if total > MAX_TOTAL_SOURCE_BYTES || &actual != expected {
            drop(output);
            let _ = std::fs::remove_file(&output_path);
            bail!("archive verification failed for {name}");
        }
    }
    Ok(())
}

fn unique_final_dir(snaps_dir: &Path, created: DateTime<Utc>, files: &[FileHash]) -> PathBuf {
    let short = files.first().map_or_else(
        || "00000000".to_string(),
        |f| f.sha256[..8.min(f.sha256.len())].to_string(),
    );
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
        .map_or(0, |d| d.as_nanos())
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
        let out = snap
            .dir
            .join(format!("extracted-{}", name.replace('.', "-")));
        extract(&snap.dir, &out).unwrap();
        std::fs::read(out.join(name)).unwrap()
    }

    fn reset_archive_hash_count() {
        ARCHIVE_HASH_COUNT.with(|count| count.set(0));
    }

    fn archive_hash_count() -> usize {
        ARCHIVE_HASH_COUNT.with(std::cell::Cell::get)
    }

    fn tamper_archive_same_size(snapshot: &Snapshot) {
        let path = snapshot.dir.join(ARCHIVE_FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        let index = bytes.len() / 2;
        bytes[index] ^= 0xff;
        std::fs::write(path, bytes).unwrap();
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
        assert_eq!(list(&e.dest, "111").len(), 1);
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
        assert_eq!(list(&e.dest, "111").len(), 2);
    }

    #[test]
    fn listing_is_structural_and_does_not_hash_archive_payloads() {
        let e = env();
        let src = sources(&e, false);
        let snapshot = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        tamper_archive_same_size(&snapshot);

        reset_archive_hash_count();
        assert_eq!(list(&e.dest, "111").len(), 1);
        assert_eq!(archive_hash_count(), 0);
    }

    #[test]
    fn newest_hashes_only_the_first_valid_candidate() {
        let e = env();
        let src = sources(&e, false);
        create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        write(&src[0], b"newer");
        let latest = create(&e.dest, "111", &src, Reason::Periodic)
            .unwrap()
            .unwrap();

        reset_archive_hash_count();
        assert_eq!(newest(&e.dest, "111").unwrap().dir, latest.dir);
        assert_eq!(archive_hash_count(), 1);
    }

    #[test]
    fn newest_skips_a_corrupt_candidate_and_finds_an_older_valid_snapshot() {
        let e = env();
        let src = sources(&e, false);
        let older = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        write(&src[0], b"newer");
        let latest = create(&e.dest, "111", &src, Reason::Periodic)
            .unwrap()
            .unwrap();
        tamper_archive_same_size(&latest);

        reset_archive_hash_count();
        assert_eq!(newest(&e.dest, "111").unwrap().dir, older.dir);
        assert_eq!(archive_hash_count(), 2);
    }

    #[test]
    fn extraction_rejects_same_size_archive_tampering() {
        let e = env();
        let src = sources(&e, false);
        let snapshot = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        tamper_archive_same_size(&snapshot);
        assert!(extract(&snapshot.dir, &snapshot.dir.join("out")).is_err());
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
        assert_eq!(list(&e.dest, "111").len(), 1);
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

    #[test]
    fn snapshots_are_scoped_by_account() {
        let e = env();
        let src = sources(&e, false);
        create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        write(&src[0], b"another-account-save");
        create(&e.dest, "222", &src, Reason::Manual)
            .unwrap()
            .unwrap();

        let first = list(&e.dest, "111");
        let second = list(&e.dest, "222");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].metadata.steamid, "111");
        assert_eq!(second[0].metadata.steamid, "222");
    }

    #[test]
    fn corrupt_archive_is_ignored_and_replaced() {
        let e = env();
        let src = sources(&e, false);
        let first = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        std::fs::remove_file(first.dir.join(ARCHIVE_FILE)).unwrap();
        assert!(list(&e.dest, "111").is_empty());

        let replacement = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .expect("invalid metadata must not suppress a replacement");
        assert!(replacement.dir.join(ARCHIVE_FILE).is_file());
        assert_eq!(list(&e.dest, "111").len(), 1);
    }

    #[test]
    fn same_size_corrupt_archive_cannot_suppress_replacement() {
        let e = env();
        let src = sources(&e, false);
        let first = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .unwrap();
        tamper_archive_same_size(&first);
        assert!(newest(&e.dest, "111").is_none());

        let replacement = create(&e.dest, "111", &src, Reason::Manual)
            .unwrap()
            .expect("corrupt archive must not deduplicate the current save");
        assert_ne!(replacement.dir, first.dir);
        assert_eq!(newest(&e.dest, "111").unwrap().dir, replacement.dir);
    }

    #[test]
    fn oversized_source_is_rejected_before_copy() {
        let e = env();
        let source = e.save_dir.join("ER0000.sl2");
        let file = File::create(&source).unwrap();
        file.set_len(MAX_SOURCE_FILE_BYTES + 1).unwrap();
        assert!(create(&e.dest, "111", &[source], Reason::Manual).is_err());
    }
}
