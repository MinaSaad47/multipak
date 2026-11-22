//! [`Manager`]: every user-facing operation, shared by the GUI and the CLI.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::apptype::{self, AppTypeTable, DetectInput, Detection};
use crate::backup::{self, BackupManifest, CancelToken, ProgressFn};
use crate::capture;
use crate::config::{Config, ConfigStore};
use crate::desktop::{self, LauncherInfo};
use crate::error::{Error, IoContext, Result};
use crate::exec::{CommandSpec, Host, LaunchOutcome, split_nul};
use crate::flatpak::{self, Flatpak, FlatpakApp};
use crate::fsutil;
use crate::icons;
use crate::instance::{DraftContext, Instance, InstanceDraft, InstanceKey, MARKER_ENV, ValidatedDraft};
use crate::keyfile::KeyFile;
use crate::launch;
use crate::paths::Paths;

/// How long to watch a freshly launched app for an immediate failure.
pub const DEFAULT_LAUNCH_GRACE: Duration = Duration::from_secs(3);
const PROFILE_TIMEOUT: Duration = Duration::from_secs(90);

/// Everything known about one installed app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDetails {
    pub app_id: String,
    pub detection: Detection,
    pub icon: Option<PathBuf>,
    pub categories: Vec<String>,
}

/// A running sandbox, matched to an instance when possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningInstance {
    pub flatpak_instance: String,
    pub app_id: String,
    pub pid: u32,
    /// `None` for copies not started by Multipak (for example the app's normal launcher).
    pub key: Option<InstanceKey>,
}

/// Result of creating or editing an instance.
#[derive(Debug, Clone)]
pub struct SaveOutcome {
    pub instance: Instance,
    pub warnings: Vec<String>,
}

/// What [`Manager::delete`] removed.
#[derive(Debug, Clone, Default)]
pub struct DeleteReport {
    pub removed_files: Vec<PathBuf>,
    pub removed_override: Option<String>,
    pub data_dir: PathBuf,
    pub data_deleted: bool,
}

/// What [`Manager::clear_cache`] removed.
#[derive(Debug, Clone, Default)]
pub struct CacheReport {
    pub dirs: Vec<PathBuf>,
    pub freed_bytes: u64,
}

/// Where [`Manager::restore`] puts a backup.
#[derive(Debug, Clone)]
pub enum RestoreTarget {
    /// Replace the data of an existing instance.
    Existing(InstanceKey),
    /// Create a new instance from the backup's settings, optionally with another slug.
    New { slug: Option<String>, allow_override: bool },
}

/// The launch command, launcher text and override (if any) a draft would produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub command: String,
    pub desktop_file: String,
    pub override_command: Option<String>,
}

/// The icon value written into a launcher: the badge PNG path or the app's icon name.
pub fn launcher_icon(paths: &Paths, instance: &Instance) -> String {
    if instance.badge.is_some() {
        paths.badge_icon(&instance.app_id, &instance.slug).display().to_string()
    } else {
        instance.app_id.clone()
    }
}

/// Renders the launcher for `instance` (pure: used for previews and the doctor).
pub fn launcher_text(
    paths: &Paths,
    instance: &Instance,
    flatpak_bin: &Path,
    categories: &[String],
    autostart: bool,
) -> String {
    let data_dir = instance.resolved_data_dir(paths);
    let argv = launch::launch_spec(flatpak_bin, instance, &data_dir).argv();
    let icon = launcher_icon(paths, instance);
    desktop::render(&LauncherInfo { instance, argv: &argv, icon: &icon, categories, autostart })
}

/// Builds the preview shown while creating or editing an instance.
pub fn preview(paths: &Paths, validated: &ValidatedDraft, flatpak_bin: &Path, categories: &[String]) -> Preview {
    let inst = &validated.instance;
    Preview {
        command: launch::launch_spec(flatpak_bin, inst, &validated.data_dir).display(),
        desktop_file: launcher_text(paths, inst, flatpak_bin, categories, false),
        override_command: validated.needs_override.as_ref().map(|dir| override_command(&inst.app_id, dir)),
    }
}

