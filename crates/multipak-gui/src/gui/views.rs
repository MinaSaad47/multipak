//! Main window layout: top bar, task bar, app list and instance cards.

use std::collections::HashSet;

use egui::{Align, Align2, Button, Image, Key, KeyboardShortcut, Layout, Margin, Modifiers, RichText, Vec2};
use multipak_core::SingleInstanceRisk;
use multipak_core::instance::{Instance, InstanceKey};

use super::app::{Dialog, Loadable, MultipakApp};
use super::model::{filter_apps, format_bytes};
use super::style;
use super::worker::PickPurpose;

const CARD_WIDTH: f32 = 330.0;
const GENERIC_ICON: egui::ImageSource<'static> = egui::include_image!("../../assets/generic-app.svg");

pub const SHORTCUT_NEW: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
pub const SHORTCUT_SEARCH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
pub const SHORTCUT_REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F5);

/// Actions collected while drawing a card, applied afterwards (the card borrows `self`).
enum CardAction {
    Launch,
    Stop,
    Edit,
    Duplicate,
    OpenFolder,
    ClearCache,
    Backup,
    Restore,
    CopyCommand,
    RefreshUsage,
    Delete,
}

impl MultipakApp {
    pub(super) fn icon_image(&self, app_id: &str, size: f32) -> Image<'static> {
        let img = match self.icons.get(app_id).and_then(|p| p.as_ref()) {
            Some(path) => Image::new(format!("file://{}", path.display())),
            None => Image::new(GENERIC_ICON),
        };
        img.fit_to_exact_size(Vec2::splat(size)).alt_text(format!("{} icon", self.app_name(app_id)))
    }

    fn instance_icon(&self, inst: &Instance, size: f32) -> Image<'static> {
        if inst.badge.is_some() {
            let path = self.mgr.paths().badge_icon(&inst.app_id, &inst.slug);
            Image::new(format!("file://{}", path.display()))
                .fit_to_exact_size(Vec2::splat(size))
                .alt_text(format!("{} icon", inst.display_name()))
        } else {
            self.icon_image(&inst.app_id, size)
        }
    }

    pub(super) fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_NEW)) {
            self.open_create_form();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_SEARCH)) {
            ctx.memory_mut(|m| m.request_focus(self.search_id));
        }
        if ctx.input_mut(|i| i.consume_shortcut(&SHORTCUT_REFRESH)) {
            self.refresh_all();
        }
    }

    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("top-bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(Margin::symmetric(14, 10)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("Multipak").strong());
                    ui.add_space(12.0);
                    let can_create = self
                        .state
                        .selected_app
                        .as_ref()
                        .is_some_and(|id| self.entries.iter().any(|e| &e.app_id == id && e.installed));
                    let new_btn = Button::new(RichText::new("+ New instance").strong())
                        .fill(style::accent_fill(ui))
                        .shortcut_text(ctx.format_shortcut(&SHORTCUT_NEW));
                    if ui
                        .add_enabled(can_create, new_btn)
                        .on_hover_text("Create a new instance of the selected app")
                        .on_disabled_hover_text("Select an installed app first")
                        .clicked()
                    {
                        self.open_create_form();
                    }
                    if ui
                        .button("Repair…")
                        .on_hover_text("Find and fix missing or outdated launchers and folders")
                        .clicked()
                    {
                        self.run_doctor();
                    }
                    ui.menu_button("Backup & transfer", |ui| {
                        if ui.button("Restore a backup as a new instance…").clicked() {
                            self.pick(PickPurpose::RestoreAsNew, None);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Export setup…").clicked() {
                            self.dialog = Some(Dialog::ExportOptions { with_data: false });
                            ui.close();
                        }
                        if ui.button("Import setup…").clicked() {
                            self.pick(PickPurpose::ImportFrom, None);
                            ui.close();
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Settings").clicked() {
                            self.dialog = Some(Dialog::Settings);
                        }
                        if ui.button("Refresh").on_hover_text("Reload apps and instances (F5)").clicked() {
                            self.refresh_all();
                        }
                    });
                });
            });
    }

    pub(super) fn task_bar(&mut self, ui: &mut egui::Ui) {
        if self.tasks.is_empty() {
            return;
        }
        egui::Panel::bottom("tasks")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                let mut ids: Vec<_> = self.tasks.keys().copied().collect();
                ids.sort_unstable();
                for id in ids {
                    let task = &self.tasks[&id];
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&task.label);
                        let (fraction, text) = match &task.progress {
                            Some(p) if p.total > 0 => {
                                (p.fraction(), format!("{} of {}", format_bytes(p.done), format_bytes(p.total)))
                            }
                            _ => (0.0, "Preparing…".to_string()),
                        };
                        ui.add(egui::ProgressBar::new(fraction).desired_width(260.0).text(text));
                        if task.cancel.is_cancelled() {
                            ui.weak("Cancelling…");
                        } else if ui.button("Cancel").clicked() {
                            task.cancel.cancel();
                        }
                    });
                }
            });
    }

    pub(super) fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("apps")
            .resizable(true)
            .default_size(280.0)
            .size_range(200.0..=460.0)
            .frame(
                egui::Frame::side_top_panel(ui.style())
                    .fill(style::palette(ui).side_fill)
                    .inner_margin(Margin::same(10)),
            )
            .show(ui, |ui| {
                let hint = format!("Search apps ({})", ui.ctx().format_shortcut(&SHORTCUT_SEARCH));
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .id(self.search_id)
                        .hint_text(hint)
                        .desired_width(f32::INFINITY),
                );
                if search.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.search.clear();
                }
                ui.add_space(4.0);
                match &self.apps {
                    Loadable::Loading | Loadable::Idle if self.entries.is_empty() => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Reading installed apps…");
                        });
                        return;
                    }
                    Loadable::Failed(e) if self.entries.is_empty() => {
                        ui.colored_label(ui.visuals().error_fg_color, e.clone());
                        return;
                    }
                    _ => {}
                }
                let filtered: Vec<(String, String, bool)> = filter_apps(&self.entries, &self.search, &self.config)
                    .into_iter()
                    .map(|e| (e.app_id.clone(), e.name.clone(), e.installed))
                    .collect();
                if filtered.is_empty() {
                    ui.weak("No apps match your search.");
                }
                let running_apps: HashSet<String> = self.running.iter().map(|r| r.app_id.clone()).collect();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for (app_id, name, installed) in filtered {
                        let count = self.config.for_app(&app_id).count();
                        let selected = self.state.selected_app.as_deref() == Some(app_id.as_str());
                        let mut sub = match count {
                            0 => String::new(),
                            1 => "1 instance".to_string(),
                            n => format!("{n} instances"),
                        };
                        if !installed {
                            sub =
                                if sub.is_empty() { "not installed".into() } else { format!("{sub} · not installed") };
                        }
                        if running_apps.contains(&app_id) {
                            sub = if sub.is_empty() { "running".into() } else { format!("{sub} · running") };
                        }
                        let icon = self.icon_image(&app_id, 30.0);
                        if app_row(ui, icon, &name, &sub, selected, !installed).on_hover_text(&app_id).clicked() {
                            self.state.selected_app = Some(app_id.clone());
                        }
                    }
                });
            });
    }

    pub(super) fn central_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(err) = self.config_error.clone() {
                warning_box(ui, true, &format!("The instance setup could not be read: {err}"));
                ui.add_space(8.0);
            }
            if matches!(&self.apps, Loadable::Failed(_)) && self.flatpak_bin.is_none() {
                warning_box(
                    ui,
                    true,
                    "The flatpak command was not found. Install Flatpak from your distribution's packages, then press Refresh.",
                );
                ui.add_space(8.0);
            }
            let Some(app_id) = self.state.selected_app.clone() else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Select an app on the left to see its instances.").weak());
                });
                return;
            };
            self.ensure_details(&app_id);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.app_header(ui, &app_id);
                ui.add_space(10.0);
                self.instance_grid(ui, &app_id);
            });
        });
    }

    fn app_header(&mut self, ui: &mut egui::Ui, app_id: &str) {
        let entry = self.entries.iter().find(|e| e.app_id == app_id).cloned();
        let name = self.app_name(app_id);
        ui.horizontal(|ui| {
            ui.add(self.icon_image(app_id, 56.0));
            ui.vertical(|ui| {
                ui.heading(&name);
                let mut sub = app_id.to_string();
                if let Some(e) = &entry
                    && !e.installations.is_empty()
                {
                    sub.push_str(&format!(" · {}", e.installations.join(", ")));
                }
                ui.weak(sub);
                match self.details.get(app_id) {
                    Some(Loadable::Ready(d)) => {
                        ui.label(format!("Detected type: {} ({})", d.detection.strategy.label(), d.detection.source));
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("Detecting app type…");
                        });
                    }
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let installed = entry.as_ref().is_some_and(|e| e.installed);
                if ui.add_enabled(installed, Button::new("+ New instance")).clicked() {
                    self.open_create_form();
                }
            });
        });
        if entry.as_ref().is_some_and(|e| !e.installed) {
            warning_box(
                ui,
                false,
                "This app is no longer installed. Its instances are kept; reinstall the app or remove them with Repair.",
            );
        }
        if let Some(Loadable::Ready(d)) = self.details.get(app_id) {
            if let Some(note) = &d.detection.note {
                ui.weak(note);
            }
            match &d.detection.single_instance {
                SingleInstanceRisk::Unlikely => {}
                SingleInstanceRisk::Possible(w) | SingleInstanceRisk::Likely(w) => warning_box(ui, false, w),
            }
        }
        if self.running.iter().any(|r| r.app_id == app_id && r.key.is_none()) {
            ui.weak("The app's normal (non-Multipak) copy is running.");
        }
    }

    fn instance_grid(&mut self, ui: &mut egui::Ui, app_id: &str) {
        let instances: Vec<Instance> = self.config.for_app(app_id).cloned().collect();
        if instances.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No instances yet").size(18.0).strong());
                ui.label(format!(
                    "Create one to run a separate copy of {} with its own accounts, settings and data.",
                    self.app_name(app_id)
                ));
            });
            return;
        }
        let running = self.running_keys();
        let spacing = ui.spacing().item_spacing.x;
        let available = ui.available_width();
        let columns = (((available + spacing) / (CARD_WIDTH + spacing)).floor().max(1.0) as usize).min(instances.len());
        // Cards stretch so each row fills the full width.
        let card_width = ((available - spacing * (columns as f32 - 1.0)) / columns as f32).floor();
        let mut actions: Vec<(InstanceKey, CardAction)> = Vec::new();
        for row in instances.chunks(columns) {
            ui.horizontal_top(|ui| {
                for inst in row {
                    self.ensure_disk_usage(&inst.key());
                    if let Some(a) = self.instance_card(ui, inst, running.contains(&inst.key()), card_width) {
                        actions.push((inst.key(), a));
                    }
                }
            });
            ui.add_space(spacing);
        }
        let ctx = ui.ctx().clone();
        for (key, action) in actions {
            self.card_action(&ctx, key, action);
        }
    }

    fn instance_card(&self, ui: &mut egui::Ui, inst: &Instance, is_running: bool, width: f32) -> Option<CardAction> {
        let mut action = None;
        let key = inst.key();
        let pal = style::palette(ui);
        egui::Frame::new()
            .fill(pal.card_fill)
            .stroke(egui::Stroke::new(1.0, if is_running { pal.accent } else { pal.card_stroke }))
            .corner_radius(12)
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                // The card sits in a horizontal row; lay its own contents out vertically
                // with a fixed width so nothing spills outside the frame.
                let inner_width = width - 30.0;
                ui.set_width(inner_width);
                ui.vertical(|ui| {
                    ui.set_width(inner_width);
                    ui.horizontal(|ui| {
                        ui.add(self.instance_icon(inst, 44.0));
                        ui.vertical(|ui| {
                            ui.add(egui::Label::new(RichText::new(&inst.name).strong().size(16.0)).truncate())
                                .on_hover_text(inst.display_name());
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("{} · {}", inst.slug, inst.strategy.label())).weak(),
                                )
                                .truncate(),
                            );
                        });
                    });
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        let (text, color) = if is_running {
                            ("Running", style::success_color(ui))
                        } else {
                            ("Not running", ui.visuals().weak_text_color())
                        };
                        status_dot(ui, color, is_running);
                        ui.label(RichText::new(text).color(color));
                        ui.separator();
                        match self.disk.get(&key) {
                            Some(Loadable::Ready(bytes)) => {
                                ui.label(format_bytes(*bytes))
                                    .on_hover_text("Disk space used by this instance's data folder");
                            }
                            Some(Loadable::Failed(e)) => {
                                ui.weak("size unknown").on_hover_text(e);
                            }
                            _ => {
                                ui.spinner();
                            }
                        }
                        if inst.autostart {
                            ui.separator();
                            ui.weak("autostart");
                        }
                    });
                    let dir = inst.resolved_data_dir(self.mgr.paths());
                    ui.add(egui::Label::new(RichText::new(self.mgr.paths().tildify(&dir)).small().weak()).truncate())
                        .on_hover_text(dir.display().to_string());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if let Some(label) = self.busy.get(&key) {
                            ui.spinner();
                            ui.label(*label);
                            return;
                        }
                        if is_running {
                            if ui.button("Stop").on_hover_text("Close every window of this instance").clicked() {
                                action = Some(CardAction::Stop);
                            }
                        } else if ui
                            .add(Button::new(RichText::new("Launch").strong()).fill(style::accent_fill(ui)))
                            .clicked()
                        {
                            action = Some(CardAction::Launch);
                        }
                        if ui.button("Edit").clicked() {
                            action = Some(CardAction::Edit);
                        }
                        ui.menu_button("More", |ui| {
                            let mut pick = |ui: &mut egui::Ui, label: &str, a: CardAction| {
                                if ui.button(label).clicked() {
                                    action = Some(a);
                                    ui.close();
                                }
                            };
                            pick(ui, "Duplicate…", CardAction::Duplicate);
                            pick(ui, "Open data folder", CardAction::OpenFolder);
                            pick(ui, "Copy launch command", CardAction::CopyCommand);
                            pick(ui, "Recalculate disk usage", CardAction::RefreshUsage);
                            ui.separator();
                            pick(ui, "Clear cache", CardAction::ClearCache);
                            pick(ui, "Back up…", CardAction::Backup);
                            pick(ui, "Restore from backup…", CardAction::Restore);
                            ui.separator();
                            pick(ui, "Delete…", CardAction::Delete);
                        });
                    });
                });
            });
        action
    }

    fn card_action(&mut self, ctx: &egui::Context, key: InstanceKey, action: CardAction) {
        match action {
            CardAction::Launch => self.launch(&key),
            CardAction::Stop => self.stop(&key),
            CardAction::Edit => self.open_edit_form(&key),
            CardAction::Duplicate => {
                let name = self.instance(&key).map(|i| format!("{} copy", i.name)).unwrap_or_default();
                self.dialog = Some(Dialog::Duplicate { key, name, working: false, error: None });
            }
            CardAction::OpenFolder => {
                if let Some(inst) = self.instance(&key) {
                    let dir = inst.resolved_data_dir(self.mgr.paths());
                    self.open_folder(dir);
                }
            }
            CardAction::ClearCache => {
                if self.running_keys().contains(&key) {
                    self.toasts.push(super::style::ToastKind::Warning, "Stop the instance before clearing its cache.");
                } else {
                    self.clear_cache(&key);
                }
            }
            CardAction::Backup => self.backup_instance(&key),
            CardAction::Restore => self.pick(PickPurpose::RestoreInto(key), None),
            CardAction::CopyCommand => self.copy_launch_command(ctx, &key),
            CardAction::RefreshUsage => self.refresh_disk_usage(&key),
            CardAction::Delete => {
                if let Some(inst) = self.instance(&key) {
                    self.dialog = Some(Dialog::Delete(super::model::DeleteConfirm::new(inst, self.mgr.paths())));
                }
            }
        }
    }
}

