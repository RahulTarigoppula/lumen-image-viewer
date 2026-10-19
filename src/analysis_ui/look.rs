//! Small drawing helpers shared by the Analysis panel's tabs, so the
//! Metrics and AI tabs share one compact look built from the app's theme
//! (type scale, 6 px corners, raised cards with a thin border).

use egui::{Align2, Color32, CornerRadius, FontId, Margin, Rect, Response, RichText, Sense, Stroke, Ui, Vec2};

use super::summary::Level;
use crate::ui::theme::{semibold, Palette};

/// Height of a primary button and of the segmented control.
pub const CONTROL_H: f32 = 28.0;

pub fn level_color(p: &Palette, l: Level) -> Color32 {
    match l {
        Level::Good => p.ok,
        Level::Fair => p.warn,
        Level::Poor => p.danger,
        Level::Info => p.faint,
    }
}

/// The raised card the results sit in.
pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A row of equal segments filling the width; returns the one clicked.
/// `items`: label, enabled, hover text (the disabled reason when disabled).
pub fn segmented(ui: &mut Ui, p: &Palette, id: &str, items: &[(&str, bool, &str)], selected: Option<usize>) -> Option<usize> {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, CONTROL_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(6), p.bg);
    painter.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    let seg = rect.width() / items.len().max(1) as f32;
    let mut clicked = None;
    for (i, (label, enabled, tip)) in items.iter().enumerate() {
        let r = Rect::from_min_size(rect.min + Vec2::new(seg * i as f32, 0.0), Vec2::new(seg, rect.height())).shrink(2.0);
        let resp = ui.interact(r, egui::Id::unique((id, i)), if *enabled { Sense::click() } else { Sense::hover() });
        let on = selected == Some(i);
        let fill = if on {
            p.hover
        } else if resp.hovered() && *enabled {
            p.raised
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect_filled(r, CornerRadius::same(5), fill);
        let color = match (*enabled, on) {
            (false, _) => p.faint,
            (true, true) => p.text,
            (true, false) => p.muted,
        };
        let font = if on { semibold(12.5) } else { FontId::proportional(12.5) };
        let g = ui.painter().layout_no_wrap(label.to_string(), font, color);
        let pos = r.center() - g.size() / 2.0;
        ui.painter().with_clip_rect(r).galley(pos, g, color);
        if !tip.is_empty() {
            let resp = resp.on_hover_text(*tip);
            if resp.clicked() && *enabled {
                clicked = Some(i);
            }
        } else if resp.clicked() && *enabled {
            clicked = Some(i);
        }
    }
    clicked
}

/// The accent-filled primary button of a tab, `width` wide.
pub fn primary(ui: &mut Ui, p: &Palette, text: &str, enabled: bool, width: f32) -> Response {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(text).font(semibold(13.0)).color(Color32::WHITE)).fill(p.accent).min_size(Vec2::new(width, CONTROL_H)))
}

/// A small status dot.
pub fn dot(ui: &mut Ui, color: Color32) -> Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::new(12.0, 16.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 3.5, color);
    resp
}

/// One summary line: a dot, the label and a muted value on the right; the
/// label wraps when the panel is narrow. The tip shows on hover.
pub fn status_row(ui: &mut Ui, p: &Palette, color: Color32, label: &str, value: &str, tip: &str, dim: bool) -> Response {
    let w = ui.available_width();
    let text = if dim { p.faint } else { p.text };
    let vg = ui.painter().layout_no_wrap(value.to_string(), FontId::proportional(12.0), p.muted);
    let label_w = (w - 20.0 - vg.size().x - 10.0).max(60.0);
    let lg = ui.painter().layout(label.to_string(), FontId::proportional(13.0), text, label_w);
    let h = (lg.size().y + 6.0).max(22.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    if resp.hovered() && !tip.is_empty() {
        ui.painter().rect_filled(rect.expand2(Vec2::new(4.0, 0.0)), CornerRadius::same(4), p.hover.gamma_multiply(0.7));
    }
    let first_line = rect.min.y + 3.0 + lg.rows.first().map(|r| r.height()).unwrap_or(16.0) / 2.0;
    ui.painter().circle_filled(egui::pos2(rect.min.x + 5.0, first_line), 3.5, if dim { p.faint } else { color });
    ui.painter().galley(egui::pos2(rect.min.x + 18.0, rect.min.y + 3.0), lg, text);
    ui.painter().text(egui::pos2(rect.max.x, first_line), Align2::RIGHT_CENTER, value, FontId::proportional(12.0), p.muted);
    if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    }
}

/// A collapsible section header in the panel's style.
pub fn section(title: impl Into<String>, p: &Palette) -> egui::CollapsingHeader {
    egui::CollapsingHeader::new(RichText::new(title.into()).font(semibold(12.5)).color(p.text))
}

/// Muted small print.
pub fn note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(11.5).color(p.muted)).wrap());
}
