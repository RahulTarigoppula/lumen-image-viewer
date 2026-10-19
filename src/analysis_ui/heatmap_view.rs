//! Heatmap overlay painted over an image rectangle, with colormap, range,
//! opacity and threshold controls and a legend bar. Per-pixel maps are drawn
//! as textures of at most 2048 px (max-pooled, so hotspots survive); tile
//! maps (64 px cells) are drawn as rectangles. Hovering shows the map value
//! plus the A/B pixel under the pointer.
//!
//! Pooling, the auto range and coloring scan every value of the map (12 M
//! for a 12 MP image), so they run on a worker thread whenever the map or
//! the coloring changes. The UI thread only uploads the result, in strips of
//! [`STRIP_ROWS`] rows, a few per frame: one 12 MB upload takes about 30 ms.
//! Maps of rotated videos are in the stream's native pixels and are drawn
//! turned like the picture.

use crate::ui::roi::ImageTransform;
use crate::ui::theme::Palette;
use crossbeam_channel::{Receiver, TryRecvError};
use egui::{Color32, ColorImage, FontId, Painter, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2};
use lumen_analysis::{heatmap, Colormap, ColorizeOptions, Heatmap, RangeMode};
use std::sync::Arc;

/// Longest side of the drawn texture. Bigger maps are max-pooled to it
/// (12 MP: 2000 x 1500 cells, a 12 MB upload instead of 48 MB).
const OVERLAY_SIDE: u32 = 2048;
/// Rows per overlay texture (2 MB at 2048 px wide).
const STRIP_ROWS: usize = 256;
/// Bytes of texture uploaded per frame (at least one strip).
const UPLOAD_BUDGET: usize = 2 << 20;

/// What the range controls select (the `RangeMode` values plus the user's lo/hi).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RangeKind {
    Auto,
    Fixed,
    Custom,
}

/// A worker's coloring of the map for one set of options.
struct Prepared {
    disp: Arc<Heatmap>,
    auto: (f32, f32),
    opts: ColorizeOptions,
    /// Per-pixel maps, in strips of `STRIP_ROWS` rows.
    strips: Vec<ColorImage>,
    /// Tile maps.
    cells: Vec<Color32>,
}

pub struct HeatmapView {
    /// Metric id of the map shown.
    id: String,
    /// Full-resolution map (hover values, export).
    full: Option<Arc<Heatmap>>,
    /// Pooled copy for drawing and its auto range, once the worker made them.
    disp: Option<Arc<Heatmap>>,
    auto: Option<(f32, f32)>,
    pub visible: bool,
    pub colormap: Colormap,
    range: RangeKind,
    user: (f32, f32),
    pub opacity: f32,
    threshold_on: bool,
    threshold: f32,
    /// A new map: the custom range and threshold start from its auto range.
    fresh: bool,
    /// Textures of the strips (per-pixel maps; `true`: it shows the current
    /// map, else it waits to be reused) or cell colors (tile maps) for
    /// `drawn`; strips still to upload wait in `uploads`. Textures are kept
    /// from map to map: creating one costs more than refilling it.
    strips: Vec<Option<(TextureHandle, bool)>>,
    uploads: Vec<(usize, ColorImage)>,
    cells: Vec<Color32>,
    drawn: Option<ColorizeOptions>,
    /// The coloring in flight, at most one.
    pending: Option<(ColorizeOptions, Receiver<Prepared>)>,
    /// Video frames are measured in native pixels and shown turned (and
    /// maybe scaled): the display rotation in degrees and the native size.
    native: Option<(i32, (f32, f32))>,
}

impl Default for HeatmapView {
    fn default() -> Self {
        HeatmapView {
            id: String::new(),
            full: None,
            disp: None,
            auto: None,
            visible: true,
            colormap: Colormap::Magma,
            range: RangeKind::Auto,
            user: (0.0, 1.0),
            opacity: 0.65,
            threshold_on: false,
            threshold: 0.0,
            fresh: false,
            strips: Vec::new(),
            uploads: Vec::new(),
            cells: Vec::new(),
            drawn: None,
            pending: None,
            native: None,
        }
    }
}

