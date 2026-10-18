//! Image editing: an undoable edit session per image plus a plug-in style
//! `EditTool` interface used by every adjustment dialog (live preview runs on a
//! screen-sized copy in the background; "Apply" runs on full resolution).

pub mod effects;
pub mod filters;
pub mod geom;
pub mod mask;
pub mod ops;
pub mod raster;
pub mod retouch;
pub mod text;
pub mod tone;
pub mod tools;

use image::RgbaImage;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transform {
    Rotate90,
    Rotate180,
    Rotate270,
    FlipH,
    FlipV,
}

impl Transform {
    pub fn apply(self, img: &RgbaImage) -> RgbaImage {
        use image::imageops;
        match self {
            Transform::Rotate90 => imageops::rotate90(img),
            Transform::Rotate180 => imageops::rotate180(img),
            Transform::Rotate270 => imageops::rotate270(img),
            Transform::FlipH => imageops::flip_horizontal(img),
            Transform::FlipV => imageops::flip_vertical(img),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Transform::Rotate90 => "Rotate right",
            Transform::Rotate180 => "Rotate 180°",
            Transform::Rotate270 => "Rotate left",
            Transform::FlipH => "Flip horizontal",
            Transform::FlipV => "Flip vertical",
        }
    }
}

const UNDO_BUDGET: usize = 1200 << 20;

pub struct EditSession {
    pub path: PathBuf,
    pub current: Arc<RgbaImage>,
    undo: Vec<(Arc<RgbaImage>, String)>,
    redo: Vec<(Arc<RgbaImage>, String)>,
    /// Increments on every change so views know to re-upload (cache key only;
    /// dirtiness is tracked by image identity, see [`is_dirty`](Self::is_dirty)).
    pub version: u64,
    /// Legacy: hosts may still set `saved_version = version` after saving;
    /// it is honoured lazily. Prefer [`mark_saved`](Self::mark_saved).
    pub saved_version: u64,
    /// The image that matches the file on disk (loaded or last saved). Weak so
    /// it never keeps pixels alive; the allocation is pinned so identity is safe.
    saved: Weak<RgbaImage>,
}

impl EditSession {
    pub fn new(path: PathBuf, img: Arc<RgbaImage>) -> Self {
        let saved = Arc::downgrade(&img);
        EditSession { path, current: img, undo: Vec::new(), redo: Vec::new(), version: 0, saved_version: 0, saved }
    }

    /// True when the current image differs from the loaded / last saved one.
    /// Undoing back to the saved state makes the session clean again.
    pub fn is_dirty(&self) -> bool {
        if self.saved_version == self.version {
            return false;
        }
        !std::ptr::eq(self.saved.as_ptr(), Arc::as_ptr(&self.current))
    }

    /// Record that the current image has been written to disk.
    pub fn mark_saved(&mut self) {
        self.saved = Arc::downgrade(&self.current);
        self.saved_version = self.version;
    }

    /// Pick up a legacy `saved_version = version` assignment before mutating.
    fn sync_saved(&mut self) {
        if self.saved_version == self.version {
            self.saved = Arc::downgrade(&self.current);
        }
    }

    pub fn push(&mut self, img: RgbaImage, what: &str) {
        self.sync_saved();
        let prev = std::mem::replace(&mut self.current, Arc::new(img));
        self.undo.push((prev, what.to_string()));
        self.redo.clear();
        self.version += 1;
        // Bound memory: drop the oldest undo steps beyond the budget.
        let mut total: usize = self.undo.iter().map(|(i, _)| i.as_raw().len()).sum();
        while total > UNDO_BUDGET && self.undo.len() > 1 {
            let (i, _) = self.undo.remove(0);
            total -= i.as_raw().len();
        }
    }

    pub fn undo(&mut self) -> Option<String> {
        self.sync_saved();
        let (img, what) = self.undo.pop()?;
        let cur = std::mem::replace(&mut self.current, img);
        self.redo.push((cur, what.clone()));
        self.version += 1;
        Some(what)
    }

    pub fn redo(&mut self) -> Option<String> {
        self.sync_saved();
        let (img, what) = self.redo.pop()?;
        let cur = std::mem::replace(&mut self.current, img);
        self.undo.push((cur, what.clone()));
        self.version += 1;
        Some(what)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|(_, s)| s.as_str())
    }
    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|(_, s)| s.as_str())
    }
}