/// The exact override command Multipak runs for a custom data folder.
pub fn override_command(app_id: &str, dir: &str) -> String {
    CommandSpec::new("flatpak")
        .args(["override".to_string(), "--user".to_string(), format!("--filesystem={dir}"), app_id.to_string()])
        .display()
}

/// Reads the categories from an app's exported launcher.
pub fn app_categories(paths: &Paths, app_id: &str) -> Vec<String> {
    paths
        .exported_desktop_file(app_id)
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|t| KeyFile::parse(&t).get_list("Desktop Entry", "Categories"))
        .unwrap_or_default()
}

/// The core API. Cheap to share between threads behind an `Arc`.
pub struct Manager {
    host: Arc<dyn Host>,
    paths: Paths,
    store: ConfigStore,
    types: AppTypeTable,
    launch_grace: Duration,
    capture_support: capture::Support,
}

const SELF_RULE_DESCRIPTION: &str = "Multipak: hide Multipak from screen capture";

impl Manager {
    /// Creates a manager. Fails only if the user's app-type table is unreadable.
    pub fn new(host: Arc<dyn Host>, paths: Paths) -> Result<Self> {
        let types = AppTypeTable::load(&paths)?;
        Ok(Self::with_types(host, paths, types))
    }

    /// Creates a manager with an explicit app-type table.
    pub fn with_types(host: Arc<dyn Host>, paths: Paths, types: AppTypeTable) -> Self {
        Self {
            store: ConfigStore::new(paths.config_file()),
            host,
            paths,
            types,
            launch_grace: DEFAULT_LAUNCH_GRACE,
            capture_support: capture::detect(|k| std::env::var_os(k)),
        }
    }

    pub fn set_launch_grace(&mut self, grace: Duration) {
        self.launch_grace = grace;
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn host(&self) -> &dyn Host {
        self.host.as_ref()
    }

    pub fn types(&self) -> &AppTypeTable {
        &self.types
    }

    pub fn store(&self) -> &ConfigStore {
        &self.store
    }

    pub fn flatpak(&self) -> Flatpak<'_> {
        Flatpak::new(self.host.as_ref())
    }

    /// The current instance setup.
    pub fn load(&self) -> Result<Config> {
        self.store.load()
    }

    /// Installed Flatpak apps.
    pub fn list_apps(&self) -> Result<Vec<FlatpakApp>> {
        self.flatpak().list_apps()
    }

    /// Absolute path of `flatpak`.
    pub fn flatpak_binary(&self) -> Result<PathBuf> {
        self.flatpak().binary()
    }

