//! The "doctor": finds broken or outdated pieces of the setup and offers one-click fixes.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::instance::InstanceKey;
use crate::keyfile::KeyFile;
use crate::manager::{Manager, app_categories, launcher_text};

/// What is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueKind {
    FlatpakMissing,
    AppNotInstalled,
    DataDirMissing,
    LauncherMissing,
    LauncherOutdated,
    AutostartMissing,
    AutostartOutdated,
    AutostartStale,
    BadgeIconMissing,
    OldOverride,
    CaptureHelperMissing,
    CaptureHelperLeftover,
    OrphanFile,
    OrphanCaptureRule,
}

/// A one-click repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
    /// Rewrite the launcher, autostart entry and badge icon.
    WriteLaunchers(InstanceKey),
    /// Create an empty data folder (and Firefox profile).
    CreateDataDir(InstanceKey),
    /// Remove the instance from Multipak; its data folder is kept.
    RemoveInstance(InstanceKey),
    /// Delete a leftover file Multipak created.
    RemoveFile(PathBuf),
    /// Remove the folder access Multipak 0.1 granted for a custom data folder.
    RemoveOldOverride(InstanceKey),
    /// Install, update or remove the KWin script that hides windows from capture.
    SyncCapture,
    /// Remove a leftover Multipak window rule from `kwinrulesrc`.
    RemoveCaptureRule(String),
}

