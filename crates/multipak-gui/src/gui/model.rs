//! GUI state that can be tested without rendering: the instance form, the delete
//! confirmation, the app list and small formatting helpers.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use multipak_core::config::Config;
use multipak_core::instance::{self, DraftContext, Field, FieldError, Instance, InstanceDraft, InstanceKey};
use multipak_core::manager::{self, Preview};
use multipak_core::{Badge, FlatpakApp, Paths, Strategy};
use serde::{Deserialize, Serialize};

/// Light/dark preference stored in the UI state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Follow system",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn to_egui(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }
}

/// UI state persisted by eframe (separate from the instance setup file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub selected_app: Option<String>,
    pub theme: ThemeChoice,
    pub zoom: f32,
}

impl Default for UiState {
    fn default() -> Self {
        Self { selected_app: None, theme: ThemeChoice::System, zoom: 1.0 }
    }
}

/// One row of the app list: an installed app (possibly in several installations) or an
/// app that is no longer installed but still has instances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub app_id: String,
    pub name: String,
    pub installations: Vec<String>,
    pub installed: bool,
}

/// Merges `flatpak list` rows by app ID and adds uninstalled apps that have instances.
pub fn build_app_entries(apps: &[FlatpakApp], config: &Config) -> Vec<AppEntry> {
    let mut entries: Vec<AppEntry> = Vec::new();
    for app in apps {
        match entries.iter_mut().find(|e| e.app_id == app.app_id) {
            Some(e) => {
                if !e.installations.contains(&app.installation) {
                    e.installations.push(app.installation.clone());
                }
            }
            None => entries.push(AppEntry {
                app_id: app.app_id.clone(),
                name: app.name.clone(),
                installations: vec![app.installation.clone()],
                installed: true,
            }),
        }
    }
    for inst in &config.instances {
        if !entries.iter().any(|e| e.app_id == inst.app_id) {
            entries.push(AppEntry {
                app_id: inst.app_id.clone(),
                name: inst.app_name.clone(),
                installations: Vec::new(),
                installed: false,
            });
        }
    }
    entries
}

/// Filters by name or app ID (case-insensitive). Apps with instances come first, then
/// alphabetical order.
pub fn filter_apps<'a>(entries: &'a [AppEntry], query: &str, config: &Config) -> Vec<&'a AppEntry> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<&AppEntry> = entries
        .iter()
        .filter(|e| q.is_empty() || e.name.to_lowercase().contains(&q) || e.app_id.to_lowercase().contains(&q))
        .collect();
    out.sort_by_key(|e| (config.for_app(&e.app_id).next().is_none(), e.name.to_lowercase(), e.app_id.clone()));
    out
}

/// `1.4 GB`, `512 KB`, …
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Whether the form creates a new instance or edits one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormMode {
    Create,
    Edit(InstanceKey),
}

/// Text-based state of the create/edit dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceForm {
    pub mode: FormMode,
    pub app_id: String,
    pub app_name: String,
    pub name: String,
    pub slug: String,
    pub slug_edited: bool,
    pub wm_class: String,
    pub wm_class_edited: bool,
    pub strategy: Strategy,
    pub detected: Option<Strategy>,
    pub custom_dir: bool,
    pub data_dir: String,
    /// One argument per line.
    pub args_text: String,
    /// One `KEY=VALUE` per line.
    pub env_text: String,
    pub badge: bool,
    pub badge_color: [u8; 3],
    pub badge_letter: String,
    pub autostart: bool,
    pub hide_from_capture: bool,
    pub saving: bool,
    pub error: Option<String>,
}

/// Default badge colors, cycled for new instances.
pub const BADGE_COLORS: [[u8; 3]; 8] = [
    [0x35, 0x84, 0xe4],
    [0xe0, 0x1b, 0x24],
    [0x2e, 0xc2, 0x7e],
    [0xf6, 0xd3, 0x2d],
    [0x91, 0x41, 0xac],
    [0xff, 0x78, 0x00],
    [0x98, 0x6a, 0x44],
    [0x5e, 0x5c, 0x64],
];

/// Result of checking the form.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormEval {
    pub errors: HashMap<Field, String>,
    pub warnings: Vec<String>,
    pub preview: Option<Preview>,
}

impl FormEval {
    pub fn error(&self, field: Field) -> Option<&str> {
        self.errors.get(&field).map(String::as_str)
    }
}

