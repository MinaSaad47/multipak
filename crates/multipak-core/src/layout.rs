//! Moves data folders written by Multipak 0.1 into the current layout.
//!
//! Multipak 0.1 isolated non-generic instances with flags and variables, so their data
//! folders hold what the app would normally keep elsewhere in its app folder: an Electron
//! app's config folder, a Chromium user data folder or a Firefox profile. Now that the data
//! folder simply replaces the app folder, that data is moved to where the app looks for it.
//! A marker file records that a folder has the current layout, so this runs once.

use std::fs;
use std::path::{Path, PathBuf};

use crate::apptype::Strategy;
use crate::error::{IoContext, Result};
use crate::instance::Instance;
use crate::keyfile::KeyFile;
use crate::launch::APP_DIRS;
use crate::paths::Paths;

/// Marks a data folder that has the current layout.
pub const MARKER: &str = ".multipak-layout";
const VERSION: &str = "2\n";
/// Name of the profile folder made from an old Firefox instance.
const FIREFOX_PROFILE: &str = "multipak";

/// What happened to a data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Already in the current layout (or empty).
    Current,
    /// Old data was moved into place.
    Migrated,
    /// Old data could not be placed yet; the message says what to do. Retried later.
    Pending(String),
}

/// Whether `data_dir` is known to have the current layout.
pub fn is_current(data_dir: &Path) -> bool {
    data_dir.join(MARKER).is_file()
}

/// Records that `data_dir` has the current layout.
pub fn mark(data_dir: &Path) -> Result<()> {
    crate::atomic::write_atomic(&data_dir.join(MARKER), VERSION.as_bytes(), 0o644)
}

/// Brings the data folder of `instance` into the current layout.
pub fn migrate(paths: &Paths, instance: &Instance) -> Result<Outcome> {
    let data_dir = instance.resolved_data_dir(paths);
    if is_current(&data_dir) {
        return Ok(Outcome::Current);
    }
    let old = old_entries(&data_dir)?;
    if old.is_empty() || instance.strategy == Strategy::Generic {
        // Generic folders already had the layout of an app folder.
        if data_dir.is_dir() {
            mark(&data_dir)?;
        }
        return Ok(Outcome::Current);
    }
    let app_dir = paths.app_var_dir(&instance.app_id);
    let name = instance.display_name();
    let not_found = |what: &str| {
        Outcome::Pending(format!(
            "{name} keeps data in the old layout. Start {} once without Multipak so it creates its {what}, then reopen Multipak to move the data.",
            instance.app_id
        ))
    };
    match instance.strategy {
        Strategy::Generic => unreachable!("handled above"),
        Strategy::Electron => move_into(&data_dir, &old, &data_dir.join("config"))?,
        Strategy::Chromium => {
            // The user data folder is the one with `Local State`, for example
            // `config/google-chrome` or `config/BraveSoftware/Brave-Browser`.
            let Some(rel) = find_in(&app_dir, &app_dir.join("config"), &data_dir, |d| d.join("Local State").is_file())
            else {
                return Ok(not_found("profile folder"));
            };
            move_into(&data_dir, &old, &data_dir.join(rel))?;
        }
        Strategy::Firefox => {
            // The folder with `profiles.ini`, for example `.mozilla/firefox` or `.zen`.
            let Some(rel) = find_in(&app_dir, &app_dir, &data_dir, |d| d.join("profiles.ini").is_file()) else {
                return Ok(not_found("profile list"));
            };
            let root = data_dir.join(&rel);
            move_into(&data_dir, &old, &root.join(FIREFOX_PROFILE))?;
            write_firefox_profile_list(&root, &app_dir.join(&rel))?;
            forget_old_firefox_profile(&app_dir.join(&rel), instance);
        }
    }
    mark(&data_dir)?;
    tracing::info!("moved the data of {} into the current layout", instance.key());
    Ok(Outcome::Migrated)
}

