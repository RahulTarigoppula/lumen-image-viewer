//! Advanced batch pipeline: an ordered list of image steps applied to every
//! file (crop, canvas, rotate, adjust, sharpen, border, text, watermark...),
//! named presets and DPI metadata patching. No UI code lives here.
//!
//! Every pixel quantity is multiplied by `StepCtx::scale`, so the live preview
//! can run the same pipeline on a reduced copy of the image and still look
//! like the full-size result.

use super::logic::{self, ResizeSpec, TemplateCtx};
use crate::edit::{effects, filters, geom, ops, text};
use crate::formats::SaveFormat;
use crate::imaging::Filter;
use image::RgbaImage;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// ------------------------------------------------------------------ context

/// Per-file values for text tokens plus the preview scale.
#[derive(Clone, Debug, Default)]
pub struct StepCtx {
    pub name: String,
    pub ext: String,
    pub folder: String,
    pub n: i64,
    pub modified: Option<chrono::NaiveDateTime>,
    pub taken: Option<chrono::NaiveDateTime>,
    /// 1.0 for real output; < 1 when rendering a reduced preview.
    pub scale: f32,
}

impl StepCtx {
    pub fn for_file(path: &Path, n: i64, taken: Option<chrono::NaiveDateTime>) -> StepCtx {
        let info = logic::NameInfo::from_path(path);
        let modified = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()).map(logic::systime_to_naive);
        StepCtx { name: info.stem, ext: info.ext, folder: info.folder, n, modified, taken, scale: 1.0 }
    }

    /// Expand `{name}`, `{date}`, `{taken}`, `{n}`, `{width}`... in overlay text.
    pub fn expand(&self, template: &str, w: u32, h: u32) -> String {
        let s = self.scale.max(1e-3);
        let cx = TemplateCtx {
            name: &self.name,
            ext: &self.ext,
            n: self.n,
            pad: 1,
            modified: self.modified,
            taken: self.taken,
            width: Some((w as f32 / s).round() as u32),
            height: Some((h as f32 / s).round() as u32),
            folder: &self.folder,
        };
        logic::expand(template, &cx)
    }
}

