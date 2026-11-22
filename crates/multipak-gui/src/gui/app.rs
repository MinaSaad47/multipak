//! Application state, background-result handling and user actions.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multipak_core::backup::{self, CancelToken, Progress};
use multipak_core::doctor::{Fix, Issue};
use multipak_core::instance::{self, InstanceKey};
use multipak_core::manager::{AppDetails, RestoreTarget, RunningInstance};
use multipak_core::transfer::{ExportFile, ImportOptions};
use multipak_core::{Config, Error, FlatpakApp, Instance, LaunchOutcome, Manager};

use super::model::{AppEntry, DeleteConfirm, InstanceForm, UiState, build_app_entries, format_bytes};
use super::style::{self, ToastKind, Toasts};
use super::worker::{JobId, Msg, Output, PickPurpose, Worker};

const RUNNING_POLL: Duration = Duration::from_secs(5);

/// A value that is loaded in the background.
pub enum Loadable<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Loadable<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(v) => Some(v),
            _ => None,
        }
    }
}

/// A long job shown in the task bar with progress and a Cancel button.
pub struct Task {
    pub label: String,
    pub progress: Option<Progress>,
    pub cancel: CancelToken,
}

pub enum Dialog {
    Form(Box<InstanceForm>),
    Delete(DeleteConfirm),
    Duplicate { key: InstanceKey, name: String, working: bool, error: Option<String> },
    Settings,
    Doctor { issues: Loadable<Vec<Issue>>, working: bool },
    ExportOptions { with_data: bool },
    Import { path: PathBuf, file: ExportFile, opts: ImportOptions, custom_dirs: Vec<String> },
    ConfirmRestore { key: InstanceKey, name: String, path: PathBuf },
    Message { title: String, body: String },
}

pub struct MultipakApp {
    pub(super) mgr: Arc<Manager>,
    pub(super) worker: Worker,
    pub(super) state: UiState,
    pub(super) apps: Loadable<Vec<FlatpakApp>>,
    pub(super) entries: Vec<AppEntry>,
    pub(super) config: Config,
    pub(super) config_error: Option<String>,
    pub(super) flatpak_bin: Option<PathBuf>,
    pub(super) running: Vec<RunningInstance>,
    running_inflight: bool,
    last_running_poll: Option<Instant>,
    pub(super) details: HashMap<String, Loadable<AppDetails>>,
    pub(super) icons: HashMap<String, Option<PathBuf>>,
    pub(super) disk: HashMap<InstanceKey, Loadable<u64>>,
    pub(super) busy: HashMap<InstanceKey, &'static str>,
    pub(super) search: String,
    pub(super) dialog: Option<Dialog>,
    pub(super) toasts: Toasts,
    pub(super) tasks: HashMap<JobId, Task>,
    pub(super) search_id: egui::Id,
    applied_look: Option<(super::model::ThemeChoice, u32)>,
    /// Whether Multipak's own window is hidden from capture (`None` while loading).
    pub(super) self_capture: Option<bool>,
}

impl MultipakApp {
    pub fn new(cc: &eframe::CreationContext<'_>, mgr: Arc<Manager>, startup_warnings: Vec<String>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        style::apply(&cc.egui_ctx);
        let state: UiState = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        let mut app = Self {
            mgr,
            worker: Worker::new(cc.egui_ctx.clone(), 4),
            state,
            apps: Loadable::Idle,
            entries: Vec::new(),
            config: Config::default(),
            config_error: None,
            flatpak_bin: None,
            running: Vec::new(),
            running_inflight: false,
            last_running_poll: None,
            details: HashMap::new(),
            icons: HashMap::new(),
            disk: HashMap::new(),
            busy: HashMap::new(),
            search: String::new(),
            dialog: None,
            toasts: Toasts::default(),
            tasks: HashMap::new(),
            search_id: egui::Id::unique("app-search"),
            applied_look: None,
            self_capture: None,
        };
        for w in startup_warnings {
            app.toasts.push(ToastKind::Warning, w);
        }
        // Bring the capture helper in line with the setup (and clean up rules from older
        // versions) without waiting for the next save.
        let mgr = app.mgr.clone();
        app.worker.spawn(move |_| Output::CaptureSynced(mgr.sync_capture()));
        app.refresh_all();
        app
    }