/// Top-level entries of an old data folder that are not part of an app folder.
fn old_entries(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let Ok(read) = fs::read_dir(data_dir) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.ctx(|| format!("could not read {}", data_dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if APP_DIRS.contains(&name.as_ref()) || matches!(name.as_ref(), ".local" | ".ld.so" | MARKER) {
            continue;
        }
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
}

/// Moves `entries` (all directly inside `data_dir`) into `target`.
fn move_into(data_dir: &Path, entries: &[PathBuf], target: &Path) -> Result<()> {
    fs::create_dir_all(target).ctx(|| format!("could not create {}", target.display()))?;
    for from in entries {
        if target.starts_with(from) {
            continue;
        }
        let to = target.join(from.strip_prefix(data_dir).unwrap_or(from));
        if to.exists() {
            tracing::warn!("{} already exists; leaving {} where it is", to.display(), from.display());
            continue;
        }
        fs::rename(from, &to).ctx(|| format!("could not move {} to {}", from.display(), to.display()))?;
    }
    Ok(())
}

/// The first folder under `start` (inside `app_dir`, at most four levels deep, never inside
/// instance data) that satisfies `found`, relative to `app_dir`.
fn find_in(app_dir: &Path, start: &Path, data_dir: &Path, found: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let instances = app_dir.join("instances");
    let mut walker = walkdir::WalkDir::new(start).max_depth(4).sort_by_file_name().into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_dir() {
            continue;
        }
        let p = entry.path();
        if p.starts_with(&instances) || p.starts_with(data_dir) {
            walker.skip_current_dir();
            continue;
        }
        if found(p) {
            return p.strip_prefix(app_dir).ok().map(Path::to_path_buf);
        }
    }
    None
}

/// Writes a profile list in `root` whose only profile is the moved one, and makes it the
/// default of every Firefox installation the app's own list knows.
fn write_firefox_profile_list(root: &Path, app_root: &Path) -> Result<()> {
    let installs = fs::read_to_string(app_root.join("installs.ini")).map(|t| KeyFile::parse(&t)).unwrap_or_default();
    let mut profiles = KeyFile::default();
    profiles.set("Profile0", "Name", FIREFOX_PROFILE);
    profiles.set("Profile0", "IsRelative", "1");
    profiles.set("Profile0", "Path", FIREFOX_PROFILE);
    profiles.set("Profile0", "Default", "1");
    profiles.set("General", "StartWithLastProfile", "1");
    profiles.set("General", "Version", "2");
    let mut ours = KeyFile::default();
    for hash in installs.groups() {
        profiles.set(&format!("Install{hash}"), "Default", FIREFOX_PROFILE);
        profiles.set(&format!("Install{hash}"), "Locked", "1");
        ours.set(&hash, "Default", FIREFOX_PROFILE);
        ours.set(&hash, "Locked", "1");
    }
    crate::atomic::write_atomic(&root.join("profiles.ini"), profiles.to_text().as_bytes(), 0o644)?;
    if !ours.groups().is_empty() {
        crate::atomic::write_atomic(&root.join("installs.ini"), ours.to_text().as_bytes(), 0o644)?;
    }
    Ok(())
}

/// Multipak 0.1 registered each Firefox instance in the app's own profile list; that entry
/// now points at a folder without a profile, so it is removed.
fn forget_old_firefox_profile(app_root: &Path, instance: &Instance) {
    let file = app_root.join("profiles.ini");
    let Ok(text) = fs::read_to_string(&file) else { return };
    let mut kf = KeyFile::parse(&text);
    let name = format!("multipak-{}", instance.slug);
    let stale: Vec<String> =
        kf.groups().into_iter().filter(|g| g.starts_with("Profile") && kf.get(g, "Name") == Some(&name)).collect();
    if stale.is_empty() {
        return;
    }
    for g in &stale {
        kf.remove_group(g);
    }
    if let Err(e) = crate::atomic::write_atomic(&file, kf.to_text().as_bytes(), 0o644) {
        tracing::warn!("could not update {}: {e}", file.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{DraftContext, InstanceDraft};

    fn setup(app: &str, strategy: Strategy) -> (tempfile::TempDir, Paths, Instance) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_home(tmp.path());
        let d = InstanceDraft::new(app, "App", "Work", strategy, &[]);
        let inst = d.validate(DraftContext { paths: &paths, existing: &[], editing: None }).unwrap().instance;
        fs::create_dir_all(inst.resolved_data_dir(&paths)).unwrap();
        (tmp, paths, inst)
    }

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "x").unwrap();
    }

    #[test]
    fn electron_config_moves_into_config() {
        let (_t, paths, inst) = setup("com.slack.Slack", Strategy::Electron);
        let dir = inst.resolved_data_dir(&paths);
        touch(&dir.join("Slack/Cookies"));
        assert_eq!(migrate(&paths, &inst).unwrap(), Outcome::Migrated);
        assert!(dir.join("config/Slack/Cookies").is_file());
        assert!(!dir.join("Slack").exists());
        assert!(is_current(&dir));
        assert_eq!(migrate(&paths, &inst).unwrap(), Outcome::Current);
    }

    #[test]
    fn chromium_data_moves_to_the_browser_folder() {
        let (_t, paths, inst) = setup("com.brave.Browser", Strategy::Chromium);
        let dir = inst.resolved_data_dir(&paths);
        touch(&dir.join("Local State"));
        touch(&dir.join("Default/Preferences"));
        assert!(matches!(migrate(&paths, &inst).unwrap(), Outcome::Pending(_)));
        assert!(!is_current(&dir));

        touch(&paths.app_var_dir(&inst.app_id).join("config/BraveSoftware/Brave-Browser/Local State"));
        assert_eq!(migrate(&paths, &inst).unwrap(), Outcome::Migrated);
        assert!(dir.join("config/BraveSoftware/Brave-Browser/Default/Preferences").is_file());
        assert!(dir.join("config/BraveSoftware/Brave-Browser/Local State").is_file());
    }

    #[test]
    fn firefox_profile_becomes_the_default_profile() {
        let (_t, paths, inst) = setup("org.mozilla.firefox", Strategy::Firefox);
        let dir = inst.resolved_data_dir(&paths);
        touch(&dir.join("prefs.js"));
        let app_root = paths.app_var_dir(&inst.app_id).join(".mozilla/firefox");
        fs::create_dir_all(&app_root).unwrap();
        fs::write(
            app_root.join("profiles.ini"),
            format!(
                "[Profile0]\nName=default-release\nPath=abc.default-release\n\n[Profile1]\nName=multipak-work\nIsRelative=0\nPath={}\n",
                dir.display()
            ),
        )
        .unwrap();
        fs::write(app_root.join("installs.ini"), "[ABCDEF]\nDefault=abc.default-release\n").unwrap();

        assert_eq!(migrate(&paths, &inst).unwrap(), Outcome::Migrated);
        let root = dir.join(".mozilla/firefox");
        assert!(root.join("multipak/prefs.js").is_file());
        let profiles = KeyFile::parse(&fs::read_to_string(root.join("profiles.ini")).unwrap());
        assert_eq!(profiles.get("Profile0", "Path"), Some("multipak"));
        assert_eq!(profiles.get("InstallABCDEF", "Default"), Some("multipak"));
        let installs = KeyFile::parse(&fs::read_to_string(root.join("installs.ini")).unwrap());
        assert_eq!(installs.get("ABCDEF", "Default"), Some("multipak"));
        let app_profiles = fs::read_to_string(app_root.join("profiles.ini")).unwrap();
        assert!(app_profiles.contains("default-release") && !app_profiles.contains("multipak-work"));
    }

    #[test]
    fn generic_and_empty_folders_are_just_marked() {
        let (_t, paths, inst) = setup("org.gnome.Fractal", Strategy::Generic);
        let dir = inst.resolved_data_dir(&paths);
        touch(&dir.join("config/fractal/settings"));
        assert_eq!(migrate(&paths, &inst).unwrap(), Outcome::Current);
        assert!(dir.join("config/fractal/settings").is_file() && is_current(&dir));
    }
}