impl Fix {
    pub fn label(&self) -> &'static str {
        match self {
            Self::WriteLaunchers(_) => "Rewrite launcher",
            Self::CreateDataDir(_) => "Create empty data folder",
            Self::RemoveInstance(_) => "Remove instance (keep data)",
            Self::RemoveFile(_) => "Delete file",
            Self::RemoveOldOverride(_) => "Remove folder access",
            Self::RemoveCaptureRule(_) => "Remove window rule",
            Self::SyncCapture => "Update capture helper",
        }
    }
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub key: Option<InstanceKey>,
    pub kind: IssueKind,
    pub message: String,
    pub fix: Option<Fix>,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.key {
            Some(k) => write!(f, "{k}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

fn file_differs(path: &Path, expected: &str) -> Option<bool> {
    match fs::read_to_string(path) {
        Ok(actual) => Some(actual != expected),
        Err(_) => None,
    }
}

impl Manager {
    /// Checks every instance and the launcher folders.
    pub fn doctor(&self) -> Result<Vec<Issue>> {
        let config = self.load()?;
        let paths = self.paths();
        let mut issues = Vec::new();
        let flatpak_bin = match self.binaries() {
            Ok(b) => Some(b),
            Err(_) => {
                issues.push(Issue {
                    key: None,
                    kind: IssueKind::FlatpakMissing,
                    message:
                        "The flatpak command was not found. Install Flatpak; launchers cannot be checked without it."
                            .into(),
                    fix: None,
                });
                None
            }
        };
        let mut installed: HashMap<String, bool> = HashMap::new();
        for inst in &config.instances {
            let key = inst.key();
            let issue = |kind, message: String, fix| Issue { key: Some(key.clone()), kind, message, fix };
            let Some(bin) = &flatpak_bin else { continue };
            let is_installed = match installed.get(&inst.app_id) {
                Some(v) => *v,
                None => {
                    let v = self.flatpak().is_installed(&inst.app_id)?;
                    installed.insert(inst.app_id.clone(), v);
                    v
                }
            };
            if !is_installed {
                issues.push(issue(
                    IssueKind::AppNotInstalled,
                    format!(
                        "{} is no longer installed. Reinstall it, or remove this instance (its data is kept).",
                        inst.app_id
                    ),
                    Some(Fix::RemoveInstance(key.clone())),
                ));
                continue;
            }
            let data_dir = inst.resolved_data_dir(paths);
            if !data_dir.is_dir() {
                issues.push(issue(
                    IssueKind::DataDirMissing,
                    format!("The data folder {} is missing.", data_dir.display()),
                    Some(Fix::CreateDataDir(key.clone())),
                ));
            }
            let categories = app_categories(paths, &inst.app_id);
            let expected = launcher_text(paths, inst, bin, &categories, false);
            let desktop = paths.desktop_file(&inst.app_id, &inst.slug);
            match file_differs(&desktop, &expected) {
                None => issues.push(issue(
                    IssueKind::LauncherMissing,
                    "The launcher is missing.".into(),
                    Some(Fix::WriteLaunchers(key.clone())),
                )),
                Some(true) => issues.push(issue(
                    IssueKind::LauncherOutdated,
                    "The launcher is out of date.".into(),
                    Some(Fix::WriteLaunchers(key.clone())),
                )),
                Some(false) => {}
            }
            let autostart = paths.autostart_file(&inst.app_id, &inst.slug);
            if inst.autostart {
                let expected = launcher_text(paths, inst, bin, &categories, true);
                match file_differs(&autostart, &expected) {
                    None => issues.push(issue(
                        IssueKind::AutostartMissing,
                        "The autostart entry is missing.".into(),
                        Some(Fix::WriteLaunchers(key.clone())),
                    )),
                    Some(true) => issues.push(issue(
                        IssueKind::AutostartOutdated,
                        "The autostart entry is out of date.".into(),
                        Some(Fix::WriteLaunchers(key.clone())),
                    )),
                    Some(false) => {}
                }
            } else if autostart.exists() {
                issues.push(issue(
                    IssueKind::AutostartStale,
                    "An autostart entry exists although autostart is off.".into(),
                    Some(Fix::WriteLaunchers(key.clone())),
                ));
            }
            if inst.badge.is_some() && !paths.badge_icon(&inst.app_id, &inst.slug).is_file() {
                issues.push(issue(
                    IssueKind::BadgeIconMissing,
                    "The badge icon is missing.".into(),
                    Some(Fix::WriteLaunchers(key.clone())),
                ));
            }
            if let Some(dir) = &inst.managed_override {
                issues.push(issue(
                    IssueKind::OldOverride,
                    format!("An older version gave the app access to {dir}, which it no longer needs."),
                    Some(Fix::RemoveOldOverride(key.clone())),
                ));
            }
        }
        issues.extend(self.capture_issues(&config));
        issues.extend(self.orphans(&config)?);
        Ok(issues)
    }

    fn capture_issues(&self, config: &crate::config::Config) -> Vec<Issue> {
        let paths = self.paths();
        let wanted = config.instances.iter().any(|i| i.hide_from_capture);
        let mut issues = Vec::new();
        if !self.capture_support().is_supported() {
            return issues;
        }
        match (wanted, self.capture_helper()) {
            (true, Some(exe)) if !crate::capture::helper_installed(paths, exe) => issues.push(Issue {
                key: None,
                kind: IssueKind::CaptureHelperMissing,
                message: "The KWin script that hides instances from screen capture is missing, disabled or points to an old Multipak location.".into(),
                fix: Some(Fix::SyncCapture),
            }),
            (false, _) if crate::capture::helper_present(paths) => issues.push(Issue {
                key: None,
                kind: IssueKind::CaptureHelperLeftover,
                message: "The KWin capture script is installed although no instance hides from capture.".into(),
                fix: Some(Fix::SyncCapture),
            }),
            _ => {}
        }
        issues
    }

    fn orphans(&self, config: &crate::config::Config) -> Result<Vec<Issue>> {
        let paths = self.paths();
        let mut issues = Vec::new();
        for dir in [paths.applications_dir(), paths.autostart_dir()] {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("multipak-") || !name.ends_with(".desktop") {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else { continue };
                let kf = KeyFile::parse(&text);
                let (Some(app), Some(slug)) =
                    (kf.get("Desktop Entry", "X-Multipak-App"), kf.get("Desktop Entry", "X-Multipak-Instance"))
                else {
                    continue;
                };
                if config.get(&InstanceKey::new(app, slug)).is_none() {
                    issues.push(Issue {
                        key: None,
                        kind: IssueKind::OrphanFile,
                        message: format!("{} belongs to an instance that no longer exists.", path.display()),
                        fix: Some(Fix::RemoveFile(path)),
                    });
                }
            }
        }
        for group in crate::capture::instance_groups(paths) {
            let used = config
                .instances
                .iter()
                .any(|i| i.hide_from_capture && crate::capture::instance_group(&i.app_id, &i.slug) == group);
            if !used {
                issues.push(Issue {
                    key: None,
                    kind: IssueKind::OrphanCaptureRule,
                    message: format!(
                        "The KWin window rule {group} belongs to an instance that no longer hides from capture."
                    ),
                    fix: Some(Fix::RemoveCaptureRule(group)),
                });
            }
        }
        if let Ok(entries) = fs::read_dir(paths.badge_icons_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                let used =
                    config.instances.iter().any(|i| i.badge.is_some() && paths.badge_icon(&i.app_id, &i.slug) == path);
                if !used && path.extension().is_some_and(|e| e == "png") {
                    issues.push(Issue {
                        key: None,
                        kind: IssueKind::OrphanFile,
                        message: format!("{} is an unused badge icon.", path.display()),
                        fix: Some(Fix::RemoveFile(path)),
                    });
                }
            }
        }
        Ok(issues)
    }

    /// Applies one fix.
    pub fn apply_fix(&self, fix: &Fix) -> Result<()> {
        let config = self.load()?;
        let get = |k: &InstanceKey| config.get(k).cloned().ok_or_else(|| Error::InstanceNotFound(k.to_string()));
        match fix {
            Fix::WriteLaunchers(k) => {
                self.write_launchers(&get(k)?).map(|w| w.iter().for_each(|w| tracing::warn!("{w}")))
            }
            Fix::CreateDataDir(k) => {
                let inst = get(k)?;
                let dir = inst.resolved_data_dir(self.paths());
                for d in crate::launch::dirs_to_create(&dir) {
                    fs::create_dir_all(&d)
                        .map_err(|e| Error::Io { context: format!("could not create {}", d.display()), source: e })?;
                }
                Ok(())
            }
            Fix::RemoveInstance(k) => self.delete(k, false).map(|_| ()),
            Fix::RemoveFile(path) => {
                let paths = self.paths();
                let allowed = [paths.applications_dir(), paths.autostart_dir(), paths.badge_icons_dir()];
                let parent_ok = path.parent().is_some_and(|p| allowed.iter().any(|a| a == p));
                let name_ok = path.file_name().is_some_and(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("multipak-") || path.parent() == Some(paths.badge_icons_dir().as_path())
                });
                if !parent_ok || !name_ok {
                    return Err(Error::UnsafeDelete {
                        path: path.clone(),
                        reason: "it is not a file Multipak created".into(),
                    });
                }
                fs::remove_file(path)
                    .map_err(|e| Error::Io { context: format!("could not remove {}", path.display()), source: e })
            }
            Fix::SyncCapture => {
                for w in self.sync_capture()? {
                    tracing::warn!("{w}");
                }
                Ok(())
            }
            Fix::RemoveCaptureRule(group) => {
                if crate::capture::set_rule(self.paths(), group, "", "", false)?
                    && self.capture_support().is_supported()
                {
                    crate::capture::reconfigure_kwin(self.host())?;
                }
                Ok(())
            }
            Fix::RemoveOldOverride(k) => self.remove_old_override(k),
        }
    }
}
