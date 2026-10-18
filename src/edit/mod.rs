//! Image editing: an undoable edit session per image plus a plug-in style
//! `EditTool` interface used by every adjustment dialog (live preview runs on a
//! screen-sized copy in the background; "Apply" runs on full resolution).

pub use lumen_core::edit::*;

pub mod tools;

use image::RgbaImage;

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

/// Painting for [`Histogram`] (the data type lives in `lumen_core`).
pub trait HistogramPaint {
    fn paint(&self, painter: &egui::Painter, rect: egui::Rect, dark: bool);
}

impl HistogramPaint for Histogram {
    fn paint(&self, painter: &egui::Painter, rect: egui::Rect, dark: bool) {
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