impl InstanceForm {
    /// A form for a new instance of `app_id`.
    pub fn create(app_id: &str, app_name: &str, detected: Strategy, existing: &[Instance]) -> Self {
        let count = existing.iter().filter(|i| i.app_id == app_id).count();
        let name = format!("Instance {}", count + 1);
        let draft = InstanceDraft::new(app_id, app_name, &name, detected, existing);
        Self {
            mode: FormMode::Create,
            app_id: app_id.to_string(),
            app_name: app_name.to_string(),
            name,
            slug: draft.slug,
            slug_edited: false,
            wm_class: draft.wm_class,
            wm_class_edited: false,
            strategy: detected,
            detected: Some(detected),
            custom_dir: false,
            data_dir: String::new(),
            args_text: String::new(),
            env_text: String::new(),
            badge: false,
            badge_color: BADGE_COLORS[count % BADGE_COLORS.len()],
            badge_letter: "I".to_string(),
            autostart: false,
            hide_from_capture: false,
            saving: false,
            error: None,
        }
    }

    /// A form pre-filled from an existing instance.
    pub fn edit(instance: &Instance, detected: Option<Strategy>) -> Self {
        let badge = instance.badge.clone();
        Self {
            mode: FormMode::Edit(instance.key()),
            app_id: instance.app_id.clone(),
            app_name: instance.app_name.clone(),
            name: instance.name.clone(),
            slug: instance.slug.clone(),
            slug_edited: true,
            wm_class: instance.wm_class.clone(),
            wm_class_edited: true,
            strategy: instance.strategy,
            detected,
            custom_dir: instance.data_dir.is_some(),
            data_dir: instance.data_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            args_text: instance.extra_args.join("\n"),
            env_text: instance.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"),
            badge: badge.is_some(),
            badge_color: badge.as_ref().and_then(|b| b.rgb().ok()).unwrap_or(BADGE_COLORS[0]),
            badge_letter: badge.and_then(|b| b.letter).unwrap_or_default(),
            autostart: instance.autostart,
            hide_from_capture: instance.hide_from_capture,
            saving: false,
            error: None,
        }
    }

    pub fn is_edit(&self) -> bool {
        matches!(self.mode, FormMode::Edit(_))
    }

    /// Keeps the slug, window class and badge letter in sync with the name until the user
    /// edits them by hand. The slug never changes when editing.
    pub fn name_changed(&mut self, existing: &[Instance]) {
        if self.is_edit() {
            return;
        }
        if !self.slug_edited {
            let app = self.app_id.clone();
            self.slug = instance::unique_slug(&instance::slugify(&self.name), |s| {
                existing.iter().any(|i| i.app_id == app && i.slug == s)
            });
        }
        if !self.wm_class_edited {
            self.wm_class = instance::default_wm_class(&self.app_id, &self.slug);
        }
        if let Some(c) = self.name.trim().chars().next() {
            self.badge_letter = c.to_uppercase().to_string();
        }
    }

    pub fn slug_changed(&mut self) {
        self.slug_edited = true;
        if !self.wm_class_edited {
            self.wm_class = instance::default_wm_class(&self.app_id, &self.slug);
        }
    }

