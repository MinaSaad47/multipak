//! Atomic file writes: write a temporary file, fsync it, rename it over the target and
//! fsync the folder, so a crash never leaves a half-written file behind.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{IoContext, Result};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temporary path next to `path`, unique within this process.
pub fn temp_path_for(path: &Path, tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    path.with_file_name(format!(".{name}.{tag}.{}.{n}", std::process::id()))
}

/// Writes `contents` to `path` atomically with the given Unix permission bits.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent).ctx(|| format!("could not create folder {}", parent.display()))?;
    let tmp = temp_path_for(path, "tmp");
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .ctx(|| format!("could not create {}", tmp.display()))?;
        file.write_all(contents).ctx(|| format!("could not write {}", tmp.display()))?;
        file.sync_all().ctx(|| format!("could not flush {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, path).ctx(|| format!("could not replace {}", path.display()))?;
        sync_dir(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Best-effort fsync of a folder so a rename inside it is durable.
pub fn sync_dir(dir: &Path) {
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/file.txt");
        write_atomic(&path, b"one", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one");
        write_atomic(&path, b"two", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left behind");
    }
}
