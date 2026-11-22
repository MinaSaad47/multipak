//! Modal dialogs. Escape closes a dialog, Enter (in a single-line field or with nothing
//! focused) or Ctrl+Enter confirms it.

use egui::{Button, Id, Key, Margin, Modifiers, RichText, TextEdit};
use multipak_core::Strategy;
use multipak_core::instance::Field;
use multipak_core::transfer::OnConflict;

use super::app::{Dialog, Loadable, MultipakApp};
use super::model::{InstanceForm, ThemeChoice};
use super::style;
use super::views::{danger_box, warning_box};
use super::worker::PickPurpose;

/// What the user did in a dialog this frame.
#[derive(PartialEq, Eq)]
enum Outcome {
    Keep,
    Close,
    Submit,
}

fn modal<R>(ctx: &egui::Context, id: &str, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> (R, bool) {
    let resp = egui::Modal::new(Id::unique(id)).show(ctx, |ui| {
        ui.set_width(width);
        add(ui)
    });
    let escape =
        resp.is_top_modal && !resp.any_popup_open && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    (resp.inner, escape)
}

/// Enter confirms when focus is in one of `single_line` fields or nowhere; Ctrl+Enter
/// always confirms. Enter on a focused button activates that button instead.
fn enter_submits(ctx: &egui::Context, single_line: &[Id]) -> bool {
    if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
        return true;
    }
    let focused = ctx.memory(|m| m.focused());
    let allowed = focused.is_none_or(|f| single_line.contains(&f));
    allowed && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
}

fn field_error(ui: &mut egui::Ui, msg: Option<&str>) {
    if let Some(m) = msg {
        ui.label(RichText::new(m).small().color(ui.visuals().error_fg_color));
    }
}

fn dialog_buttons(ui: &mut egui::Ui, confirm: &str, enabled: bool, danger: bool, working: bool) -> Outcome {
    let mut out = Outcome::Keep;
    ui.add_space(6.0);
    ui.separator();
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let fill = if danger { ui.visuals().error_fg_color } else { style::accent_fill(ui) };
            let text = RichText::new(confirm).strong().color(egui::Color32::WHITE);
            if ui.add_enabled(enabled && !working, Button::new(text).fill(fill)).clicked() {
                out = Outcome::Submit;
            }
            if ui.button("Cancel").clicked() {
                out = Outcome::Close;
            }
            if working {
                ui.spinner();
            }
        });
    });
    out
}

