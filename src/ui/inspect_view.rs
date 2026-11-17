//! Inspect views drawn over the image in the viewer and in every Compare pane:
//! clipping warnings, false-color exposure, focus peaking and single
//! channels. Each view is computed off the UI thread on a copy of at most
//! `MAX_SIDE` pixels and drawn as a texture over the image.

use crossbeam_channel::Receiver;
use egui::{Color32, Rect, TextureHandle};
use image::RgbaImage;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Long side of the computed views (pixels).
pub const MAX_SIDE: u32 = 2400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InspectMode {
    Off,
    /// Red where any channel is at 254 or more, blue where all are at 1 or less.
    Clipping,
    /// Brightness bands in colors (like a video false-color monitor).
    FalseColor,
    /// Green on the sharpest edges.
    Peaking,
    Luma,
    Red,
    Green,
    Blue,
}

impl InspectMode {
    pub const ALL: [InspectMode; 8] = [InspectMode::Off, InspectMode::Clipping, InspectMode::FalseColor, InspectMode::Peaking, InspectMode::Luma, InspectMode::Red, InspectMode::Green, InspectMode::Blue];

    pub fn label(self) -> &'static str {
        match self {
            InspectMode::Off => "Normal",
            InspectMode::Clipping => "Clipping",
            InspectMode::FalseColor => "False color",
            InspectMode::Peaking => "Focus peaking",
            InspectMode::Luma => "Luma only",
            InspectMode::Red => "Red channel",
            InspectMode::Green => "Green channel",
            InspectMode::Blue => "Blue channel",
        }
    }

    pub fn tip(self) -> &'static str {
        match self {
            InspectMode::Off => "The image as it is",
            InspectMode::Clipping => "Red: highlights clipped (a channel at 254-255). Blue: shadows crushed to black (every channel at 0-1)",
            InspectMode::FalseColor => "Brightness in bands: purple / blue for deep shadows, gray for midtones, green near 18% gray, yellow / orange / red toward white",
            InspectMode::Peaking => "Green on the sharpest edges: where the camera put its focus and how much detail survived noise reduction",
            InspectMode::Luma => "Brightness only (Rec. 709 luma): judge tone and noise without color",
            InspectMode::Red => "The red channel alone as gray",
            InspectMode::Green => "The green channel alone as gray",
            InspectMode::Blue => "The blue channel alone as gray (usually the noisiest)",
        }
    }

    /// Next mode (keyboard cycling).
    pub fn next(self) -> InspectMode {
        let i = InspectMode::ALL.iter().position(|m| *m == self).unwrap_or(0);
        InspectMode::ALL[(i + 1) % InspectMode::ALL.len()]
    }

    /// Drawn over the image (true) or replacing it (false).
    pub fn is_overlay(self) -> bool {
        matches!(self, InspectMode::Clipping | InspectMode::Peaking)
    }
}

fn luma(p: [u8; 4]) -> f32 {
    0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32
}

/// False-color band for a luma value (0..255).
fn false_color(y: f32) -> [u8; 3] {
    // In percent of white, roughly like broadcast false color.
    let pct = y / 255.0 * 100.0;
    match pct {
        x if x < 2.5 => [96, 0, 128],
        x if x < 10.0 => [32, 64, 200],
        x if x < 20.0 => [40, 140, 220],
        x if x < 38.0 => [110, 110, 110],
        x if x < 48.0 => [60, 190, 70],
        x if x < 58.0 => [150, 150, 150],
        x if x < 70.0 => [210, 210, 210],
        x if x < 85.0 => [240, 220, 40],
        x if x < 97.0 => [250, 140, 30],
        _ => [230, 30, 30],
    }
}

