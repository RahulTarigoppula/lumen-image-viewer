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
    /// Whether Apply would do anything (e.g. false while Effects has no effect picked).
    fn can_apply(&self) -> bool {
        true
    }
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

/// Which curves a histogram shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistMode {
    /// Brightness only.
    Luminance,
    /// Red, green and blue.
    Rgb,
    /// Brightness behind the three channels.
    Both,
}

impl HistMode {
    pub const ALL: [HistMode; 3] = [HistMode::Luminance, HistMode::Rgb, HistMode::Both];
    pub fn label(self) -> &'static str {
        match self {
            HistMode::Luminance => "Luminance",
            HistMode::Rgb => "RGB",
            HistMode::Both => "Both",
        }
    }
    /// Short name for settings files.
    pub fn key(self) -> &'static str {
        match self {
            HistMode::Luminance => "lum",
            HistMode::Rgb => "rgb",
            HistMode::Both => "both",
        }
    }
    pub fn from_key(k: &str) -> HistMode {
        HistMode::ALL.into_iter().find(|m| m.key() == k).unwrap_or(HistMode::Both)
    }
}

/// Painting for [`Histogram`] (the data type lives in `lumen_core`).
pub trait HistogramPaint {
    /// Red, green and blue (see [`HistogramPaint::paint_mode`]).
    fn paint(&self, painter: &egui::Painter, rect: egui::Rect, dark: bool) {
        self.paint_mode(painter, rect, dark, HistMode::Rgb);
    }
    fn paint_mode(&self, painter: &egui::Painter, rect: egui::Rect, dark: bool, mode: HistMode);
}

impl HistogramPaint for Histogram {
    /// Channels as smoothed, filled areas (45% alpha, so overlaps mix) with a
    /// 1 pt line on top, on the input-well background with radius 6.
    /// Luminance is a neutral area (behind the channels in `Both`).
    fn paint_mode(&self, painter: &egui::Painter, rect: egui::Rect, dark: bool, mode: HistMode) {
        use egui::Color32;
        let p = crate::ui::theme::palette(painter.ctx());
        painter.rect_filled(rect, 6.0, p.input_bg);
        if self.l.is_empty() {
            return;
        }
        let shown: Vec<&Vec<u32>> = match mode {
            HistMode::Luminance => vec![&self.l],
            HistMode::Rgb => vec![&self.r, &self.g, &self.b],
            HistMode::Both => vec![&self.l, &self.r, &self.g, &self.b],
        };
        // The extreme bins (pure black / white) can dwarf everything else: scale
        // to the tallest bin inside them.
        let max = shown.iter().flat_map(|d| d.iter().skip(1).take(254)).copied().max().unwrap_or(1).max(1);
        let area = rect.shrink2(egui::vec2(4.0, 4.0));
        let line_a = if dark { 230 } else { 255 };
        if mode != HistMode::Rgb {
            let bins = tools::smooth_bins(&self.l, max);
            let (fill, line) = if dark { (Color32::from_white_alpha(64), Color32::from_white_alpha(210)) } else { (Color32::from_black_alpha(50), Color32::from_black_alpha(170)) };
            tools::paint_area(painter, area, &bins, fill, line);
        }
        if mode != HistMode::Luminance {
            let alpha = if mode == HistMode::Both { 80 } else { 115 };
            for (data, rgb) in [(&self.r, [235, 80, 80]), (&self.g, [80, 200, 120]), (&self.b, [80, 150, 255])] {
                let bins = tools::smooth_bins(data, max);
                let fill = Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], alpha);
                let line = Color32::from_rgba_unmultiplied(rgb[0], rgb[1], rgb[2], line_a);
                tools::paint_area(painter, area, &bins, fill, line);
            }
        }
    }
}
