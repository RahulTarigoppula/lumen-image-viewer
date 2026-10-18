//! Image editing core: the undoable edit session, histograms and the pure
//! pixel operations (filters, effects, geometry, tone, text, masks, retouch).
//! The egui tool UIs (`EditTool` and its implementations) live in the app crate.

pub mod effects;
pub mod filters;
pub mod geom;
pub mod mask;
pub mod ops;
pub mod raster;
pub mod retouch;
pub mod text;
pub mod tone;

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
