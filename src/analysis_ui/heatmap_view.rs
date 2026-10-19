//! Heatmap overlay painted over an image rectangle, with colormap, range,
//! opacity and threshold controls and a legend bar. Per-pixel maps are drawn
//! as a texture at map resolution (max-pooled to at most 4096 px so hotspots
//! survive); tile maps (64 px cells) are drawn as rectangles. Hovering shows
//! the map value plus the A/B pixel under the pointer.

use crate::ui::roi::ImageTransform;
use crate::ui::theme::Palette;
use egui::{Color32, ColorImage, FontId, Painter, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2};
use lumen_analysis::{heatmap, Colormap, ColorizeOptions, Heatmap, RangeMode};
use std::sync::Arc;

/// What the range controls select (the `RangeMode` values plus the user's lo/hi).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RangeKind {
    Auto,
    Fixed,
    Custom,
}

pub struct HeatmapView {
    /// Metric id of the map shown.
    id: String,
    /// Full-resolution map (hover values, export).
    full: Option<Arc<Heatmap>>,
    /// Pooled copy for drawing.
    disp: Option<Arc<Heatmap>>,
    pub visible: bool,
    pub colormap: Colormap,
    range: RangeKind,
    user: (f32, f32),
    pub opacity: f32,
    threshold_on: bool,
    threshold: f32,
    /// Texture (per-pixel maps) or cell colors (tile maps) for `drawn`.
    tex: Option<TextureHandle>,
    cells: Vec<Color32>,
    drawn: Option<ColorizeOptions>,
}

impl Default for HeatmapView {
    fn default() -> Self {
        HeatmapView {
            id: String::new(),
            full: None,
            disp: None,
            visible: true,
            colormap: Colormap::Magma,
            range: RangeKind::Auto,
            user: (0.0, 1.0),
            opacity: 0.65,
            threshold_on: false,
            threshold: 0.0,
            tex: None,
            cells: Vec::new(),
            drawn: None,
        }
    }
}

/// True for maps whose cells are larger than a pixel (NR tile maps).
pub fn is_tile_map(m: &Heatmap) -> bool {
    m.cell_w > 1.5 || m.cell_h > 1.5
}

impl HeatmapView {
    /// Show `map` (the result of metric `id`), keeping the user's settings
    /// when it is the same metric again.
    pub fn set_map(&mut self, id: &str, map: Heatmap) {
        let same = self.id == id;
        let disp = if is_tile_map(&map) { map.clone() } else { map.for_display() };
        if !same {
            self.colormap = map.colormap;
            self.range = RangeKind::Auto;
            self.threshold_on = false;
            self.user = disp.range(RangeMode::Auto);
            self.threshold = (self.user.0 + self.user.1) / 2.0;
        }
        self.id = id.to_string();
        self.full = Some(Arc::new(map));
        self.disp = Some(Arc::new(disp));
        self.drawn = None;
    }

    pub fn clear(&mut self) {
        self.id.clear();
        self.full = None;
        self.disp = None;
        self.tex = None;
        self.cells.clear();
        self.drawn = None;
    }

    pub fn id(&self) -> Option<&str> {
        self.full.as_ref().map(|_| self.id.as_str())
    }

    pub fn map(&self) -> Option<&Heatmap> {
        self.full.as_deref()
    }

    /// Whether an overlay will be drawn.
    pub fn active(&self) -> bool {
        self.visible && self.full.is_some()
    }

    /// The coloring the controls describe (opaque; opacity is applied when drawing).
    pub fn options(&self) -> ColorizeOptions {
        let invert = self.full.as_ref().is_some_and(|m| m.invert);
        let range = match self.range {
            RangeKind::Auto => RangeMode::Auto,
            RangeKind::Fixed => RangeMode::Fixed,
            RangeKind::Custom => RangeMode::User { lo: self.user.0, hi: self.user.1 },
        };
        ColorizeOptions { colormap: self.colormap, range, invert, threshold: self.threshold_on.then_some(self.threshold), alpha: 255 }
    }

    /// Value range the current options color.
    pub fn value_range(&self) -> Option<(f32, f32)> {
        let m = self.disp.as_ref()?;
        Some(m.range(self.options().range))
    }

