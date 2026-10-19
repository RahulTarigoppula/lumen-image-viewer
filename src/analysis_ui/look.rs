//! Small drawing helpers shared by the Analysis panel's tabs, so the
//! Metrics and AI tabs share one compact look. They follow the proposed
//! design tokens (docs: design review, section 1): type scale 15 / 13 /
//! 11.5 / 11, 28 pt controls, radius 6 for controls and 8 for cards, one
//! filled accent button per surface. `segmented`, `primary` and `section`
//! are local stand-ins for the shared `widgets` helpers and can be swapped
//! for them one to one.

use egui::{Align2, Color32, CornerRadius, FontId, Margin, Rect, Response, RichText, Sense, Shadow, Stroke, Ui, Vec2};

use super::summary::Level;
use crate::ui::icons as I;
use crate::ui::theme::{semibold, Palette};

/// Height of a primary button, a combo row and the segmented control.
pub const CONTROL_H: f32 = 28.0;
/// Type scale: panel title, section and card headers, body, small, caption.
pub const TITLE: f32 = 15.0;
pub const SUBTITLE: f32 = 13.0;
pub const BODY: f32 = 13.0;
pub const SMALL: f32 = 11.5;
pub const CAPTION: f32 = 11.0;

pub fn level_color(p: &Palette, l: Level) -> Color32 {
    match l {
        Level::Good => p.ok,
        Level::Fair => p.warn,
        Level::Poor => p.danger,
        Level::Info => p.faint,
    }
}

/// What a status dot means, for its tooltip.
pub fn level_text(l: Level) -> &'static str {
    match l {
        Level::Good => "Looks fine",
        Level::Fair => "Worth a look",
        Level::Poor => "Likely a problem",
        Level::Info => "For information",
    }
}

/// The primary button fill: the accent a quarter of the way to black, so
/// white text on it is readable (the `accent_fill` token).
pub fn accent_fill(p: &Palette) -> Color32 {
    let a = p.accent;
    let k = |c: u8| (c as f32 * 0.75).round() as u8;
    Color32::from_rgb(k(a.r()), k(a.g()), k(a.b()))
}

/// The raised card the results sit in.
pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A row of equal segments filling the width; returns the one clicked.
/// `items`: label, enabled, hover text (the disabled reason when disabled).
/// The container is sunken, the selected segment raised.
pub fn segmented(ui: &mut Ui, p: &Palette, id: &str, items: &[(&str, bool, &str)], selected: Option<usize>) -> Option<usize> {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, CONTROL_H), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(6), p.bg);
    let seg = rect.width() / items.len().max(1) as f32;
    let mut clicked = None;
    for (i, (label, enabled, tip)) in items.iter().enumerate() {
        let r = Rect::from_min_size(rect.min + Vec2::new(seg * i as f32, 0.0), Vec2::new(seg, rect.height())).shrink(2.0);
        let resp = ui.interact(r, egui::Id::unique((id, i)), if *enabled { Sense::click() } else { Sense::hover() });
        let on = selected == Some(i);
        if on {
            let shadow = Shadow { offset: [0, 1], blur: 4, spread: 0, color: Color32::from_black_alpha(if p.dark { 90 } else { 30 }) };
            ui.painter().add(shadow.as_shape(r, CornerRadius::same(5)));
            ui.painter().rect_filled(r, CornerRadius::same(5), p.raised);
        } else if resp.hovered() && *enabled {
            ui.painter().rect_filled(r, CornerRadius::same(5), p.hover.gamma_multiply(0.6));
        }
        let color = match (*enabled, on) {
            (false, _) => p.faint,
            (true, true) => p.text,
            (true, false) => p.muted,
        };
        let font = if on { semibold(BODY) } else { FontId::proportional(BODY) };
        let g = ui.painter().layout_no_wrap(label.to_string(), font, color);
        let pos = r.center() - g.size() / 2.0;
        ui.painter().with_clip_rect(r).galley(pos, g, color);
        let resp = if tip.is_empty() { resp } else { resp.on_hover_text(*tip) };
        if resp.clicked() && *enabled {
            clicked = Some(i);
        }
    }
    clicked
}

/// The one filled accent button of a tab, `width` wide.
pub fn primary(ui: &mut Ui, p: &Palette, text: &str, enabled: bool, width: f32) -> Response {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(text).font(semibold(BODY)).color(Color32::WHITE)).fill(accent_fill(p)).min_size(Vec2::new(width, CONTROL_H)))
}

/// A small status dot with what it means on hover.
pub fn dot(ui: &mut Ui, color: Color32, meaning: &str) -> Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::new(12.0, 16.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 3.5, color);
    if meaning.is_empty() {
        resp
    } else {
        resp.on_hover_text(meaning)
    }
}

/// One summary line: a dot, the label and a muted value on the right; the
/// label wraps when the panel is narrow. On hover: what the dot means, then
/// `tip`.
pub fn status_row(ui: &mut Ui, p: &Palette, level: Level, label: &str, value: &str, tip: &str, dim: bool) -> Response {
    let w = ui.available_width();
    let text = if dim { p.faint } else { p.text };
    let vg = ui.painter().layout_no_wrap(value.to_string(), FontId::proportional(SMALL), p.muted);
    let label_w = (w - 20.0 - vg.size().x - 10.0).max(60.0);
    let lg = ui.painter().layout(label.to_string(), FontId::proportional(BODY), text, label_w);
    let h = (lg.size().y + 6.0).max(22.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    if resp.hovered() {
        ui.painter().rect_filled(rect.expand2(Vec2::new(4.0, 0.0)), CornerRadius::same(6), p.hover.gamma_multiply(0.6));
    }
    let first_line = rect.min.y + 3.0 + lg.rows.first().map(|r| r.height()).unwrap_or(16.0) / 2.0;
    ui.painter().circle_filled(egui::pos2(rect.min.x + 5.0, first_line), 3.5, if dim { p.faint } else { level_color(p, level) });
    ui.painter().galley(egui::pos2(rect.min.x + 18.0, rect.min.y + 3.0), lg, text);
    ui.painter().text(egui::pos2(rect.max.x, first_line), Align2::RIGHT_CENTER, value, FontId::proportional(SMALL), p.muted);
    let hover = if dim { format!("Out of date: analyze again.\n{tip}") } else { format!("{}. {tip}", level_text(level)) };
    resp.on_hover_text(hover)
}

/// A collapsible section header in the panel's style, with a caret icon
/// instead of egui's filled triangle.
pub fn section(title: impl Into<String>, p: &Palette) -> egui::CollapsingHeader {
    egui::CollapsingHeader::new(RichText::new(title.into()).font(semibold(SUBTITLE)).color(p.text)).icon(caret(p.muted))
}

/// A small collapsible group (inside a section), same caret.
pub fn group(title: RichText, p: &Palette) -> egui::CollapsingHeader {
    egui::CollapsingHeader::new(title).icon(caret(p.muted))
}

/// A 12 pt Phosphor caret, right when closed, down when open.
fn caret(color: Color32) -> impl FnOnce(&mut Ui, f32, &Response) + 'static {
    move |ui, openness, resp| {
        let icon = if openness > 0.5 { I::CARET_DOWN } else { I::CARET_RIGHT };
        ui.painter().text(resp.rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(12.0), color);
    }
}

/// Muted small print.
pub fn note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(SMALL).color(p.muted)).wrap());
}