    // ----- loading -------------------------------------------------------------------

    pub(super) fn refresh_all(&mut self) {
        self.reload_config();
        self.apps = Loadable::Loading;
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::Apps(mgr.list_apps()));
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::FlatpakBinary(mgr.flatpak_binary().ok()));
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::SelfCapture(Ok((mgr.self_hidden_from_capture(), Vec::new()))));
        self.details.clear();
        self.disk.clear();
        self.poll_running();
    }

    pub(super) fn reload_config(&mut self) {
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::Config(mgr.load()));
    }

    fn poll_running(&mut self) {
        if self.running_inflight {
            return;
        }
        self.running_inflight = true;
        self.last_running_poll = Some(Instant::now());
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::Running(mgr.running()));
    }

    /// Polls `flatpak ps` every few seconds.
    pub(super) fn tick(&mut self, ctx: &egui::Context) {
        let due = self.last_running_poll.is_none_or(|t| t.elapsed() >= RUNNING_POLL);
        if due {
            self.poll_running();
        }
        ctx.request_repaint_after(RUNNING_POLL);
    }

    /// Makes the next running-state poll happen soon (after a launch or stop).
    fn poll_soon(&mut self, after: Duration) {
        self.last_running_poll = Some(Instant::now() - RUNNING_POLL + after);
    }

    pub(super) fn ensure_details(&mut self, app_id: &str) {
        if self.details.contains_key(app_id) {
            return;
        }
        self.details.insert(app_id.to_string(), Loadable::Loading);
        let mgr = self.mgr.clone();
        let id = app_id.to_string();
        self.worker.spawn(move |_| Output::Details(mgr.app_details(&id)));
    }

    pub(super) fn ensure_disk_usage(&mut self, key: &InstanceKey) {
        if self.disk.contains_key(key) {
            return;
        }
        self.refresh_disk_usage(key);
    }

    pub(super) fn refresh_disk_usage(&mut self, key: &InstanceKey) {
        self.disk.insert(key.clone(), Loadable::Loading);
        let mgr = self.mgr.clone();
        let key = key.clone();
        self.worker.spawn(move |ctx| {
            let r = mgr.disk_usage(&key, &ctx.cancel);
            Output::DiskUsage(key, r)
        });
    }

    fn rebuild_entries(&mut self) {
        let apps = self.apps.ready().cloned().unwrap_or_default();
        self.entries = build_app_entries(&apps, &self.config);
        let missing: Vec<String> =
            self.entries.iter().map(|e| e.app_id.clone()).filter(|id| !self.icons.contains_key(id)).collect();
        if !missing.is_empty() {
            for id in &missing {
                self.icons.insert(id.clone(), None);
            }
            let paths = self.mgr.paths().clone();
            self.worker.spawn(move |_| {
                Output::Icons(
                    missing
                        .into_iter()
                        .map(|id| {
                            let p = multipak_core::icons::find_app_icon(&paths, &id);
                            (id, p)
                        })
                        .collect(),
                )
            });
        }
        let selected_valid =
            self.state.selected_app.as_ref().is_some_and(|s| self.entries.iter().any(|e| &e.app_id == s));
        if !selected_valid {
            self.state.selected_app = self
                .entries
                .iter()
                .find(|e| self.config.for_app(&e.app_id).next().is_some())
                .or_else(|| self.entries.first())
                .map(|e| e.app_id.clone());
        }
    }

    pub(super) fn running_keys(&self) -> HashSet<InstanceKey> {
        self.running.iter().filter_map(|r| r.key.clone()).collect()
    }

    pub(super) fn app_name(&self, app_id: &str) -> String {
        self.entries.iter().find(|e| e.app_id == app_id).map(|e| e.name.clone()).unwrap_or_else(|| app_id.to_string())
    }

    pub(super) fn instance(&self, key: &InstanceKey) -> Option<&Instance> {
        self.config.get(key)
    }

    // ----- background results --------------------------------------------------------

    pub(super) fn process_messages(&mut self, ctx: &egui::Context) {
        for msg in self.worker.drain() {
            match msg {
                Msg::Progress(id, p) => {
                    if let Some(t) = self.tasks.get_mut(&id) {
                        t.progress = Some(p);
                    }
                }
                Msg::Done(id, out) => {
                    self.tasks.remove(&id);
                    self.handle(ctx, *out);
                }
            }
        }
    }

    fn error(&mut self, context: &str, e: impl std::fmt::Display) {
        self.toasts.push(ToastKind::Error, format!("{context}: {e}"));
    }

    fn handle(&mut self, ctx: &egui::Context, out: Output) {
        match out {
            Output::Apps(Ok(apps)) => {
                self.apps = Loadable::Ready(apps);
                self.rebuild_entries();
            }
            Output::Apps(Err(e)) => {
                if !matches!(e, Error::FlatpakMissing) {
                    self.error("Could not list installed apps", &e);
                }
                self.apps = Loadable::Failed(e.to_string());
                self.rebuild_entries();
            }
            Output::Config(Ok(config)) => {
                self.config = config;
                self.config_error = None;
                self.rebuild_entries();
            }
            Output::Config(Err(e)) => {
                self.config_error = Some(e.to_string());
            }
            Output::FlatpakBinary(bin) => self.flatpak_bin = bin,
            Output::Running(r) => {
                self.running_inflight = false;
                match r {
                    Ok(list) => {
                        let now: HashSet<_> = list.iter().filter_map(|r| r.key.clone()).collect();
                        self.busy.retain(|k, label| {
                            !(*label == "Starting…" && now.contains(k)) && !(*label == "Stopping…" && !now.contains(k))
                        });
                        self.running = list;
                    }
                    Err(Error::FlatpakMissing) => self.running.clear(),
                    Err(e) => tracing::warn!("could not read running instances: {e}"),
                }
            }
            Output::Details(d) => {
                self.details.insert(d.app_id.clone(), Loadable::Ready(d));
            }
            Output::Icons(list) => {
                for (id, p) in list {
                    self.icons.insert(id, p);
                }
            }
            Output::DiskUsage(key, r) => {
                self.disk.insert(
                    key,
                    match r {
                        Ok(v) => Loadable::Ready(v),
                        Err(e) => Loadable::Failed(e.to_string()),
                    },
                );
            }
            Output::Saved { editing, result } => {
                match result {
                    Ok(outcome) => {
                        let inst = &outcome.instance;
                        forget_badge(ctx, &self.mgr, inst);
                        let verb = if editing { "Saved" } else { "Created" };
                        self.toasts.push(ToastKind::Success, format!("{verb} {}", inst.display_name()));
                        for w in &outcome.warnings {
                            self.toasts.push(ToastKind::Warning, w.clone());
                        }
                        self.disk.remove(&inst.key());
                        if matches!(self.dialog, Some(Dialog::Form(_))) {
                            self.dialog = None;
                        }
                    }
                    Err(e) => {
                        if let Some(Dialog::Form(f)) = &mut self.dialog {
                            f.saving = false;
                            f.error = Some(e.to_string());
                        } else {
                            self.error("Could not save the instance", e);
                        }
                    }
                }
                self.reload_config();
            }
            Output::Duplicated(r) => {
                match r {
                    Ok(o) => {
                        self.toasts.push(ToastKind::Success, format!("Created {}", o.instance.display_name()));
                        if matches!(self.dialog, Some(Dialog::Duplicate { .. })) {
                            self.dialog = None;
                        }
                    }
                    Err(e) => {
                        if let Some(Dialog::Duplicate { working, error, .. }) = &mut self.dialog {
                            *working = false;
                            *error = Some(e.to_string());
                        } else {
                            self.error("Could not duplicate the instance", e);
                        }
                    }
                }
                self.reload_config();
            }
            Output::Deleted(r) => {
                match r {
                    Ok(report) => {
                        let mut text = String::from("Instance deleted.");
                        if report.data_deleted {
                            text.push_str(&format!(" Its data folder {} was deleted.", report.data_dir.display()));
                        } else {
                            text.push_str(&format!(" Its data was kept in {}.", report.data_dir.display()));
                        }
                        if let Some(dir) = report.removed_override {
                            text.push_str(&format!(" Folder access to {dir} was removed."));
                        }
                        self.toasts.push(ToastKind::Success, text);
                        if matches!(self.dialog, Some(Dialog::Delete(_))) {
                            self.dialog = None;
                        }
                    }
                    Err(e) => {
                        if let Some(Dialog::Delete(d)) = &mut self.dialog {
                            d.working = false;
                            d.error = Some(e.to_string());
                        } else {
                            self.error("Could not delete the instance", e);
                        }
                    }
                }
                self.reload_config();
            }
            Output::Launched(key, r) => {
                let name = self.instance(&key).map(|i| i.display_name()).unwrap_or_else(|| key.to_string());
                match r {
                    Ok(LaunchOutcome::Running { .. }) => {
                        self.toasts.push(ToastKind::Success, format!("Started {name}"));
                        self.busy.insert(key.clone(), "Starting…");
                        self.poll_soon(Duration::from_millis(1500));
                    }
                    Ok(LaunchOutcome::ExitedOk) => {
                        self.busy.remove(&key);
                        self.toasts.push(
                            ToastKind::Info,
                            format!("{name} exited right away without an error. If the app was already open, it may have opened a window in the running copy instead."),
                        );
                        self.poll_soon(Duration::from_millis(500));
                    }
                    Ok(LaunchOutcome::Failed { .. }) => unreachable!("Manager::launch turns failures into errors"),
                    Err(e) => {
                        self.busy.remove(&key);
                        self.error(&format!("Could not start {name}"), e);
                    }
                }
                self.disk.remove(&key);
            }
            Output::Stopped(key, r) => {
                match r {
                    Ok(0) => {
                        self.busy.remove(&key);
                        self.toasts.push(ToastKind::Info, "It was not running.");
                    }
                    Ok(_) => {
                        self.busy.insert(key.clone(), "Stopping…");
                    }
                    Err(e) => {
                        self.busy.remove(&key);
                        self.error("Could not stop the instance", e);
                    }
                }
                self.poll_soon(Duration::from_millis(700));
            }
            Output::CacheCleared(key, r) => {
                self.busy.remove(&key);
                match r {
                    Ok(report) if report.dirs.is_empty() => {
                        self.toasts.push(ToastKind::Info, "No cache folders were found.")
                    }
                    Ok(report) => self.toasts.push(
                        ToastKind::Success,
                        format!(
                            "Cleared {} from {} cache folder(s).",
                            format_bytes(report.freed_bytes),
                            report.dirs.len()
                        ),
                    ),
                    Err(e) => self.error("Could not clear the cache", e),
                }
                self.refresh_disk_usage(&key);
            }
            Output::BackedUp(path, r) => match r {
                Ok(m) => self.toasts.push(
                    ToastKind::Success,
                    format!(
                        "Backed up {} ({}) to {}",
                        m.instance.display_name(),
                        format_bytes(m.total_bytes),
                        path.display()
                    ),
                ),
                Err(Error::Cancelled) => self.toasts.push(ToastKind::Info, "Backup cancelled."),
                Err(e) => self.error("Backup failed", e),
            },
            Output::Restored(r) => {
                match r {
                    Ok(inst) => {
                        self.toasts.push(ToastKind::Success, format!("Restored {}", inst.display_name()));
                        self.disk.remove(&inst.key());
                        self.state.selected_app = Some(inst.app_id.clone());
                    }
                    Err(Error::Cancelled) => {
                        self.toasts.push(ToastKind::Info, "Restore cancelled. The existing data was not changed.")
                    }
                    Err(e) => self.error("Restore failed", e),
                }
                self.reload_config();
            }
            Output::Doctor(r) => {
                if let Some(Dialog::Doctor { issues, working }) = &mut self.dialog {
                    *working = false;
                    *issues = match r {
                        Ok(v) => Loadable::Ready(v),
                        Err(e) => Loadable::Failed(e.to_string()),
                    };
                }
            }
            Output::FixApplied(r) => {
                if let Err(e) = r {
                    self.error("Repair failed", e);
                }
                self.reload_config();
                if matches!(self.dialog, Some(Dialog::Doctor { .. })) {
                    self.run_doctor();
                }
            }
            Output::ExportRead(path, r) => match r {
                Ok(file) => {
                    let custom_dirs: Vec<String> = file.instances.iter().filter_map(|e| e.data_dir.clone()).collect();
                    let opts = ImportOptions {
                        restore_data: file.instances.iter().any(|e| e.backup.is_some()),
                        ..Default::default()
                    };
                    self.dialog = Some(Dialog::Import { path, file, opts, custom_dirs });
                }
                Err(e) => self.error("Could not read the export", e),
            },
            Output::Exported(path, r) => match r {
                Ok(report) => {
                    let mut text = format!("Exported {} instance(s) to {}", report.instances, path.display());
                    if !report.backups.is_empty() {
                        text.push_str(&format!(
                            " with {} data backup(s) in {}",
                            report.backups.len(),
                            multipak_core::transfer::data_dir_for(&path).display()
                        ));
                    }
                    self.toasts.push(ToastKind::Success, text);
                    for w in report.warnings {
                        self.toasts.push(ToastKind::Warning, w);
                    }
                }
                Err(Error::Cancelled) => self.toasts.push(ToastKind::Info, "Export cancelled."),
                Err(e) => self.error("Export failed", e),
            },
            Output::Imported(r) => {
                match r {
                    Ok(report) => {
                        let mut body = format!("Created {} instance(s)", report.created.len());
                        if !report.restored.is_empty() {
                            body.push_str(&format!(", restored data for {}", report.restored.len()));
                        }
                        body.push('.');
                        let mut details = Vec::new();
                        for (k, why) in &report.skipped {
                            details.push(format!("Skipped {k}: {why}"));
                        }
                        details.extend(report.warnings.iter().cloned());
                        if details.is_empty() {
                            self.toasts.push(ToastKind::Success, body);
                        } else {
                            self.dialog = Some(Dialog::Message {
                                title: "Import finished".into(),
                                body: format!("{body}\n\n{}", details.join("\n")),
                            });
                        }
                    }
                    Err(Error::Cancelled) => self.toasts.push(ToastKind::Info, "Import cancelled."),
                    Err(e) => self.error("Import failed", e),
                }
                self.reload_config();
            }
            Output::Picked(purpose, path) => {
                if let Some(path) = path {
                    self.picked(purpose, path);
                }
            }
            Output::CaptureSynced(r) => match r {
                Ok(warnings) => {
                    for w in warnings {
                        self.toasts.push(ToastKind::Warning, w);
                    }
                }
                Err(e) => tracing::warn!("could not sync the capture helper: {e}"),
            },
            Output::SelfCapture(r) => match r {
                Ok((hidden, warnings)) => {
                    self.self_capture = Some(hidden);
                    for w in warnings {
                        self.toasts.push(ToastKind::Warning, w);
                    }
                }
                Err(e) => {
                    self.error("Could not change the window rule", e);
                    let mgr = self.mgr.clone();
                    self.worker.spawn(move |_| Output::SelfCapture(Ok((mgr.self_hidden_from_capture(), Vec::new()))));
                }
            },
            Output::Opened(r) => {
                if let Err(e) = r {
                    self.toasts.push(ToastKind::Error, e);
                }
            }
        }
    }

    fn picked(&mut self, purpose: PickPurpose, path: PathBuf) {
        match purpose {
            PickPurpose::FormDataDir => {
                if let Some(Dialog::Form(f)) = &mut self.dialog {
                    f.custom_dir = true;
                    f.data_dir = path.display().to_string();
                }
            }
            PickPurpose::BackupTo(key) => self.start_backup(key, path),
            PickPurpose::RestoreInto(key) => {
                let name = self.instance(&key).map(|i| i.display_name()).unwrap_or_else(|| key.to_string());
                self.dialog = Some(Dialog::ConfirmRestore { key, name, path });
            }
            PickPurpose::RestoreAsNew => self.start_restore_as_new(path),
            PickPurpose::ExportTo { with_data } => self.start_export(path, with_data),
            PickPurpose::ImportFrom => {
                self.worker.spawn(move |_| {
                    let r = multipak_core::transfer::read_export(&path);
                    Output::ExportRead(path, r)
                });
            }
        }
    }

    // ----- actions -------------------------------------------------------------------

    pub(super) fn open_create_form(&mut self) {
        let Some(app_id) = self.state.selected_app.clone() else {
            self.toasts.push(ToastKind::Info, "Select an app first.");
            return;
        };
        if !self.entries.iter().any(|e| e.app_id == app_id && e.installed) {
            self.toasts
                .push(ToastKind::Warning, "This app is not installed, so new instances cannot be created for it.");
            return;
        }
        self.ensure_details(&app_id);
        let detected = self.details.get(&app_id).and_then(|d| d.ready()).map(|d| d.detection.strategy);
        let name = self.app_name(&app_id);
        let mut form = InstanceForm::create(
            &app_id,
            &name,
            detected.unwrap_or(multipak_core::Strategy::Generic),
            &self.config.instances,
        );
        if detected.is_none() {
            form.detected = None;
        }
        self.dialog = Some(Dialog::Form(Box::new(form)));
    }

    pub(super) fn open_edit_form(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instance(key).cloned() else { return };
        self.ensure_details(&inst.app_id);
        let detected = self.details.get(&inst.app_id).and_then(|d| d.ready()).map(|d| d.detection.strategy);
        self.dialog = Some(Dialog::Form(Box::new(InstanceForm::edit(&inst, detected))));
    }

    /// Fills in the detected strategy once details arrive while a create form is open.
    pub(super) fn sync_form_detection(&mut self) {
        let Some(Dialog::Form(form)) = &mut self.dialog else { return };
        if form.detected.is_some() || form.is_edit() {
            return;
        }
        if let Some(Loadable::Ready(d)) = self.details.get(&form.app_id) {
            form.detected = Some(d.detection.strategy);
            form.strategy = d.detection.strategy;
        }
    }

    pub(super) fn submit_form(&mut self) {
        let Some(Dialog::Form(form)) = &mut self.dialog else { return };
        let draft = match form.to_draft() {
            Ok(d) => d,
            Err(errs) => {
                form.error = Some(errs.into_iter().map(|e| e.message).collect::<Vec<_>>().join("; "));
                return;
            }
        };
        form.saving = true;
        form.error = None;
        let allow = form.allow_override;
        let mode = form.mode.clone();
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| match mode {
            super::model::FormMode::Create => Output::Saved { editing: false, result: mgr.create(&draft, allow) },
            super::model::FormMode::Edit(key) => {
                Output::Saved { editing: true, result: mgr.update(&key, &draft, allow) }
            }
        });
    }

    pub(super) fn launch(&mut self, key: &InstanceKey) {
        self.busy.insert(key.clone(), "Starting…");
        let mgr = self.mgr.clone();
        let key = key.clone();
        self.worker.spawn(move |_| {
            let r = mgr.launch(&key);
            Output::Launched(key, r)
        });
    }

    pub(super) fn stop(&mut self, key: &InstanceKey) {
        self.busy.insert(key.clone(), "Stopping…");
        let mgr = self.mgr.clone();
        let key = key.clone();
        self.worker.spawn(move |_| {
            let r = mgr.stop(&key);
            Output::Stopped(key, r)
        });
    }

    pub(super) fn clear_cache(&mut self, key: &InstanceKey) {
        self.busy.insert(key.clone(), "Clearing cache…");
        let mgr = self.mgr.clone();
        let key = key.clone();
        self.worker.spawn(move |ctx| {
            let r = mgr.clear_cache(&key, &ctx.cancel);
            Output::CacheCleared(key, r)
        });
    }

    pub(super) fn duplicate(&mut self, key: InstanceKey, name: String) {
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::Duplicated(mgr.duplicate(&key, &name)));
    }

    pub(super) fn delete(&mut self, key: InstanceKey, delete_data: bool) {
        let mgr = self.mgr.clone();
        self.disk.remove(&key);
        self.worker.spawn(move |_| Output::Deleted(mgr.delete(&key, delete_data)));
    }

    /// Hides or shows Multipak's own window in screen captures.
    pub(super) fn set_self_capture(&mut self, hidden: bool) {
        self.self_capture = None;
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::SelfCapture(mgr.set_self_hidden_from_capture(hidden).map(|w| (hidden, w))));
    }

    pub(super) fn open_folder(&mut self, path: PathBuf) {
        self.worker.spawn(move |_| {
            if !path.exists() {
                return Output::Opened(Err(format!(
                    "{} does not exist yet. Launch the instance once to create it.",
                    path.display()
                )));
            }
            Output::Opened(open::that_detached(&path).map_err(|e| format!("Could not open {}: {e}", path.display())))
        });
    }

    pub(super) fn copy_launch_command(&mut self, ctx: &egui::Context, key: &InstanceKey) {
        let Some(inst) = self.instance(key) else { return };
        let bin = self.flatpak_bin.clone().unwrap_or_else(|| PathBuf::from("flatpak"));
        let dir = inst.resolved_data_dir(self.mgr.paths());
        let cmd = multipak_core::launch::launch_spec(&bin, inst, &dir).display();
        ctx.copy_text(cmd);
        self.toasts.push(ToastKind::Info, "Launch command copied to the clipboard.");
    }

    /// Opens a native file dialog (through the XDG desktop portal) on a background thread.
    pub(super) fn pick(&mut self, purpose: PickPurpose, default_name: Option<String>) {
        self.worker.spawn_long("file-dialog", move |_| {
            let mut dialog = rfd::FileDialog::new();
            let path = match &purpose {
                PickPurpose::FormDataDir => dialog.set_title("Choose a data folder").pick_folder(),
                PickPurpose::BackupTo(_) => {
                    dialog = dialog.set_title("Save backup").add_filter("Multipak backup", &["zst"]);
                    if let Some(n) = &default_name {
                        dialog = dialog.set_file_name(n);
                    }
                    dialog.save_file()
                }
                PickPurpose::RestoreInto(_) | PickPurpose::RestoreAsNew => {
                    dialog.set_title("Choose a backup").add_filter("Multipak backup", &["zst"]).pick_file()
                }
                PickPurpose::ExportTo { .. } => {
                    dialog = dialog.set_title("Export setup").add_filter("JSON", &["json"]);
                    if let Some(n) = &default_name {
                        dialog = dialog.set_file_name(n);
                    }
                    dialog.save_file()
                }
                PickPurpose::ImportFrom => dialog.set_title("Import setup").add_filter("JSON", &["json"]).pick_file(),
            };
            Output::Picked(purpose, path)
        });
    }

    pub(super) fn backup_instance(&mut self, key: &InstanceKey) {
        let Some(inst) = self.instance(key) else { return };
        let name = backup::default_file_name(inst);
        self.pick(PickPurpose::BackupTo(key.clone()), Some(name));
    }

    fn start_backup(&mut self, key: InstanceKey, path: PathBuf) {
        let path = if path.extension().is_none() { path.with_extension("tar.zst") } else { path };
        let label =
            format!("Backing up {}", self.instance(&key).map(|i| i.display_name()).unwrap_or_else(|| key.to_string()));
        let mgr = self.mgr.clone();
        let out = path.clone();
        let (id, cancel) = self.worker.spawn_long("backup", move |ctx| {
            let r = mgr.backup(&key, &out, &|p| ctx.progress(p), &ctx.cancel);
            Output::BackedUp(out, r)
        });
        self.tasks.insert(id, Task { label, progress: None, cancel });
    }

    pub(super) fn start_restore_into(&mut self, key: InstanceKey, path: PathBuf) {
        let label =
            format!("Restoring {}", self.instance(&key).map(|i| i.display_name()).unwrap_or_else(|| key.to_string()));
        let mgr = self.mgr.clone();
        let (id, cancel) = self.worker.spawn_long("restore", move |ctx| {
            Output::Restored(mgr.restore(&path, RestoreTarget::Existing(key), &|p| ctx.progress(p), &ctx.cancel))
        });
        self.tasks.insert(id, Task { label, progress: None, cancel });
    }

    fn start_restore_as_new(&mut self, path: PathBuf) {
        let mgr = self.mgr.clone();
        let (id, cancel) = self.worker.spawn_long("restore", move |ctx| {
            let result = (|| {
                let manifest = backup::read_manifest(&path)?;
                let config = mgr.load()?;
                let app = manifest.instance.app_id.clone();
                let slug = instance::unique_slug(&manifest.instance.slug, |s| {
                    config.get(&InstanceKey::new(&app, s)).is_some()
                });
                mgr.restore(
                    &path,
                    RestoreTarget::New { slug: Some(slug), allow_override: false },
                    &|p| ctx.progress(p),
                    &ctx.cancel,
                )
            })();
            Output::Restored(result)
        });
        self.tasks.insert(id, Task { label: "Restoring backup as a new instance".into(), progress: None, cancel });
    }

    fn start_export(&mut self, path: PathBuf, with_data: bool) {
        let path = if path.extension().is_none() { path.with_extension("json") } else { path };
        let mgr = self.mgr.clone();
        let out = path.clone();
        let (id, cancel) = self.worker.spawn_long("export", move |ctx| {
            let r = mgr.export(&out, with_data, &|p| ctx.progress(p), &ctx.cancel);
            Output::Exported(out, r)
        });
        self.tasks.insert(id, Task { label: "Exporting setup".into(), progress: None, cancel });
    }

    pub(super) fn start_import(&mut self, path: PathBuf, opts: ImportOptions) {
        let mgr = self.mgr.clone();
        let (id, cancel) = self.worker.spawn_long("import", move |ctx| {
            Output::Imported(mgr.import(&path, opts, &|p| ctx.progress(p), &ctx.cancel))
        });
        self.tasks.insert(id, Task { label: "Importing setup".into(), progress: None, cancel });
    }

    /// Opens the repair dialog and starts a check.
    pub(super) fn run_doctor(&mut self) {
        match &mut self.dialog {
            Some(Dialog::Doctor { issues, working }) => {
                *issues = Loadable::Loading;
                *working = true;
            }
            _ => self.dialog = Some(Dialog::Doctor { issues: Loadable::Loading, working: true }),
        }
        self.start_doctor();
    }

    /// Starts a check without touching the dialog state.
    pub(super) fn start_doctor(&mut self) {
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| Output::Doctor(mgr.doctor()));
    }

    pub(super) fn apply_fixes(&mut self, fixes: Vec<Fix>) {
        let mgr = self.mgr.clone();
        self.worker.spawn(move |_| {
            let mut first_err = None;
            for f in &fixes {
                if let Err(e) = mgr.apply_fix(f) {
                    first_err.get_or_insert(e);
                }
            }
            Output::FixApplied(first_err.map_or(Ok(()), Err))
        });
    }

    /// Applies theme and zoom when they change.
    pub(super) fn apply_look(&mut self, ctx: &egui::Context) {
        let zoom = self.state.zoom.clamp(0.5, 3.0);
        let look = (self.state.theme, (zoom * 100.0).round() as u32);
        if self.applied_look != Some(look) {
            ctx.set_theme(self.state.theme.to_egui());
            ctx.set_zoom_factor(zoom);
            self.applied_look = Some(look);
        }
    }

    pub(super) fn handle_toasts(&mut self, ctx: &egui::Context) {
        if let Some(style::ToastAction::Details(text)) = self.toasts.show(ctx) {
            self.dialog = Some(Dialog::Message { title: "Details".into(), body: text });
        }
    }
}

/// Makes egui reload a regenerated badge icon.
fn forget_badge(ctx: &egui::Context, mgr: &Manager, inst: &Instance) {
    let path = mgr.paths().badge_icon(&inst.app_id, &inst.slug);
    ctx.forget_image(&format!("file://{}", path.display()));
}

impl eframe::App for MultipakApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.process_messages(&ctx);
        self.sync_form_detection();
        self.apply_look(&ctx);
        self.tick(&ctx);
        self.shortcuts(&ctx);
        self.top_bar(ui);
        self.task_bar(ui);
        self.side_panel(ui);
        self.central_panel(ui);
        self.show_dialog(&ctx);
        self.handle_toasts(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.state);
    }
}