impl MultipakApp {
    pub(super) fn show_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else { return };
        let keep = match &mut dialog {
            Dialog::Form(form) => self.form_dialog(ctx, form),
            Dialog::Delete(d) => {
                let (out, esc) = modal(ctx, "delete", 460.0, |ui| {
                    ui.heading(format!("Delete {}?", d.display_name));
                    ui.add_space(4.0);
                    ui.label("This removes:");
                    ui.label("  • the launcher from the app menu");
                    if d.autostart {
                        ui.label("  • the autostart entry");
                    }
                    if d.has_override {
                        ui.label("  • the folder access Multipak granted to the app");
                    }
                    ui.add_space(6.0);
                    ui.checkbox(&mut d.delete_data, "Also delete the data folder (accounts, settings, files)");
                    ui.weak(d.data_dir.display().to_string());
                    let confirm_id = Id::unique("delete-confirm");
                    if d.delete_data {
                        danger_box(ui, "The data folder will be permanently deleted. This cannot be undone.");
                        ui.label(format!("Type {} to confirm:", d.confirmation_word()));
                        let r = ui.add(TextEdit::singleline(&mut d.typed).id(confirm_id).desired_width(200.0));
                        if r.gained_focus() || d.typed.is_empty() {
                            r.request_focus();
                        }
                    } else {
                        ui.weak("The data folder is kept. You can create a new instance that uses it later.");
                    }
                    if let Some(e) = &d.error {
                        warning_box(ui, true, e);
                    }
                    let label = if d.delete_data { "Delete instance and data" } else { "Delete instance" };
                    let mut out = dialog_buttons(ui, label, d.can_confirm(), true, d.working);
                    if out == Outcome::Keep && d.can_confirm() && enter_submits(ui.ctx(), &[confirm_id]) {
                        out = Outcome::Submit;
                    }
                    out
                });
                match (out, esc) {
                    (Outcome::Submit, _) => {
                        d.working = true;
                        d.error = None;
                        self.delete(d.key.clone(), d.delete_data);
                        true
                    }
                    (Outcome::Close, _) | (_, true) => false,
                    _ => true,
                }
            }
            Dialog::Duplicate { key, name, working, error } => {
                let (out, esc) = modal(ctx, "duplicate", 420.0, |ui| {
                    ui.heading("Duplicate instance");
                    ui.label("The copy gets the same settings and a new, empty data folder.");
                    ui.add_space(4.0);
                    let id = Id::unique("dup-name");
                    let label = ui.label("Name of the copy:");
                    let r =
                        ui.add(TextEdit::singleline(name).id(id).desired_width(f32::INFINITY)).labelled_by(label.id);
                    if !r.has_focus() && ui.memory(|m| m.focused().is_none()) {
                        r.request_focus();
                    }
                    if let Some(e) = error {
                        warning_box(ui, true, e);
                    }
                    let valid = multipak_core::instance::validate_name(name).is_ok();
                    let mut out = dialog_buttons(ui, "Duplicate", valid, false, *working);
                    if out == Outcome::Keep && valid && !*working && enter_submits(ui.ctx(), &[id]) {
                        out = Outcome::Submit;
                    }
                    out
                });
                match (out, esc) {
                    (Outcome::Submit, _) => {
                        *working = true;
                        *error = None;
                        self.duplicate(key.clone(), name.clone());
                        true
                    }
                    (Outcome::Close, _) | (_, true) => false,
                    _ => true,
                }
            }
            Dialog::Settings => self.settings_dialog(ctx),
            Dialog::Doctor { issues, working } => self.doctor_dialog(ctx, issues, working),
            Dialog::ExportOptions { with_data } => {
                let (out, esc) = modal(ctx, "export", 460.0, |ui| {
                    ui.heading("Export setup");
                    ui.label("Saves every instance's settings to a JSON file you can import on another computer.");
                    ui.add_space(4.0);
                    ui.checkbox(with_data, "Also include data backups (accounts, settings, files)");
                    if *with_data {
                        ui.weak("Backups are saved as .tar.zst files in a folder next to the JSON file. They can be large and contain private data such as login sessions.");
                    }
                    let mut out = dialog_buttons(ui, "Choose file…", true, false, false);
                    if out == Outcome::Keep && enter_submits(ui.ctx(), &[]) {
                        out = Outcome::Submit;
                    }
                    out
                });
                match (out, esc) {
                    (Outcome::Submit, _) => {
                        self.pick(PickPurpose::ExportTo { with_data: *with_data }, Some("multipak-export.json".into()));
                        false
                    }
                    (Outcome::Close, _) | (_, true) => false,
                    _ => true,
                }
            }
            Dialog::Import { path, file, opts } => {
                let has_backups = file.instances.iter().any(|e| e.backup.is_some());
                let (out, esc) = modal(ctx, "import", 500.0, |ui| {
                    ui.heading("Import setup");
                    ui.label(format!("{} contains {} instance(s):", path.display(), file.instances.len()));
                    egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                        for e in &file.instances {
                            let extra = if e.backup.is_some() { " (with data)" } else { "" };
                            ui.label(format!("  • {}{extra}", e.instance.display_name()));
                        }
                    });
                    ui.add_space(4.0);
                    ui.label("If an instance with the same short name exists:");
                    ui.radio_value(&mut opts.on_conflict, OnConflict::Skip, "Skip it");
                    ui.radio_value(&mut opts.on_conflict, OnConflict::Rename, "Import it under a new short name");
                    if has_backups {
                        ui.checkbox(&mut opts.restore_data, "Restore included data backups");
                    }
                    let mut out = dialog_buttons(ui, "Import", true, false, false);
                    if out == Outcome::Keep && enter_submits(ui.ctx(), &[]) {
                        out = Outcome::Submit;
                    }
                    out
                });
                match (out, esc) {
                    (Outcome::Submit, _) => {
                        self.start_import(path.clone(), *opts);
                        false
                    }
                    (Outcome::Close, _) | (_, true) => false,
                    _ => true,
                }
            }
            Dialog::ConfirmRestore { key, name, path } => {
                let running = self.running_keys().contains(key);
                let (out, esc) = modal(ctx, "restore", 460.0, |ui| {
                    ui.heading(format!("Restore {name}?"));
                    ui.label(format!(
                        "The current data of this instance will be replaced with the contents of {}.",
                        path.display()
                    ));
                    if running {
                        danger_box(ui, "The instance is running. Stop it before restoring.");
                    }
                    dialog_buttons(ui, "Replace data", !running, true, false)
                });
                match (out, esc) {
                    (Outcome::Submit, _) => {
                        self.start_restore_into(key.clone(), path.clone());
                        false
                    }
                    (Outcome::Close, _) | (_, true) => false,
                    _ => true,
                }
            }
            Dialog::Message { title, body } => {
                let (out, esc) = modal(ctx, "message", 520.0, |ui| {
                    ui.heading(title.as_str());
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        let mut text = body.as_str();
                        ui.add(TextEdit::multiline(&mut text).desired_width(f32::INFINITY));
                    });
                    let mut out = Outcome::Keep;
                    ui.horizontal(|ui| {
                        if ui.button("Copy").clicked() {
                            ui.ctx().copy_text(body.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Close").clicked() {
                                out = Outcome::Close;
                            }
                        });
                    });
                    if enter_submits(ui.ctx(), &[]) {
                        out = Outcome::Close;
                    }
                    out
                });
                !(out == Outcome::Close || esc)
            }
        };
        // A handler may have opened a different dialog while this one was taken out.
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    fn form_dialog(&mut self, ctx: &egui::Context, form: &mut InstanceForm) -> bool {
        let details = self.details.get(&form.app_id).and_then(|d| d.ready()).cloned();
        let categories = details.as_ref().map(|d| d.categories.clone()).unwrap_or_default();
        let eval = form.evaluate(self.mgr.paths(), &self.config.instances, self.flatpak_bin.as_ref(), &categories);
        let ids = [
            Id::unique("f-name"),
            Id::unique("f-slug"),
            Id::unique("f-class"),
            Id::unique("f-dir"),
            Id::unique("f-letter"),
        ];
        let existing = self.config.instances.clone();
        let mut browse = false;
        let title = if form.is_edit() {
            format!("Edit {} ({})", form.app_name, form.name)
        } else {
            format!("New {} instance", form.app_name)
        };

        let (out, esc) = modal(ctx, "instance-form", 620.0, |ui| {
            ui.heading(title);
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(ui.ctx().content_rect().height() * 0.72).show(ui, |ui| {
                egui::Grid::new("form-grid").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                    let l = ui.label("Name");
                    ui.vertical(|ui| {
                        let r = ui
                            .add(
                                TextEdit::singleline(&mut form.name)
                                    .id(ids[0])
                                    .desired_width(f32::INFINITY)
                                    .hint_text("e.g. Work"),
                            )
                            .labelled_by(l.id);
                        if r.changed() {
                            form.name_changed(&existing);
                        }
                        if ui.memory(|m| m.focused().is_none()) && !form.is_edit() && form.name.starts_with("Instance ")
                        {
                            r.request_focus();
                        }
                        field_error(ui, eval.error(Field::Name));
                    });
                    ui.end_row();

                    let l = ui.label("Short name");
                    ui.vertical(|ui| {
                        let r = ui
                            .add_enabled(
                                !form.is_edit(),
                                TextEdit::singleline(&mut form.slug).id(ids[1]).desired_width(f32::INFINITY),
                            )
                            .labelled_by(l.id);
                        if r.changed() {
                            form.slug_changed();
                        }
                        ui.weak(if form.is_edit() {
                            "Used in file names; cannot change after creation."
                        } else {
                            "Used in file and folder names."
                        });
                        field_error(ui, eval.error(Field::Slug));
                    });
                    ui.end_row();

                    let l = ui.label("Window class");
                    ui.vertical(|ui| {
                        let r = ui
                            .add(TextEdit::singleline(&mut form.wm_class).id(ids[2]).desired_width(f32::INFINITY))
                            .labelled_by(l.id);
                        if r.changed() {
                            form.wm_class_edited = true;
                        }
                        ui.weak("Lets the taskbar and dock show this instance as its own app.");
                        field_error(ui, eval.error(Field::WmClass));
                    });
                    ui.end_row();

                    ui.label("Launch type");
                    ui.vertical(|ui| {
                        let label_for = |s: Strategy| {
                            if Some(s) == form.detected {
                                format!("{} (detected)", s.label())
                            } else {
                                s.label().to_string()
                            }
                        };
                        egui::ComboBox::from_id_salt("f-strategy")
                            .selected_text(label_for(form.strategy))
                            .width(300.0)
                            .show_ui(ui, |ui| {
                                for s in Strategy::ALL {
                                    ui.selectable_value(&mut form.strategy, s, label_for(s));
                                }
                            });
                        ui.weak(form.strategy.description());
                        if form.detected.is_none() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.weak("Detecting app type…");
                            });
                        }
                    });
                    ui.end_row();

                    ui.label("Data folder");
                    ui.vertical(|ui| {
                        ui.radio_value(&mut form.custom_dir, false, "Default (inside the app's own folder)");
                        if !form.custom_dir {
                            let dir = self.mgr.paths().default_instance_dir(&form.app_id, &form.slug);
                            ui.weak(self.mgr.paths().tildify(&dir));
                        }
                        ui.radio_value(&mut form.custom_dir, true, "Custom folder");
                        if form.custom_dir {
                            ui.horizontal(|ui| {
                                ui.add(
                                    TextEdit::singleline(&mut form.data_dir)
                                        .id(ids[3])
                                        .desired_width(330.0)
                                        .hint_text("/path/to/folder"),
                                );
                                if ui.button("Browse…").clicked() {
                                    browse = true;
                                }
                            });
                        }
                        field_error(ui, eval.error(Field::DataDir));
                    });
                    ui.end_row();

                    let l = ui.label("Extra arguments");
                    ui.vertical(|ui| {
                        ui.add(
                            TextEdit::multiline(&mut form.args_text)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("One argument per line")
                                .font(egui::TextStyle::Monospace),
                        )
                        .labelled_by(l.id);
                        field_error(ui, eval.error(Field::Args));
                    });
                    ui.end_row();

                    let l = ui.label("Environment");
                    ui.vertical(|ui| {
                        ui.add(
                            TextEdit::multiline(&mut form.env_text)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("KEY=value, one per line")
                                .font(egui::TextStyle::Monospace),
                        )
                        .labelled_by(l.id);
                        field_error(ui, eval.error(Field::Env));
                    });
                    ui.end_row();

                    ui.label("Icon");
                    ui.vertical(|ui| {
                        ui.checkbox(&mut form.badge, "Add a colored badge so this instance is easy to recognize");
                        if form.badge {
                            ui.horizontal(|ui| {
                                ui.label("Color");
                                ui.color_edit_button_srgb(&mut form.badge_color);
                                for c in super::model::BADGE_COLORS {
                                    let swatch = Button::new("  ")
                                        .fill(egui::Color32::from_rgb(c[0], c[1], c[2]))
                                        .min_size(egui::vec2(18.0, 18.0));
                                    if ui
                                        .add(swatch)
                                        .on_hover_text(multipak_core::instance::format_hex_color(c))
                                        .clicked()
                                    {
                                        form.badge_color = c;
                                    }
                                }
                            });
                            ui.horizontal(|ui| {
                                let l = ui.label("Letter");
                                ui.add(
                                    TextEdit::singleline(&mut form.badge_letter)
                                        .id(ids[4])
                                        .char_limit(1)
                                        .desired_width(30.0),
                                )
                                .labelled_by(l.id);
                                ui.weak("(optional)");
                            });
                        }
                        field_error(ui, eval.error(Field::Badge));
                    });
                    ui.end_row();

                    ui.label("Startup");
                    ui.checkbox(&mut form.autostart, "Start automatically when I log in");
                    ui.end_row();

                    ui.label("Privacy");
                    ui.vertical(|ui| {
                        ui.checkbox(
                            &mut form.hide_from_capture,
                            "Hide from screenshots, recordings and screen sharing",
                        );
                        let support = self.mgr.capture_support();
                        if form.hide_from_capture || !support.is_supported() {
                            ui.weak(support.description());
                        }
                    });
                    ui.end_row();
                });

                for w in &eval.warnings {
                    warning_box(ui, false, w);
                }
                if let Some(p) = &eval.preview {
                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("Preview: launch command and launcher file").id_salt("f-preview").show(
                        ui,
                        |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Command").strong());
                                if ui.small_button("Copy").clicked() {
                                    ui.ctx().copy_text(p.command.clone());
                                }
                            });
                            let mut cmd = p.command.as_str();
                            ui.add(
                                TextEdit::multiline(&mut cmd)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "~/.local/share/applications/{}",
                                    multipak_core::Paths::desktop_file_name(&form.app_id, &form.slug)
                                ))
                                .strong(),
                            );
                            let mut desktop = p.desktop_file.as_str();
                            ui.add(
                                TextEdit::multiline(&mut desktop)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                        },
                    );
                }
                if let Some(e) = &form.error {
                    ui.add_space(4.0);
                    warning_box(ui, true, e);
                }
            });
            let can = form.can_submit(&eval);
            let mut out =
                dialog_buttons(ui, if form.is_edit() { "Save" } else { "Create instance" }, can, false, form.saving);
            if out == Outcome::Keep && can && enter_submits(ui.ctx(), &ids) {
                out = Outcome::Submit;
            }
            out
        });
        if browse {
            self.pick(PickPurpose::FormDataDir, None);
        }
        match (out, esc) {
            (Outcome::Submit, _) => {
                // Put the form back first so submit_form can mark it as saving.
                self.dialog = Some(Dialog::Form(Box::new(form.clone())));
                self.submit_form();
                false
            }
            (Outcome::Close, _) | (_, true) => false,
            _ => true,
        }
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) -> bool {
        let paths = self.mgr.paths().clone();
        let mut open: Option<std::path::PathBuf> = None;
        let mut toggle_self: Option<bool> = None;
        let (out, esc) = modal(ctx, "settings", 520.0, |ui| {
            ui.heading("Settings");
            ui.add_space(4.0);
            egui::Grid::new("settings-grid").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
                ui.label("Theme");
                ui.horizontal(|ui| {
                    for t in ThemeChoice::ALL {
                        ui.radio_value(&mut self.state.theme, t, t.label());
                    }
                });
                ui.end_row();
                ui.label("Interface size");
                ui.add(
                    egui::Slider::new(&mut self.state.zoom, 0.75..=2.0)
                        .step_by(0.05)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                ui.end_row();
                ui.label("Privacy");
                ui.vertical(|ui| {
                    match self.self_capture {
                        Some(mut hidden) => {
                            let r = ui.checkbox(&mut hidden, "Hide Multipak from screenshots and screen sharing");
                            if r.changed() {
                                toggle_self = Some(hidden);
                            }
                        }
                        None => {
                            ui.spinner();
                        }
                    }
                    ui.weak(self.mgr.capture_support().description());
                });
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.label(RichText::new("Files").strong());
            for (label, path, folder) in [
                ("Instance setup", paths.config_file(), paths.config_dir()),
                ("App type additions", paths.user_app_types_file(), paths.config_dir()),
                ("Launchers", paths.applications_dir(), paths.applications_dir()),
                ("Logs", paths.state_dir(), paths.state_dir()),
            ] {
                ui.horizontal(|ui| {
                    ui.label(format!("{label}:"));
                    ui.monospace(paths.tildify(&path));
                    if ui.small_button("Open folder").clicked() {
                        open = Some(folder);
                    }
                });
            }
            ui.weak(format!(
                "{} apps are in the app type table. Add your own entries to app_types.toml (see CONTRIBUTING.md) and restart Multipak.",
                self.mgr.types().len()
            ));
            ui.add_space(8.0);
            ui.label(RichText::new("About").strong());
            ui.label(format!("Multipak {} · MIT license", env!("CARGO_PKG_VERSION")));
            ui.label("Privacy: Multipak makes no network connections and collects no data.");
            ui.hyperlink_to("Source code and documentation", env!("CARGO_PKG_REPOSITORY"));
            let mut out = Outcome::Keep;
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    out = Outcome::Close;
                }
            });
            out
        });
        if let Some(hidden) = toggle_self {
            self.set_self_capture(hidden);
        }
        if let Some(dir) = open {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                self.toasts.push(style::ToastKind::Error, format!("Could not create {}: {e}", dir.display()));
            } else {
                self.open_folder(dir);
            }
        }
        !(out == Outcome::Close || esc)
    }

    fn doctor_dialog(
        &mut self,
        ctx: &egui::Context,
        issues: &mut Loadable<Vec<multipak_core::Issue>>,
        working_flag: &mut bool,
    ) -> bool {
        let working = *working_flag;
        let mut fixes = Vec::new();
        let mut rerun = false;
        let (out, esc) = modal(ctx, "doctor", 600.0, |ui| {
            ui.heading("Repair");
            ui.label("Checks launchers, autostart entries, data folders, folder access and installed apps.");
            ui.add_space(6.0);
            match issues {
                Loadable::Loading | Loadable::Idle => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Checking…");
                    });
                }
                Loadable::Failed(e) => warning_box(ui, true, e),
                Loadable::Ready(list) if list.is_empty() => {
                    ui.label(RichText::new("Everything looks good.").color(style::success_color(ui)));
                }
                Loadable::Ready(list) => {
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        for issue in list {
                            egui::Frame::group(ui.style()).inner_margin(Margin::same(8)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                if let Some(k) = &issue.key {
                                    ui.label(RichText::new(k.to_string()).strong());
                                }
                                ui.label(&issue.message);
                                if let Some(fix) = &issue.fix {
                                    if ui.add_enabled(!working, Button::new(fix.label())).clicked() {
                                        fixes.push(fix.clone());
                                    }
                                } else {
                                    ui.weak("No automatic fix available.");
                                }
                            });
                        }
                    });
                }
            }
            let mut out = Outcome::Keep;
            ui.separator();
            ui.horizontal(|ui| {
                if ui.add_enabled(!working, Button::new("Check again")).clicked() {
                    rerun = true;
                }
                if let Loadable::Ready(list) = issues {
                    let all: Vec<_> = list.iter().filter_map(|i| i.fix.clone()).collect();
                    // Removing an instance makes its other fixes moot, so "Fix all" skips removals.
                    let safe: Vec<_> =
                        all.into_iter().filter(|f| !matches!(f, multipak_core::Fix::RemoveInstance(_))).collect();
                    if !safe.is_empty()
                        && ui
                            .add_enabled(!working, Button::new(format!("Fix all ({})", safe.len())))
                            .on_hover_text("Applies every fix except removing instances")
                            .clicked()
                    {
                        fixes = safe;
                    }
                }
                if working {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        out = Outcome::Close;
                    }
                });
            });
            out
        });
        if !fixes.is_empty() {
            *working_flag = true;
            self.apply_fixes(fixes);
        } else if rerun {
            *working_flag = true;
            *issues = Loadable::Loading;
            self.start_doctor();
        }
        !(out == Outcome::Close || esc)
    }
}
