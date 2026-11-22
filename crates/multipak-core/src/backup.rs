//! Backups of one instance as a `.tar.zst` archive.
//!
//! Layout: the first entry is `multipak-backup.json` (a [`BackupManifest`] with the
//! instance's settings), followed by the data folder under `data/`. Archives are written
//! to a temporary file and renamed into place only when complete, so a cancelled or
//! failed backup never leaves a truncated archive behind. Restores extract into a
//! temporary folder next to the target and swap it in at the end.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::atomic::{sync_dir, temp_path_for};
use crate::error::{Error, IoContext, Result};
use crate::instance::Instance;

pub const MANIFEST_NAME: &str = "multipak-backup.json";
pub const FORMAT_NAME: &str = "multipak-backup";
pub const FORMAT_VERSION: u32 = 1;
const DATA_PREFIX: &str = "data";

/// Cooperative cancellation flag shared between a job and the UI.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Returns [`Error::Cancelled`] once cancelled.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }
}

/// Progress of a long job in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub current: Option<String>,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 { 0.0 } else { (self.done as f64 / self.total as f64).min(1.0) as f32 }
    }
}

/// Callback receiving progress updates.
pub type ProgressFn<'a> = &'a (dyn Fn(Progress) + Sync);

/// Metadata stored at the start of every backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format: String,
    pub version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created: time::OffsetDateTime,
    pub multipak_version: String,
    /// Uncompressed size of the data, for restore progress.
    pub total_bytes: u64,
    pub instance: Instance,
}

/// Default backup file name: `<app-id>-<slug>-<YYYYmmdd-HHMMSS>.tar.zst`.
pub fn default_file_name(instance: &Instance) -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let stamp = now
        .format(time::macros::format_description!("[year][month][day]-[hour][minute][second]"))
        .unwrap_or_else(|_| "backup".into());
    format!("{}-{}-{stamp}.tar.zst", instance.app_id, instance.slug)
}

struct ExactReader<'a, R> {
    inner: R,
    remaining: u64,
    cancel: &'a CancelToken,
    on_bytes: &'a dyn Fn(u64),
}

impl<R: Read> Read for ExactReader<'_, R> {
    /// Yields exactly the size recorded in the tar header: a file that shrinks while being
    /// read is padded with zeros so the archive stays well-formed.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if self.remaining == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.remaining as usize);
        let mut n = loop {
            match self.inner.read(&mut buf[..want]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        if n == 0 {
            buf[..want].fill(0);
            n = want;
        }
        self.remaining -= n as u64;
        (self.on_bytes)(n as u64);
        Ok(n)
    }
}

fn header_for(md: &fs::Metadata, entry_type: tar::EntryType, size: u64) -> tar::Header {
    use std::os::unix::fs::MetadataExt;
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(entry_type);
    h.set_mode(md.mode() & 0o7777);
    h.set_mtime(md.mtime().max(0) as u64);
    h.set_uid(0);
    h.set_gid(0);
    h.set_size(size);
    h
}

/// Total size of regular files under `dir`.
pub fn data_size(dir: &Path, cancel: &CancelToken) -> Result<u64> {
    let mut total = 0;
    for entry in walkdir::WalkDir::new(dir).follow_links(false) {
        cancel.check()?;
        let entry =
            entry.map_err(|e| Error::Io { context: format!("could not read {}", dir.display()), source: e.into() })?;
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

/// Writes a backup of `instance` (data in `data_dir`) to `out`.
pub fn create_backup(
    instance: &Instance,
    data_dir: &Path,
    out: &Path,
    progress: ProgressFn<'_>,
    cancel: &CancelToken,
) -> Result<BackupManifest> {
    if !data_dir.is_dir() {
        return Err(Error::Io {
            context: format!("the data folder {} does not exist", data_dir.display()),
            source: io::Error::from(io::ErrorKind::NotFound),
        });
    }
    let total = data_size(data_dir, cancel)?;
    let manifest = BackupManifest {
        format: FORMAT_NAME.into(),
        version: FORMAT_VERSION,
        created: time::OffsetDateTime::now_utc(),
        multipak_version: env!("CARGO_PKG_VERSION").into(),
        total_bytes: total,
        instance: instance.clone(),
    };
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).ctx(|| format!("could not create {}", parent.display()))?;
    }
    let tmp = temp_path_for(out, "partial");
    let result = write_archive(&manifest, data_dir, &tmp, progress, cancel).and_then(|()| {
        fs::rename(&tmp, out).ctx(|| format!("could not write {}", out.display()))?;
        if let Some(parent) = out.parent() {
            sync_dir(parent);
        }
        Ok(())
    });
    match result {
        Ok(()) => Ok(manifest),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(if cancel.is_cancelled() { Error::Cancelled } else { e })
        }
    }
}