    /// Colorized PNG of the full map with the current settings.
    pub fn export_png(&self, path: &std::path::Path) -> std::io::Result<()> {
        let m = self.full.as_ref().ok_or_else(|| std::io::Error::other("no heatmap"))?;
        let o = ColorizeOptions { alpha: (self.opacity.clamp(0.0, 1.0) * 255.0).round().max(1.0) as u8, ..self.options() };
        m.write_png(path, &o)
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        let o = self.options();
        if self.drawn == Some(o) {
            return;
        }
        let Some(m) = self.disp.clone() else { return };
        let rgba = m.colorize(&o);
        if is_tile_map(&m) {
            self.tex = None;
            self.cells = rgba.as_chunks::<4>().0.iter().map(|c| Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])).collect();
        } else {
            self.cells.clear();
            let img = ColorImage::from_rgba_unmultiplied([m.width as usize, m.height as usize], &rgba);
            match &mut self.tex {
                Some(t) => t.set(img, TextureOptions::NEAREST),
                None => self.tex = Some(ctx.load_texture("analysis-heatmap", img, TextureOptions::NEAREST)),
            }
        }
        self.drawn = Some(o);
    }

    /// Draw the overlay over the image that `xf` maps to the screen, clipped
    /// to it. With the pointer at `hover`, show the map value there and the
    /// A/B pixels `pixels(x, y)` returns for image coordinates.
    pub fn paint(&mut self, ui: &Ui, painter: &Painter, xf: &ImageTransform, hover: Option<Pos2>, pixels: &dyn Fn(f32, f32) -> (Option<[u8; 4]>, Option<[u8; 4]>)) {
        if !self.active() {
            return;
        }
        self.refresh(ui.ctx());
        let Some(m) = self.disp.clone() else { return };
        let painter = painter.with_clip_rect(painter.clip_rect().intersect(xf.dest));
        let tint = Color32::WHITE.gamma_multiply(self.opacity.clamp(0.0, 1.0));
        let (x0, y0, w, h) = m.extent();
        if let Some(t) = &self.tex {
            let r = xf.rect_to_screen([x0, y0, x0 + w, y0 + h]);
            painter.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
        } else {
            let vis = painter.clip_rect();
            for j in 0..m.height {
                for i in 0..m.width {
                    let c = self.cells[(j * m.width + i) as usize];
                    if c.a() == 0 {
                        continue;
                    }
                    let (cx, cy) = (x0 + i as f32 * m.cell_w, y0 + j as f32 * m.cell_h);
                    let r = xf.rect_to_screen([cx, cy, cx + m.cell_w, cy + m.cell_h]);
                    if r.intersects(vis) {
                        painter.rect_filled(r, 0.0, c.gamma_multiply(self.opacity.clamp(0.0, 1.0)));
                    }
                }
            }
        }
        let Some(pos) = hover.filter(|p| xf.dest.contains(*p)) else { return };
        let ip = xf.to_image(pos);
        let full = self.full.clone().expect("active map");
        let value = full.value_at(ip.x, ip.y);
        let (pa, pb) = pixels(ip.x, ip.y);
        let mut lines = vec![match value {
            Some(v) => format!("{} {}", full.label, fmt_value(v, full.unit)),
            None => format!("{} -", full.label),
        }];
        let px = |p: [u8; 4]| format!("{} {} {}", p[0], p[1], p[2]);
        match (pa, pb) {
            (Some(a), Some(b)) => lines.push(format!("A {}   B {}", px(a), px(b))),
            (Some(a), None) => lines.push(format!("RGB {}", px(a))),
            _ => {}
        }
        lines.push(format!("x {}  y {}", ip.x.floor() as i64, ip.y.floor() as i64));
        readout(ui, &painter, pos, &lines.join("\n"));
    }

    /// Colormap, range, opacity and threshold controls and the legend.
    pub fn controls(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(m) = self.disp.clone() else { return };
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.visible, "Overlay");
            egui::ComboBox::from_id_salt("heatmap-colormap").width(90.0).selected_text(self.colormap.name()).show_ui(ui, |ui| {
                for c in Colormap::ALL {
                    ui.selectable_value(&mut self.colormap, c, c.name());
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Range").color(p.muted));
            let before = self.range;
            ui.selectable_value(&mut self.range, RangeKind::Auto, "Auto").on_hover_text("1st to 99th percentile");
            ui.selectable_value(&mut self.range, RangeKind::Fixed, "Fixed").on_hover_text("The metric's default range");
            ui.selectable_value(&mut self.range, RangeKind::Custom, "Custom");
            if before != self.range && self.range == RangeKind::Custom {
                let o = ColorizeOptions { range: if before == RangeKind::Fixed { RangeMode::Fixed } else { RangeMode::Auto }, ..self.options() };
                self.user = m.range(o.range);
            }
        });
        let (lo, hi) = m.range(self.options().range);
        let speed = ((hi - lo).abs() / 200.0).max(1e-4) as f64;
        if self.range == RangeKind::Custom {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.user.0).speed(speed).max_decimals(4).prefix("lo "));
                ui.add(egui::DragValue::new(&mut self.user.1).speed(speed).max_decimals(4).prefix("hi "));
            });
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Opacity").color(p.muted));
            ui.add(egui::Slider::new(&mut self.opacity, 0.0..=1.0).show_value(false));
        });
        ui.horizontal(|ui| {
            let tip = if m.invert { "Show only values at or below" } else { "Show only values at or above" };
            ui.checkbox(&mut self.threshold_on, "Threshold").on_hover_text(tip);
            if self.threshold_on {
                ui.add(egui::DragValue::new(&mut self.threshold).speed(speed).max_decimals(4));
            }
        });
        legend(ui, p, self.colormap, m.invert, (lo, hi), m.unit, self.threshold_on.then_some(self.threshold));
    }
}