    /// Converts the text fields into a draft. Errors in the free-text fields (environment
    /// lines) are reported per field.
    pub fn to_draft(&self) -> Result<InstanceDraft, Vec<FieldError>> {
        let mut errors = Vec::new();
        let extra_args: Vec<String> = self
            .args_text
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        let mut env = BTreeMap::new();
        for (n, line) in self.env_text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match instance::parse_env_pair(line.trim()) {
                Ok((k, v)) => {
                    env.insert(k, v);
                }
                Err(e) => errors
                    .push(FieldError { field: Field::Env, message: format!("line {}: {}", n + 1, strip_invalid(&e)) }),
            }
        }
        let data_dir = if self.custom_dir {
            if self.data_dir.trim().is_empty() {
                errors.push(FieldError {
                    field: Field::DataDir,
                    message: "choose a folder or switch back to the default location".into(),
                });
                None
            } else {
                Some(PathBuf::from(self.data_dir.trim()))
            }
        } else {
            None
        };
        let badge = self.badge.then(|| Badge {
            color: instance::format_hex_color(self.badge_color),
            letter: Some(self.badge_letter.trim().to_string()).filter(|l| !l.is_empty()),
        });
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(InstanceDraft {
            app_id: self.app_id.clone(),
            app_name: self.app_name.clone(),
            name: self.name.clone(),
            slug: self.slug.trim().to_string(),
            wm_class: self.wm_class.trim().to_string(),
            strategy: self.strategy,
            data_dir,
            extra_args,
            env,
            badge,
            autostart: self.autostart,
            hide_from_capture: self.hide_from_capture,
        })
    }

    /// Validates the form and builds the live preview.
    pub fn evaluate(
        &self,
        paths: &Paths,
        existing: &[Instance],
        flatpak_bin: Option<&multipak_core::launch::Binaries>,
        categories: &[String],
    ) -> FormEval {
        let mut eval = FormEval::default();
        let draft = match self.to_draft() {
            Ok(d) => d,
            Err(errs) => {
                for e in errs {
                    eval.errors.entry(e.field).or_insert(e.message);
                }
                return eval;
            }
        };
        let editing = match &self.mode {
            FormMode::Edit(k) => Some(k),
            FormMode::Create => None,
        };
        match draft.validate(DraftContext { paths, existing, editing }) {
            Ok(v) => {
                eval.warnings = v.warnings.clone();
                let bins = flatpak_bin.cloned().unwrap_or_else(multipak_core::launch::Binaries::standard);
                eval.preview = Some(manager::preview(paths, &v, &bins, categories));
            }
            Err(errs) => {
                for e in errs {
                    eval.errors.entry(e.field).or_insert(e.message);
                }
            }
        }
        eval
    }

    /// Whether the Save/Create button is enabled.
    pub fn can_submit(&self, eval: &FormEval) -> bool {
        !self.saving && eval.errors.is_empty()
    }
}