// -------------------------------------------------------------------- steps

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CropMode {
    /// A rectangle given by its left/top offset and size in pixels.
    Rect,
    /// The largest area with the given aspect ratio.
    Aspect,
    /// A fixed-size area placed by the anchor (centered by default).
    Size,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CropStep {
    pub mode: CropMode,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub aspect_w: f32,
    pub aspect_h: f32,
    /// 0..8, 3x3 grid; 4 = centre.
    pub anchor: u8,
}

impl Default for CropStep {
    fn default() -> Self {
        CropStep { mode: CropMode::Aspect, x: 0, y: 0, w: 1000, h: 1000, aspect_w: 3.0, aspect_h: 2.0, anchor: 4 }
    }
}

/// The crop rectangle (x, y, w, h) for an image of `w` x `h`.
pub fn crop_rect(w: u32, h: u32, c: &CropStep, scale: f32) -> (u32, u32, u32, u32) {
    let s = scale.max(1e-3);
    let sc = |v: u32| ((v as f32 * s).round() as u32).max(1);
    let place = |cw: u32, ch: u32| {
        let cw = cw.clamp(1, w.max(1));
        let ch = ch.clamp(1, h.max(1));
        let (fx, fy) = geom::anchor_frac(c.anchor);
        let x = ((w - cw) as f32 * fx).round() as u32;
        let y = ((h - ch) as f32 * fy).round() as u32;
        (x, y, cw, ch)
    };
    match c.mode {
        CropMode::Rect => {
            let x = ((c.x as f32 * s).round() as u32).min(w.saturating_sub(1));
            let y = ((c.y as f32 * s).round() as u32).min(h.saturating_sub(1));
            (x, y, sc(c.w).min(w - x), sc(c.h).min(h - y))
        }
        CropMode::Size => place(sc(c.w), sc(c.h)),
        CropMode::Aspect => {
            let (aw, ah) = (c.aspect_w.max(0.01), c.aspect_h.max(0.01));
            let r = aw / ah;
            let (wf, hf) = (w as f32, h as f32);
            if wf / hf > r {
                place((hf * r).round() as u32, h)
            } else {
                place(w, (wf / r).round() as u32)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CanvasMode {
    /// A fixed canvas size in pixels.
    Absolute,
    /// Add pixels to every side.
    Add,
    /// Width and height as a percentage of the image.
    Percent,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CanvasStep {
    pub mode: CanvasMode,
    pub w: u32,
    pub h: u32,
    /// Pixels per side for `Add`.
    pub add: u32,
    pub percent_w: f32,
    pub percent_h: f32,
    pub anchor: u8,
    pub color: [u8; 4],
}

impl Default for CanvasStep {
    fn default() -> Self {
        CanvasStep { mode: CanvasMode::Add, w: 2000, h: 2000, add: 40, percent_w: 110.0, percent_h: 110.0, anchor: 4, color: [255, 255, 255, 255] }
    }
}

pub fn canvas_size(w: u32, h: u32, c: &CanvasStep, scale: f32) -> (u32, u32) {
    let s = scale.max(1e-3) as f64;
    match c.mode {
        CanvasMode::Absolute => (geom::clamp_dim(c.w as f64 * s), geom::clamp_dim(c.h as f64 * s)),
        CanvasMode::Add => {
            let a = (c.add as f64 * s).round() * 2.0;
            (geom::clamp_dim(w as f64 + a), geom::clamp_dim(h as f64 + a))
        }
        CanvasMode::Percent => (geom::clamp_dim(w as f64 * c.percent_w as f64 / 100.0), geom::clamp_dim(h as f64 * c.percent_h as f64 / 100.0)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RotateWhen {
    Always,
    /// Only portrait (taller than wide) images.
    Portrait,
    /// Only landscape (wider than tall) images.
    Landscape,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RotateStep {
    /// Clockwise quarter turns, 0..=3.
    pub quarter: u8,
    /// Extra free rotation in degrees (clockwise).
    pub angle: f32,
    /// Crop the free rotation to hide the corners.
    pub crop: bool,
    pub bg: [u8; 4],
    pub flip_h: bool,
    pub flip_v: bool,
    pub when: RotateWhen,
}

impl Default for RotateStep {
    fn default() -> Self {
        RotateStep { quarter: 1, angle: 0.0, crop: false, bg: [255, 255, 255, 255], flip_h: false, flip_v: false, when: RotateWhen::Always }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AdjustStep {
    /// -100..100
    pub brightness: f32,
    /// -100..100
    pub contrast: f32,
    /// 0.2..3, 1 = unchanged
    pub gamma: f32,
    /// -100..100
    pub saturation: f32,
}

impl Default for AdjustStep {
    fn default() -> Self {
        AdjustStep { brightness: 0.0, contrast: 0.0, gamma: 1.0, saturation: 0.0 }
    }
}

impl AdjustStep {
    pub fn is_identity(&self) -> bool {
        self.brightness == 0.0 && self.contrast == 0.0 && (self.gamma - 1.0).abs() < 1e-4 && self.saturation == 0.0
    }

    /// Same math as the editor's Adjust colors tool.
    pub fn apply(&self, img: &RgbaImage) -> RgbaImage {
        if self.is_identity() {
            return img.clone();
        }
        let bright = self.brightness / 100.0 * 0.5;
        let contrast = if self.contrast >= 0.0 { 1.0 + self.contrast / 100.0 * 2.0 } else { 1.0 + self.contrast / 100.0 };
        let inv_gamma = 1.0 / self.gamma.max(0.01);
        let sat = 1.0 + self.saturation / 100.0;
        ops::map_pixels(img, move |[r, g, b]| {
            let f = |v: f32| {
                let v = (v + bright - 0.5) * contrast + 0.5;
                if inv_gamma != 1.0 {
                    v.max(0.0).powf(inv_gamma)
                } else {
                    v
                }
            };
            let (r, g, b) = (f(r), f(g), f(b));
            if sat != 1.0 {
                let [h, s, l] = ops::rgb_to_hsl([r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]);
                ops::hsl_to_rgb([h, (s * sat).clamp(0.0, 1.0), l])
            } else {
                [r, g, b]
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EffectKind {
    Grayscale,
    Sepia,
    Invert,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SharpenStep {
    /// Percent, 0..500.
    pub amount: f32,
    /// Blur radius (sigma) in pixels.
    pub radius: f32,
    pub threshold: u8,
}

impl Default for SharpenStep {
    fn default() -> Self {
        SharpenStep { amount: 60.0, radius: 1.0, threshold: 2 }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BorderStep {
    pub width: f32,
    pub bottom: f32,
    pub color: [u8; 4],
    pub line_width: f32,
    pub line_color: [u8; 4],
    pub radius: f32,
    pub shadow: bool,
    pub shadow_blur: f32,
    pub shadow_offset: f32,
    pub shadow_opacity: f32,
    pub shadow_bg: [u8; 4],
}

impl Default for BorderStep {
    fn default() -> Self {
        BorderStep {
            width: 20.0,
            bottom: 20.0,
            color: [255, 255, 255, 255],
            line_width: 0.0,
            line_color: [0, 0, 0, 255],
            radius: 0.0,
            shadow: false,
            shadow_blur: 12.0,
            shadow_offset: 6.0,
            shadow_opacity: 50.0,
            shadow_bg: [255, 255, 255, 255],
        }
    }
}

impl BorderStep {
    pub fn spec(&self, scale: f32) -> geom::BorderSpec {
        geom::BorderSpec {
            width: self.width * scale,
            bottom: self.bottom * scale,
            color: self.color,
            line_width: self.line_width * scale,
            line_color: self.line_color,
            radius: self.radius * scale,
            shadow: self.shadow,
            shadow_blur: self.shadow_blur * scale,
            shadow_offset: self.shadow_offset * scale,
            shadow_opacity: self.shadow_opacity / 100.0,
            shadow_bg: self.shadow_bg,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TextStep {
    /// Supports tokens: {name}, {date}, {taken}, {n}, {width}, {height}, {folder}.
    pub text: String,
    /// Font family name (resolved against the installed fonts).
    pub font: String,
    pub bold: bool,
    pub italic: bool,
    /// Font size as a percentage of the image's shorter side.
    pub size_pct: f32,
    pub color: [u8; 4],
    /// 0 = left, 1 = centre, 2 = right (for multi-line text).
    pub align: u8,
    pub anchor: u8,
    /// Distance from the edges, % of the shorter side.
    pub margin_pct: f32,
    /// 0..100
    pub opacity: f32,
    /// Outline width as a % of the font size (0 = none).
    pub outline_pct: f32,
    pub outline_color: [u8; 4],
    pub shadow: bool,
    pub background: bool,
    pub bg_color: [u8; 4],
}

impl Default for TextStep {
    fn default() -> Self {
        TextStep {
            text: "{name}  {taken}".into(),
            font: "Segoe UI".into(),
            bold: false,
            italic: false,
            size_pct: 4.0,
            color: [255, 255, 255, 255],
            align: 0,
            anchor: 8,
            margin_pct: 2.0,
            opacity: 100.0,
            outline_pct: 0.0,
            outline_color: [0, 0, 0, 255],
            shadow: true,
            background: false,
            bg_color: [0, 0, 0, 140],
        }
    }
}

fn font_index(name: &str) -> usize {
    // Unknown names (a preset from another PC) use the first installed family.
    text::families().iter().position(|f| f.name.eq_ignore_ascii_case(name)).unwrap_or(0)
}

impl TextStep {
    fn style(&self, px: f32) -> text::TextStyle {
        text::TextStyle {
            px,
            color: self.color,
            align: self.align.min(2),
            outline: self.outline_pct / 100.0 * px,
            outline_color: self.outline_color,
            shadow: self.shadow,
            shadow_dx: (px * 0.05).max(1.0),
            shadow_dy: (px * 0.05).max(1.0),
            shadow_blur: (px * 0.06).max(1.0),
            background: self.background,
            bg_color: self.bg_color,
            bg_padding: px * 0.3,
            bg_radius: px * 0.15,
            ..Default::default()
        }
    }

    pub fn apply(&self, img: &mut RgbaImage, cx: &StepCtx) {
        let (w, h) = img.dimensions();
        let s = cx.expand(&self.text, w, h);
        if s.trim().is_empty() {
            return;
        }
        let short = w.min(h) as f32;
        let px = (self.size_pct / 100.0 * short).clamp(4.0, 4000.0);
        let font = text::load(font_index(&self.font), self.bold, self.italic);
        let stamp = text::render_stamp(&font, &s, &self.style(px));
        let margin = self.margin_pct / 100.0 * short;
        let (x, y) = text::anchor_pos(w as f32, h as f32, stamp.anchor_w, stamp.anchor_h, self.anchor, margin, 0.0, 0.0);
        text::place_stamp(img, &stamp, x, y, (self.opacity / 100.0).clamp(0.0, 1.0));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WatermarkSource {
    Text,
    Image,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WatermarkStep {
    pub source: WatermarkSource,
    pub text: String,
    pub font: String,
    pub bold: bool,
    pub color: [u8; 4],
    /// Logo / PNG with transparency.
    pub image: Option<PathBuf>,
    /// Text: size % of the shorter side. Image: logo width % of the image width.
    pub size_pct: f32,
    /// 0..100
    pub opacity: f32,
    pub tiled: bool,
    pub anchor: u8,
    pub margin_pct: f32,
    /// Tile rotation in degrees.
    pub angle: f32,
    /// Tile gap, % of the shorter side.
    pub gap_pct: f32,
}

impl Default for WatermarkStep {
    fn default() -> Self {
        WatermarkStep {
            source: WatermarkSource::Text,
            text: "(c) {taken:%Y}".into(),
            font: "Segoe UI".into(),
            bold: true,
            color: [255, 255, 255, 255],
            image: None,
            size_pct: 6.0,
            opacity: 40.0,
            tiled: false,
            anchor: 8,
            margin_pct: 3.0,
            angle: -30.0,
            gap_pct: 12.0,
        }
    }
}

type LogoCache = Mutex<HashMap<PathBuf, Option<Arc<RgbaImage>>>>;

/// Load (and cache) a watermark image.
pub fn load_logo(path: &Path) -> Option<Arc<RgbaImage>> {
    static CACHE: OnceLock<LogoCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().get(path) {
        return v.clone();
    }
    let img = crate::decode::decode_file(path, Default::default()).ok().map(|d| d.image);
    cache.lock().insert(path.to_path_buf(), img.clone());
    img
}

impl WatermarkStep {
    pub fn apply(&self, img: &mut RgbaImage, cx: &StepCtx) {
        let (w, h) = img.dimensions();
        let (wf, hf) = (w as f32, h as f32);
        let short = wf.min(hf);
        let op = (self.opacity / 100.0).clamp(0.0, 1.0);
        // (layer, anchor box x, y, w, h inside the layer)
        let layer: Option<(RgbaImage, f32, f32, f32, f32)> = match self.source {
            WatermarkSource::Text => {
                let s = cx.expand(&self.text, w, h);
                if s.trim().is_empty() {
                    None
                } else {
                    let px = (self.size_pct / 100.0 * short).clamp(4.0, 4000.0);
                    let font = text::load(font_index(&self.font), self.bold, false);
                    let st = text::TextStyle { px, color: self.color, shadow: true, shadow_blur: (px * 0.05).max(1.0), shadow_color: [0, 0, 0, 120], ..Default::default() };
                    let stamp = text::render_stamp(&font, &s, &st);
                    Some((stamp.image, stamp.anchor_x, stamp.anchor_y, stamp.anchor_w, stamp.anchor_h))
                }
            }
            WatermarkSource::Image => self.image.as_deref().and_then(load_logo).map(|logo| {
                let lw = (self.size_pct / 100.0 * wf).clamp(1.0, geom::MAX_DIM as f32);
                let lh = (lw * logo.height() as f32 / logo.width().max(1) as f32).max(1.0);
                let l = crate::imaging::resize_exact(&logo, lw.round() as u32, lh.round() as u32, Filter::Lanczos3);
                let (a, b) = (l.width() as f32, l.height() as f32);
                (l, 0.0, 0.0, a, b)
            }),
        };
        let Some((layer, ax, ay, aw, ah)) = layer else { return };
        if self.tiled {
            text::tile_layer(img, &layer, self.angle, self.gap_pct / 100.0 * short, op);
        } else {
            let margin = self.margin_pct / 100.0 * short;
            let (x, y) = text::anchor_pos(wf, hf, aw, ah, self.anchor, margin, 0.0, 0.0);
            geom::composite_over(img, &layer, (x - ax).round() as i64, (y - ay).round() as i64, op);
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ResizeStep {
    pub spec: ResizeSpec,
    pub filter: Filter,
}

impl Default for ResizeStep {
    fn default() -> Self {
        ResizeStep { spec: ResizeSpec { mode: logic::ResizeMode::LongEdge, ..Default::default() }, filter: Filter::Lanczos3 }
    }
}

/// One pipeline step. Serialized with a `kind` tag so presets stay readable.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum Step {
    Resize(ResizeStep),
    Crop(CropStep),
    Canvas(CanvasStep),
    Rotate(RotateStep),
    Adjust(AdjustStep),
    AutoLevels,
    Effect { effect: EffectKind },
    Sharpen(SharpenStep),
    Border(BorderStep),
    Text(TextStep),
    Watermark(WatermarkStep),
    /// Only metadata: written to JPEG (JFIF), PNG (pHYs), BMP and TIFF output.
    Dpi { dpi: u16 },
}

impl Step {
    /// Every step kind with default parameters, in menu order.
    pub fn templates() -> Vec<Step> {
        vec![
            Step::Resize(ResizeStep::default()),
            Step::Crop(CropStep::default()),
            Step::Canvas(CanvasStep::default()),
            Step::Rotate(RotateStep::default()),
            Step::Adjust(AdjustStep::default()),
            Step::AutoLevels,
            Step::Effect { effect: EffectKind::Grayscale },
            Step::Sharpen(SharpenStep::default()),
            Step::Border(BorderStep::default()),
            Step::Text(TextStep::default()),
            Step::Watermark(WatermarkStep::default()),
            Step::Dpi { dpi: 300 },
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            Step::Resize(_) => "Resize",
            Step::Crop(_) => "Crop",
            Step::Canvas(_) => "Canvas size",
            Step::Rotate(_) => "Rotate / flip",
            Step::Adjust(_) => "Adjust colors",
            Step::AutoLevels => "Auto levels",
            Step::Effect { .. } => "Effect",
            Step::Sharpen(_) => "Sharpen",
            Step::Border(_) => "Border",
            Step::Text(_) => "Text overlay",
            Step::Watermark(_) => "Watermark",
            Step::Dpi { .. } => "DPI",
        }
    }

    /// One-line description of the parameters.
    pub fn summary(&self) -> String {
        match self {
            Step::Resize(r) => match r.spec.mode {
                logic::ResizeMode::None => "No change".into(),
                logic::ResizeMode::Percent => format!("{}%", r.spec.percent),
                logic::ResizeMode::Fit => format!("Fit {} x {}", r.spec.a, r.spec.b),
                m => format!("{} {} px", m.label(), r.spec.a),
            },
            Step::Crop(c) => match c.mode {
                CropMode::Rect => format!("{} x {} at {}, {}", c.w, c.h, c.x, c.y),
                CropMode::Aspect => format!("Aspect {}:{}", c.aspect_w, c.aspect_h),
                CropMode::Size => format!("{} x {}", c.w, c.h),
            },
            Step::Canvas(c) => match c.mode {
                CanvasMode::Absolute => format!("{} x {}", c.w, c.h),
                CanvasMode::Add => format!("+{} px per side", c.add),
                CanvasMode::Percent => format!("{}% x {}%", c.percent_w, c.percent_h),
            },
            Step::Rotate(r) => {
                let mut parts = Vec::new();
                if r.quarter % 4 != 0 {
                    parts.push(format!("{} deg", (r.quarter % 4) as u32 * 90));
                }
                if r.angle != 0.0 {
                    parts.push(format!("{:+.1} deg", r.angle));
                }
                if r.flip_h {
                    parts.push("flip H".into());
                }
                if r.flip_v {
                    parts.push("flip V".into());
                }
                match r.when {
                    RotateWhen::Portrait => parts.push("portrait only".into()),
                    RotateWhen::Landscape => parts.push("landscape only".into()),
                    RotateWhen::Always => {}
                }
                if parts.is_empty() {
                    "No change".into()
                } else {
                    parts.join(", ")
                }
            }
            Step::Adjust(a) => format!("B {:+.0}, C {:+.0}, G {:.2}, S {:+.0}", a.brightness, a.contrast, a.gamma, a.saturation),
            Step::AutoLevels => "Stretch levels".into(),
            Step::Effect { effect } => format!("{effect:?}"),
            Step::Sharpen(s) => format!("{:.0}%, radius {:.1}", s.amount, s.radius),
            Step::Border(b) => format!("{:.0} px", b.width),
            Step::Text(t) => t.text.replace('\n', " "),
            Step::Watermark(w) => match w.source {
                WatermarkSource::Text => format!("{}{}", w.text.replace('\n', " "), if w.tiled { " (tiled)" } else { "" }),
                WatermarkSource::Image => w.image.as_ref().map(|p| crate::ui::batch::file_name(p)).unwrap_or_else(|| "No image chosen".into()),
            },
            Step::Dpi { dpi } => format!("{dpi} dpi"),
        }
    }

    pub fn apply(&self, img: RgbaImage, cx: &StepCtx) -> RgbaImage {
        let (w, h) = img.dimensions();
        if w == 0 || h == 0 {
            return img;
        }
        match self {
            Step::Resize(r) => {
                let s = cx.scale.max(1e-3);
                let (lw, lh) = (((w as f32 / s).round() as u32).max(1), ((h as f32 / s).round() as u32).max(1));
                let (nw, nh) = logic::output_size(lw, lh, &r.spec);
                let (nw, nh) = (((nw as f32 * s).round() as u32).clamp(1, geom::MAX_DIM), ((nh as f32 * s).round() as u32).clamp(1, geom::MAX_DIM));
                if (nw, nh) == (w, h) {
                    img
                } else {
                    crate::imaging::resize_exact(&img, nw, nh, r.filter)
                }
            }
            Step::Crop(c) => {
                let (x, y, cw, ch) = crop_rect(w, h, c, cx.scale);
                if (x, y, cw, ch) == (0, 0, w, h) {
                    img
                } else {
                    ops::crop(&img, x, y, cw, ch)
                }
            }
            Step::Canvas(c) => {
                let (nw, nh) = canvas_size(w, h, c, cx.scale);
                geom::canvas(&img, nw, nh, c.anchor, c.color)
            }
            Step::Rotate(r) => {
                let applies = match r.when {
                    RotateWhen::Always => true,
                    RotateWhen::Portrait => h > w,
                    RotateWhen::Landscape => w > h,
                };
                if !applies {
                    return img;
                }
                let mut out = match r.quarter % 4 {
                    1 => image::imageops::rotate90(&img),
                    2 => image::imageops::rotate180(&img),
                    3 => image::imageops::rotate270(&img),
                    _ => img,
                };
                if r.angle.abs() > 0.001 {
                    out = geom::rotate(&out, r.angle, r.crop, r.bg);
                }
                if r.flip_h {
                    image::imageops::flip_horizontal_in_place(&mut out);
                }
                if r.flip_v {
                    image::imageops::flip_vertical_in_place(&mut out);
                }
                out
            }
            Step::Adjust(a) => a.apply(&img),
            Step::AutoLevels => ops::auto_levels(&img),
            Step::Effect { effect } => match effect {
                EffectKind::Grayscale => effects::grayscale(&img, 1.0),
                EffectKind::Sepia => effects::sepia(&img, 1.0),
                EffectKind::Invert => effects::invert(&img),
            },
            Step::Sharpen(s) => filters::unsharp_mask(&img, s.amount / 100.0, (s.radius * cx.scale.max(1e-3)).max(0.2), s.threshold),
            Step::Border(b) => geom::add_border(&img, &b.spec(cx.scale.max(1e-3))),
            Step::Text(t) => {
                let mut img = img;
                t.apply(&mut img, cx);
                img
            }
            Step::Watermark(wm) => {
                let mut img = img;
                wm.apply(&mut img, cx);
                img
            }
            Step::Dpi { .. } => img,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StepEntry {
    pub enabled: bool,
    pub step: Step,
}

impl Default for StepEntry {
    fn default() -> Self {
        StepEntry { enabled: true, step: Step::AutoLevels }
    }
}

/// The ordered list of advanced steps.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Pipeline {
    pub steps: Vec<StepEntry>,
}

impl Pipeline {
    pub fn push(&mut self, step: Step) {
        self.steps.push(StepEntry { enabled: true, step });
    }

    /// Move the step at `from` to position `to` (both clamped).
    pub fn move_step(&mut self, from: usize, to: usize) {
        if from >= self.steps.len() {
            return;
        }
        let to = to.min(self.steps.len() - 1);
        let s = self.steps.remove(from);
        self.steps.insert(to, s);
    }

    pub fn active(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|e| e.enabled).map(|e| &e.step)
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.active().next().is_none()
    }

    /// The DPI to write (the last enabled DPI step wins).
    pub fn dpi(&self) -> Option<u16> {
        self.active().filter_map(|s| if let Step::Dpi { dpi } = s { Some(*dpi) } else { None }).last()
    }

    /// Run every enabled step in order.
    pub fn apply(&self, img: RgbaImage, cx: &StepCtx) -> RgbaImage {
        self.active().fold(img, |img, s| s.apply(img, cx))
    }
}

// ------------------------------------------------------------------ presets

/// A named snapshot of all batch convert settings.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Preset {
    pub name: String,
    pub settings: super::convert::ConvertSettings,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PresetFile {
    pub presets: Vec<Preset>,
}

impl PresetFile {
    /// Insert or replace (case-insensitive name match), keeping names sorted.
    pub fn upsert(&mut self, p: Preset) {
        match self.presets.iter_mut().find(|x| x.name.eq_ignore_ascii_case(&p.name)) {
            Some(slot) => *slot = p,
            None => self.presets.push(p),
        }
        self.presets.sort_by_key(|p| p.name.to_lowercase());
    }

    pub fn remove(&mut self, name: &str) {
        self.presets.retain(|p| !p.name.eq_ignore_ascii_case(name));
    }

    pub fn get(&self, name: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }
}

pub fn presets_path() -> PathBuf {
    crate::settings::data_dir().join("batch-presets.json")
}

pub fn load_presets_from(path: &Path) -> PresetFile {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_presets_to(path: &Path, f: &PresetFile) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(f)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::encode::write_atomic(path, &json)
}

pub fn load_presets() -> PresetFile {
    load_presets_from(&presets_path())
}

pub fn save_presets(f: &PresetFile) -> anyhow::Result<()> {
    save_presets_to(&presets_path(), f)
}

// ---------------------------------------------------------------------- DPI

/// Standard CRC-32 (as used by PNG and ZIP).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *v = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// Write the pixel density into encoded image bytes where the format has a
/// header field for it (JPEG JFIF, PNG pHYs, BMP, TIFF via the image crate's
/// tags is not supported). Other formats are returned unchanged.
pub fn set_dpi(mut bytes: Vec<u8>, fmt: SaveFormat, dpi: u16) -> Vec<u8> {
    let dpi = dpi.max(1);
    match fmt {
        SaveFormat::Jpeg => {
            if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
                return bytes;
            }
            if bytes.len() >= 18 && bytes[2] == 0xFF && bytes[3] == 0xE0 && &bytes[6..11] == b"JFIF\0" {
                bytes[13] = 1; // dots per inch
                bytes[14..16].copy_from_slice(&dpi.to_be_bytes());
                bytes[16..18].copy_from_slice(&dpi.to_be_bytes());
            } else {
                let mut app0 = vec![0xFF, 0xE0, 0x00, 0x10];
                app0.extend_from_slice(b"JFIF\0");
                app0.extend_from_slice(&[1, 1, 1]);
                app0.extend_from_slice(&dpi.to_be_bytes());
                app0.extend_from_slice(&dpi.to_be_bytes());
                app0.extend_from_slice(&[0, 0]);
                bytes.splice(2..2, app0);
            }
            bytes
        }
        SaveFormat::Png => {
            // Signature (8) + IHDR chunk (4 len + 4 type + 13 data + 4 crc).
            const AFTER_IHDR: usize = 33;
            if bytes.len() < AFTER_IHDR || &bytes[12..16] != b"IHDR" {
                return bytes;
            }
            let ppm = (dpi as f64 / 0.0254).round() as u32;
            let mut chunk = Vec::with_capacity(21);
            chunk.extend_from_slice(&9u32.to_be_bytes());
            let mut body = b"pHYs".to_vec();
            body.extend_from_slice(&ppm.to_be_bytes());
            body.extend_from_slice(&ppm.to_be_bytes());
            body.push(1);
            chunk.extend_from_slice(&body);
            chunk.extend_from_slice(&crc32(&body).to_be_bytes());
            bytes.splice(AFTER_IHDR..AFTER_IHDR, chunk);
            bytes
        }
        SaveFormat::Bmp => {
            if bytes.len() >= 46 && &bytes[0..2] == b"BM" {
                let ppm = ((dpi as f64 / 0.0254).round() as i32).to_le_bytes();
                bytes[38..42].copy_from_slice(&ppm);
                bytes[42..46].copy_from_slice(&ppm);
            }
            bytes
        }
        _ => bytes,
    }
}

/// Read the stored pixel density (dpi) of a JPEG (JFIF), PNG (pHYs) or BMP file.
pub fn read_dpi(bytes: &[u8]) -> Option<f32> {
    if bytes.len() >= 18 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF && bytes[3] == 0xE0 && &bytes[6..11] == b"JFIF\0" {
        let x = u16::from_be_bytes([bytes[14], bytes[15]]) as f32;
        return match bytes[13] {
            1 if x > 0.0 => Some(x),
            2 if x > 0.0 => Some(x * 2.54),
            _ => None,
        };
    }
    if bytes.len() > 33 && &bytes[1..4] == b"PNG" {
        let mut i = 8;
        while i + 12 <= bytes.len() {
            let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
            let ty = &bytes[i + 4..i + 8];
            if ty == b"pHYs" && len == 9 && i + 17 <= bytes.len() {
                let d = &bytes[i + 8..];
                let ppu = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as f32;
                return if d[8] == 1 && ppu > 0.0 { Some(ppu * 0.0254) } else { None };
            }
            if ty == b"IDAT" || ty == b"IEND" {
                break;
            }
            i += 12 + len;
        }
        return None;
    }
    if bytes.len() >= 46 && &bytes[0..2] == b"BM" {
        let ppm = i32::from_le_bytes([bytes[38], bytes[39], bytes[40], bytes[41]]);
        return if ppm > 0 { Some(ppm as f32 * 0.0254) } else { None };
    }
    None
}

/// The stored dpi of an image file (reads only the first 64 KB).
pub fn read_dpi_file(path: &Path) -> Option<f32> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 65536];
    let n = f.read(&mut buf).ok()?;
    read_dpi(&buf[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_img(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 7 % 256) as u8, (y * 3 % 256) as u8, 120, 255]))
    }

    #[test]
    fn steps_apply_in_order() {
        let cx = StepCtx { name: "IMG_1".into(), scale: 1.0, ..Default::default() };
        let mut p = Pipeline::default();
        p.push(Step::Crop(CropStep { mode: CropMode::Size, w: 100, h: 50, anchor: 4, ..Default::default() }));
        p.push(Step::Canvas(CanvasStep { mode: CanvasMode::Add, add: 10, ..Default::default() }));
        let out = p.apply(test_img(300, 200), &cx);
        assert_eq!(out.dimensions(), (120, 70));
        // Reversing the order gives a different result: the crop happens last.
        p.move_step(1, 0);
        assert!(matches!(p.steps[0].step, Step::Canvas(_)));
        let out = p.apply(test_img(300, 200), &cx);
        assert_eq!(out.dimensions(), (100, 50));
        // Disabled steps are skipped.
        p.steps[1].enabled = false;
        assert_eq!(p.apply(test_img(300, 200), &cx).dimensions(), (320, 220));
        // Rotating a portrait-only step leaves landscape images alone.
        let mut r = Pipeline::default();
        r.push(Step::Rotate(RotateStep { quarter: 1, when: RotateWhen::Portrait, ..Default::default() }));
        assert_eq!(r.apply(test_img(30, 20), &cx).dimensions(), (30, 20));
        assert_eq!(r.apply(test_img(20, 30), &cx).dimensions(), (30, 20));
    }

    #[test]
    fn every_step_is_safe_on_tiny_images_and_scales() {
        let mut p = Pipeline::default();
        for s in Step::templates() {
            p.push(s);
        }
        for (w, h) in [(1, 1), (3, 2), (60, 40)] {
            for scale in [1.0, 0.25] {
                let cx = StepCtx { scale, ..Default::default() };
                let out = p.apply(test_img(w, h), &cx);
                assert!(out.width() >= 1 && out.height() >= 1);
            }
        }
        assert_eq!(p.dpi(), Some(300));
    }

    #[test]
    fn preview_scale_matches_full_size_geometry() {
        let mut p = Pipeline::default();
        p.push(Step::Resize(ResizeStep { spec: ResizeSpec { mode: logic::ResizeMode::LongEdge, a: 800, ..Default::default() }, filter: Filter::Bilinear }));
        p.push(Step::Canvas(CanvasStep { mode: CanvasMode::Add, add: 20, ..Default::default() }));
        let full = p.apply(test_img(1600, 1200), &StepCtx { scale: 1.0, ..Default::default() });
        assert_eq!(full.dimensions(), (840, 640));
        let small = p.apply(test_img(400, 300), &StepCtx { scale: 0.25, ..Default::default() });
        assert_eq!(small.dimensions(), (210, 160));
    }

    #[test]
    fn crop_rects() {
        let c = CropStep { mode: CropMode::Aspect, aspect_w: 1.0, aspect_h: 1.0, anchor: 4, ..Default::default() };
        assert_eq!(crop_rect(300, 200, &c, 1.0), (50, 0, 200, 200));
        let c = CropStep { anchor: 0, ..c };
        assert_eq!(crop_rect(200, 300, &c, 1.0), (0, 0, 200, 200));
        let c = CropStep { mode: CropMode::Rect, x: 250, y: 10, w: 100, h: 100, ..Default::default() };
        assert_eq!(crop_rect(300, 200, &c, 1.0), (250, 10, 50, 100));
        let c = CropStep { mode: CropMode::Size, w: 1000, h: 50, anchor: 8, ..Default::default() };
        assert_eq!(crop_rect(300, 200, &c, 1.0), (0, 150, 300, 50));
    }

    #[test]
    fn preset_serde_roundtrip() {
        let mut s = super::super::convert::ConvertSettings::default();
        s.advanced.push(Step::Text(TextStep { text: "{name} - {date}".into(), ..Default::default() }));
        s.advanced.push(Step::AutoLevels);
        s.advanced.push(Step::Effect { effect: EffectKind::Sepia });
        s.advanced.push(Step::Dpi { dpi: 240 });
        s.advanced.steps[1].enabled = false;
        let mut f = PresetFile::default();
        f.upsert(Preset { name: "Web".into(), settings: s.clone() });
        f.upsert(Preset { name: "archive".into(), settings: Default::default() });
        f.upsert(Preset { name: "web".into(), settings: s.clone() });
        assert_eq!(f.presets.len(), 2);
        assert_eq!(f.presets[0].name, "archive");
        let dir = std::env::temp_dir().join(format!("lumen-presets-{}", std::process::id()));
        let path = dir.join("batch-presets.json");
        save_presets_to(&path, &f).unwrap();
        let back = load_presets_from(&path);
        assert_eq!(back, f);
        let got = &back.get("WEB").unwrap().settings;
        assert_eq!(got.advanced, s.advanced);
        assert!(matches!(got.advanced.steps[0].step, Step::Text(_)));
        assert!(!got.advanced.steps[1].enabled);
        let json = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        assert!(json.contains("\"kind\": \"Text\""), "{json}");
        let _ = std::fs::remove_dir_all(&dir);
        // Missing fields fall back to defaults.
        let p: Pipeline = serde_json::from_str(r#"{"steps":[{"step":{"kind":"Sharpen","amount":10}}]}"#).unwrap();
        assert!(p.steps[0].enabled);
        assert_eq!(p.steps[0].step, Step::Sharpen(SharpenStep { amount: 10.0, ..Default::default() }));
    }

    #[test]
    fn text_tokens_expand() {
        let cx = StepCtx {
            name: "IMG_7".into(),
            taken: chrono::NaiveDate::from_ymd_opt(2024, 5, 6).and_then(|d| d.and_hms_opt(1, 2, 3)),
            scale: 0.5,
            ..Default::default()
        };
        assert_eq!(cx.expand("{name} {taken} {width}", 100, 50), "IMG_7 2024-05-06 200");
        let mut img = RgbaImage::from_pixel(200, 120, image::Rgba([0, 0, 0, 255]));
        TextStep { text: "{name}".into(), shadow: false, ..Default::default() }.apply(&mut img, &cx);
        assert!(img.pixels().any(|p| p[0] > 128));
    }

    #[test]
    fn dpi_patching() {
        let img = test_img(16, 8);
        for fmt in [SaveFormat::Jpeg, SaveFormat::Png, SaveFormat::Bmp] {
            let opts = crate::encode::SaveOptions { format: fmt, ..Default::default() };
            let bytes = crate::encode::encode(&img, &opts).unwrap();
            let out = set_dpi(bytes, fmt, 300);
            let dpi = read_dpi(&out).unwrap_or(0.0);
            assert!((dpi - 300.0).abs() < 0.5, "{fmt:?}: {dpi}");
            let back = image::load_from_memory(&out).unwrap();
            assert_eq!(back.width(), 16, "{fmt:?} still decodes");
        }
        // A JPEG without APP0 gets one.
        let raw = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let out = set_dpi(raw, SaveFormat::Jpeg, 72);
        assert_eq!(read_dpi(&out), Some(72.0));
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
