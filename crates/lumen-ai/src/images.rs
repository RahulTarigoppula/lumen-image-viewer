//! What pixels are sent, and how model coordinates map back to them.
//!
//! - Overview: the whole image downscaled with Lanczos3 so the long side is
//!   at most `max_side_overview`, both sides multiples of 32 (Qwen3.x uses
//!   16 px patches merged 2x2, so 32 px per visual token; this stops the
//!   server rescaling again). Images already small enough are not resampled,
//!   only padded.
//! - ROI: native pixels. A ROI that fits one tile is sent as is; larger ROIs
//!   are cut into tiles of at most `tile_side` with 32 px overlap. When the
//!   grid needs more tiles than the budget allows, the ROI is downscaled by
//!   the smallest factor that fits. Padding to a multiple of 32 always
//!   replicates edge pixels; it never resamples.
//! - Composite: A | B side by side with a 16 px gap and burned-in labels,
//!   only for servers limited to one image (or when chosen).
//! - Encoding: PNG or JPEG q95 4:4:4 from raw pixels, so no EXIF, XMP, GPS,
//!   ICC or text chunks can leak.
//!
//! Every sent image keeps `Placement`s that map its pixels back to source
//! pixels, so findings (bbox in 0..1000 of the image as sent) can be drawn
//! on the viewer.

use std::io::Cursor;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use base64::Engine;
use fast_image_resize as fr;
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::settings::{CoordConvention, ImageEncoding};

/// Side multiple for everything sent.
pub const ALIGN: u32 = 32;
/// Overlap between neighbouring ROI tiles.
pub const TILE_OVERLAP: u32 = 32;
/// Gap between the halves of a composite.
pub const COMPOSITE_GAP: u32 = 16;
/// Long side of send-preview thumbnails.
pub const THUMB_SIDE: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ImageLabel {
    A,
    B,
}

impl ImageLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageLabel::A => "A",
            ImageLabel::B => "B",
        }
    }
    pub fn other(self) -> ImageLabel {
        match self {
            ImageLabel::A => ImageLabel::B,
            ImageLabel::B => ImageLabel::A,
        }
    }
}