/// Context passed to tool UIs.
pub struct ToolUiCtx<'a> {
    pub palette: &'a crate::ui::theme::Palette,
    /// Full-resolution size of the image being edited.
    pub image_size: (u32, u32),
    /// 256-bin luminance + RGB histograms of the current image.
    pub histogram: &'a Histogram,
    /// The screen-sized preview copy the live preview is rendered from (for
    /// tools that show their own thumbnails).
    pub preview: Option<&'a RgbaImage>,
}

/// Context passed to [`EditTool::canvas`]: where the image is on screen and
/// how to map between image pixels (full resolution) and screen points.
pub struct CanvasCtx<'a> {
    pub palette: &'a crate::ui::theme::Palette,
    /// Full-resolution size of the image being edited.
    pub image_size: (u32, u32),
    /// Screen rect (points) the whole image is drawn into (may exceed `view`).
    pub dest: egui::Rect,
    /// Visible viewport (points).
    pub view: egui::Rect,
    /// Physical screen pixels per image pixel.
    pub zoom: f32,
    pub pixels_per_point: f32,
    /// The preview-sized copy the live preview renders from, and its scale
    /// (preview size / full size).
    pub preview: Option<&'a RgbaImage>,
    pub preview_scale: f32,
    /// True when the displayed preview reflects the tool's latest parameters.
    pub preview_current: bool,
    /// False while the user holds Space to pan: draw overlays only.
    pub interactive: bool,
}

impl CanvasCtx<'_> {
    /// Screen points per image pixel.
    pub fn pts_per_px(&self) -> f32 {
        if self.image_size.0 == 0 {
            1.0
        } else {
            (self.dest.width() / self.image_size.0 as f32).max(1e-6)
        }
    }
    /// Image pixel (full resolution) to screen point.
    pub fn to_screen(&self, p: [f32; 2]) -> egui::Pos2 {
        let s = self.pts_per_px();
        egui::pos2(self.dest.min.x + p[0] * s, self.dest.min.y + p[1] * s)
    }
    /// Screen point to image pixel (full resolution, unclamped).
    pub fn to_image(&self, p: egui::Pos2) -> [f32; 2] {
        let s = self.pts_per_px();
        [(p.x - self.dest.min.x) / s, (p.y - self.dest.min.y) / s]
    }
    /// A screen distance (points) expressed in image pixels.
    pub fn screen_to_px(&self, d: f32) -> f32 {
        d / self.pts_per_px()
    }
    pub fn clamp_to_image(&self, p: [f32; 2]) -> [f32; 2] {
        [p[0].clamp(0.0, self.image_size.0 as f32), p[1].clamp(0.0, self.image_size.1 as f32)]
    }
    /// Sample the preview image at a full-resolution image position.
    pub fn sample(&self, p: [f32; 2]) -> Option<[u8; 4]> {
        let img = self.preview?;
        let x = (p[0] * self.preview_scale).floor();
        let y = (p[1] * self.preview_scale).floor();
        if x < 0.0 || y < 0.0 || x >= img.width() as f32 || y >= img.height() as f32 {
            return None;
        }
        Some(img.get_pixel(x as u32, y as u32).0)
    }
}

/// An adjustment dialog. Implementations hold their own parameters.
pub trait EditTool: Send {
    fn title(&self) -> String;
    /// Draw parameter controls; return true when a parameter changed.
    fn ui(&mut self, ui: &mut egui::Ui, cx: &ToolUiCtx) -> bool;
    /// Apply to `img`. `scale` is preview-size / full-size (1.0 for the final
    /// render) so spatial parameters (blur radius, border width, font size...)
    /// can be scaled to look the same in the preview.
    fn apply(&self, img: &RgbaImage, scale: f32) -> RgbaImage;
    fn clone_box(&self) -> Box<dyn EditTool>;
    /// Reset parameters to defaults.
    fn reset(&mut self) {}
    /// Whether the output size differs from the input (resize, canvas, border ...).
    fn changes_size(&self) -> bool {
        false
    }
    /// Whether the tool works directly on the image (draws overlays and takes
    /// primary-button clicks/drags; the viewer then pans only while Space is held).
    fn wants_canvas(&self) -> bool {
        false
    }
    /// Interactive canvas hook, called every frame after the image is painted
    /// when [`wants_canvas`](Self::wants_canvas) is true. `resp` is the image
    /// area's click-and-drag response. Return true when parameters changed
    /// (triggers a new preview render).
    fn canvas(&mut self, _cx: &CanvasCtx, _painter: &egui::Painter, _resp: &egui::Response) -> bool {
        false
    }
    /// Tool-local undo / redo (e.g. Draw Board objects). Return true when
    /// handled, so the host does not undo the image history instead.
    fn undo(&mut self) -> bool {
        false
    }
    fn redo(&mut self) -> bool {
        false
    }
}

