//! Image editing: an undoable edit session per image plus a plug-in style
//! `EditTool` interface used by every adjustment dialog (live preview runs on a
//! screen-sized copy in the background; "Apply" runs on full resolution).

pub mod effects;
pub mod filters;
pub mod geom;
pub mod ops;
pub mod text;
pub mod tone;
pub mod tools;

use image::RgbaImage;
use std::path::PathBuf;
use std::sync::Arc;

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
    /// Increments on every change so views know to re-upload.
    pub version: u64,
    pub saved_version: u64,
}

impl EditSession {
    pub fn new(path: PathBuf, img: Arc<RgbaImage>) -> Self {
        EditSession { path, current: img, undo: Vec::new(), redo: Vec::new(), version: 0, saved_version: 0 }
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn push(&mut self, img: RgbaImage, what: &str) {
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
        let (img, what) = self.undo.pop()?;
        let cur = std::mem::replace(&mut self.current, img);
        self.redo.push((cur, what.clone()));
        self.version += 1;
        Some(what)
    }

    pub fn redo(&mut self) -> Option<String> {
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