/// The view of `src` in `mode` (the same size as `src`).
pub fn render(src: &RgbaImage, mode: InspectMode) -> RgbaImage {
    let (w, h) = src.dimensions();
    let mut out = RgbaImage::new(w, h);
    if w == 0 || h == 0 {
        return out;
    }
    let row = w as usize * 4;
    let s = src.as_raw();
    match mode {
        InspectMode::Off => out.copy_from_slice(s),
        InspectMode::Peaking => {
            // Sobel magnitude on luma; the strongest few percent of edges.
            let lum: Vec<f32> = s.par_chunks_exact(4).map(|p| luma([p[0], p[1], p[2], p[3]])).collect();
            let (wu, hu) = (w as usize, h as usize);
            let mag: Vec<f32> = (0..wu * hu)
                .into_par_iter()
                .map(|i| {
                    let (x, y) = (i % wu, i / wu);
                    if x == 0 || y == 0 || x + 1 >= wu || y + 1 >= hu {
                        return 0.0;
                    }
                    let l = |dx: isize, dy: isize| lum[(y as isize + dy) as usize * wu + (x as isize + dx) as usize];
                    let gx = l(1, -1) + 2.0 * l(1, 0) + l(1, 1) - l(-1, -1) - 2.0 * l(-1, 0) - l(-1, 1);
                    let gy = l(-1, 1) + 2.0 * l(0, 1) + l(1, 1) - l(-1, -1) - 2.0 * l(0, -1) - l(1, -1);
                    (gx * gx + gy * gy).sqrt()
                })
                .collect();
            // Threshold: the 92nd percentile of edge strength, at least a
            // clear edge (so flat noise never lights up).
            let mut sample: Vec<f32> = mag.iter().step_by(7).copied().filter(|m| *m > 0.0).collect();
            sample.sort_by(f32::total_cmp);
            let p92 = sample.get(sample.len() * 92 / 100).copied().unwrap_or(f32::MAX);
            let thr = p92.max(120.0);
            out.par_chunks_exact_mut(row).enumerate().for_each(|(y, line)| {
                for x in 0..wu {
                    if mag[y * wu + x] >= thr {
                        line[x * 4..x * 4 + 4].copy_from_slice(&[40, 255, 60, 230]);
                    }
                }
            });
        }
        _ => {
            out.par_chunks_exact_mut(row).zip(s.par_chunks_exact(row)).for_each(|(o, i)| {
                for (op, ip) in o.chunks_exact_mut(4).zip(i.chunks_exact(4)) {
                    let p = [ip[0], ip[1], ip[2], ip[3]];
                    let v: [u8; 4] = match mode {
                        InspectMode::Clipping => {
                            if p[0] >= 254 || p[1] >= 254 || p[2] >= 254 {
                                [255, 30, 30, 220]
                            } else if p[0] <= 1 && p[1] <= 1 && p[2] <= 1 {
                                [40, 90, 255, 220]
                            } else {
                                [0, 0, 0, 0]
                            }
                        }
                        InspectMode::FalseColor => {
                            let c = false_color(luma(p));
                            [c[0], c[1], c[2], 255]
                        }
                        InspectMode::Luma => {
                            let y = luma(p).round().clamp(0.0, 255.0) as u8;
                            [y, y, y, 255]
                        }
                        InspectMode::Red => [p[0], p[0], p[0], 255],
                        InspectMode::Green => [p[1], p[1], p[1], 255],
                        InspectMode::Blue => [p[2], p[2], p[2], 255],
                        InspectMode::Off | InspectMode::Peaking => p,
                    };
                    op.copy_from_slice(&v);
                }
            });
        }
    }
    out
}

/// Computed views as textures, per (file, mode); a few kept.
#[derive(Default)]
pub struct InspectCache {
    done: HashMap<(PathBuf, InspectMode), TextureHandle>,
    pending: HashMap<(PathBuf, InspectMode), Receiver<RgbaImage>>,
    order: Vec<(PathBuf, InspectMode)>,
}