/// A rectangle in source pixels (inclusive start, size).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roi {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Roi {
    pub fn full(w: u32, h: u32) -> Roi {
        Roi { x: 0, y: 0, w, h }
    }

    /// Clips to an image; `None` when nothing is left.
    pub fn clamp_to(self, w: u32, h: u32) -> Option<Roi> {
        let x = self.x.min(w);
        let y = self.y.min(h);
        let x1 = self.x.saturating_add(self.w).min(w);
        let y1 = self.y.saturating_add(self.h).min(h);
        (x1 > x && y1 > y).then_some(Roi { x, y, w: x1 - x, h: y1 - y })
    }

    pub fn to_f(self) -> RectF {
        RectF { x: self.x as f64, y: self.y as f64, w: self.w as f64, h: self.h as f64 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RectF {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl RectF {
    /// The same relative rectangle on an image of another size.
    pub fn rescaled(self, from: (u32, u32), to: (u32, u32)) -> RectF {
        let kx = to.0 as f64 / from.0.max(1) as f64;
        let ky = to.1 as f64 / from.1.max(1) as f64;
        RectF { x: self.x * kx, y: self.y * ky, w: self.w * kx, h: self.h * ky }
    }
}

/// How part of a sent image maps to its source image.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub label: ImageLabel,
    /// Source pixels covered by the content.
    pub src: RectF,
    /// Content size in sent pixels (before padding).
    pub content_w: u32,
    pub content_h: u32,
    /// Top-left of the content inside the sent image (composite halves).
    pub offset_x: u32,
    pub offset_y: u32,
}

impl Placement {
    /// Sent pixel to source pixel; points in the padding clamp to the edge.
    pub fn to_source(&self, x: f64, y: f64) -> (f64, f64) {
        let cx = (x - self.offset_x as f64).clamp(0.0, self.content_w as f64);
        let cy = (y - self.offset_y as f64).clamp(0.0, self.content_h as f64);
        (
            self.src.x + cx * self.src.w / self.content_w.max(1) as f64,
            self.src.y + cy * self.src.h / self.content_h.max(1) as f64,
        )
    }

    /// Source pixel to sent pixel.
    pub fn to_sent(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.offset_x as f64 + (x - self.src.x) * self.content_w as f64 / self.src.w.max(1e-9),
            self.offset_y as f64 + (y - self.src.y) * self.content_h as f64 / self.src.h.max(1e-9),
        )
    }

    fn contains_sent(&self, x: f64, y: f64) -> bool {
        let (ox, oy) = (self.offset_x as f64, self.offset_y as f64);
        x >= ox && y >= oy && x <= ox + self.content_w as f64 && y <= oy + self.content_h as f64
    }

    /// Source pixels per sent pixel (1 = native).
    pub fn downscale(&self) -> f64 {
        (self.src.w / self.content_w.max(1) as f64).max(self.src.h / self.content_h.max(1) as f64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageRole {
    Overview,
    /// 1-based index.
    RoiTile { index: u32, count: u32 },
    Composite,
}

/// One image as it will be sent, before encoding.
#[derive(Clone, Debug)]
pub struct SentImage {
    pub role: ImageRole,
    /// `None` for a composite (both).
    pub label: Option<ImageLabel>,
    /// The tile number findings refer to: 0 for overview or composite.
    pub tile: u32,
    pub pixels: RgbaImage,
    pub regions: Vec<Placement>,
    /// For the send preview and the text part before the image.
    pub description: String,
}

impl SentImage {
    /// The pixel-free part, kept with results to map findings.
    pub fn layout(&self) -> SentLayout {
        SentLayout {
            role: self.role,
            label: self.label,
            tile: self.tile,
            width: self.pixels.width(),
            height: self.pixels.height(),
            regions: self.regions.clone(),
            description: self.description.clone(),
        }
    }

    /// Maps a model bbox to source pixels (see `SentLayout::map_bbox`).
    pub fn map_bbox(&self, hint: ImageLabel, bbox: [f64; 4], conv: CoordConvention) -> Option<SourceBox> {
        self.layout().map_bbox(hint, bbox, conv)
    }
}

/// Where one sent image came from, without its pixels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SentLayout {
    pub role: ImageRole,
    pub label: Option<ImageLabel>,
    pub tile: u32,
    /// Sent size, padding included.
    pub width: u32,
    pub height: u32,
    pub regions: Vec<Placement>,
    pub description: String,
}

impl SentLayout {
    /// Maps a model bbox (in `conv` coordinates of this sent image) to
    /// source pixels. `hint` is the image the model named; in a composite
    /// the half containing the box center wins.
    pub fn map_bbox(&self, hint: ImageLabel, bbox: [f64; 4], conv: CoordConvention) -> Option<SourceBox> {
        let (w, h) = (self.width as f64, self.height as f64);
        let k = |v: f64, side: f64| match conv {
            CoordConvention::Relative1000 => v.clamp(0.0, 1000.0) / 1000.0 * side,
            CoordConvention::AbsolutePixels => v.clamp(0.0, side),
        };
        let (x0, x1) = (k(bbox[0].min(bbox[2]), w), k(bbox[0].max(bbox[2]), w));
        let (y0, y1) = (k(bbox[1].min(bbox[3]), h), k(bbox[1].max(bbox[3]), h));
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let region = if self.regions.len() == 1 {
            &self.regions[0]
        } else {
            self.regions
                .iter()
                .find(|r| r.contains_sent(cx, cy))
                .or_else(|| self.regions.iter().find(|r| r.label == hint))
                .or(self.regions.first())?
        };
        let (sx0, sy0) = region.to_source(x0, y0);
        let (sx1, sy1) = region.to_source(x1, y1);
        Some(SourceBox { label: region.label, x0: sx0, y0: sy0, x1: sx1, y1: sy1 })
    }
}

/// A finding's box in source pixels of image `label`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceBox {
    pub label: ImageLabel,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

pub fn round_up(v: u32, m: u32) -> u32 {
    v.div_ceil(m) * m
}

/// Pads right and bottom to a multiple of `m` by repeating edge pixels.
pub fn pad_to_multiple(img: &RgbaImage, m: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (pw, ph) = (round_up(w.max(1), m), round_up(h.max(1), m));
    if (pw, ph) == (w, h) {
        return img.clone();
    }
    let mut out = RgbaImage::new(pw, ph);
    for y in 0..ph {
        let sy = y.min(h - 1);
        let src_row = &img.as_raw()[(sy * w * 4) as usize..((sy + 1) * w * 4) as usize];
        let dst_row = &mut out.as_mut()[(y * pw * 4) as usize..((y + 1) * pw * 4) as usize];
        dst_row[..src_row.len()].copy_from_slice(src_row);
        let last = [src_row[src_row.len() - 4], src_row[src_row.len() - 3], src_row[src_row.len() - 2], src_row[src_row.len() - 1]];
        for px in dst_row[src_row.len()..].as_chunks_mut::<4>().0 {
            px.copy_from_slice(&last);
        }
    }
    out
}

/// Cuts `src` (source pixels) out of `img` at `out_w` x `out_h`: a plain
/// crop when the sizes match on whole pixels, Lanczos3 otherwise.
pub fn extract(img: &RgbaImage, src: RectF, out_w: u32, out_h: u32) -> RgbaImage {
    let whole = |v: f64| (v - v.round()).abs() < 1e-9;
    if whole(src.x) && whole(src.y) && src.w.round() as u32 == out_w && src.h.round() as u32 == out_h
        && whole(src.w) && whole(src.h)
    {
        return image::imageops::crop_imm(img, src.x as u32, src.y as u32, out_w, out_h).to_image();
    }
    let mut dst = RgbaImage::new(out_w.max(1), out_h.max(1));
    let opts = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3))
        .crop(src.x, src.y, src.w, src.h)
        .use_alpha(true);
    if fr::Resizer::new().resize(img, &mut dst, &opts).is_err() {
        let crop = image::imageops::crop_imm(
            img,
            src.x as u32,
            src.y as u32,
            (src.w as u32).max(1),
            (src.h as u32).max(1),
        )
        .to_image();
        return image::imageops::resize(&crop, out_w.max(1), out_h.max(1), image::imageops::FilterType::Lanczos3);
    }
    dst
}

/// Output size of an overview: unchanged when it fits, else long side
/// `max_side` rounded down to 32 and short side rounded to 32.
pub fn overview_size(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let max_side = (max_side / ALIGN * ALIGN).max(ALIGN);
    let long = w.max(h);
    if long <= max_side {
        return (w, h);
    }
    let s = max_side as f64 / long as f64;
    let r = |v: u32| (((v as f64 * s) / ALIGN as f64).round() as u32 * ALIGN).clamp(ALIGN, max_side);
    if w >= h {
        (max_side, r(h))
    } else {
        (r(w), max_side)
    }
}

/// ", padded to 800x1216 (edge pixels repeated at the bottom)" when the
/// content is padded to a multiple of 32, else "". The model's coordinates
/// are relative to the padded image, so it has to know the content ends
/// before the edge.
fn pad_note(content: (u32, u32), padded: (u32, u32)) -> String {
    let where_ = match (padded.0 > content.0, padded.1 > content.1) {
        (true, true) => "right and bottom",
        (true, false) => "right",
        (false, true) => "bottom",
        (false, false) => return String::new(),
    };
    format!(", padded to {}x{} (edge pixels repeated at the {where_})", padded.0, padded.1)
}

fn fmt_factor(f: f64) -> String {
    let s = format!("{f:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}x")
}

/// The full-view image of one side.
pub fn overview(img: &RgbaImage, label: ImageLabel, max_side: u32) -> SentImage {
    let (w, h) = img.dimensions();
    let (tw, th) = overview_size(w, h, max_side);
    let content = if (tw, th) == (w, h) { img.clone() } else { extract(img, Roi::full(w, h).to_f(), tw, th) };
    let factor = (w as f64 / tw as f64).max(h as f64 / th as f64);
    let pixels = pad_to_multiple(&content, ALIGN);
    let pad = pad_note((tw, th), pixels.dimensions());
    let description = if (tw, th) == (w, h) {
        format!("{} full view, {}x{}, native resolution{pad}", label.as_str(), w, h)
    } else {
        format!("{} full view, {}x{}, downscaled {} from {}x{}{pad}", label.as_str(), tw, th, fmt_factor(factor), w, h)
    };
    SentImage {
        role: ImageRole::Overview,
        label: Some(label),
        tile: 0,
        pixels,
        regions: vec![Placement {
            label,
            src: Roi::full(w, h).to_f(),
            content_w: tw,
            content_h: th,
            offset_x: 0,
            offset_y: 0,
        }],
        description,
    }
}

/// Tile layout for a ROI.
#[derive(Clone, Debug, PartialEq)]
pub struct GridPlan {
    /// Source pixels per sent pixel (1 = native).
    pub factor: f64,
    /// ROI size after scaling.
    pub scaled_w: u32,
    pub scaled_h: u32,
    /// Tiles in scaled ROI coordinates: x, y, w, h.
    pub tiles: Vec<(u32, u32, u32, u32)>,
}

fn tiles_along(len: u32, side: u32) -> u32 {
    if len <= side {
        1
    } else {
        (len - TILE_OVERLAP).div_ceil(side - TILE_OVERLAP)
    }
}

fn positions(len: u32, side: u32, n: u32) -> Vec<(u32, u32)> {
    let t = side.min(len);
    if n <= 1 {
        return vec![(0, t)];
    }
    let step = (len - t) as f64 / (n - 1) as f64;
    (0..n).map(|i| ((i as f64 * step).round() as u32, t)).collect()
}

/// Splits a `w` x `h` ROI into at most `max_tiles` tiles of at most `side`
/// with `TILE_OVERLAP` overlap, downscaling by the smallest factor that fits.
pub fn plan_grid(w: u32, h: u32, side: u32, max_tiles: u32) -> GridPlan {
    let side = (side / ALIGN * ALIGN).max(2 * TILE_OVERLAP + ALIGN);
    let max_tiles = max_tiles.max(1);
    let cap = |n: u32| (n * (side - TILE_OVERLAP) + TILE_OVERLAP) as f64;
    let (mut factor, mut sw, mut sh) = (1.0f64, w, h);
    if tiles_along(w, side) * tiles_along(h, side) > max_tiles {
        factor = (1..=max_tiles)
            .map(|nx| {
                let ny = max_tiles / nx;
                (w as f64 / cap(nx)).max(h as f64 / cap(ny)).max(1.0)
            })
            .fold(f64::INFINITY, f64::min);
        sw = ((w as f64 / factor).floor() as u32).max(1);
        sh = ((h as f64 / factor).floor() as u32).max(1);
        // Guard against float edge cases: shrink until it fits.
        while tiles_along(sw, side) * tiles_along(sh, side) > max_tiles {
            factor *= 1.01;
            sw = ((w as f64 / factor).floor() as u32).max(1);
            sh = ((h as f64 / factor).floor() as u32).max(1);
        }
        factor = (w as f64 / sw as f64).max(h as f64 / sh as f64);
    }
    let xs = positions(sw, side, tiles_along(sw, side));
    let ys = positions(sh, side, tiles_along(sh, side));
    let tiles = ys.iter().flat_map(|&(y, th)| xs.iter().map(move |&(x, tw)| (x, y, tw, th))).collect();
    GridPlan { factor, scaled_w: sw, scaled_h: sh, tiles }
}

/// ROI tiles of one image.
pub fn roi_tiles(img: &RgbaImage, label: ImageLabel, roi: Roi, side: u32, max_tiles: u32) -> Vec<SentImage> {
    let Some(roi) = roi.clamp_to(img.width(), img.height()) else { return Vec::new() };
    let plan = plan_grid(roi.w, roi.h, side, max_tiles);
    let kx = roi.w as f64 / plan.scaled_w as f64;
    let ky = roi.h as f64 / plan.scaled_h as f64;
    let count = plan.tiles.len() as u32;
    plan.tiles
        .iter()
        .enumerate()
        .map(|(i, &(x, y, tw, th))| {
            let src = RectF { x: roi.x as f64 + x as f64 * kx, y: roi.y as f64 + y as f64 * ky, w: tw as f64 * kx, h: th as f64 * ky };
            let content = extract(img, src, tw, th);
            let index = i as u32 + 1;
            let (x0, y0) = (src.x.round() as u64, src.y.round() as u64);
            let (x1, y1) = ((src.x + src.w).round() as u64, (src.y + src.h).round() as u64);
            let pixels = pad_to_multiple(&content, ALIGN);
            let pad = pad_note((tw, th), pixels.dimensions());
            let description = if plan.factor <= 1.0 + 1e-9 {
                format!("{} ROI tile {index}/{count}: native pixels x={x0}..{x1}, y={y0}..{y1}{pad}", label.as_str())
            } else {
                format!(
                    "{} ROI tile {index}/{count}: source pixels x={x0}..{x1}, y={y0}..{y1}, downscaled {}{pad}",
                    label.as_str(),
                    fmt_factor(plan.factor)
                )
            };
            SentImage {
                role: ImageRole::RoiTile { index, count },
                label: Some(label),
                tile: index,
                pixels,
                regions: vec![Placement { label, src, content_w: tw, content_h: th, offset_x: 0, offset_y: 0 }],
                description,
            }
        })
        .collect()
}

/// B's counterpart of A's images: the same grid and sent sizes over the
/// same relative region (identical when A and B have equal dimensions).
pub fn matching(b: &RgbaImage, a_images: &[SentImage], a_size: (u32, u32)) -> Vec<SentImage> {
    let same = b.dimensions() == a_size;
    a_images
        .iter()
        .map(|ai| {
            let pa = ai.regions[0];
            let src = pa.src.rescaled(a_size, b.dimensions());
            let content = extract(b, src, pa.content_w, pa.content_h);
            let pixels = pad_to_multiple(&content, ALIGN);
            let pad = pad_note((pa.content_w, pa.content_h), pixels.dimensions());
            let description = match ai.role {
                ImageRole::RoiTile { index, count } if same => format!("B ROI tile {index}/{count}: same region"),
                ImageRole::RoiTile { index, count } => format!(
                    "B ROI tile {index}/{count}: same relative region, source pixels x={}..{}, y={}..{} of {}x{}{pad}",
                    src.x.round(),
                    (src.x + src.w).round(),
                    src.y.round(),
                    (src.y + src.h).round(),
                    b.width(),
                    b.height()
                ),
                _ if same => format!("B full view, same scaling{pad}"),
                _ => format!(
                    "B full view, {}x{}, resampled from {}x{} to match A{pad}",
                    pa.content_w,
                    pa.content_h,
                    b.width(),
                    b.height()
                ),
            };
            SentImage {
                role: ai.role,
                label: Some(ImageLabel::B),
                tile: ai.tile,
                pixels,
                regions: vec![Placement { label: ImageLabel::B, src, ..pa }],
                description,
            }
        })
        .collect()
}

/// A | B side by side with a gap and burned-in labels. `region_*` are the
/// source rectangles to show (full image or ROI). The whole composite
/// fits `max_side`, so each half gets about half the resolution.
pub fn composite(a: &RgbaImage, region_a: RectF, b: &RgbaImage, region_b: RectF, max_side: u32) -> SentImage {
    let max_side = (max_side / ALIGN * ALIGN).max(4 * ALIGN);
    let half_w = (max_side - COMPOSITE_GAP) / 2;
    let fit = |r: RectF| {
        let s = (half_w as f64 / r.w).min(max_side as f64 / r.h).min(1.0);
        (((r.w * s).round() as u32).max(1), ((r.h * s).round() as u32).max(1))
    };
    let (aw, ah) = fit(region_a);
    let (bw, bh) = fit(region_b);
    let ca = extract(a, region_a, aw, ah);
    let cb = extract(b, region_b, bw, bh);
    let w = round_up(aw + COMPOSITE_GAP + bw, ALIGN);
    let h = round_up(ah.max(bh), ALIGN);
    let mut out = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
    image::imageops::replace(&mut out, &ca, 0, 0);
    let bx = aw + COMPOSITE_GAP;
    image::imageops::replace(&mut out, &cb, bx as i64, 0);
    let px = (h as f32 / 14.0).clamp(20.0, 64.0);
    draw_label(&mut out, 0, 0, "A", px);
    draw_label(&mut out, bx, 0, "B", px);
    let factor = |r: RectF, cw: u32| r.w / cw as f64;
    SentImage {
        role: ImageRole::Composite,
        label: None,
        tile: 0,
        pixels: out,
        regions: vec![
            Placement { label: ImageLabel::A, src: region_a, content_w: aw, content_h: ah, offset_x: 0, offset_y: 0 },
            Placement { label: ImageLabel::B, src: region_b, content_w: bw, content_h: bh, offset_x: bx, offset_y: 0 },
        ],
        description: format!(
            "Composite {}x{}: A on the left ({}x{}, downscaled {}), B on the right ({}x{}, downscaled {}), {} px black gap, labels burned in at the top-left of each half",
            w,
            h,
            aw,
            ah,
            fmt_factor(factor(region_a, aw)),
            bw,
            bh,
            fmt_factor(factor(region_b, bw)),
            COMPOSITE_GAP
        ),
    }
}

/// White text on a dark box at (x, y).
fn draw_label(img: &mut RgbaImage, x: u32, y: u32, text: &str, px: f32) {
    let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).expect("built-in font");
    let scaled = font.as_scaled(PxScale::from(px));
    let pad = (px / 4.0).round() as u32;
    let text_w: f32 = text.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum();
    let box_w = text_w.ceil() as u32 + 2 * pad;
    let box_h = (scaled.ascent() - scaled.descent()).ceil() as u32 + 2 * pad;
    let (iw, ih) = img.dimensions();
    for by in y..(y + box_h).min(ih) {
        for bx in x..(x + box_w).min(iw) {
            let p = img.get_pixel_mut(bx, by);
            for c in 0..3 {
                p.0[c] = (p.0[c] as u32 * 40 / 255) as u8;
            }
            p.0[3] = 255;
        }
    }
    let mut pen = x as f32 + pad as f32;
    let base = y as f32 + pad as f32 + scaled.ascent();
    for c in text.chars() {
        let id = font.glyph_id(c);
        let g = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(pen, base));
        pen += scaled.h_advance(id);
        if let Some(o) = font.outline_glyph(g) {
            let b = o.px_bounds();
            o.draw(|gx, gy, cov| {
                let (px_x, px_y) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                if px_x >= 0 && px_y >= 0 && (px_x as u32) < iw && (px_y as u32) < ih {
                    let p = img.get_pixel_mut(px_x as u32, px_y as u32);
                    for ch in 0..3 {
                        p.0[ch] = (p.0[ch] as f32 * (1.0 - cov) + 255.0 * cov).round() as u8;
                    }
                }
            });
        }
    }
}

