//! Inspect views drawn over the image in the viewer and in every Compare pane:
//! clipping warnings, false-color exposure, focus peaking, single channels
//! and, for photos with an HDR gain map, the gain map itself and the HDR
//! rendition. Each view is computed off the UI thread on a copy of at most
//! `MAX_SIDE` pixels and drawn as a texture over the image; gain maps are
//! read and decoded in the background once per file.

use crossbeam_channel::Receiver;
use egui::{Color32, Pos2, Rect, RichText, TextureHandle, Vec2};
use image::RgbaImage;
use lumen_core::inspect::gainmap::{self, NoGainMap, Prepared};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// The HDR gain map image, scaled to the picture.
    GainMap,
    /// The picture with its gain map applied for a chosen screen headroom.
    HdrRendition,
}

impl InspectMode {
    pub const ALL: [InspectMode; 10] = [
        InspectMode::Off,
        InspectMode::Clipping,
        InspectMode::FalseColor,
        InspectMode::Peaking,
        InspectMode::Luma,
        InspectMode::Red,
        InspectMode::Green,
        InspectMode::Blue,
        InspectMode::GainMap,
        InspectMode::HdrRendition,
    ];

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
            InspectMode::GainMap => "Gain map",
            InspectMode::HdrRendition => "HDR rendition",
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
            InspectMode::GainMap => "The photo's HDR gain map, scaled to the picture: the brighter, the more an HDR screen brightens that area (JPEG files with an Ultra HDR, ISO 21496-1 or Apple gain map)",
            InspectMode::HdrRendition => "The photo with its HDR gain map applied for the chosen screen headroom, drawn with a straight curve: the headroom's peak is white and SDR white is gray, so the areas an HDR screen brightens stand out",
        }
    }

    /// The gain map views (they need a photo with a gain map).
    pub fn is_gain(self) -> bool {
        matches!(self, InspectMode::GainMap | InspectMode::HdrRendition)
    }

    /// Next mode (keyboard cycling); the gain map views only when `gain_maps`.
    pub fn next_for(self, gain_maps: bool) -> InspectMode {
        let step = |m: InspectMode| {
            let i = InspectMode::ALL.iter().position(|x| *x == m).unwrap_or(0);
            InspectMode::ALL[(i + 1) % InspectMode::ALL.len()]
        };
        let mut m = step(self);
        while !gain_maps && m.is_gain() {
            m = step(m);
        }
        m
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
                        InspectMode::Off | InspectMode::Peaking | InspectMode::GainMap | InspectMode::HdrRendition => p,
                    };
                    op.copy_from_slice(&v);
                }
            });
        }
    }
    out
}

/// A gain map being read, read, or known to be missing.
enum GainLoad {
    Loading(Receiver<Result<gainmap::GainMap, NoGainMap>>),
    Ready(Arc<gainmap::GainMap>),
    Missing(NoGainMap),
}

/// What the gain map views know about a file.
#[derive(Clone)]
pub enum GainInfo {
    Loading,
    Missing(NoGainMap),
    Ready(Arc<gainmap::GainMap>),
}

/// Result of an HDR rendition job: the reduced base with the resampled map, and the view.
type HdrJob = Receiver<(Arc<(RgbaImage, Prepared)>, RgbaImage)>;

/// The HDR rendition of one file: the newest texture and the job making the next.
#[derive(Default)]
struct HdrView {
    /// Identity of the base image the rest was made from.
    base: usize,
    /// The base reduced to at most `MAX_SIDE` and the gain map resampled to it (made once).
    prepared: Option<Arc<(RgbaImage, Prepared)>>,
    /// (headroom, texture) of the newest finished view.
    tex: Option<(f32, TextureHandle)>,
    job: Option<(f32, HdrJob)>,
    /// A job failed at this headroom: do not start it again.
    failed: Option<f32>,
}

/// Gain maps of this many files are kept.
const GAIN_KEEP: usize = 6;