/// Value with its unit for readouts.
pub fn fmt_value(v: f32, unit: Option<lumen_analysis::Unit>) -> String {
    let d = unit.map(|u| u.decimals().min(4)).unwrap_or(4);
    let label = unit.map(|u| u.label()).unwrap_or("");
    if label.is_empty() {
        format!("{v:.d$}")
    } else {
        format!("{v:.d$} {label}")
    }
}

/// A horizontal colormap bar with its value range (and the threshold mark).
pub fn legend(ui: &mut Ui, p: &Palette, cm: Colormap, invert: bool, (lo, hi): (f32, f32), unit: Option<lumen_analysis::Unit>, threshold: Option<f32>) {
    let w = ui.available_width().clamp(80.0, 320.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 12.0), Sense::hover());
    let colors = heatmap::legend(cm, 128, invert);
    let step = rect.width() / colors.len() as f32;
    let painter = ui.painter();
    for (i, c) in colors.iter().enumerate() {
        let x = rect.min.x + i as f32 * step;
        painter.rect_filled(Rect::from_min_max(Pos2::new(x, rect.min.y), Pos2::new(x + step + 0.5, rect.max.y)), 0.0, Color32::from_rgb(c[0], c[1], c[2]));
    }
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, p.border), StrokeKind::Outside);
    if let Some(t) = threshold.filter(|_| hi > lo) {
        let x = rect.min.x + ((t - lo) / (hi - lo)).clamp(0.0, 1.0) * rect.width();
        painter.line_segment([Pos2::new(x, rect.min.y - 2.0), Pos2::new(x, rect.max.y + 2.0)], Stroke::new(2.0, p.text));
    }
    ui.horizontal(|ui| {
        ui.set_width(w);
        ui.label(RichText::new(fmt_value(lo, unit)).size(11.0).color(p.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(fmt_value(hi, unit)).size(11.0).color(p.muted));
        });
    });
}

/// A small text box next to the pointer.
pub fn readout(ui: &Ui, painter: &Painter, pos: Pos2, text: &str) {
    let galley = painter.layout_no_wrap(text.to_string(), FontId::monospace(12.0), Color32::WHITE);
    let pad = Vec2::new(6.0, 4.0);
    let size = galley.size() + pad * 2.0;
    let screen = ui.ctx().content_rect();
    let mut at = pos + Vec2::new(16.0, 16.0);
    if at.x + size.x > screen.max.x {
        at.x = pos.x - 16.0 - size.x;
    }
    if at.y + size.y > screen.max.y {
        at.y = pos.y - 16.0 - size.y;
    }
    let r = Rect::from_min_size(at, size);
    let painter = painter.with_clip_rect(screen);
    painter.rect_filled(r, 4.0, Color32::from_black_alpha(210));
    painter.galley(r.min + pad, galley, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(w: usize, h: usize) -> Heatmap {
        Heatmap::new(w, h, (0..w * h).map(|i| i as f32).collect(), "abs diff")
    }

    #[test]
    fn settings_survive_the_same_metric() {
        let mut v = HeatmapView::default();
        v.set_map("diff.abs", map(10, 10));
        assert_eq!(v.id(), Some("diff.abs"));
        v.colormap = Colormap::Turbo;
        v.threshold_on = true;
        v.set_map("diff.abs", map(10, 10));
        assert_eq!((v.colormap, v.options().threshold.is_some()), (Colormap::Turbo, true));
        // Another metric starts from its own defaults.
        v.set_map("deltae.2000", map(4, 4));
        assert_eq!((v.colormap, v.options().threshold), (Colormap::Magma, None));
        v.clear();
        assert!(!v.active());
    }

    #[test]
    fn tile_maps_are_detected() {
        assert!(!is_tile_map(&map(3, 3)));
        assert!(is_tile_map(&map(3, 3).with_cell(64.0, 64.0)));
    }

    #[test]
    fn custom_range_and_values() {
        let mut v = HeatmapView::default();
        v.set_map("diff.abs", map(10, 10));
        v.range = RangeKind::Custom;
        v.user = (5.0, 50.0);
        assert_eq!(v.value_range(), Some((5.0, 50.0)));
        assert_eq!(fmt_value(1.23456, Some(lumen_analysis::Unit::DeltaE)), "1.2346 dE");
        assert_eq!(fmt_value(0.5, None), "0.5000");
    }
}
