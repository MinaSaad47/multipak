//! Look and feel, plus the toast notifications.

use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, FontId, Margin, RichText, Stroke, TextStyle, Vec2};

/// Colors used by Multipak's own surfaces (side panel, cards), per theme.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub accent: Color32,
    pub accent_strong: Color32,
    pub side_fill: Color32,
    pub card_fill: Color32,
    pub card_stroke: Color32,
    pub row_hover: Color32,
    pub row_selected: Color32,
}

pub fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

const DARK: Palette = Palette {
    accent: Color32::from_rgb(0x4f, 0x8f, 0xea),
    accent_strong: Color32::from_rgb(0x35, 0x74, 0xd6),
    side_fill: Color32::from_rgb(0x17, 0x19, 0x1e),
    card_fill: Color32::from_rgb(0x25, 0x28, 0x30),
    card_stroke: Color32::from_rgb(0x33, 0x37, 0x42),
    row_hover: Color32::from_rgb(0x24, 0x27, 0x2f),
    row_selected: Color32::from_rgb(0x22, 0x33, 0x4f),
};

const LIGHT: Palette = Palette {
    accent: Color32::from_rgb(0x2f, 0x6f, 0xd6),
    accent_strong: Color32::from_rgb(0x24, 0x5e, 0xc0),
    side_fill: Color32::from_rgb(0xec, 0xee, 0xf2),
    card_fill: Color32::from_rgb(0xff, 0xff, 0xff),
    card_stroke: Color32::from_rgb(0xdc, 0xe0, 0xe6),
    row_hover: Color32::from_rgb(0xe1, 0xe5, 0xeb),
    row_selected: Color32::from_rgb(0xd6, 0xe4, 0xfa),
};

fn widget(bg: Color32, stroke: Color32, fg: Color32, radius: u8) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: CornerRadius::same(radius),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    }
}

fn dark_visuals() -> egui::Visuals {
    let p = DARK;
    let text = Color32::from_rgb(0xe6, 0xe8, 0xee);
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(0x1d, 0x20, 0x26);
    v.window_fill = Color32::from_rgb(0x23, 0x26, 0x2e);
    v.window_stroke = Stroke::new(1.0, p.card_stroke);
    v.extreme_bg_color = Color32::from_rgb(0x13, 0x15, 0x19);
    v.faint_bg_color = p.card_fill;
    v.code_bg_color = Color32::from_rgb(0x15, 0x17, 0x1c);
    v.hyperlink_color = Color32::from_rgb(0x7c, 0xb0, 0xf5);
    v.warn_fg_color = Color32::from_rgb(0xf0, 0xb4, 0x4c);
    v.error_fg_color = Color32::from_rgb(0xf2, 0x6d, 0x6d);
    v.selection.bg_fill = Color32::from_rgb(0x2b, 0x55, 0x99);
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.noninteractive = widget(v.panel_fill, p.card_stroke, text, 6);
    v.widgets.inactive = widget(Color32::from_rgb(0x2e, 0x32, 0x3c), Color32::TRANSPARENT, text, 6);
    v.widgets.hovered = widget(Color32::from_rgb(0x39, 0x3e, 0x4a), p.accent, Color32::WHITE, 6);
    v.widgets.active = widget(Color32::from_rgb(0x44, 0x4a, 0x58), p.accent, Color32::WHITE, 6);
    v.widgets.open = widget(Color32::from_rgb(0x33, 0x38, 0x44), p.card_stroke, text, 6);
    v
}

fn light_visuals() -> egui::Visuals {
    let p = LIGHT;
    let text = Color32::from_rgb(0x1f, 0x23, 0x29);
    let mut v = egui::Visuals::light();
    v.panel_fill = Color32::from_rgb(0xf6, 0xf7, 0xf9);
    v.window_fill = Color32::WHITE;
    v.window_stroke = Stroke::new(1.0, p.card_stroke);
    v.extreme_bg_color = Color32::WHITE;
    v.faint_bg_color = p.card_fill;
    v.code_bg_color = Color32::from_rgb(0xee, 0xf0, 0xf3);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = Color32::from_rgb(0xa3, 0x5c, 0x00);
    v.error_fg_color = Color32::from_rgb(0xc0, 0x1c, 0x28);
    v.selection.bg_fill = Color32::from_rgb(0xb9, 0xd2, 0xf7);
    v.selection.stroke = Stroke::new(1.0, text);
    v.widgets.noninteractive = widget(v.panel_fill, p.card_stroke, text, 6);
    v.widgets.inactive = widget(Color32::from_rgb(0xe6, 0xe9, 0xee), Color32::TRANSPARENT, text, 6);
    v.widgets.hovered = widget(Color32::from_rgb(0xdb, 0xe0, 0xe8), p.accent, text, 6);
    v.widgets.active = widget(Color32::from_rgb(0xcd, 0xd4, 0xdf), p.accent, text, 6);
    v.widgets.open = widget(Color32::from_rgb(0xe1, 0xe5, 0xeb), p.card_stroke, text, 6);
    v
}

