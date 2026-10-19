//! Small reusable widgets with a consistent look.

use super::theme::Palette;
use egui::{Align2, Color32, CornerRadius, FontId, Response, Sense, Stroke, Ui, Vec2};
use std::time::{Duration, Instant};

/// Square icon button with hover highlight and tooltip.
pub fn icon_button(ui: &mut Ui, icon: &str, tip: &str, selected: bool) -> Response {
    icon_button_sized(ui, icon, tip, selected, 30.0, 17.0)
}

pub fn icon_button_sized(ui: &mut Ui, icon: &str, tip: &str, selected: bool, size: f32, font: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let vis = ui.visuals();
    let accent = vis.selection.bg_fill;
    let hovered = resp.hovered();
    let bg = if selected {
        accent.gamma_multiply(0.9)
    } else if resp.is_pointer_button_down_on() {
        vis.widgets.active.weak_bg_fill
    } else if hovered {
        vis.widgets.hovered.weak_bg_fill
    } else {
        Color32::TRANSPARENT
    };
    let fg = if selected || hovered { vis.strong_text_color() } else { vis.text_color() };
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, CornerRadius::same(6), bg);
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(font), fg);
    }
    if !tip.is_empty() {
        resp.on_hover_text(tip)
    } else {
        resp
    }
}

pub fn separator_v(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 22.0), Sense::hover());
    let c = ui.visuals().widgets.noninteractive.bg_stroke.color;
    ui.painter().line_segment([rect.center_top(), rect.center_bottom()], Stroke::new(1.0, c));
}

pub fn section_title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(text.to_uppercase()).size(11.0).color(p.faint).strong());
    ui.add_space(2.0);
}

pub fn kv_row(ui: &mut Ui, p: &Palette, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(96.0, 16.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_width(96.0);
            ui.add(egui::Label::new(egui::RichText::new(key).color(p.muted).size(12.0)).truncate());
        });
        ui.add(egui::Label::new(egui::RichText::new(value).size(12.5)).wrap());
    });
}

/// Five clickable stars. Returns a new rating if clicked.
pub fn star_rating(ui: &mut Ui, rating: u8, size: f32, p: &Palette) -> Option<u8> {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 1.0;
        for i in 1..=5u8 {
            let filled = i <= rating;
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size + 4.0), Sense::click());
            let color = if filled { Color32::from_rgb(250, 196, 60) } else if resp.hovered() { p.muted } else { p.faint };
            let font = if filled { super::theme::icon_fill(size) } else { FontId::proportional(size) };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, super::icons::STAR, font, color);
            if resp.clicked() {
                out = Some(if rating == i { 0 } else { i });
            }
        }
    });
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub until: Instant,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub fn info(&mut self, t: impl Into<String>) {
        self.push(t.into(), ToastKind::Info, 2.5);
    }
    pub fn success(&mut self, t: impl Into<String>) {
        self.push(t.into(), ToastKind::Success, 2.5);
    }
    pub fn error(&mut self, t: impl Into<String>) {
        self.push(t.into(), ToastKind::Error, 6.0);
    }
    fn push(&mut self, text: String, kind: ToastKind, secs: f32) {
        self.items.retain(|t| t.text != text);
        self.items.push(Toast { text, kind, until: Instant::now() + Duration::from_secs_f32(secs) });
        if self.items.len() > 4 {
            self.items.remove(0);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, p: &Palette) {
        let now = Instant::now();
        self.items.retain(|t| t.until > now);
        if self.items.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let mut y = screen.max.y - 44.0;
        for (i, t) in self.items.iter().enumerate().rev() {
            let (icon, color) = match t.kind {
                ToastKind::Info => (super::icons::INFO, p.accent),
                ToastKind::Success => (super::icons::CHECK_CIRCLE, p.ok),
                ToastKind::Error => (super::icons::WARNING_CIRCLE, p.danger),
            };
            // Above the panels and the status bar but below floating windows and
            // dialogs, so a toast never hides a window's buttons.
            let resp = egui::Area::new(egui::Id::unique(("toast", i)))
                .order(egui::Order::Background)
                .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, y - screen.max.y))
                .interactable(true)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(p.raised)
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(14, 9))
                        .show(ui, |ui| {
                            ui.set_max_width(560.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(icon).color(color).size(16.0));
                                ui.add(egui::Label::new(egui::RichText::new(&t.text).color(p.text)).wrap());
                            });
                        })
                });
            y -= resp.response.rect.height() + 8.0;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

/// Simple modal-like text prompt state (rename, new folder, ...).
pub struct Prompt {
    pub title: String,
    pub value: String,
    pub hint: String,
    pub focus_requested: bool,
    /// Shown under the field (e.g. "a file with that name exists"); the prompt stays open.
    pub error: Option<String>,
}

impl Prompt {
    pub fn new(title: &str, value: &str, hint: &str) -> Self {
        Prompt { title: title.into(), value: value.into(), hint: hint.into(), focus_requested: false, error: None }
    }
}

/// Returns Some(true)=OK, Some(false)=Cancel, None=still open.
pub fn show_prompt(ctx: &egui::Context, prompt: &mut Prompt) -> Option<bool> {
    let mut result = None;
    egui::Modal::new(egui::Id::unique("prompt")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading(&prompt.title);
        ui.add_space(6.0);
        let te = ui.add(egui::TextEdit::singleline(&mut prompt.value).hint_text(&prompt.hint).desired_width(f32::INFINITY));
        if !prompt.focus_requested {
            te.request_focus();
            // Select the base name (without extension) like Explorer does.
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), te.id) {
                let end = prompt.value.rfind('.').filter(|&i| i > 0).unwrap_or(prompt.value.len());
                let range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(prompt.value[..end].chars().count()));
                state.cursor.set_char_range(Some(range));
                state.store(ui.ctx(), te.id);
            }
            prompt.focus_requested = true;
        } else if prompt.error.is_some() && !te.has_focus() && !te.lost_focus() {
            te.request_focus();
        }
        if te.changed() {
            prompt.error = None;
        }
        if te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            result = Some(true);
        }
        if let Some(e) = &prompt.error {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(e).color(ui.visuals().error_fg_color).size(12.5));
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    result = Some(false);
                }
                if ui.add(egui::Button::new("OK").fill(ui.visuals().selection.bg_fill)).clicked() {
                    result = Some(true);
                }
            });
        });
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(false);
        }
    });
    result
}

/// Confirmation dialog. Returns Some(true) confirm / Some(false) cancel.
pub fn show_confirm(ctx: &egui::Context, title: &str, body: &str, confirm: &str, danger: bool, p: &Palette) -> Option<bool> {
    let mut result = None;
    egui::Modal::new(egui::Id::unique(("confirm", title))).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.heading(title);
        ui.add_space(6.0);
        ui.label(body);
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Cancel").clicked() {
                result = Some(false);
            }
            let fill = if danger { p.danger } else { p.accent };
            if ui.add(egui::Button::new(egui::RichText::new(confirm).color(Color32::WHITE)).fill(fill)).clicked() {
                result = Some(true);
            }
        });
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(false);
        }
        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            result = Some(true);
        }
    });
    result
}