/// True for maps of a few large cells (NR tile maps), drawn as rectangles.
/// Pooled or preview maps also have cells over a pixel but millions of
/// them: those are drawn as textures.
pub fn is_tile_map(m: &Heatmap) -> bool {
    (m.cell_w > 1.5 || m.cell_h > 1.5) && m.width as usize * m.height as usize <= 1 << 16
}

/// A point of a picture of native size `(w, h)` as displayed turned by
/// `rotation` degrees (the inverse of `metrics_view::native_roi`).
pub fn rotate_point(p: Pos2, rotation: i32, (w, h): (f32, f32)) -> Pos2 {
    match rotation.rem_euclid(360) {
        90 => Pos2::new(h - p.y, p.x),
        180 => Pos2::new(w - p.x, h - p.y),
        270 => Pos2::new(p.y, w - p.x),
        _ => p,
    }
}

/// The native point under displayed point `p` (inverse of [`rotate_point`]).
pub fn unrotate_point(p: Pos2, rotation: i32, (w, h): (f32, f32)) -> Pos2 {
    match rotation.rem_euclid(360) {
        90 => Pos2::new(p.y, h - p.x),
        180 => Pos2::new(w - p.x, h - p.y),
        270 => Pos2::new(w - p.y, p.x),
        _ => p,
    }
}

impl HeatmapView {
    /// Show `map` (the result of metric `id`), keeping the user's settings
    /// when it is the same metric again.
    pub fn set_map(&mut self, id: &str, map: Arc<Heatmap>) {
        if self.id != id {
            self.colormap = map.colormap;
            self.range = RangeKind::Auto;
            self.threshold_on = false;
            self.fresh = true;
        }
        self.id = id.to_string();
        // The old full map may be the last owner of 48 MB: free it elsewhere.
        if let Some(old) = self.full.replace(map) {
            let _ = std::thread::Builder::new().name("lumen-free".into()).spawn(move || drop(old));
        }
        self.reset_drawing();
    }

    pub fn clear(&mut self) {
        self.id.clear();
        self.full = None;
        self.reset_drawing();
        self.strips.clear();
    }

    fn reset_drawing(&mut self) {
        self.disp = None;
        self.auto = None;
        for (_, current) in self.strips.iter_mut().flatten() {
            *current = false;
        }
        self.uploads.clear();
        self.cells.clear();
        self.drawn = None;
        self.pending = None;
    }

    /// The map is in native pixels of a video of size `.1` displayed turned
    /// by `.0` degrees (`None`: in the displayed image's pixels).
    pub fn set_native(&mut self, native: Option<(i32, (u32, u32))>) {
        self.native = native.map(|(r, (w, h))| (r.rem_euclid(360), (w.max(1) as f32, h.max(1) as f32)));
    }

    /// Map pixels to displayed image pixels (`size`: the displayed image).
    fn to_display(&self, p: Pos2, size: Vec2) -> Pos2 {
        let Some((rot, n)) = self.native else { return p };
        let q = rotate_point(p, rot, n);
        let turned = if rot % 180 == 90 { (n.1, n.0) } else { n };
        Pos2::new(q.x * size.x / turned.0, q.y * size.y / turned.1)
    }

    /// Displayed image pixels to map pixels (inverse of `to_display`).
    fn from_display(&self, p: Pos2, size: Vec2) -> Pos2 {
        let Some((rot, n)) = self.native else { return p };
        let turned = if rot % 180 == 90 { (n.1, n.0) } else { n };
        unrotate_point(Pos2::new(p.x * turned.0 / size.x, p.y * turned.1 / size.y), rot, n)
    }

    pub fn id(&self) -> Option<&str> {
        self.full.as_ref().map(|_| self.id.as_str())
    }

    pub fn map(&self) -> Option<&Heatmap> {
        self.full.as_deref()
    }