/// A framed warning or error message.
pub fn warning_box(ui: &mut egui::Ui, error: bool, text: &str) {
    message_box(ui, error, if error { "Error" } else { "Note" }, text);
}

/// A red box for destructive actions ("Warning"), as opposed to actual errors.
pub fn danger_box(ui: &mut egui::Ui, text: &str) {
    message_box(ui, true, "Warning", text);
}

fn message_box(ui: &mut egui::Ui, red: bool, title: &str, text: &str) {
    let error = red;
    let color = if error { ui.visuals().error_fg_color } else { ui.visuals().warn_fg_color };
    egui::Frame::new().stroke(egui::Stroke::new(1.0, color)).corner_radius(8).inner_margin(Margin::same(10)).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(title).strong().color(color));
                ui.label(text);
            });
        },
    );
}

/// A small filled (running) or hollow (stopped) circle. Painted rather than drawn with a
/// glyph, because the bundled fonts have no circle characters.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32, filled: bool) {
    let size = Vec2::splat(ui.text_style_height(&egui::TextStyle::Body));
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let r = size.x * 0.28;
    if filled {
        ui.painter().circle_filled(rect.center(), r, color);
    } else {
        ui.painter().circle_stroke(rect.center(), r, egui::Stroke::new(1.5, color));
    }
}