/// Multipak's theme (dark and light variants), spacing and fonts.
pub fn apply(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, dark_visuals());
    ctx.set_visuals_of(egui::Theme::Light, light_visuals());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 7.0);
        style.spacing.button_padding = Vec2::new(12.0, 5.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.window_margin = Margin::same(18);
        style.spacing.menu_margin = Margin::same(8);
        style.text_styles.insert(TextStyle::Heading, FontId::proportional(22.0));
        style.text_styles.insert(TextStyle::Body, FontId::proportional(14.5));
        style.text_styles.insert(TextStyle::Button, FontId::proportional(14.5));
        style.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        style.text_styles.insert(TextStyle::Monospace, FontId::monospace(13.0));
        let v = &mut style.visuals;
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_shadow = egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(90) };
        v.popup_shadow = egui::Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(60) };
    });
}

pub fn success_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(0x57, 0xe3, 0x89) } else { Color32::from_rgb(0x1a, 0x7f, 0x37) }
}

pub fn accent_fill(ui: &egui::Ui) -> Color32 {
    palette(ui).accent_strong
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub text: String,
    pub created: Instant,
}

impl Toast {
    fn lifetime(&self) -> Duration {
        match self.kind {
            ToastKind::Error => Duration::from_secs(20),
            ToastKind::Warning => Duration::from_secs(10),
            _ => Duration::from_secs(5),
        }
    }
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
    next: u64,
}

/// What the user clicked on a toast.
pub enum ToastAction {
    Details(String),
}

impl Toasts {
    pub fn push(&mut self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        match kind {
            ToastKind::Error => tracing::error!("{text}"),
            ToastKind::Warning => tracing::warn!("{text}"),
            _ => tracing::info!("{text}"),
        }
        self.next += 1;
        self.items.push(Toast { id: self.next, kind, text, created: Instant::now() });
        if self.items.len() > 6 {
            self.items.remove(0);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<ToastAction> {
        let now = Instant::now();
        self.items.retain(|t| now.duration_since(t.created) < t.lifetime());
        if self.items.is_empty() {
            return None;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        let mut action = None;
        let mut dismiss = None;
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-14.0, -14.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(380.0);
                for t in &self.items {
                    let (accent, label) = match t.kind {
                        ToastKind::Info => (ui.visuals().hyperlink_color, "Info"),
                        ToastKind::Success => (success_color(ui), "Done"),
                        ToastKind::Warning => (ui.visuals().warn_fg_color, "Warning"),
                        ToastKind::Error => (ui.visuals().error_fg_color, "Error"),
                    };
                    egui::Frame::popup(ui.style())
                        .stroke(egui::Stroke::new(1.5, accent))
                        .inner_margin(Margin::same(10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(label).strong().color(accent));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("×").on_hover_text("Dismiss").clicked() {
                                        dismiss = Some(t.id);
                                    }
                                });
                            });
                            let long = t.text.len() > 160 || t.text.lines().count() > 3;
                            let shown: String = if long {
                                let first: String = t.text.lines().next().unwrap_or("").chars().take(160).collect();
                                format!("{first}…")
                            } else {
                                t.text.clone()
                            };
                            ui.label(shown);
                            if long && ui.small_button("Show details").clicked() {
                                action = Some(ToastAction::Details(t.text.clone()));
                                dismiss = Some(t.id);
                            }
                        });
                    ui.add_space(6.0);
                }
            });
        if let Some(id) = dismiss {
            self.items.retain(|t| t.id != id);
        }
        action
    }
}