    /// The full map, shared (exports on a worker thread).
    pub fn shared_map(&self) -> Option<Arc<Heatmap>> {
        self.full.clone()
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

    /// The options with the range resolved to numbers (the auto range of the
    /// drawn map once known), for exports that must match the overlay.
    pub fn resolved_options(&self) -> ColorizeOptions {
        let o = self.options();
        let (lo, hi) = self.range_of(o.range);
        ColorizeOptions { range: RangeMode::User { lo, hi }, ..o }
    }

    /// `(lo, hi)` of a range mode without scanning the map.
    fn range_of(&self, mode: RangeMode) -> (f32, f32) {
        let fixed = self.full.as_ref().map(|m| m.fixed_range).unwrap_or((0.0, 1.0));
        match mode {
            RangeMode::Auto => self.auto.unwrap_or(fixed),
            RangeMode::Fixed => fixed,
            RangeMode::User { lo, hi } => (lo, hi),
        }
    }

    /// Picks up the worker's coloring and starts the next one when the map
    /// or the options changed.
    fn refresh(&mut self, ctx: &egui::Context) {
        self.upload(ctx);
        if let Some((_, rx)) = &self.pending {
            match rx.try_recv() {
                Ok(p) => {
                    self.pending = None;
                    self.apply(ctx, p);
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => self.pending = None,
            }
        }
        let o = self.options();
        if self.drawn == Some(o) {
            return;
        }
        let Some(full) = self.full.clone() else { return };
        let (disp, auto) = (self.disp.clone(), self.auto);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let repaint = ctx.clone();
        std::thread::Builder::new()
            .name("lumen-heatmap".into())
            .spawn(move || {
                lumen_analysis::job::lower_current_thread_priority();
                let disp = disp.unwrap_or_else(|| Arc::new(if is_tile_map(&full) { (*full).clone() } else { full.downsample(OVERLAY_SIDE) }));
                let auto = auto.unwrap_or_else(|| disp.range(RangeMode::Auto));
                let range = match o.range {
                    RangeMode::Auto => auto,
                    m => disp.range(m),
                };
                let rgba = disp.colorize(&ColorizeOptions { range: RangeMode::User { lo: range.0, hi: range.1 }, ..o });
                let (strips, cells) = if is_tile_map(&full) {
                    (Vec::new(), rgba.as_chunks::<4>().0.iter().map(|c| Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])).collect())
                } else {
                    let w = disp.width as usize;
                    let strips = rgba.chunks(w * 4 * STRIP_ROWS).map(|rows| ColorImage::from_rgba_unmultiplied([w, rows.len() / (w * 4)], rows)).collect();
                    (strips, Vec::new())
                };
                let _ = tx.send(Prepared { disp, auto, opts: o, strips, cells });
                repaint.request_repaint();
            })
            .ok();
        self.pending = Some((o, rx));
    }

    fn apply(&mut self, ctx: &egui::Context, p: Prepared) {
        // Recolored strips replace the old ones in place as they upload.
        self.strips.resize_with(p.strips.len(), || None);
        self.disp = Some(p.disp);
        self.auto = Some(p.auto);
        if std::mem::take(&mut self.fresh) {
            self.user = p.auto;
            self.threshold = (p.auto.0 + p.auto.1) / 2.0;
        }
        self.cells = p.cells;
        self.uploads = p.strips.into_iter().enumerate().collect();
        self.drawn = Some(p.opts);
        self.upload(ctx);
    }

    /// Uploads waiting strips, [`UPLOAD_BUDGET`] bytes per frame.
    fn upload(&mut self, ctx: &egui::Context) {
        let mut bytes = 0;
        while bytes < UPLOAD_BUDGET && !self.uploads.is_empty() {
            let (i, img) = self.uploads.remove(0);
            bytes += img.pixels.len() * 4;
            match self.strips.get_mut(i) {
                Some(Some((t, current))) if t.size() == img.size => {
                    t.set(img, TextureOptions::NEAREST);
                    *current = true;
                }
                Some(slot) => *slot = Some((ctx.load_texture(format!("analysis-heatmap-{i}"), img, TextureOptions::NEAREST), true)),
                None => {}
            }
        }
        if !self.uploads.is_empty() {
            ctx.request_repaint();
        }
    }