fn write_archive(
    manifest: &BackupManifest,
    data_dir: &Path,
    tmp: &Path,
    progress: ProgressFn<'_>,
    cancel: &CancelToken,
) -> Result<()> {
    let io_err = |e: io::Error| {
        if e.kind() == io::ErrorKind::Interrupted {
            Error::Cancelled
        } else {
            Error::Io { context: format!("could not write {}", tmp.display()), source: e }
        }
    };
    let file = File::create(tmp).ctx(|| format!("could not create {}", tmp.display()))?;
    let encoder = zstd::Encoder::new(BufWriter::new(file), 3).map_err(io_err)?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);

    let json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| Error::Parse { what: "backup manifest".into(), reason: e.to_string() })?;
    let mut h = tar::Header::new_gnu();
    h.set_size(json.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(manifest.created.unix_timestamp().max(0) as u64);
    h.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut h, MANIFEST_NAME, json.as_slice()).map_err(io_err)?;

    let done = AtomicU64::new(0);
    let last_report = AtomicU64::new(0);
    let total = manifest.total_bytes;
    for entry in walkdir::WalkDir::new(data_dir).follow_links(false).sort_by_file_name() {
        cancel.check()?;
        let entry = entry
            .map_err(|e| Error::Io { context: format!("could not read {}", data_dir.display()), source: e.into() })?;
        let rel = entry.path().strip_prefix(data_dir).expect("walkdir stays inside root");
        let name = Path::new(DATA_PREFIX).join(rel);
        let md = match entry.path().symlink_metadata() {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // removed while scanning
            Err(e) => return Err(e).ctx(|| format!("could not read {}", entry.path().display())),
        };
        let ft = md.file_type();
        if ft.is_dir() {
            let mut h = header_for(&md, tar::EntryType::Directory, 0);
            builder.append_data(&mut h, &name, io::empty()).map_err(io_err)?;
        } else if ft.is_symlink() {
            let target =
                fs::read_link(entry.path()).ctx(|| format!("could not read link {}", entry.path().display()))?;
            let mut h = header_for(&md, tar::EntryType::Symlink, 0);
            builder.append_link(&mut h, &name, &target).map_err(io_err)?;
        } else if ft.is_file() {
            let f = match File::open(entry.path()) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).ctx(|| format!("could not read {}", entry.path().display())),
            };
            let mut h = header_for(&md, tar::EntryType::Regular, md.len());
            let current = rel.display().to_string();
            let on_bytes = |n: u64| {
                let d = done.fetch_add(n, Ordering::Relaxed) + n;
                if d >= last_report.load(Ordering::Relaxed) + (1 << 20) || d == total {
                    last_report.store(d, Ordering::Relaxed);
                    progress(Progress { done: d, total, current: Some(current.clone()) });
                }
            };
            let reader = ExactReader { inner: BufReader::new(f), remaining: md.len(), cancel, on_bytes: &on_bytes };
            builder.append_data(&mut h, &name, reader).map_err(io_err)?;
        }
        // Sockets, FIFOs and device nodes are skipped: they cannot be restored meaningfully.
    }
    let encoder = builder.into_inner().map_err(io_err)?;
    let mut writer = encoder.finish().map_err(io_err)?;
    writer.flush().map_err(io_err)?;
    let file = writer.into_inner().map_err(|e| io_err(e.into_error()))?;
    file.sync_all().map_err(io_err)?;
    progress(Progress { done: total, total, current: None });
    Ok(())
}

fn open_archive(path: &Path) -> Result<tar::Archive<zstd::Decoder<'static, BufReader<File>>>> {
    let file = File::open(path).ctx(|| format!("could not open {}", path.display()))?;
    let decoder = zstd::Decoder::new(file)
        .map_err(|e| Error::InvalidBackup { path: path.to_path_buf(), reason: e.to_string() })?;
    Ok(tar::Archive::new(decoder))
}

fn parse_manifest(path: &Path, mut entry: impl Read) -> Result<BackupManifest> {
    let invalid = |reason: String| Error::InvalidBackup { path: path.to_path_buf(), reason };
    let mut buf = Vec::new();
    entry.by_ref().take(1 << 20).read_to_end(&mut buf).map_err(|e| invalid(e.to_string()))?;
    let manifest: BackupManifest = serde_json::from_slice(&buf).map_err(|e| invalid(format!("bad manifest: {e}")))?;
    if manifest.format != FORMAT_NAME {
        return Err(invalid(format!("unknown format `{}`", manifest.format)));
    }
    if manifest.version > FORMAT_VERSION {
        return Err(invalid(format!(
            "it was made by a newer Multipak (backup format {}); please update",
            manifest.version
        )));
    }
    Ok(manifest)
}