/// An encoded image ready for a data URL.
#[derive(Clone, Debug)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

impl Encoded {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, base64::engine::general_purpose::STANDARD.encode(&self.bytes))
    }
}

/// RGB over mid gray (transparency means nothing to a quality judgement,
/// and black or white would read as content).
fn flatten(img: &RgbaImage) -> (Vec<u8>, bool) {
    let mut had_alpha = false;
    let mut out = Vec::with_capacity((img.width() * img.height() * 3) as usize);
    for p in img.as_raw().as_chunks::<4>().0 {
        let a = p[3] as u32;
        if a == 255 {
            out.extend_from_slice(&p[..3]);
        } else {
            had_alpha = true;
            for &c in &p[..3] {
                out.push(((c as u32 * a + 128 * (255 - a) + 127) / 255) as u8);
            }
        }
    }
    (out, had_alpha)
}

/// Encodes with Lumen's own encoders, which write pixels only: no EXIF,
/// XMP, ICC, text or time chunks. Returns whether transparency was flattened.
pub fn encode(img: &RgbaImage, enc: ImageEncoding) -> Result<(Encoded, bool), AiError> {
    use image::ImageEncoder;
    let (w, h) = img.dimensions();
    let (rgb, had_alpha) = flatten(img);
    let mut out = Vec::new();
    match enc {
        ImageEncoding::Png => {
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            PngEncoder::new_with_quality(Cursor::new(&mut out), CompressionType::Default, FilterType::Adaptive)
                .write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .map_err(|e| AiError::Image(e.to_string()))?;
            Ok((Encoded { bytes: out, mime: "image/png" }, had_alpha))
        }
        ImageEncoding::Jpeg => {
            if w > u16::MAX as u32 || h > u16::MAX as u32 {
                return Err(AiError::Image("image too large for JPEG".into()));
            }
            let mut e = jpeg_encoder::Encoder::new(&mut out, 95);
            e.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
            e.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| AiError::Image(e.to_string()))?;
            Ok((Encoded { bytes: out, mime: "image/jpeg" }, had_alpha))
        }
    }
}