impl InspectCache {
    /// The texture of `mode` for `path` (whose pixels are `img`), started in
    /// the background when missing.
    pub fn get(&mut self, ctx: &egui::Context, path: &std::path::Path, img: &Arc<RgbaImage>, mode: InspectMode) -> Option<TextureHandle> {
        if mode == InspectMode::Off {
            return None;
        }
        let key = (path.to_path_buf(), mode);
        if let Some(t) = self.done.get(&key) {
            return Some(t.clone());
        }
        if let Some(rx) = self.pending.get(&key) {
            if let Ok(im) = rx.try_recv() {
                self.pending.remove(&key);
                let tex = ctx.load_texture(format!("inspect:{}:{mode:?}", path.display()), egui::ColorImage::from_rgba_unmultiplied([im.width() as usize, im.height() as usize], im.as_raw()), egui::TextureOptions::LINEAR);
                self.done.insert(key.clone(), tex.clone());
                self.order.push(key);
                // Keep the last 8 views.
                while self.order.len() > 8 {
                    let old = self.order.remove(0);
                    self.done.remove(&old);
                }
                return Some(tex);
            }
            return None;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (img, ctx2) = (img.clone(), ctx.clone());
        std::thread::spawn(move || {
            crate::bg::background_priority();
            let small;
            let src: &RgbaImage = if img.width().max(img.height()) > MAX_SIDE {
                small = lumen_core::imaging::resize_to_fit(&img, MAX_SIDE, MAX_SIDE);
                &small
            } else {
                &img
            };
            if let Ok(out) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render(src, mode))) {
                let _ = tx.send(out);
            }
            ctx2.request_repaint();
        });
        self.pending.insert(key, rx);
        None
    }

    /// Draw the view of `mode` over an image drawn at `dest` (clipped to `clip`).
    pub fn paint(&mut self, ui: &egui::Ui, path: &std::path::Path, img: &Arc<RgbaImage>, mode: InspectMode, dest: Rect, clip: Rect) {
        let Some(tex) = self.get(ui.ctx(), path, img, mode) else {
            if mode != InspectMode::Off {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            return;
        };
        let painter = ui.painter().with_clip_rect(clip);
        painter.image(tex.id(), dest, Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)), Color32::WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_marks_both_ends() {
        let img = RgbaImage::from_fn(3, 1, |x, _| image::Rgba(match x {
            0 => [255, 120, 10, 255],
            1 => [0, 1, 0, 255],
            _ => [128, 128, 128, 255],
        }));
        let out = render(&img, InspectMode::Clipping);
        assert_eq!(out.get_pixel(0, 0).0[0], 255);
        assert_eq!(out.get_pixel(1, 0).0[2], 255);
        assert_eq!(out.get_pixel(2, 0).0[3], 0);
    }

    #[test]
    fn channels_and_luma() {
        let img = RgbaImage::from_pixel(2, 2, image::Rgba([200, 100, 50, 255]));
        assert_eq!(render(&img, InspectMode::Red).get_pixel(0, 0).0, [200, 200, 200, 255]);
        assert_eq!(render(&img, InspectMode::Blue).get_pixel(1, 1).0, [50, 50, 50, 255]);
        let y = render(&img, InspectMode::Luma).get_pixel(0, 0).0[0];
        assert!((y as i32 - 117).abs() <= 1, "{y}");
        assert_eq!(render(&img, InspectMode::Off), img);
    }

    #[test]
    fn false_color_bands_are_ordered() {
        assert_eq!(false_color(0.0), [96, 0, 128]);
        assert_eq!(false_color(255.0), [230, 30, 30]);
        assert_eq!(false_color(0.45 * 255.0), [60, 190, 70]);
    }

    #[test]
    fn peaking_finds_edges_not_flat_noise() {
        // Left half black, right half white: one strong vertical edge.
        let img = RgbaImage::from_fn(40, 20, |x, y| {
            let n = ((x * 7 + y * 13) % 5) as u8;
            if x < 20 { image::Rgba([n, n, n, 255]) } else { image::Rgba([250 - n, 250 - n, 250 - n, 255]) }
        });
        let out = render(&img, InspectMode::Peaking);
        assert!(out.get_pixel(20, 10).0[3] > 0 || out.get_pixel(19, 10).0[3] > 0);
        assert_eq!(out.get_pixel(5, 10).0[3], 0);
        assert_eq!(out.get_pixel(35, 10).0[3], 0);
    }

    #[test]
    fn modes_cycle() {
        let mut m = InspectMode::Off;
        for _ in 0..InspectMode::ALL.len() {
            m = m.next();
        }
        assert_eq!(m, InspectMode::Off);
        assert!(InspectMode::Clipping.is_overlay() && !InspectMode::Luma.is_overlay());
    }
}