/// Computed views as textures, per (file, mode); a few kept.
#[derive(Default)]
pub struct InspectCache {
    done: HashMap<(PathBuf, InspectMode), TextureHandle>,
    pending: HashMap<(PathBuf, InspectMode), Receiver<RgbaImage>>,
    order: Vec<(PathBuf, InspectMode)>,
    gain: HashMap<PathBuf, GainLoad>,
    gain_order: Vec<PathBuf>,
    map_tex: HashMap<PathBuf, TextureHandle>,
    hdr: HashMap<PathBuf, HdrView>,
}

impl InspectCache {
    /// The gain map of the file at `path`, read in the background on first ask.
    pub fn gain_info(&mut self, ctx: &egui::Context, path: &Path) -> GainInfo {
        if let Some(GainLoad::Loading(rx)) = self.gain.get(path) {
            let state = match rx.try_recv() {
                Ok(Ok(m)) => Some(GainLoad::Ready(Arc::new(m))),
                Ok(Err(e)) => Some(GainLoad::Missing(e)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(GainLoad::Missing(NoGainMap { reason: "Reading the gain map failed unexpectedly (the file may be damaged).".into(), announced: true })),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            };
            if let Some(s) = state {
                self.gain.insert(path.to_path_buf(), s);
            }
        }
        match self.gain.get(path) {
            Some(GainLoad::Ready(m)) => return GainInfo::Ready(m.clone()),
            Some(GainLoad::Missing(e)) => return GainInfo::Missing(e.clone()),
            Some(GainLoad::Loading(_)) => return GainInfo::Loading,
            None => {}
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (p, ctx2) = (path.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            crate::bg::background_priority();
            if let Ok(r) = std::panic::catch_unwind(|| gainmap::load(&p)) {
                let _ = tx.send(r);
            }
            ctx2.request_repaint();
        });
        self.gain.insert(path.to_path_buf(), GainLoad::Loading(rx));
        self.gain_order.retain(|p| p != path);
        self.gain_order.push(path.to_path_buf());
        while self.gain_order.len() > GAIN_KEEP {
            let old = self.gain_order.remove(0);
            self.gain.remove(&old);
            self.map_tex.remove(&old);
            self.hdr.remove(&old);
        }
        GainInfo::Loading
    }

    /// The file is known to have no gain map and does not claim one: cycling
    /// skips the gain map views for it.
    pub fn lacks_gain_map(&self, path: &Path) -> bool {
        matches!(self.gain.get(path), Some(GainLoad::Missing(e)) if !e.announced)
    }

    /// Draw the gain map view `mode` of the file at `path` (whose decoded
    /// pixels are `img`) at `dest`, clipped to `clip`. Nothing is drawn while
    /// the gain map is read or when the file has none (the legend says why).
    #[allow(clippy::too_many_arguments)]
    pub fn paint_gain(&mut self, ui: &egui::Ui, path: &Path, img: &Arc<RgbaImage>, mode: InspectMode, headroom: f32, dest: Rect, clip: Rect) {
        let map = match self.gain_info(ui.ctx(), path) {
            GainInfo::Ready(m) => m,
            GainInfo::Loading => {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            GainInfo::Missing(_) => return,
        };
        let tex = match mode {
            InspectMode::GainMap => Some(self.map_texture(ui.ctx(), path, &map)),
            InspectMode::HdrRendition => self.hdr_texture(ui.ctx(), path, img, &map, headroom),
            _ => None,
        };
        if let Some(tex) = tex {
            let painter = ui.painter().with_clip_rect(clip);
            painter.image(tex.id(), dest, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
    }

    fn map_texture(&mut self, ctx: &egui::Context, path: &Path, map: &gainmap::GainMap) -> TextureHandle {
        if let Some(t) = self.map_tex.get(path) {
            return t.clone();
        }
        let im = &map.image;
        let ci = egui::ColorImage::from_rgba_unmultiplied([im.width() as usize, im.height() as usize], im.as_raw());
        let tex = ctx.load_texture(format!("gainmap:{}", path.display()), ci, egui::TextureOptions::LINEAR);
        self.map_tex.insert(path.to_path_buf(), tex.clone());
        tex
    }

    /// The newest HDR rendition texture (possibly for another headroom while
    /// the one for `headroom` is made in the background).
    fn hdr_texture(&mut self, ctx: &egui::Context, path: &Path, img: &Arc<RgbaImage>, map: &Arc<gainmap::GainMap>, headroom: f32) -> Option<TextureHandle> {
        let h = headroom.clamp(1.0, map.meta.headroom_max());
        let base = Arc::as_ptr(img) as usize;
        let v = self.hdr.entry(path.to_path_buf()).or_default();
        if v.base != base {
            *v = HdrView { base, ..Default::default() };
        }
        let finished = match v.job.as_ref().map(|(jh, rx)| (*jh, rx.try_recv())) {
            Some((jh, Ok(r))) => Some((jh, Some(r))),
            Some((jh, Err(crossbeam_channel::TryRecvError::Disconnected))) => Some((jh, None)),
            _ => None,
        };
        if let Some((jh, r)) = finished {
            v.job = None;
            match r {
                Some((prep, out)) => {
                    let ci = egui::ColorImage::from_rgba_unmultiplied([out.width() as usize, out.height() as usize], out.as_raw());
                    if let Some((th, t)) = v.tex.as_mut() {
                        t.set(ci, egui::TextureOptions::LINEAR);
                        *th = jh;
                    } else {
                        v.tex = Some((jh, ctx.load_texture(format!("hdr:{}", path.display()), ci, egui::TextureOptions::LINEAR)));
                    }
                    v.prepared = Some(prep);
                }
                None => v.failed = Some(jh),
            }
        }
        let stale = v.tex.as_ref().is_none_or(|(th, _)| (th - h).abs() > 1e-3);
        if stale && v.job.is_none() && v.failed != Some(h) {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let (img, map, prepared, ctx2) = (img.clone(), map.clone(), v.prepared.clone(), ctx.clone());
            std::thread::spawn(move || {
                crate::bg::background_priority();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let prep = prepared.unwrap_or_else(|| {
                        let small = lumen_core::imaging::resize_to_fit(&img, MAX_SIDE, MAX_SIDE);
                        let gain = gainmap::prepare(&map, small.width(), small.height());
                        Arc::new((small, gain))
                    });
                    let out = gainmap::render_hdr(&prep.0, &prep.1, &map.meta, h);
                    (prep, out)
                }));
                if let Ok(r) = r {
                    let _ = tx.send(r);
                }
                ctx2.request_repaint();
            });
            v.job = Some((h, rx));
        }
        if v.job.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        v.tex.as_ref().map(|(_, t)| t.clone())
    }

    /// The texture of `mode` for `path` (whose pixels are `img`), started in
    /// the background when missing.
    pub fn get(&mut self, ctx: &egui::Context, path: &std::path::Path, img: &Arc<RgbaImage>, mode: InspectMode) -> Option<TextureHandle> {
        if mode == InspectMode::Off || mode.is_gain() {
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

/// "4", "2.5".
fn trim_x(v: f32) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// Legend text: small, in `c`.
fn small(s: &str, c: Color32) -> RichText {
    RichText::new(s).size(super::theme::text::SMALL).color(c)
}

/// The legend of the gain map views in a box whose top left is `at` (at most
/// `max_w` wide), with the screen headroom slider in the HDR rendition.
/// `headroom` may be shared by several panes; the slider shows it within
/// this map's range and writes it back only when moved. Returns the box.
pub fn gain_legend(ui: &mut egui::Ui, id: egui::Id, at: Pos2, max_w: f32, info: &GainInfo, mode: InspectMode, headroom: &mut f32) -> Rect {
    let p = super::theme::palette(ui.ctx());
    let w = max_w.clamp(160.0, 330.0);
    let builder = egui::UiBuilder::new().id_salt(id).max_rect(Rect::from_min_size(at, Vec2::new(w, 600.0))).layout(egui::Layout::top_down(egui::Align::Min));
    let mut child = ui.new_child(builder);
    let frame = egui::Frame::new().fill(p.overlay).stroke(egui::Stroke::new(1.0, p.border)).corner_radius(egui::CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(10, 8));
    let r = frame.show(&mut child, |ui| {
        ui.set_max_width(w - 20.0);
        ui.spacing_mut().item_spacing.y = 3.0;
        match info {
            GainInfo::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(small("Reading the gain map...", p.muted));
                });
            }
            GainInfo::Missing(e) => {
                ui.add(egui::Label::new(small(&e.reason, p.text)).wrap());
                if !e.announced {
                    ui.add(egui::Label::new(small("The gain map views need a JPEG with an Ultra HDR, ISO 21496-1 or Apple gain map.", p.muted)).wrap());
                }
            }
            GainInfo::Ready(map) => {
                for (k, l) in map.legend().iter().enumerate() {
                    ui.add(egui::Label::new(small(l, if k == 0 { p.text } else { p.muted })).wrap());
                }
                if mode == InspectMode::HdrRendition {
                    let max = map.meta.headroom_max();
                    let mut h = headroom.clamp(1.0, max);
                    if max > 1.01 {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(small("Screen headroom", p.text)).on_hover_text("How much brighter than SDR white the screen can show: 1x is a normal (SDR) screen, 4x an HDR screen whose highlights reach four times SDR white. Phones and HDR monitors reach about 2x to 16x.");
                            let slider = super::widgets::Slider::new(&mut h, 1.0..=max).logarithmic(true).fixed_decimals(1).suffix("x").default_value(max).width(100.0).value_width(52.0);
                            if ui.add(slider).on_hover_text("Double-click for the map's full headroom").changed() {
                                *headroom = h;
                            }
                        });
                    }
                    let used = gainmap::weight(&map.meta, h);
                    ui.label(small(&format!("Map applied: {:.0}%", used * 100.0), p.muted)).on_hover_text("How much of the gain map a screen with this headroom uses, from the headroom range stored in the file.");
                    ui.label(small(&format!("Straight curve: {}x peak = white, SDR white = 1/{}", trim_x(h), trim_x(h)), p.muted))
                        .on_hover_text("The HDR version is scaled down so the brightest a screen with this headroom shows is white. The areas the HDR version lifts stay bright; the rest looks darker than in the normal view.");
                }
            }
        }
    });
    r.response.rect
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
            m = m.next_for(true);
        }
        assert_eq!(m, InspectMode::Off);
        assert!(InspectMode::Clipping.is_overlay() && !InspectMode::Luma.is_overlay());
        // Without a gain map the cycle skips the gain map views.
        assert_eq!(InspectMode::Blue.next_for(false), InspectMode::Off);
        assert_eq!(InspectMode::Blue.next_for(true), InspectMode::GainMap);
        assert_eq!(InspectMode::GainMap.next_for(false), InspectMode::Off);
        assert!(InspectMode::HdrRendition.is_gain() && !InspectMode::HdrRendition.is_overlay());
        assert_eq!(trim_x(4.0), "4");
        assert_eq!(trim_x(2.46), "2.5");
    }

    #[test]
    fn gain_views_pass_pixels_through_render() {
        // The gain map views are drawn from their own textures; the per-pixel renderer leaves them alone.
        let img = RgbaImage::from_pixel(2, 1, image::Rgba([10, 20, 30, 255]));
        assert_eq!(render(&img, InspectMode::GainMap), img);
        assert_eq!(render(&img, InspectMode::HdrRendition), img);
    }
}