/// Send-preview thumbnail (long side at most `THUMB_SIDE`).
pub fn thumbnail(img: &RgbaImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let s = (THUMB_SIDE as f64 / w.max(h) as f64).min(1.0);
    if s >= 1.0 {
        return img.clone();
    }
    let (tw, th) = (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1));
    let mut dst = RgbaImage::new(tw, th);
    let opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear)).use_alpha(true);
    if fr::Resizer::new().resize(img, &mut dst, &opts).is_err() {
        return image::imageops::thumbnail(img, tw, th);
    }
    dst
}

/// The 64x64 mid-gray PNG of the vision probe.
pub fn probe_png() -> Encoded {
    let img = RgbaImage::from_pixel(64, 64, Rgba([128, 128, 128, 255]));
    encode(&img, ImageEncoding::Png).expect("encode probe").0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]))
    }

    #[test]
    fn overview_sizes_are_multiples_of_32() {
        assert_eq!(overview_size(3840, 2160, 1536), (1536, 864));
        assert_eq!(overview_size(2160, 3840, 1536), (864, 1536));
        assert_eq!(overview_size(1000, 700, 1536), (1000, 700));
        assert_eq!(overview_size(6000, 100, 1536), (1536, 32));
        let (w, h) = overview_size(4000, 3000, 1500);
        assert_eq!((w % 32, h % 32), (0, 0));
        assert_eq!(w, 1472);
        let o = overview(&gradient(3840, 2160), ImageLabel::A, 1536);
        assert_eq!(o.pixels.dimensions(), (1536, 864));
        assert!(o.description.contains("downscaled 2.5x from 3840x2160"), "{}", o.description);
        let small = overview(&gradient(1000, 700), ImageLabel::A, 1536);
        assert_eq!(small.pixels.dimensions(), (1024, 704));
        assert_eq!(small.regions[0].content_w, 1000);
        // Padding is named, since box coordinates are relative to the padded image.
        let tall = overview(&gradient(800, 1200), ImageLabel::A, 1536);
        assert_eq!(tall.description, "A full view, 800x1200, native resolution, padded to 800x1216 (edge pixels repeated at the bottom)");
        let bottom = tall.map_bbox(ImageLabel::A, [0.0, 0.0, 1000.0, 1200.0 / 1216.0 * 1000.0], CoordConvention::Relative1000).unwrap();
        assert!((bottom.y1 - 1200.0).abs() < 1e-6, "{bottom:?}");
    }

    #[test]
    fn padding_replicates_edges() {
        let img = gradient(33, 2);
        let p = pad_to_multiple(&img, 32);
        assert_eq!(p.dimensions(), (64, 32));
        assert_eq!(p.get_pixel(63, 0), img.get_pixel(32, 0));
        assert_eq!(p.get_pixel(10, 31), img.get_pixel(10, 1));
        assert_eq!(p.get_pixel(63, 31), img.get_pixel(32, 1));
        assert_eq!(p.get_pixel(5, 1), img.get_pixel(5, 1));
    }

    #[test]
    fn grid_native_when_budget_allows() {
        let g = plan_grid(800, 600, 1024, 4);
        assert_eq!((g.factor, g.tiles.len()), (1.0, 1));
        let g = plan_grid(2000, 900, 1024, 4);
        assert_eq!(g.factor, 1.0);
        assert_eq!(g.tiles, vec![(0, 0, 1024, 900), (976, 0, 1024, 900)]);
        // Overlap is at least 32 everywhere.
        let g = plan_grid(3000, 2048, 1024, 16);
        assert_eq!(g.factor, 1.0);
        for w in g.tiles.windows(2) {
            if w[0].1 == w[1].1 {
                assert!(w[0].0 + w[0].2 >= w[1].0 + TILE_OVERLAP);
            }
        }
        let last = g.tiles.last().unwrap();
        assert_eq!((last.0 + last.2, last.1 + last.3), (3000, 2048));
    }

    #[test]
    fn grid_downscales_to_budget() {
        let g = plan_grid(4000, 4000, 1024, 4);
        assert!(g.tiles.len() <= 4);
        assert!(g.factor > 1.0);
        // The smallest factor for a 2x2 grid: 4000 / (2*992+32) = 1.984...
        assert!((g.factor - 4000.0 / 2016.0).abs() < 0.01, "{}", g.factor);
        let g = plan_grid(8000, 1000, 1024, 2);
        assert!(g.tiles.len() <= 2);
        assert!(g.tiles.iter().all(|t| t.2 <= 1024 && t.3 <= 1024));
        let g = plan_grid(5000, 5000, 1024, 1);
        assert_eq!(g.tiles.len(), 1);
        assert!(g.scaled_w <= 1024 && g.scaled_h <= 1024);
    }

    #[test]
    fn tiles_are_native_and_described() {
        let img = gradient(3000, 1500);
        let t = roi_tiles(&img, ImageLabel::A, Roi { x: 1200, y: 400, w: 1024, h: 1024 }, 1024, 4);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].pixels.dimensions(), (1024, 1024));
        assert_eq!(t[0].pixels.get_pixel(0, 0), img.get_pixel(1200, 400));
        assert_eq!(t[0].description, "A ROI tile 1/1: native pixels x=1200..2224, y=400..1424");
        let t = roi_tiles(&img, ImageLabel::A, Roi { x: 10, y: 10, w: 50, h: 40 }, 1024, 4);
        assert_eq!(t[0].pixels.dimensions(), (64, 64));
        assert_eq!(t[0].pixels.get_pixel(63, 63), img.get_pixel(59, 49));
        let t = roi_tiles(&img, ImageLabel::A, Roi::full(3000, 1500), 1024, 2);
        assert_eq!(t.len(), 2);
        assert!(t[0].description.contains("downscaled"), "{}", t[0].description);
    }

    #[test]
    fn b_matches_a_grid() {
        let a = gradient(2000, 1000);
        let b = gradient(2000, 1000);
        let ta = roi_tiles(&a, ImageLabel::A, Roi { x: 0, y: 0, w: 2000, h: 1000 }, 1024, 2);
        let tb = matching(&b, &ta, a.dimensions());
        assert_eq!(ta.len(), tb.len());
        for (x, y) in ta.iter().zip(&tb) {
            assert_eq!(x.pixels, y.pixels);
            assert_eq!(x.regions[0].src, y.regions[0].src);
            assert_eq!(y.label, Some(ImageLabel::B));
        }
        assert_eq!(tb[0].description, "B ROI tile 1/2: same region");
        // B of another size: same sent size, proportional region.
        let b2 = gradient(1000, 500);
        let tb2 = matching(&b2, &ta, a.dimensions());
        assert_eq!(tb2[0].pixels.dimensions(), ta[0].pixels.dimensions());
        assert_eq!(tb2[1].regions[0].src.x, ta[1].regions[0].src.x / 2.0);
    }

    #[test]
    fn coordinates_round_trip() {
        let img = gradient(3840, 2160);
        let mut sent = vec![overview(&img, ImageLabel::A, 1536)];
        sent.extend(roi_tiles(&img, ImageLabel::A, Roi { x: 1000, y: 500, w: 2500, h: 1300 }, 1024, 4));
        sent.extend(roi_tiles(&img, ImageLabel::A, Roi { x: 100, y: 100, w: 300, h: 200 }, 1024, 4));
        for s in &sent {
            let p = s.regions[0];
            let (sw, sh) = (s.pixels.width() as f64, s.pixels.height() as f64);
            for &(fx, fy) in &[(0.0, 0.0), (0.3, 0.7), (1.0, 1.0), (0.5, 0.5)] {
                let (sx, sy) = (p.src.x + fx * p.src.w, p.src.y + fy * p.src.h);
                let (tx, ty) = p.to_sent(sx, sy);
                // To model coordinates and back.
                let (rx, ry) = ((tx / sw * 1000.0).round(), (ty / sh * 1000.0).round());
                let b = s.map_bbox(ImageLabel::A, [rx, ry, rx, ry], CoordConvention::Relative1000).unwrap();
                let tol_x = sw / 1000.0 * p.src.w / p.content_w as f64 + 1e-6;
                let tol_y = sh / 1000.0 * p.src.h / p.content_h as f64 + 1e-6;
                assert!((b.x0 - sx).abs() <= tol_x, "{} x {} vs {}", s.description, b.x0, sx);
                assert!((b.y0 - sy).abs() <= tol_y, "{} y {} vs {}", s.description, b.y0, sy);
                // Absolute pixels are exact.
                let a = s.map_bbox(ImageLabel::A, [tx, ty, tx, ty], CoordConvention::AbsolutePixels).unwrap();
                assert!((a.x0 - sx).abs() < 1e-6 && (a.y0 - sy).abs() < 1e-6);
            }
            // Padding clamps to the content edge.
            let far = s.map_bbox(ImageLabel::A, [1000.0, 1000.0, 1000.0, 1000.0], CoordConvention::Relative1000).unwrap();
            assert!((far.x1 - (p.src.x + p.src.w)).abs() < 1e-6);
        }
    }

    #[test]
    fn composite_maps_halves() {
        let a = gradient(1920, 1080);
        let b = gradient(1920, 1080);
        let c = composite(&a, Roi::full(1920, 1080).to_f(), &b, Roi::full(1920, 1080).to_f(), 1536);
        let (w, h) = c.pixels.dimensions();
        assert!(w <= 1536 && w % 32 == 0 && h % 32 == 0);
        // Label boxes are dark with white glyph pixels.
        let mut white = 0;
        for y in 0..30 {
            for x in 0..30 {
                if c.pixels.get_pixel(x, y).0[0] > 200 {
                    white += 1;
                }
            }
        }
        assert!(white > 10, "label A drawn");
        // Right half center maps to B's center.
        let rb = c.regions[1];
        let cx = rb.offset_x as f64 + rb.content_w as f64 / 2.0;
        let rel = |v: f64, s: u32| v / s as f64 * 1000.0;
        let bx = rel(cx, w);
        let by = rel(rb.content_h as f64 / 2.0, h);
        let m = c.map_bbox(ImageLabel::A, [bx, by, bx, by], CoordConvention::Relative1000).unwrap();
        assert_eq!(m.label, ImageLabel::B, "the box center decides the half");
        assert!((m.x0 - 960.0).abs() < 4.0 && (m.y0 - 540.0).abs() < 4.0, "{m:?}");
    }

    fn png_chunks(b: &[u8]) -> Vec<String> {
        assert_eq!(&b[..8], b"\x89PNG\r\n\x1a\n");
        let mut i = 8;
        let mut out = Vec::new();
        while i + 8 <= b.len() {
            let len = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            out.push(String::from_utf8_lossy(&b[i + 4..i + 8]).into_owned());
            i += 12 + len;
        }
        out
    }

    fn jpeg_markers(b: &[u8]) -> Vec<u8> {
        assert_eq!(&b[..2], &[0xFF, 0xD8]);
        let mut i = 2;
        let mut out = Vec::new();
        while i + 4 <= b.len() && b[i] == 0xFF {
            let m = b[i + 1];
            out.push(m);
            if m == 0xDA {
                break;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            i += 2 + len;
        }
        out
    }

    #[test]
    fn encoded_images_carry_no_metadata() {
        let mut img = gradient(100, 60);
        img.put_pixel(3, 3, Rgba([10, 20, 30, 0]));
        let (png, had_alpha) = encode(&img, ImageEncoding::Png).unwrap();
        assert!(had_alpha);
        let chunks = png_chunks(&png.bytes);
        for bad in ["eXIf", "iTXt", "tEXt", "zTXt", "iCCP", "tIME", "sRGB", "gAMA", "cHRM", "pHYs"] {
            assert!(!chunks.iter().any(|c| c == bad), "PNG has {bad}: {chunks:?}");
        }
        assert!(chunks.iter().all(|c| ["IHDR", "IDAT", "IEND"].contains(&c.as_str())), "{chunks:?}");
        let dec = image::load_from_memory(&png.bytes).unwrap().to_rgb8();
        assert_eq!(dec.get_pixel(3, 3).0, [128, 128, 128]);
        assert_eq!(dec.get_pixel(50, 20).0, [50, 20, 70]);

        let (jpg, _) = encode(&img, ImageEncoding::Jpeg).unwrap();
        assert_eq!(jpg.mime, "image/jpeg");
        let markers = jpeg_markers(&jpg.bytes);
        for bad in [0xE1u8, 0xE2, 0xED, 0xFE] {
            assert!(!markers.contains(&bad), "JPEG has APP/COM marker {bad:X}: {markers:X?}");
        }
        // 4:4:4: every component samples 1x1 in SOF.
        let sof = jpg.bytes.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        let n = jpg.bytes[sof + 9] as usize;
        for c in 0..n {
            assert_eq!(jpg.bytes[sof + 11 + c * 3], 0x11);
        }
        assert!(png.data_url().starts_with("data:image/png;base64,iVBORw0"));
    }

    #[test]
    fn thumbnails_and_probe() {
        assert_eq!(thumbnail(&gradient(1024, 512)).dimensions(), (256, 128));
        assert_eq!(thumbnail(&gradient(100, 50)).dimensions(), (100, 50));
        let p = probe_png();
        assert_eq!(image::load_from_memory(&p.bytes).unwrap().width(), 64);
    }
}
