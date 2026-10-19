//! Drawing helpers only the Analysis panel's tabs need (the result card,
//! status dots and rows, small print), so the Metrics and AI tabs share one
//! look. Everything else (buttons, segmented controls, carets, sliders, the
//! type scale and control sizes) comes from `ui::widgets` and `ui::theme`.

use egui::{Align2, Color32, CornerRadius, Margin, Rect, Response, RichText, Sense, Stroke, Ui, Vec2};

use super::summary::Level;
use crate::ui::theme::{radius, text, Palette};

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

/// The raised card the results sit in.
pub fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
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
/// `tip`. `link` ("Show where") is a small text link between the label and
/// the value. Returns the row and whether the link was clicked.
#[allow(clippy::too_many_arguments)]
pub fn status_row(ui: &mut Ui, p: &Palette, level: Level, label: &str, value: &str, tip: &str, dim: bool, link: Option<&str>) -> (Response, bool) {
    let w = ui.available_width();
    let text = if dim { p.faint } else { p.text };
    let vg = ui.painter().layout_no_wrap(value.to_string(), text::small(), p.muted);
    let link_g = link.map(|l| ui.painter().layout_no_wrap(l.to_string(), text::small(), p.accent_text));
    let link_w = link_g.as_ref().map(|g| g.size().x + 12.0).unwrap_or(0.0);
    let label_w = (w - 20.0 - vg.size().x - 10.0 - link_w).max(60.0);
    let lg = ui.painter().layout(label.to_string(), text::body(), text, label_w);
    let h = (lg.size().y + 6.0).max(22.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    let first_line = rect.min.y + 3.0 + lg.rows.first().map(|r| r.height()).unwrap_or(16.0) / 2.0;
    // The link is its own target, right of the label, before the value.
    let link = link_g.map(|g| {
        let r = Rect::from_min_size(egui::pos2(rect.max.x - vg.size().x - 10.0 - g.size().x, first_line - g.size().y / 2.0), g.size()).expand2(Vec2::new(4.0, 2.0));
        (g, r, ui.interact(r, resp.id.with("link"), Sense::click()))
    });
    let over_link = link.as_ref().is_some_and(|l| l.2.hovered());
    if resp.hovered() || over_link {
        ui.painter().rect_filled(rect.expand2(Vec2::new(4.0, 0.0)), CornerRadius::same(6), p.hover.gamma_multiply(0.6));
    }
    let mut clicked = false;
    if let Some((g, r, lr)) = link {
        clicked = lr.clicked();
        ui.painter().galley(r.min + Vec2::new(4.0, 2.0), g, p.accent_text);
        if lr.hovered() {
            ui.painter().hline(r.x_range().shrink(4.0), r.max.y - 1.0, Stroke::new(1.0, p.accent_text));
        }
        lr.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    ui.painter().circle_filled(egui::pos2(rect.min.x + 5.0, first_line), 3.5, if dim { p.faint } else { level_color(p, level) });
    ui.painter().galley(egui::pos2(rect.min.x + 18.0, rect.min.y + 3.0), lg, text);
    ui.painter().text(egui::pos2(rect.max.x, first_line), Align2::RIGHT_CENTER, value, text::small(), p.muted);
    let hover = if dim { format!("Out of date: analyze again.\n{tip}") } else { format!("{}. {tip}", level_text(level)) };
    (resp.on_hover_text(hover), clicked)
}

/// The badge color of compare pane `i` (A, B, C, D), as compare mode draws them.
pub fn pane_color(p: &Palette, i: usize) -> Color32 {
    [p.accent, Color32::from_rgb(232, 120, 60), Color32::from_rgb(80, 180, 120), Color32::from_rgb(170, 110, 230)][i % 4]
}

/// Muted small print.
pub fn note(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(text::SMALL).color(p.muted)).wrap());
}