    /// Map pixels to the screen.
    fn to_screen(&self, xf: &ImageTransform, p: Pos2) -> Pos2 {
        xf.to_screen(self.to_display(p, xf.size))
    }

    /// Draw the overlay over the image that `xf` maps to the screen, clipped
    /// to it. With the pointer at `hover`, show the map value there and the
    /// A/B pixels `pixels(x, y)` returns for image coordinates.
    pub fn paint(&mut self, ui: &Ui, painter: &Painter, xf: &ImageTransform, hover: Option<Pos2>, pixels: &dyn Fn(f32, f32) -> (Option<[u8; 4]>, Option<[u8; 4]>)) {
        if !self.active() {
            return;
        }
        self.refresh(ui.ctx());
        let painter = painter.with_clip_rect(painter.clip_rect().intersect(xf.dest));
        let opacity = self.opacity.clamp(0.0, 1.0);
        if let (Some(m), Some(_)) = (self.disp.clone(), self.drawn) {
            let (x0, y0, w, _) = m.extent();
            if !self.strips.is_empty() {
                let tint = Color32::WHITE.gamma_multiply(opacity);
                for (i, t) in self.strips.iter().enumerate() {
                    let Some((t, true)) = t else { continue };
                    // Map pixels covered by the strip (rows i * STRIP_ROWS ..).
                    let top = y0 + (i * STRIP_ROWS) as f32 * m.cell_h;
                    let bottom = top + t.size()[1] as f32 * m.cell_h;
                    let corners = [Pos2::new(x0, top), Pos2::new(x0 + w, top), Pos2::new(x0 + w, bottom), Pos2::new(x0, bottom)];
                    let uvs = [Pos2::new(0.0, 0.0), Pos2::new(1.0, 0.0), Pos2::new(1.0, 1.0), Pos2::new(0.0, 1.0)];
                    let mut mesh = egui::Mesh::with_texture(t.id());
                    for (c, uv) in corners.iter().zip(uvs) {
                        mesh.vertices.push(egui::epaint::Vertex { pos: self.to_screen(xf, *c), uv, color: tint });
                    }
                    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
                    painter.add(egui::Shape::mesh(mesh));
                }
            } else {
                let vis = painter.clip_rect();
                for j in 0..m.height {
                    for i in 0..m.width {
                        let Some(&c) = self.cells.get((j * m.width + i) as usize) else { continue };
                        if c.a() == 0 {
                            continue;
                        }
                        let (cx, cy) = (x0 + i as f32 * m.cell_w, y0 + j as f32 * m.cell_h);
                        let r = Rect::from_two_pos(self.to_screen(xf, Pos2::new(cx, cy)), self.to_screen(xf, Pos2::new(cx + m.cell_w, cy + m.cell_h)));
                        if r.intersects(vis) {
                            painter.rect_filled(r, 0.0, c.gamma_multiply(opacity));
                        }
                    }
                }
            }
        }
        let Some(pos) = hover.filter(|p| xf.dest.contains(*p)) else { return };
        let ip = xf.to_image(pos);
        let full = self.full.clone().expect("active map");
        let np = self.from_display(ip, xf.size);
        let value = full.value_at(np.x, np.y);
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
        readout(ui, pos, &lines.join("\n"));
    }

