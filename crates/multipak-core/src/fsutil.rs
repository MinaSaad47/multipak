//! Filesystem helpers: disk usage, cache cleanup and guarded deletion.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::apptype::Strategy;
use crate::backup::CancelToken;
use crate::error::{Error, IoContext, Result};
use crate::paths::Paths;

/// Whether one path equals or contains the other.
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Space used on disk by everything under `dir` (allocated blocks, hard links counted
/// once, symlinks not followed). Returns 0 for a missing folder.
pub fn disk_usage(dir: &Path, cancel: &CancelToken) -> Result<u64> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(dir).follow_links(false) {
        cancel.check()?;
        let Ok(entry) = entry else { continue };
        let Ok(md) = entry.metadata() else { continue };
        if md.nlink() > 1 && !md.is_dir() && !seen.insert((md.dev(), md.ino())) {
            continue;
        }
        total += md.blocks() * 512;
    }
    Ok(total)
}

/// Folder names that only hold caches, for Electron/Chromium and Firefox profiles.
const CACHE_DIR_NAMES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GrShaderCache",
    "GraphiteDawnCache",
    "ShaderCache",
    "CacheStorage",
    "ScriptCache",
    "cache2",
    "startupCache",
    "thumbnails",
];

/// Cache folders inside an instance's data folder.
pub fn find_cache_dirs(data_dir: &Path, strategy: Strategy) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if strategy == Strategy::Generic {
        let cache = data_dir.join("cache");
        if cache.is_dir() {
            found.push(cache);
        }
    }
    let mut walker = walkdir::WalkDir::new(data_dir).min_depth(1).max_depth(5).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if CACHE_DIR_NAMES.contains(&name.as_ref()) {
            if !found.iter().any(|f: &PathBuf| entry.path().starts_with(f)) {
                found.push(entry.path().to_path_buf());
            }
            walker.skip_current_dir();
        }
    }
    found
}

/// Removes everything inside `dir` but keeps the folder. Returns bytes freed.
pub fn clear_dir_contents(dir: &Path, cancel: &CancelToken) -> Result<u64> {
    let freed = disk_usage(dir, cancel)?;
    for entry in fs::read_dir(dir).ctx(|| format!("could not read {}", dir.display()))? {
        cancel.check()?;
        let entry = entry.ctx(|| format!("could not read {}", dir.display()))?;
        let path = entry.path();
        let ft = entry.file_type().ctx(|| format!("could not inspect {}", path.display()))?;
        if ft.is_dir() {
            fs::remove_dir_all(&path).ctx(|| format!("could not remove {}", path.display()))?;
        } else {
            fs::remove_file(&path).ctx(|| format!("could not remove {}", path.display()))?;
        }
    }
    Ok(freed)
}

/// Checks that deleting `dir` cannot destroy anything but one instance's own data.
pub fn check_safe_to_delete(dir: &Path, paths: &Paths, app_id: &str, other_dirs: &[PathBuf]) -> Result<()> {
    let refuse = |reason: &str| Err(Error::UnsafeDelete { path: dir.to_path_buf(), reason: reason.to_string() });
    if !dir.is_absolute() {
        return refuse("the path is not absolute");
    }
    let md = match fs::symlink_metadata(dir) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return refuse("it does not exist"),
        Err(e) => return Err(e).ctx(|| format!("could not inspect {}", dir.display())),
    };
    if md.file_type().is_symlink() {
        return refuse("it is a symbolic link");
    }
    if !md.is_dir() {
        return refuse("it is not a folder");
    }
    let canonical = fs::canonicalize(dir).ctx(|| format!("could not resolve {}", dir.display()))?;
    if canonical.components().count() < 3 {
        return refuse("it is too close to the root of the filesystem");
    }
    let mut protected = paths.protected_dirs();
    protected.push(paths.app_var_dir(app_id));
    protected.push(paths.instances_root(app_id));
    for p in protected {
        let p = fs::canonicalize(&p).unwrap_or(p);
        if p.starts_with(&canonical) {
            return refuse("it is, or contains, a system or shared folder");
        }
    }
    for other in other_dirs {
        let other = fs::canonicalize(other).unwrap_or_else(|_| other.clone());
        if paths_overlap(&other, &canonical) {
            return refuse("it overlaps the data folder of another instance");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap() {
        assert!(paths_overlap(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(paths_overlap(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!paths_overlap(Path::new("/a/b"), Path::new("/a/bc")));
    }

    #[test]
    fn usage_counts_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), vec![1u8; 10_000]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b"), vec![1u8; 10_000]).unwrap();
        fs::hard_link(dir.path().join("a"), dir.path().join("a-link")).unwrap();
        let cancel = CancelToken::new();
        let used = disk_usage(dir.path(), &cancel).unwrap();
        assert!(used >= 20_000, "{used}");
        assert!(used < 60_000, "hard link counted twice: {used}");
        assert_eq!(disk_usage(&dir.path().join("missing"), &cancel).unwrap(), 0);
        cancel.cancel();
        assert!(matches!(disk_usage(dir.path(), &cancel), Err(Error::Cancelled)));
    }

    #[test]
    fn cache_dirs_and_clearing() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fs::create_dir_all(d.join("Slack/Cache/Cache_Data")).unwrap();
        fs::write(d.join("Slack/Cache/Cache_Data/x"), "data").unwrap();
        fs::create_dir_all(d.join("Slack/Code Cache/js")).unwrap();
        fs::create_dir_all(d.join("Slack/Local Storage")).unwrap();
        fs::write(d.join("Slack/Local Storage/keep"), "keep").unwrap();
        let mut found = find_cache_dirs(d, Strategy::Electron);
        found.sort();
        assert_eq!(found, vec![d.join("Slack/Cache"), d.join("Slack/Code Cache")]);
        let freed = clear_dir_contents(&d.join("Slack/Cache"), &CancelToken::new()).unwrap();
        assert!(freed > 0);
        assert!(d.join("Slack/Cache").is_dir());
        assert!(!d.join("Slack/Cache/Cache_Data").exists());
        assert!(d.join("Slack/Local Storage/keep").exists());

        fs::create_dir_all(d.join("cache/fontconfig")).unwrap();
        assert!(find_cache_dirs(d, Strategy::Generic).contains(&d.join("cache")));
    }

    #[test]
    fn delete_guard() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let paths = Paths::for_home(&home);
        let inst = paths.default_instance_dir("com.x.Y", "work");
        fs::create_dir_all(&inst).unwrap();
        let other = paths.default_instance_dir("com.x.Y", "home");
        fs::create_dir_all(&other).unwrap();
        assert!(check_safe_to_delete(&inst, &paths, "com.x.Y", std::slice::from_ref(&other)).is_ok());
        for bad in [home.clone(), paths.app_var_dir("com.x.Y"), paths.instances_root("com.x.Y"), PathBuf::from("/")] {
            assert!(check_safe_to_delete(&bad, &paths, "com.x.Y", &[]).is_err(), "{}", bad.display());
        }
        assert!(check_safe_to_delete(&inst, &paths, "com.x.Y", &[inst.join("nested")]).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&inst, &link).unwrap();
        assert!(check_safe_to_delete(&link, &paths, "com.x.Y", &[]).is_err());
        assert!(check_safe_to_delete(&dir.path().join("nope"), &paths, "com.x.Y", &[]).is_err());
    }
}