/// Reads only the manifest of a backup.
pub fn read_manifest(path: &Path) -> Result<BackupManifest> {
    let mut archive = open_archive(path)?;
    let invalid = |reason: String| Error::InvalidBackup { path: path.to_path_buf(), reason };
    let mut entries = archive.entries().map_err(|e| invalid(e.to_string()))?;
    let first =
        entries.next().ok_or_else(|| invalid("the archive is empty".into()))?.map_err(|e| invalid(e.to_string()))?;
    if first.path().map_err(|e| invalid(e.to_string()))?.as_ref() != Path::new(MANIFEST_NAME) {
        return Err(invalid(format!("{MANIFEST_NAME} is missing")));
    }
    parse_manifest(path, first)
}

fn is_safe_relative(p: &Path) -> bool {
    p.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Restores a backup's data into `target_dir`, replacing what is there.
pub fn restore_backup(
    archive_path: &Path,
    target_dir: &Path,
    progress: ProgressFn<'_>,
    cancel: &CancelToken,
) -> Result<BackupManifest> {
    let invalid = |reason: String| Error::InvalidBackup { path: archive_path.to_path_buf(), reason };
    let parent =
        target_dir.parent().ok_or_else(|| Error::invalid("data folder", "the restore target has no parent folder"))?;
    fs::create_dir_all(parent).ctx(|| format!("could not create {}", parent.display()))?;
    let staging = temp_path_for(target_dir, "restore");
    fs::create_dir(&staging).ctx(|| format!("could not create {}", staging.display()))?;
    let staged_data = staging.join(DATA_PREFIX);
    fs::create_dir(&staged_data).ctx(|| format!("could not create {}", staged_data.display()))?;

    let result = (|| -> Result<BackupManifest> {
        let mut archive = open_archive(archive_path)?;
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive.set_unpack_xattrs(false);
        archive.set_overwrite(true);
        let mut manifest: Option<BackupManifest> = None;
        let mut done = 0u64;
        for entry in archive.entries().map_err(|e| invalid(e.to_string()))? {
            cancel.check()?;
            let mut entry = entry.map_err(|e| invalid(e.to_string()))?;
            let path = entry.path().map_err(|e| invalid(e.to_string()))?.into_owned();
            if manifest.is_none() {
                if path != Path::new(MANIFEST_NAME) {
                    return Err(invalid(format!("{MANIFEST_NAME} is missing")));
                }
                manifest = Some(parse_manifest(archive_path, &mut entry)?);
                continue;
            }
            let Ok(rel) = path.strip_prefix(DATA_PREFIX) else {
                return Err(invalid(format!("unexpected entry {}", path.display())));
            };
            if !is_safe_relative(rel) {
                return Err(invalid(format!("unsafe path {}", path.display())));
            }
            let kind = entry.header().entry_type();
            if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
                continue;
            }
            if rel.as_os_str().is_empty() {
                continue;
            }
            let size = entry.size();
            // unpack_in refuses paths that would escape the staging folder (including
            // through symlinks created by earlier entries).
            let unpacked = entry
                .unpack_in(&staging)
                .map_err(|e| Error::Io { context: format!("could not extract {}", path.display()), source: e })?;
            if !unpacked {
                return Err(invalid(format!("unsafe path {}", path.display())));
            }
            done += size;
            let total = manifest.as_ref().map(|m| m.total_bytes).unwrap_or(0);
            progress(Progress {
                done: done.min(total.max(done)),
                total: total.max(done),
                current: Some(rel.display().to_string()),
            });
        }
        manifest.ok_or_else(|| invalid("the archive is empty".into()))
    })();

    let manifest = match result {
        Ok(m) => m,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // Swap the restored data in, keeping the old data until the swap succeeded.
    let old = temp_path_for(target_dir, "old");
    let had_old = target_dir.exists();
    if had_old && let Err(e) = fs::rename(target_dir, &old) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e).ctx(|| format!("could not move {} aside", target_dir.display()));
    }
    if let Err(e) = fs::rename(&staged_data, target_dir) {
        if had_old {
            let _ = fs::rename(&old, target_dir);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(e).ctx(|| format!("could not move restored data to {}", target_dir.display()));
    }
    sync_dir(parent);
    let _ = fs::remove_dir_all(&staging);
    if had_old {
        fs::remove_dir_all(&old)
            .ctx(|| format!("restored, but could not remove the previous data at {}", old.display()))?;
    }
    progress(Progress { done: manifest.total_bytes, total: manifest.total_bytes, current: None });
    Ok(manifest)
}