    /// Colormap, range, opacity and threshold controls and the legend.
    pub fn controls(&mut self, ui: &mut Ui, p: &Palette) {
        let Some(m) = self.full.clone() else { return };
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.visible, "Overlay");
            egui::ComboBox::from_id_salt("heatmap-colormap").width(90.0).selected_text(self.colormap.name()).show_ui(ui, |ui| {
                for c in Colormap::ALL {
                    ui.selectable_value(&mut self.colormap, c, c.name());
                }
            });
            if self.pending.is_some() {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Range").color(p.muted));
            let before = self.range;
            let auto_tip = if m.min_span > 0.0 { "From the ideal value to the 99th percentile" } else { "1st to 99th percentile" };
            ui.selectable_value(&mut self.range, RangeKind::Auto, "Auto").on_hover_text(auto_tip);
            ui.selectable_value(&mut self.range, RangeKind::Fixed, "Fixed").on_hover_text("The metric's default range");
            ui.selectable_value(&mut self.range, RangeKind::Custom, "Custom");
            if before != self.range && self.range == RangeKind::Custom {
                self.user = self.range_of(if before == RangeKind::Fixed { RangeMode::Fixed } else { RangeMode::Auto });
            }
        });
        let (lo, hi) = self.range_of(self.options().range);
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

/// A small text box beside the pointer, above everything (not clipped to
/// the pane or plot it describes).
pub fn readout(ui: &Ui, pos: Pos2, text: &str) {
    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("analysis-readout")));
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

    painter.rect_filled(r, 4.0, Color32::from_black_alpha(210));
    painter.galley(r.min + pad, galley, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(w: usize, h: usize) -> Arc<Heatmap> {
        Arc::new(Heatmap::new(w, h, (0..w * h).map(|i| i as f32).collect(), "abs diff"))
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
        assert!(is_tile_map(&(*map(3, 3)).clone().with_cell(64.0, 64.0)));
        // A 12 MP map pooled to 2000 x 1500 cells of 2 px is not one.
        assert!(!is_tile_map(&(*map(2000, 1500)).clone().with_cell(2.0, 2.0)));
        assert!(!is_tile_map(&map(4000, 3000).downsample(OVERLAY_SIDE)));
    }

    #[test]
    fn custom_range_and_values() {
        let mut v = HeatmapView::default();
        v.set_map("diff.abs", map(10, 10));
        v.range = RangeKind::Custom;
        v.user = (5.0, 50.0);
        assert_eq!(v.options().range, RangeMode::User { lo: 5.0, hi: 50.0 });
        assert_eq!(v.resolved_options().range, RangeMode::User { lo: 5.0, hi: 50.0 });
        assert_eq!(fmt_value(1.23456, Some(lumen_analysis::Unit::DeltaE)), "1.2346 dE");
        assert_eq!(fmt_value(0.5, None), "0.5000");
    }

    #[test]
    fn native_maps_follow_the_turned_picture() {
        let mut v = HeatmapView::default();
        // 1920x1080 stream shown turned 90 degrees, at half size (540x960).
        v.set_native(Some((90, (1920, 1080))));
        let size = Vec2::new(540.0, 960.0);
        assert_eq!(v.to_display(Pos2::ZERO, size), Pos2::new(540.0, 0.0));
        let p = Pos2::new(300.0, 200.0);
        let back = v.from_display(v.to_display(p, size), size);
        assert!((back - p).length() < 1e-3, "{back:?}");
        v.set_native(None);
        assert_eq!(v.to_display(p, size), p);
    }

    #[test]
    fn rotation_round_trips() {
        let size = (1920.0, 1080.0);
        for rot in [0, 90, 180, 270] {
            let p = Pos2::new(100.0, 40.0);
            let d = rotate_point(p, rot, size);
            assert_eq!(unrotate_point(d, rot, size), p, "{rot}");
        }
        // Turned 90 degrees, the native top-left corner is at the top right.
        assert_eq!(rotate_point(Pos2::ZERO, 90, size), Pos2::new(1080.0, 0.0));
        // Matches native_roi: displayed region -> native region.
        let r = super::super::metrics_view::native_roi(lumen_analysis::Roi::new(0, 0, 10, 20), 90, (1920, 1080));
        let (a, b) = (rotate_point(Pos2::new(r.x as f32, r.y as f32), 90, size), rotate_point(Pos2::new((r.x + r.w) as f32, (r.y + r.h) as f32), 90, size));
        let back = Rect::from_two_pos(a, b);
        assert_eq!((back.min, back.max), (Pos2::new(0.0, 0.0), Pos2::new(10.0, 20.0)));
    }
}