/// One full-width, keyboard-focusable row in the app list.
fn app_row(
    ui: &mut egui::Ui,
    icon: Image<'static>,
    name: &str,
    sub: &str,
    selected: bool,
    dim: bool,
) -> egui::Response {
    let height = 48.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, name));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let pal = style::palette(ui);
    let painter = ui.painter_at(rect);
    let fill = if selected {
        pal.row_selected
    } else if resp.hovered() || resp.has_focus() {
        pal.row_hover
    } else {
        egui::Color32::TRANSPARENT
    };
    painter.rect_filled(rect, 8, fill);
    if selected {
        let bar = egui::Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 10.0), Vec2::new(3.0, height - 20.0));
        painter.rect_filled(bar, 2, pal.accent);
    }
    if resp.has_focus() {
        painter.rect_stroke(rect.shrink(1.0), 8, egui::Stroke::new(1.5, pal.accent), egui::StrokeKind::Inside);
    }
    let icon_rect = egui::Rect::from_center_size(rect.left_center() + Vec2::new(26.0, 0.0), Vec2::splat(30.0));
    icon.paint_at(ui, icon_rect);
    let text_x = rect.left() + 50.0;
    let visuals = ui.visuals();
    let mut name_color = if selected { visuals.strong_text_color() } else { visuals.text_color() };
    if dim {
        name_color = visuals.weak_text_color();
    }
    let body = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::TextStyle::Small.resolve(ui.style());
    let clip = painter.with_clip_rect(rect.shrink2(Vec2::new(6.0, 0.0)));
    if sub.is_empty() {
        clip.text(egui::pos2(text_x, rect.center().y), Align2::LEFT_CENTER, name, body, name_color);
    } else {
        clip.text(egui::pos2(text_x, rect.center().y + 1.0), Align2::LEFT_BOTTOM, name, body, name_color);
        clip.text(egui::pos2(text_x, rect.center().y + 2.0), Align2::LEFT_TOP, sub, small, visuals.weak_text_color());
    }
    resp
}