    /// Detection, icon and categories of an app. Never fails: missing information just
    /// makes detection fall back to less specific guesses.
    pub fn app_details(&self, app_id: &str) -> AppDetails {
        let fp = self.flatpak();
        let metadata = fp.metadata(app_id).map_err(|e| tracing::debug!("metadata of {app_id}: {e}")).ok();
        let files = fp.location(app_id).ok().map(|l| l.join("files")).filter(|p| p.is_dir());
        let desktop = self
            .paths
            .exported_desktop_file(app_id)
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|t| KeyFile::parse(&t));
        let detection = apptype::detect(
            &self.types,
            app_id,
            DetectInput { metadata: metadata.as_ref(), files_dir: files.as_deref(), desktop: desktop.as_ref() },
        );
        AppDetails {
            app_id: app_id.to_string(),
            detection,
            icon: icons::find_app_icon(&self.paths, app_id),
            categories: desktop.map(|d| d.get_list("Desktop Entry", "Categories")).unwrap_or_default(),
        }
    }

    /// Validates a draft against the current setup, turning field errors into one error.
    pub fn validate(&self, draft: &InstanceDraft, editing: Option<&InstanceKey>) -> Result<ValidatedDraft> {
        let config = self.load()?;
        validate_against(&self.paths, &config, draft, editing)
    }

    fn find(&self, config: &Config, key: &InstanceKey) -> Result<Instance> {
        config.get(key).cloned().ok_or_else(|| Error::InstanceNotFound(key.to_string()))
    }

    /// Grants the sandbox access to a custom data folder. Returns the override value if
    /// Multipak added it (so it can be removed later), `None` if it already existed.
    fn grant_access(&self, app_id: &str, dir: &str, allow: bool) -> Result<Option<String>> {
        if flatpak::override_has_filesystem(&self.paths, app_id, dir) {
            return Ok(None);
        }
        if !allow {
            return Err(Error::OverrideRequired { app_id: app_id.to_string(), dir: PathBuf::from(dir) });
        }
        tracing::info!("adding filesystem override {dir} for {app_id}");
        self.flatpak().add_filesystem_override(app_id, dir)?;
        Ok(Some(dir.to_string()))
    }

    fn revoke_access(&self, app_id: &str, dir: &str) -> Result<bool> {
        tracing::info!("removing filesystem override {dir} for {app_id}");
        flatpak::remove_filesystem_override(&self.paths, app_id, dir)
    }

    /// Creates the data folder (and Firefox profile) if needed. Returns warnings.
    fn prepare_data_dir(&self, instance: &Instance) -> Result<Vec<String>> {
        let data_dir = instance.resolved_data_dir(&self.paths);
        for dir in launch::dirs_to_create(instance, &data_dir) {
            fs::create_dir_all(&dir).ctx(|| format!("could not create {}", dir.display()))?;
        }
        let mut warnings = Vec::new();
        if instance.strategy == apptype::Strategy::Firefox && !launch::firefox_profile_exists(&data_dir) {
            let spec =
                launch::create_profile_spec(&self.flatpak_binary()?, instance, &data_dir).timeout(PROFILE_TIMEOUT);
            match self.host.run(&spec).and_then(|o| o.check(&spec)) {
                Ok(_) => tracing::info!("created Firefox profile in {}", data_dir.display()),
                Err(e) => warnings.push(format!(
                    "Could not register the profile with -CreateProfile ({e}). The instance still works: the browser creates the profile on first launch."
                )),
            }
        }
        Ok(warnings)
    }

    /// Renders the badge icon. Returns a warning on failure (the launcher then still works).
    fn render_badge(&self, instance: &Instance) -> Option<String> {
        let badge = instance.badge.as_ref()?;
        let out = self.paths.badge_icon(&instance.app_id, &instance.slug);
        let source = icons::find_app_icon(&self.paths, &instance.app_id);
        match icons::render_badged_icon(source.as_deref(), badge, &out) {
            Ok(()) => None,
            Err(e) => Some(format!("Could not create the badge icon: {e}")),
        }
    }

    /// Writes (or rewrites) the launcher, autostart entry and badge icon.
    pub fn write_launchers(&self, instance: &Instance) -> Result<Vec<String>> {
        let flatpak_bin = self.flatpak_binary()?;
        let mut warnings = Vec::new();
        warnings.extend(self.render_badge(instance));
        if instance.badge.is_none() {
            let _ = fs::remove_file(self.paths.badge_icon(&instance.app_id, &instance.slug));
        }
        let categories = app_categories(&self.paths, &instance.app_id);
        let desktop_path = self.paths.desktop_file(&instance.app_id, &instance.slug);
        let text = launcher_text(&self.paths, instance, &flatpak_bin, &categories, false);
        crate::atomic::write_atomic(&desktop_path, text.as_bytes(), 0o644)?;
        let autostart_path = self.paths.autostart_file(&instance.app_id, &instance.slug);
        if instance.autostart {
            let text = launcher_text(&self.paths, instance, &flatpak_bin, &categories, true);
            crate::atomic::write_atomic(&autostart_path, text.as_bytes(), 0o644)?;
        } else {
            remove_if_exists(&autostart_path)?;
        }
        warnings.extend(self.refresh_desktop_database());
        warnings.extend(self.apply_capture_rule(instance)?);
        Ok(warnings)
    }

    /// Description written into the KWin rule of an instance.
    pub fn capture_rule_description(instance: &Instance) -> String {
        format!("Multipak: hide {} from screen capture", instance.display_name())
    }

    /// Writes or removes the instance's "exclude from capture" window rule.
    fn apply_capture_rule(&self, instance: &Instance) -> Result<Vec<String>> {
        let group = capture::instance_group(&instance.app_id, &instance.slug);
        let changed = capture::set_rule(
            &self.paths,
            &group,
            &instance.wm_class,
            &Self::capture_rule_description(instance),
            instance.hide_from_capture,
        )?;
        Ok(self.after_rule_change(changed, instance.hide_from_capture))
    }

    /// Reloads KWin after a rule change and explains when the setting cannot take effect.
    fn after_rule_change(&self, changed: bool, enabled: bool) -> Vec<String> {
        let mut warnings = Vec::new();
        if enabled && !self.capture_support.is_supported() {
            warnings.push(self.capture_support.description());
        }
        if changed
            && self.capture_support.is_supported()
            && let Err(e) = capture::reconfigure_kwin(self.host.as_ref())
        {
            warnings.push(format!(
                "Saved the window rule, but could not ask KWin to reload it ({e}). It takes effect after you log in again."
            ));
        }
        warnings
    }

    /// Whether Multipak's own window is hidden from screen capture.
    pub fn self_hidden_from_capture(&self) -> bool {
        capture::has_rule(&self.paths, capture::SELF_GROUP, capture::SELF_WM_CLASS, SELF_RULE_DESCRIPTION)
    }

    /// Hides (or shows) Multipak's own window in screenshots and screen sharing.
    pub fn set_self_hidden_from_capture(&self, hidden: bool) -> Result<Vec<String>> {
        let changed =
            capture::set_rule(&self.paths, capture::SELF_GROUP, capture::SELF_WM_CLASS, SELF_RULE_DESCRIPTION, hidden)?;
        Ok(self.after_rule_change(changed, hidden))
    }

    /// How (and whether) this desktop can hide windows from capture.
    pub fn capture_support(&self) -> &capture::Support {
        &self.capture_support
    }

    /// Overrides the detected desktop support (used by tests).
    pub fn set_capture_support(&mut self, support: capture::Support) {
        self.capture_support = support;
    }

    fn remove_launchers(&self, instance: &Instance) -> Result<Vec<PathBuf>> {
        let mut removed = Vec::new();
        for p in [
            self.paths.desktop_file(&instance.app_id, &instance.slug),
            self.paths.autostart_file(&instance.app_id, &instance.slug),
            self.paths.badge_icon(&instance.app_id, &instance.slug),
        ] {
            if remove_if_exists(&p)? {
                removed.push(p);
            }
        }
        let _ = self.refresh_desktop_database();
        let group = capture::instance_group(&instance.app_id, &instance.slug);
        if capture::set_rule(&self.paths, &group, "", "", false)? {
            let _ = self.after_rule_change(true, false);
        }
        Ok(removed)
    }

    fn refresh_desktop_database(&self) -> Option<String> {
        let bin = self.host.which("update-desktop-database")?;
        let spec = CommandSpec::new(bin.display().to_string())
            .arg(self.paths.applications_dir().display().to_string())
            .timeout(Duration::from_secs(30));
        match self.host.run(&spec).and_then(|o| o.check(&spec)) {
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("update-desktop-database failed: {e}");
                Some(format!("update-desktop-database failed: {e}"))
            }
        }
    }

    /// Creates an instance: data folder, optional override, launchers and config entry.
    /// `allow_override` is the user's consent to run `flatpak override` for a custom folder.
    pub fn create(&self, draft: &InstanceDraft, allow_override: bool) -> Result<SaveOutcome> {
        let config = self.load()?;
        let v = validate_against(&self.paths, &config, draft, None)?;
        let mut instance = v.instance;
        let mut warnings = v.warnings;
        if let Some(dir) = &v.needs_override {
            instance.managed_override = self.grant_access(&instance.app_id, dir, allow_override)?;
        }
        let rollback = |inst: &Instance| {
            let _ = self.remove_launchers(inst);
            if let Some(dir) = &inst.managed_override {
                let _ = self.revoke_access(&inst.app_id, dir);
            }
        };
        let result = (|| {
            warnings.extend(self.prepare_data_dir(&instance)?);
            warnings.extend(self.write_launchers(&instance)?);
            self.store.update(|c| {
                // Re-check under the lock in case another process added a conflicting instance.
                validate_against(&self.paths, c, &instance.to_draft(), None)?;
                c.instances.push(instance.clone());
                Ok(())
            })
        })();
        if let Err(e) = result {
            rollback(&instance);
            return Err(e);
        }
        tracing::info!("created instance {}", instance.key());
        Ok(SaveOutcome { instance, warnings })
    }

    /// Saves changes to an existing instance. The app and short name cannot change.
    pub fn update(&self, key: &InstanceKey, draft: &InstanceDraft, allow_override: bool) -> Result<SaveOutcome> {
        if draft.app_id != key.app_id || draft.slug != key.slug {
            return Err(Error::invalid(
                "short name",
                "the app and short name of an existing instance cannot be changed; duplicate it instead",
            ));
        }
        let config = self.load()?;
        let old = self.find(&config, key)?;
        let v = validate_against(&self.paths, &config, draft, Some(key))?;
        let mut instance = v.instance;
        let mut warnings = v.warnings;
        instance.created = old.created;
        let old_dir = old.resolved_data_dir(&self.paths);
        let dir_changed = old_dir != v.data_dir;
        instance.managed_override = if dir_changed {
            match &v.needs_override {
                Some(dir) => self.grant_access(&instance.app_id, dir, allow_override)?,
                None => None,
            }
        } else {
            old.managed_override.clone()
        };
        if dir_changed {
            warnings.push(format!("The data folder changed. Existing data in {} was not moved.", old_dir.display()));
        }
        warnings.extend(self.prepare_data_dir(&instance)?);
        warnings.extend(self.write_launchers(&instance)?);
        self.store.update(|c| {
            let slot = c.get_mut(key).ok_or_else(|| Error::InstanceNotFound(key.to_string()))?;
            *slot = instance.clone();
            Ok(())
        })?;
        if dir_changed && let Some(dir) = &old.managed_override {
            self.revoke_access(&old.app_id, dir)?;
        }
        Ok(SaveOutcome { instance, warnings })
    }

    /// Copies an instance's settings into a new instance with its own, empty data folder.
    pub fn duplicate(&self, key: &InstanceKey, new_name: &str) -> Result<SaveOutcome> {
        let config = self.load()?;
        let old = self.find(&config, key)?;
        let fresh = InstanceDraft::new(&old.app_id, &old.app_name, new_name, old.strategy, &config.instances);
        let mut draft = old.to_draft();
        draft.name = fresh.name;
        draft.slug = fresh.slug;
        draft.wm_class = fresh.wm_class;
        draft.data_dir = None;
        if let Some(b) = &mut draft.badge
            && b.letter.is_some()
        {
            b.letter = new_name.trim().chars().next().map(|c| c.to_uppercase().to_string());
        }
        self.create(&draft, false)
    }

    /// Removes an instance: launchers, autostart entry, badge icon and the override
    /// Multipak added. The data folder is deleted only if `delete_data` is set.
    pub fn delete(&self, key: &InstanceKey, delete_data: bool) -> Result<DeleteReport> {
        let config = self.load()?;
        let instance = self.find(&config, key)?;
        let data_dir = instance.resolved_data_dir(&self.paths);
        if delete_data {
            if self.is_running(key)? {
                return Err(Error::InstanceRunning(instance.display_name()));
            }
            if data_dir.exists() {
                let others: Vec<PathBuf> = config
                    .instances
                    .iter()
                    .filter(|i| i.key() != *key)
                    .map(|i| i.resolved_data_dir(&self.paths))
                    .collect();
                fsutil::check_safe_to_delete(&data_dir, &self.paths, &instance.app_id, &others)?;
            }
        }
        let mut report = DeleteReport { data_dir: data_dir.clone(), ..Default::default() };
        report.removed_files = self.remove_launchers(&instance)?;
        if let Some(dir) = &instance.managed_override
            && self.revoke_access(&instance.app_id, dir)?
        {
            report.removed_override = Some(dir.clone());
        }
        if delete_data && data_dir.exists() {
            fs::remove_dir_all(&data_dir).ctx(|| format!("could not delete {}", data_dir.display()))?;
            report.data_deleted = true;
        }
        let _ = fs::remove_file(self.paths.launch_log(&instance.app_id, &instance.slug));
        self.store.update(|c| {
            c.instances.retain(|i| i.key() != *key);
            Ok(())
        })?;
        tracing::info!("deleted instance {key} (data deleted: {})", report.data_deleted);
        Ok(report)
    }

    /// Starts an instance in the background. Fails if it exits right away with an error.
    pub fn launch(&self, key: &InstanceKey) -> Result<LaunchOutcome> {
        let config = self.load()?;
        let instance = self.find(&config, key)?;
        let flatpak_bin = self.flatpak_binary()?;
        for w in self.prepare_data_dir(&instance)? {
            tracing::warn!("{w}");
        }
        if let Some(dir) = &instance.managed_override
            && !flatpak::override_has_filesystem(&self.paths, &instance.app_id, dir)
        {
            tracing::warn!("override for {dir} disappeared; adding it again");
            self.flatpak().add_filesystem_override(&instance.app_id, dir)?;
        }
        let data_dir = instance.resolved_data_dir(&self.paths);
        let spec = launch::launch_spec(&flatpak_bin, &instance, &data_dir);
        let log = self.paths.launch_log(&instance.app_id, &instance.slug);
        match self.host.launch_detached(&spec, &log, self.launch_grace)? {
            LaunchOutcome::Failed { code, log_tail } => Err(Error::LaunchFailed {
                instance: instance.display_name(),
                code,
                log: if log_tail.trim().is_empty() { "(no output)".into() } else { log_tail },
            }),
            other => Ok(other),
        }
    }

    /// Running sandboxes, matched to instances.
    pub fn running(&self) -> Result<Vec<RunningInstance>> {
        let config = self.load()?;
        let entries = self.flatpak().ps()?;
        Ok(entries
            .into_iter()
            .map(|e| {
                let key = self.identify(&e, &config);
                RunningInstance { flatpak_instance: e.instance, app_id: e.app_id, pid: e.pid, key }
            })
            .collect())
    }

    /// Finds which instance a sandbox belongs to: first by the marker variable in the
    /// environment of its processes, then by the instance's arguments on their command lines.
    fn identify(&self, entry: &flatpak::PsEntry, config: &Config) -> Option<InstanceKey> {
        let mut queue = vec![entry.pid, entry.child_pid];
        let mut seen = HashSet::new();
        let mut cmdlines = Vec::new();
        while let Some(pid) = queue.pop() {
            if pid == 0 || !seen.insert(pid) || seen.len() > 256 {
                continue;
            }
            if let Some(env) = self.host.proc_environ(pid) {
                let prefix = format!("{MARKER_ENV}=");
                if let Some(k) = split_nul(&env).find_map(|v| v.strip_prefix(&prefix).and_then(InstanceKey::parse)) {
                    return Some(k);
                }
            }
            if let Some(cmd) = self.host.proc_cmdline(pid) {
                cmdlines.push(split_nul(&cmd).collect::<Vec<_>>());
            }
            queue.extend(self.host.proc_children(pid));
        }
        config.for_app(&entry.app_id).find_map(|inst| {
            let class_arg = format!("--class={}", inst.wm_class);
            let dir = inst.resolved_data_dir(&self.paths).display().to_string();
            let udd = format!("--user-data-dir={dir}");
            cmdlines
                .iter()
                .any(|args| {
                    args.iter().any(|a| *a == class_arg || *a == udd)
                        || args.windows(2).any(|w| w[0] == "-profile" && w[1] == dir)
                })
                .then(|| inst.key())
        })
    }

    pub fn is_running(&self, key: &InstanceKey) -> Result<bool> {
        match self.running() {
            Ok(list) => Ok(list.iter().any(|r| r.key.as_ref() == Some(key))),
            Err(Error::FlatpakMissing) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Stops every running sandbox of an instance. Returns how many were stopped.
    pub fn stop(&self, key: &InstanceKey) -> Result<usize> {
        let running = self.running()?;
        let mut n = 0;
        for r in running.iter().filter(|r| r.key.as_ref() == Some(key)) {
            self.flatpak().kill(&r.flatpak_instance)?;
            n += 1;
        }
        Ok(n)
    }

    pub fn disk_usage(&self, key: &InstanceKey, cancel: &CancelToken) -> Result<u64> {
        let config = self.load()?;
        let instance = self.find(&config, key)?;
        fsutil::disk_usage(&instance.resolved_data_dir(&self.paths), cancel)
    }

    /// Deletes cache folders inside the instance's data folder.
    pub fn clear_cache(&self, key: &InstanceKey, cancel: &CancelToken) -> Result<CacheReport> {
        let config = self.load()?;
        let instance = self.find(&config, key)?;
        if self.is_running(key)? {
            return Err(Error::InstanceRunning(instance.display_name()));
        }
        let data_dir = instance.resolved_data_dir(&self.paths);
        let mut report = CacheReport::default();
        for dir in fsutil::find_cache_dirs(&data_dir, instance.strategy) {
            report.freed_bytes += fsutil::clear_dir_contents(&dir, cancel)?;
            report.dirs.push(dir);
        }
        Ok(report)
    }

    /// Backs up one instance to `out`.
    pub fn backup(
        &self,
        key: &InstanceKey,
        out: &Path,
        progress: ProgressFn<'_>,
        cancel: &CancelToken,
    ) -> Result<BackupManifest> {
        let config = self.load()?;
        let instance = self.find(&config, key)?;
        backup::create_backup(&instance, &instance.resolved_data_dir(&self.paths), out, progress, cancel)
    }

    /// Restores a backup, into an existing instance or as a new one.
    pub fn restore(
        &self,
        archive: &Path,
        target: RestoreTarget,
        progress: ProgressFn<'_>,
        cancel: &CancelToken,
    ) -> Result<Instance> {
        let manifest = backup::read_manifest(archive)?;
        let instance = match target {
            RestoreTarget::Existing(key) => {
                let config = self.load()?;
                let instance = self.find(&config, &key)?;
                if self.is_running(&key)? {
                    return Err(Error::InstanceRunning(instance.display_name()));
                }
                instance
            }
            RestoreTarget::New { slug, allow_override } => {
                let config = self.load()?;
                let mut draft = manifest.instance.to_draft();
                if let Some(slug) = slug {
                    let original_exists = config.get(&manifest.instance.key()).is_some();
                    if slug != draft.slug && original_exists {
                        draft.name = crate::instance::copy_name(&draft.name);
                    }
                    draft.wm_class = crate::instance::default_wm_class(&draft.app_id, &slug);
                    draft.slug = slug;
                }
                if config.get(&InstanceKey::new(&draft.app_id, &draft.slug)).is_some() {
                    return Err(Error::DuplicateSlug { app_id: draft.app_id, slug: draft.slug });
                }
                if config.instances.iter().any(|i| i.wm_class == draft.wm_class) {
                    draft.wm_class = crate::instance::default_wm_class(&draft.app_id, &draft.slug);
                }
                self.create(&draft, allow_override)?.instance
            }
        };
        backup::restore_backup(archive, &instance.resolved_data_dir(&self.paths), progress, cancel)?;
        Ok(instance)
    }
}

/// Validates `draft` against `config`, summarizing field errors into one [`Error::Invalid`].
pub fn validate_against(
    paths: &Paths,
    config: &Config,
    draft: &InstanceDraft,
    editing: Option<&InstanceKey>,
) -> Result<ValidatedDraft> {
    draft.validate(DraftContext { paths, existing: &config.instances, editing }).map_err(|errors| {
        let field = errors.first().map(|e| e.field.label()).unwrap_or("input");
        Error::Invalid { field, reason: errors.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join("; ") }
    })
}

fn remove_if_exists(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).ctx(|| format!("could not remove {}", path.display())),
    }
}