/// Path of the backups folder inside an export, and helpers for its file names.
pub fn backup_file_for_key(dir: &Path, instance: &Instance) -> PathBuf {
    dir.join(format!("{}--{}.tar.zst", instance.app_id, instance.slug))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apptype::Strategy;
    use crate::instance::{DraftContext, InstanceDraft};
    use crate::paths::Paths;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    fn instance() -> Instance {
        let paths = Paths::for_home("/home/u");
        InstanceDraft::new("com.slack.Slack", "Slack", "Work", Strategy::Electron, &[])
            .validate(DraftContext { paths: &paths, existing: &[], editing: None })
            .unwrap()
            .instance
    }

    fn populate(dir: &Path) {
        fs::create_dir_all(dir.join("Slack/Local Storage")).unwrap();
        fs::write(dir.join("Slack/Local Storage/leveldb"), vec![7u8; 300_000]).unwrap();
        fs::write(dir.join("Slack/prefs.json"), "{\"a\":1}").unwrap();
        fs::set_permissions(dir.join("Slack/prefs.json"), fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink("prefs.json", dir.join("Slack/prefs-link")).unwrap();
        fs::create_dir_all(dir.join("empty")).unwrap();
        let long = "x".repeat(150);
        fs::write(dir.join(&long), "long name").unwrap();
    }

    #[test]
    fn backup_and_restore_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        populate(&src);
        let archive = tmp.path().join("b/backup.tar.zst");
        let seen = Mutex::new(Vec::new());
        let report = |p: Progress| seen.lock().unwrap().push(p);
        let m = create_backup(&instance(), &src, &archive, &report, &CancelToken::new()).unwrap();
        assert_eq!(m.total_bytes, 300_000 + 7 + 9);
        assert!(seen.lock().unwrap().last().unwrap().done == m.total_bytes);
        assert_eq!(read_manifest(&archive).unwrap().instance, m.instance);

        let target = tmp.path().join("restored");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("stale"), "old").unwrap();
        restore_backup(&archive, &target, &|_| {}, &CancelToken::new()).unwrap();
        assert!(!target.join("stale").exists());
        assert_eq!(fs::read(target.join("Slack/Local Storage/leveldb")).unwrap().len(), 300_000);
        assert_eq!(fs::read_to_string(target.join("Slack/prefs.json")).unwrap(), "{\"a\":1}");
        assert_eq!(fs::metadata(target.join("Slack/prefs.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_link(target.join("Slack/prefs-link")).unwrap(), PathBuf::from("prefs.json"));
        assert!(target.join("empty").is_dir());
        assert!(target.join("x".repeat(150)).is_file());
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn cancelled_backup_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        populate(&src);
        let archive = tmp.path().join("backup.tar.zst");
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = create_backup(&instance(), &src, &archive, &|_| {}, &cancel).unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1, "only src should remain");
    }

    #[test]
    fn rejects_foreign_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("bad.tar.zst");
        fs::write(&bad, b"not zstd").unwrap();
        assert!(read_manifest(&bad).is_err());

        // A valid tar.zst without a manifest.
        let other = tmp.path().join("other.tar.zst");
        {
            let enc = zstd::Encoder::new(File::create(&other).unwrap(), 3).unwrap().auto_finish();
            let mut b = tar::Builder::new(enc);
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "evil", &b"x"[..]).unwrap();
            b.finish().unwrap();
        }
        assert!(matches!(read_manifest(&other), Err(Error::InvalidBackup { .. })));
        let target = tmp.path().join("t");
        assert!(restore_backup(&other, &target, &|_| {}, &CancelToken::new()).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let evil = tmp.path().join("evil.tar.zst");
        {
            let enc = zstd::Encoder::new(File::create(&evil).unwrap(), 3).unwrap().auto_finish();
            let mut b = tar::Builder::new(enc);
            let manifest = BackupManifest {
                format: FORMAT_NAME.into(),
                version: 1,
                created: time::OffsetDateTime::now_utc(),
                multipak_version: "0".into(),
                total_bytes: 1,
                instance: instance(),
            };
            let json = serde_json::to_vec(&manifest).unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_size(json.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, MANIFEST_NAME, json.as_slice()).unwrap();
            // Write a raw "data/../../escape" path, bypassing the builder's checks.
            let mut h = tar::Header::new_gnu();
            h.as_gnu_mut().unwrap().name[..17].copy_from_slice(b"data/../../escape");
            h.set_size(1);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append(&h, &b"x"[..]).unwrap();
            b.finish().unwrap();
        }
        let target = tmp.path().join("inner/t");
        let err = restore_backup(&evil, &target, &|_| {}, &CancelToken::new()).unwrap_err();
        assert!(matches!(err, Error::InvalidBackup { .. }), "{err}");
        assert!(!tmp.path().join("escape").exists());
        assert!(!target.exists());
    }
}