fn strip_invalid(e: &multipak_core::Error) -> String {
    match e {
        multipak_core::Error::Invalid { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

/// State of the delete dialog. Deleting data requires typing the short name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteConfirm {
    pub key: InstanceKey,
    pub display_name: String,
    pub data_dir: PathBuf,
    pub has_override: bool,
    pub autostart: bool,
    pub delete_data: bool,
    pub typed: String,
    pub working: bool,
    pub error: Option<String>,
}

impl DeleteConfirm {
    pub fn new(instance: &Instance, paths: &Paths) -> Self {
        Self {
            key: instance.key(),
            display_name: instance.display_name(),
            data_dir: instance.resolved_data_dir(paths),
            has_override: instance.managed_override.is_some(),
            autostart: instance.autostart,
            delete_data: false,
            typed: String::new(),
            working: false,
            error: None,
        }
    }

    /// The text the user must type to delete data.
    pub fn confirmation_word(&self) -> &str {
        &self.key.slug
    }

    pub fn can_confirm(&self) -> bool {
        !self.working && (!self.delete_data || self.typed.trim() == self.confirmation_word())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::for_home("/home/u")
    }

    fn existing_work() -> Vec<Instance> {
        let d = InstanceDraft::new("com.slack.Slack", "Slack", "Work", Strategy::Electron, &[]);
        vec![d.validate(DraftContext { paths: &paths(), existing: &[], editing: None }).unwrap().instance]
    }

    #[test]
    fn name_drives_slug_until_edited() {
        let existing = existing_work();
        let mut f = InstanceForm::create("com.slack.Slack", "Slack", Strategy::Electron, &existing);
        assert_eq!(f.name, "Instance 2");
        f.name = "Work".into();
        f.name_changed(&existing);
        assert_eq!(f.slug, "work-2");
        assert_eq!(f.wm_class, "multipak-slack-work-2");
        assert_eq!(f.badge_letter, "W");
        f.slug = "office".into();
        f.slug_changed();
        assert_eq!(f.wm_class, "multipak-slack-office");
        f.name = "Something else".into();
        f.name_changed(&existing);
        assert_eq!(f.slug, "office", "manual slug is kept");
    }

    #[test]
    fn evaluation_reports_field_errors_and_preview() {
        let existing = existing_work();
        let mut f = InstanceForm::create("com.slack.Slack", "Slack", Strategy::Electron, &existing);
        let eval = f.evaluate(&paths(), &existing, None, &[]);
        assert!(eval.errors.is_empty(), "{:?}", eval.errors);
        let preview = eval.preview.clone().unwrap();
        assert!(preview.command.starts_with("/usr/bin/bwrap --dev-bind / / --bind "));
        assert!(preview.command.contains(" -- /usr/bin/flatpak run "));
        assert!(preview.desktop_file.contains("Name=Slack (Instance 2)"));
        assert!(f.can_submit(&eval));

        f.slug = "work".into();
        f.env_text = "GOOD=1\nnot valid".into();
        let eval = f.evaluate(&paths(), &existing, None, &[]);
        assert!(eval.error(Field::Env).unwrap().contains("line 2"));
        assert!(!f.can_submit(&eval));
        f.env_text.clear();
        let eval = f.evaluate(&paths(), &existing, None, &[]);
        assert!(eval.error(Field::Slug).is_some());
    }

    #[test]
    fn custom_folder_needs_no_extra_permission() {
        let mut f = InstanceForm::create("com.slack.Slack", "Slack", Strategy::Electron, &[]);
        f.custom_dir = true;
        let eval = f.evaluate(&paths(), &[], None, &[]);
        assert!(eval.error(Field::DataDir).is_some());
        f.data_dir = "/home/u/Clients/acme".into();
        let eval = f.evaluate(&paths(), &[], None, &[]);
        assert!(
            eval.preview
                .as_ref()
                .unwrap()
                .command
                .contains("--bind /home/u/Clients/acme /home/u/.var/app/com.slack.Slack")
        );
        assert!(f.can_submit(&eval));
    }

    #[test]
    fn edit_form_round_trips_instance() {
        let mut inst = existing_work().remove(0);
        inst.extra_args = vec!["--a".into(), "b c".into()];
        inst.env.insert("LANG".into(), "C".into());
        inst.badge = Some(Badge { color: "#e01b24".into(), letter: Some("W".into()) });
        inst.hide_from_capture = true;
        let f = InstanceForm::edit(&inst, Some(Strategy::Electron));
        let draft = f.to_draft().unwrap();
        assert_eq!(draft, inst.to_draft());
        let eval = f.evaluate(&paths(), std::slice::from_ref(&inst), None, &[]);
        assert!(eval.errors.is_empty(), "{:?}", eval.errors);
    }

    #[test]
    fn delete_requires_typed_slug_only_for_data() {
        let inst = existing_work().remove(0);
        let mut d = DeleteConfirm::new(&inst, &paths());
        assert!(d.can_confirm());
        d.delete_data = true;
        assert!(!d.can_confirm());
        d.typed = "Work".into();
        assert!(!d.can_confirm());
        d.typed = " work ".into();
        assert!(d.can_confirm());
    }

    #[test]
    fn app_entries_merge_and_filter() {
        let apps = vec![
            FlatpakApp {
                app_id: "org.b.Zed".into(),
                name: "Zed".into(),
                origin: "flathub".into(),
                installation: "user".into(),
            },
            FlatpakApp {
                app_id: "com.slack.Slack".into(),
                name: "Slack".into(),
                origin: "flathub".into(),
                installation: "user".into(),
            },
            FlatpakApp {
                app_id: "com.slack.Slack".into(),
                name: "Slack".into(),
                origin: "flathub".into(),
                installation: "system".into(),
            },
            FlatpakApp {
                app_id: "org.a.Alpha".into(),
                name: "Alpha".into(),
                origin: "flathub".into(),
                installation: "system".into(),
            },
        ];
        let mut config = Config { instances: existing_work(), ..Default::default() };
        let mut gone = config.instances[0].clone();
        gone.app_id = "com.gone.App".into();
        gone.app_name = "Gone".into();
        config.instances.push(gone);
        let entries = build_app_entries(&apps, &config);
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries.iter().find(|e| e.app_id == "com.slack.Slack").unwrap().installations,
            vec!["user", "system"]
        );
        assert!(!entries.iter().find(|e| e.app_id == "com.gone.App").unwrap().installed);
        let names: Vec<_> = filter_apps(&entries, "", &config).iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["Gone", "Slack", "Alpha", "Zed"]);
        let names: Vec<_> = filter_apps(&entries, "ORG.A", &config).iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["Alpha"]);
    }

    #[test]
    fn bytes_formatting() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(25_000_000), "25 MB");
        assert_eq!(format_bytes(3_200_000_000), "3.2 GB");
    }
}