#[derive(Clone, Default)]
pub struct Histogram {
    pub r: Vec<u32>,
    pub g: Vec<u32>,
    pub b: Vec<u32>,
    pub l: Vec<u32>,
    pub max: u32,
}

impl Histogram {
    pub fn compute(img: &RgbaImage) -> Histogram {
        use rayon::prelude::*;
        let raw = img.as_raw();
        // Subsample very large images; the shape is what matters.
        let step = ((raw.len() / 4) / 4_000_000).max(1) * 4;
        let chunks: Vec<&[u8]> = raw.chunks(4 * 65536).collect();
        let parts: Vec<[[u32; 256]; 4]> = chunks
            .par_iter()
            .map(|c| {
                let mut h = [[0u32; 256]; 4];
                let mut i = 0;
                while i + 3 < c.len() {
                    let (r, g, b) = (c[i], c[i + 1], c[i + 2]);
                    h[0][r as usize] += 1;
                    h[1][g as usize] += 1;
                    h[2][b as usize] += 1;
                    let l = (r as u32 * 54 + g as u32 * 183 + b as u32 * 19) >> 8;
                    h[3][l.min(255) as usize] += 1;
                    i += step;
                }
                h
            })
            .collect();
        let mut out = [[0u32; 256]; 4];
        for p in parts {
            for c in 0..4 {
                for i in 0..256 {
                    out[c][i] += p[c][i];
                }
            }
        }
        // Ignore the extreme bins when scaling so clipped blacks/whites don't flatten the graph.
        let max = (0..4).flat_map(|c| out[c][1..255].iter().copied()).max().unwrap_or(1).max(1);
        Histogram { r: out[0].to_vec(), g: out[1].to_vec(), b: out[2].to_vec(), l: out[3].to_vec(), max }
    }

    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect, dark: bool) {
        use egui::{Color32, Pos2};
        painter.rect_filled(rect, 4.0, if dark { Color32::from_black_alpha(90) } else { Color32::from_black_alpha(14) });
        if self.l.is_empty() {
            return;
        }
        let series: [(&Vec<u32>, Color32); 4] = [
            (&self.r, Color32::from_rgba_unmultiplied(255, 80, 80, 110)),
            (&self.g, Color32::from_rgba_unmultiplied(80, 220, 110, 110)),
            (&self.b, Color32::from_rgba_unmultiplied(90, 140, 255, 110)),
            (&self.l, Color32::from_rgba_unmultiplied(230, 230, 230, if dark { 120 } else { 90 })),
        ];
        for (data, color) in series {
            let mut mesh = egui::Mesh::default();
            let w = rect.width() / 256.0;
            for (i, &v) in data.iter().enumerate() {
                let hgt = ((v as f32 / self.max as f32).sqrt().min(1.0)) * rect.height();
                let x = rect.min.x + i as f32 * w;
                mesh.add_colored_rect(
                    egui::Rect::from_min_max(Pos2::new(x, rect.max.y - hgt), Pos2::new(x + w + 0.3, rect.max.y)),
                    color,
                );
            }
            painter.add(egui::Shape::mesh(mesh));
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    fn img(v: u8) -> RgbaImage {
        RgbaImage::from_pixel(2, 2, image::Rgba([v, v, v, 255]))
    }

    #[test]
    fn undo_to_original_is_clean() {
        let mut s = EditSession::new(PathBuf::from("a.png"), Arc::new(img(1)));
        assert!(!s.is_dirty());
        s.push(img(2), "x");
        assert!(s.is_dirty());
        s.undo();
        assert!(!s.is_dirty());
        s.redo();
        assert!(s.is_dirty());
    }

    #[test]
    fn saved_state_tracking() {
        let mut s = EditSession::new(PathBuf::from("a.png"), Arc::new(img(1)));
        s.push(img(2), "x");
        s.mark_saved();
        assert!(!s.is_dirty());
        s.push(img(3), "y");
        assert!(s.is_dirty());
        s.undo();
        assert!(!s.is_dirty());
        s.undo();
        assert!(s.is_dirty(), "original differs from the saved file now");
        // Legacy assignment still works.
        s.saved_version = s.version;
        assert!(!s.is_dirty());
        s.redo();
        assert!(s.is_dirty());
        s.undo();
        assert!(!s.is_dirty());
    }
}
