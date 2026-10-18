//! Text rendering for the Add text / Watermark tools: system font discovery,
//! kerned multi-line layout with ab_glyph, outline, soft shadow and an optional
//! rounded background box, all rendered into a straight-alpha RGBA "stamp".

use super::{filters, geom};
use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use image::RgbaImage;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct FontFamily {
    pub name: String,
    /// regular, bold, italic, bold italic (None = not installed / built-in only).
    pub files: [Option<PathBuf>; 4],
}

const CANDIDATES: &[(&str, [&str; 4])] = &[
    ("Segoe UI", ["segoeui.ttf", "segoeuib.ttf", "segoeuii.ttf", "segoeuiz.ttf"]),
    ("Arial", ["arial.ttf", "arialbd.ttf", "ariali.ttf", "arialbi.ttf"]),
    ("Georgia", ["georgia.ttf", "georgiab.ttf", "georgiai.ttf", "georgiaz.ttf"]),
    ("Times New Roman", ["times.ttf", "timesbd.ttf", "timesi.ttf", "timesbi.ttf"]),
    ("Calibri", ["calibri.ttf", "calibrib.ttf", "calibrii.ttf", "calibriz.ttf"]),
    ("Verdana", ["verdana.ttf", "verdanab.ttf", "verdanai.ttf", "verdanaz.ttf"]),
    ("Trebuchet MS", ["trebuc.ttf", "trebucbd.ttf", "trebucit.ttf", "trebucbi.ttf"]),
    ("Consolas", ["consola.ttf", "consolab.ttf", "consolai.ttf", "consolaz.ttf"]),
    ("Impact", ["impact.ttf", "", "", ""]),
    ("Comic Sans MS", ["comic.ttf", "comicbd.ttf", "comici.ttf", "comicz.ttf"]),
    ("Segoe Script", ["segoesc.ttf", "segoescb.ttf", "", ""]),
    ("Segoe Print", ["segoepr.ttf", "segoeprb.ttf", "", ""]),
];

/// Installed font families (plus a built-in fallback that always works).
pub fn families() -> &'static [FontFamily] {
    static F: OnceLock<Vec<FontFamily>> = OnceLock::new();
    F.get_or_init(|| {
        let dir = PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into())).join("Fonts");
        let mut out: Vec<FontFamily> = CANDIDATES
            .iter()
            .filter_map(|(name, files)| {
                let f: [Option<PathBuf>; 4] = std::array::from_fn(|i| Some(dir.join(files[i])).filter(|p| !files[i].is_empty() && p.is_file()));
                f[0].as_ref()?;
                Some(FontFamily { name: name.to_string(), files: f })
            })
            .collect();
        out.push(FontFamily { name: "Ubuntu Light (built-in)".into(), files: [None, None, None, None] });
        out
    })
}

fn builtin() -> FontArc {
    static B: OnceLock<FontArc> = OnceLock::new();
    B.get_or_init(|| FontArc::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("built-in font")).clone()
}

/// Load (cached) the font for a family index; falls back gracefully when a
/// style or the whole family is missing.
pub fn load(family: usize, bold: bool, italic: bool) -> FontArc {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, FontArc>>> = OnceLock::new();
    let fams = families();
    let Some(f) = fams.get(family) else { return builtin() };
    let want = (bold as usize) + 2 * (italic as usize);
    let path = f.files[want].clone().or_else(|| f.files[if bold { 1 } else { 0 }].clone()).or_else(|| f.files[0].clone());
    let Some(path) = path else { return builtin() };
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(font) = cache.lock().get(&path) {
        return font.clone();
    }
    match std::fs::read(&path).ok().and_then(|b| FontArc::try_from_vec(b).ok()) {
        Some(font) => {
            cache.lock().insert(path, font.clone());
            font
        }
        None => builtin(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    /// Font size in pixels (line height of the font).
    pub px: f32,
    pub color: [u8; 4],
    /// 0 = left, 1 = centre, 2 = right.
    pub align: u8,
    pub line_spacing: f32,
    /// Extra spacing between letters in px.
    pub tracking: f32,
    pub outline: f32,
    pub outline_color: [u8; 4],
    pub shadow: bool,
    pub shadow_dx: f32,
    pub shadow_dy: f32,
    pub shadow_blur: f32,
    pub shadow_color: [u8; 4],
    pub background: bool,
    pub bg_color: [u8; 4],
    pub bg_padding: f32,
    pub bg_radius: f32,
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle {
            px: 48.0,
            color: [255, 255, 255, 255],
            align: 0,
            line_spacing: 1.0,
            tracking: 0.0,
            outline: 0.0,
            outline_color: [0, 0, 0, 255],
            shadow: false,
            shadow_dx: 2.0,
            shadow_dy: 2.0,
            shadow_blur: 3.0,
            shadow_color: [0, 0, 0, 160],
            background: false,
            bg_color: [0, 0, 0, 140],
            bg_padding: 10.0,
            bg_radius: 6.0,
        }
    }
}

/// A rendered text layer and the rectangle (inside it) that should be used for
/// positioning - the text block (plus background padding), excluding shadow
/// and outline overhang.
pub struct Stamp {
    pub image: RgbaImage,
    pub anchor_x: f32,
    pub anchor_y: f32,
    pub anchor_w: f32,
    pub anchor_h: f32,
}

/// Coverage mask of the laid-out text. Returns (mask, w, h, text box x/y/w/h in mask coords).
/// Largest stamp (pixels) rendered; bigger requests are scaled down so a huge
/// font size with a wide outline can't exhaust memory (about 24 bytes/px peak).
pub const MAX_STAMP_PX: usize = 24_000_000;

type Layout = (Vec<f32>, usize, usize, [f32; 4]);

/// Lay out and rasterize; `Err(area)` when the stamp would exceed [`MAX_STAMP_PX`].
fn layout_mask(font: &FontArc, text: &str, st: &TextStyle, pad: f32) -> Result<Layout, usize> {
    let px = st.px.clamp(1.0, 4000.0);
    let sf = font.as_scaled(PxScale::from(px));
    let line_h = (sf.ascent() - sf.descent() + sf.line_gap()) * st.line_spacing.clamp(0.5, 4.0);
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let mut placed = Vec::new();
    let mut widths = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let mut x = 0.0f32;
        let mut prev = None;
        let mut glyphs = Vec::new();
        for ch in line.chars() {
            if ch.is_control() {
                continue;
            }
            let id = sf.glyph_id(ch);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            glyphs.push((id, x, li));
            x += sf.h_advance(id) + st.tracking;
            prev = Some(id);
        }
        widths.push((x - if glyphs.is_empty() { 0.0 } else { st.tracking }).max(0.0));
        placed.push(glyphs);
    }
    let tw = widths.iter().cloned().fold(0.0f32, f32::max).max(1.0);
    let th = (line_h * (lines.len() as f32 - 1.0) + (sf.ascent() - sf.descent())).max(1.0);
    let mut outlined = Vec::new();
    let (mut minx, mut miny, mut maxx, mut maxy) = (0.0f32, 0.0f32, tw, th);
    for (li, glyphs) in placed.into_iter().enumerate() {
        let off = (tw - widths[li]) * st.align.min(2) as f32 / 2.0;
        let base = li as f32 * line_h + sf.ascent();
        for (id, x, _) in glyphs {
            let g = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(off + x, base));
            if let Some(o) = font.outline_glyph(g) {
                let b = o.px_bounds();
                minx = minx.min(b.min.x);
                miny = miny.min(b.min.y);
                maxx = maxx.max(b.max.x);
                maxy = maxy.max(b.max.y);
                outlined.push(o);
            }
        }
    }
    let ox = (pad - minx).ceil();
    let oy = (pad - miny).ceil();
    let w = ((maxx + ox + pad).ceil() as usize).clamp(1, geom::MAX_DIM as usize);
    let h = ((maxy + oy + pad).ceil() as usize).clamp(1, geom::MAX_DIM as usize);
    if w * h > MAX_STAMP_PX {
        return Err(w * h);
    }
    let mut mask = vec![0f32; w * h];
    for o in outlined {
        let b = o.px_bounds();
        let (bx, by) = ((b.min.x + ox) as i64, (b.min.y + oy) as i64);
        o.draw(|x, y, c| {
            let (xx, yy) = (bx + x as i64, by + y as i64);
            if xx >= 0 && yy >= 0 && (xx as usize) < w && (yy as usize) < h {
                let m = &mut mask[yy as usize * w + xx as usize];
                *m = (*m + c).min(1.0);
            }
        });
    }
    Ok((mask, w, h, [ox, oy, tw, th]))
}

/// Scale every size in a style by `f`.
fn scaled_style(st: &TextStyle, f: f32) -> TextStyle {
    TextStyle {
        px: st.px * f,
        tracking: st.tracking * f,
        outline: st.outline * f,
        shadow_dx: st.shadow_dx * f,
        shadow_dy: st.shadow_dy * f,
        shadow_blur: st.shadow_blur * f,
        bg_padding: st.bg_padding * f,
        bg_radius: st.bg_radius * f,
        ..st.clone()
    }
}

/// 1-D squared Euclidean distance transform (Felzenszwalb and Huttenlocher)
/// of `f` (0 = inside, `EDT_INF` = outside) into `d`. Scratch: `v`, `z`.
fn edt_1d(f: &[f32], d: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    for q in 1..n {
        let fq = f[q] + (q * q) as f32;
        let mut s;
        loop {
            let p = v[k];
            s = (fq - (f[p] + (p * p) as f32)) / (2.0 * (q - p) as f32);
            // z[0] is -inf, so k never underflows.
            if k > 0 && s <= z[k] {
                k -= 1;
            } else {
                break;
            }
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }
    k = 0;
    for (q, out) in d.iter_mut().enumerate() {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let p = v[k];
        let dq = q as f32 - p as f32;
        *out = dq * dq + f[p];
    }
}

const EDT_INF: f32 = 1e10;

/// Squared distance from every pixel to the nearest pixel where `inside` is
/// true. Memory: two `w*h` f32 planes; time O(w*h) regardless of radius.
pub(crate) fn distance_sq(inside: impl Fn(usize) -> bool + Sync, w: usize, h: usize) -> Vec<f32> {
    use rayon::prelude::*;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    // Columns first, written transposed (h-long rows) so both passes are row-parallel.
    let mut t = vec![0f32; w * h];
    t.par_chunks_mut(h).enumerate().for_each_init(
        || (vec![0f32; h], vec![0usize; h], vec![0f32; h + 1]),
        |(f, v, z), (x, col)| {
            for (y, fy) in f.iter_mut().enumerate() {
                *fy = if inside(y * w + x) { 0.0 } else { EDT_INF };
            }
            edt_1d(f, col, v, z);
        },
    );
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each_init(
        || (vec![0f32; w], vec![0usize; w], vec![0f32; w + 1]),
        |(f, v, z), (y, row)| {
            for (x, fx) in f.iter_mut().enumerate() {
                *fx = t[x * h + y];
            }
            edt_1d(f, row, v, z);
        },
    );
    out
}

/// Circular dilation of a coverage mask by `r` pixels (anti-aliased edge).
/// Uses a Euclidean distance transform: memory is bounded by the mask size
/// and time does not grow with the radius.
fn dilate(mask: &[f32], w: usize, h: usize, r: f32) -> Vec<f32> {
    use rayon::prelude::*;
    if !(r > 0.0) || mask.len() != w * h {
        return mask.to_vec();
    }
    let d2 = distance_sq(|i| mask[i] >= 0.5, w, h);
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w.max(1)).zip(d2.par_chunks(w.max(1))).zip(mask.par_chunks(w.max(1))).for_each(|((o, d), m)| {
        for i in 0..o.len() {
            let cov = (r + 1.0 - d[i].sqrt()).clamp(0.0, 1.0);
            o[i] = cov.max(m[i]);
        }
    });
    out
}

fn mask_layer(mask: &[f32], w: usize, h: usize, color: [u8; 4]) -> RgbaImage {
    let a = color[3] as f32;
    let mut img = RgbaImage::new(w as u32, h as u32);
    for (p, &m) in img.as_mut().chunks_exact_mut(4).zip(mask) {
        p[0] = color[0];
        p[1] = color[1];
        p[2] = color[2];
        p[3] = (m * a + 0.5).min(255.0) as u8;
    }
    img
}

pub fn render_stamp(font: &FontArc, text: &str, st: &TextStyle) -> Stamp {
    let mut st = std::borrow::Cow::Borrowed(st);
    for _ in 0..4 {
        match render_stamp_inner(font, text, &st) {
            Ok(s) => return s,
            Err(area) => {
                let f = ((MAX_STAMP_PX as f64 / area as f64).sqrt() * 0.95) as f32;
                st = std::borrow::Cow::Owned(scaled_style(&st, f.clamp(0.01, 0.95)));
            }
        }
    }
    Stamp { image: RgbaImage::new(1, 1), anchor_x: 0.0, anchor_y: 0.0, anchor_w: 1.0, anchor_h: 1.0 }
}

fn render_stamp_inner(font: &FontArc, text: &str, st: &TextStyle) -> Result<Stamp, usize> {
    let outline = st.outline.clamp(0.0, 400.0);
    let bgpad = if st.background { st.bg_padding.clamp(0.0, 2000.0) } else { 0.0 };
    let shadow_ext = if st.shadow { st.shadow_blur.clamp(0.0, 500.0) * 3.0 + st.shadow_dx.abs().max(st.shadow_dy.abs()).min(2000.0) } else { 0.0 };
    let pad = (outline + 2.0).max(bgpad + 1.0) + shadow_ext;
    let (mask, w, h, [bx, by, tw, th]) = layout_mask(font, text, st, pad)?;
    let outer = if outline > 0.0 { Some(dilate(&mask, w, h, outline)) } else { None };
    let mut img = RgbaImage::new(w as u32, h as u32);
    let (ax, ay, aw, ah) = (bx - bgpad, by - bgpad, tw + 2.0 * bgpad, th + 2.0 * bgpad);
    if st.background && st.bg_color[3] > 0 {
        let c = st.bg_color;
        let r = st.bg_radius.max(0.0);
        for (i, p) in img.as_mut().chunks_exact_mut(4).enumerate() {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            let cov = geom::rrect_coverage(x, y, ax, ay, ax + aw, ay + ah, r);
            if cov > 0.0 {
                geom::blend_px(p, [c[0] as f32, c[1] as f32, c[2] as f32, cov * c[3] as f32 / 255.0]);
            }
        }
    }
    if st.shadow && st.shadow_color[3] > 0 {
        let sil = outer.as_deref().unwrap_or(&mask);
        let layer = mask_layer(sil, w, h, st.shadow_color);
        let layer = filters::gaussian_blur(&layer, st.shadow_blur.max(0.0));
        geom::composite_over(&mut img, &layer, st.shadow_dx.round() as i64, st.shadow_dy.round() as i64, 1.0);
    }
    if let Some(o) = &outer {
        geom::composite_over(&mut img, &mask_layer(o, w, h, st.outline_color), 0, 0, 1.0);
    }
    geom::composite_over(&mut img, &mask_layer(&mask, w, h, st.color), 0, 0, 1.0);
    Ok(Stamp { image: img, anchor_x: ax, anchor_y: ay, anchor_w: aw, anchor_h: ah })
}

/// Top-left position (in image pixels) of a box of size `bw`×`bh` placed at a
/// 3×3 `anchor` with `margin` px from the edges plus an extra offset.
pub fn anchor_pos(img_w: f32, img_h: f32, bw: f32, bh: f32, anchor: u8, margin: f32, dx: f32, dy: f32) -> (f32, f32) {
    let (fx, fy) = geom::anchor_frac(anchor);
    let x = margin + (img_w - 2.0 * margin - bw) * fx + dx;
    let y = margin + (img_h - 2.0 * margin - bh) * fy + dy;
    (x, y)
}

/// Composite a stamp onto `img` with its anchor rectangle at (`x`,`y`).
pub fn place_stamp(img: &mut RgbaImage, s: &Stamp, x: f32, y: f32, opacity: f32) {
    geom::composite_over(img, &s.image, (x - s.anchor_x).round() as i64, (y - s.anchor_y).round() as i64, opacity);
}

/// Tile a layer diagonally across the whole image (rotated by `angle_deg`),
/// with `gap` px between repetitions.
pub fn tile_layer(img: &mut RgbaImage, layer: &RgbaImage, angle_deg: f32, gap: f32, opacity: f32) {
    let layer = if angle_deg.abs() > 0.01 { geom::rotate(layer, angle_deg, false, [0, 0, 0, 0]) } else { layer.clone() };
    let (lw, lh) = (layer.width() as f32, layer.height() as f32);
    let sx = (lw + gap.max(0.0)).max(4.0);
    let sy = (lh + gap.max(0.0)).max(4.0);
    let (w, h) = (img.width() as f32, img.height() as f32);
    let rows = ((h / sy).ceil() as i64 + 2).min(4000);
    let cols = ((w / sx).ceil() as i64 + 2).min(4000);
    for r in -1..rows {
        let shift = if r.rem_euclid(2) == 1 { sx / 2.0 } else { 0.0 };
        for c in -1..cols {
            let x = c as f32 * sx + shift - sx / 4.0;
            let y = r as f32 * sy;
            if x > w || y > h || x + lw < 0.0 || y + lh < 0.0 {
                continue;
            }
            geom::composite_over(img, &layer, x.round() as i64, y.round() as i64, opacity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_visible_pixels() {
        let font = load(usize::MAX, false, false);
        let st = TextStyle { px: 32.0, ..Default::default() };
        let s = render_stamp(&font, "Hello\nLumen", &st);
        assert!(s.image.width() > 40 && s.image.height() > 50);
        assert!(s.image.pixels().any(|p| p[3] > 200));
        let mut img = RgbaImage::from_pixel(200, 120, image::Rgba([0, 0, 0, 255]));
        let (x, y) = anchor_pos(200.0, 120.0, s.anchor_w, s.anchor_h, 4, 0.0, 0.0, 0.0);
        place_stamp(&mut img, &s, x, y, 1.0);
        assert!(img.pixels().any(|p| p[0] > 200));
        assert!(img.pixels().all(|p| p[3] == 255));
    }

    #[test]
    fn system_fonts_and_effects() {
        assert!(!families().is_empty());
        let font = load(0, true, true);
        let st = TextStyle { px: 24.0, outline: 3.0, shadow: true, background: true, align: 1, ..Default::default() };
        let s = render_stamp(&font, "Ab\n\nwide line", &st);
        assert!(s.image.pixels().any(|p| p[3] == 255));
        // Empty text and absurd sizes don't panic.
        let e = render_stamp(&font, "", &TextStyle { px: 0.0, ..Default::default() });
        assert!(e.image.width() >= 1);
        let mut img = RgbaImage::from_pixel(64, 64, image::Rgba([10, 10, 10, 255]));
        tile_layer(&mut img, &s.image, -30.0, 10.0, 0.5);
    }

    #[test]
    fn huge_stamp_is_capped() {
        let font = load(usize::MAX, false, false);
        let st = TextStyle { px: 3000.0, outline: 900.0, ..Default::default() };
        let s = render_stamp(&font, "Wide text line", &st);
        assert!((s.image.width() as usize * s.image.height() as usize) <= MAX_STAMP_PX);
        assert!(s.image.pixels().any(|p| p[3] == 255));
    }

    #[test]
    fn distance_transform_matches_brute_force() {
        let (w, h) = (23usize, 17usize);
        let inside = |i: usize| (i * 7919) % 31 == 0;
        let d = distance_sq(inside, w, h);
        for y in 0..h {
            for x in 0..w {
                let mut best = f32::MAX;
                for j in 0..w * h {
                    if inside(j) {
                        let (dx, dy) = (x as f32 - (j % w) as f32, y as f32 - (j / w) as f32);
                        best = best.min(dx * dx + dy * dy);
                    }
                }
                assert!((d[y * w + x] - best).abs() < 1e-3, "({x},{y}) {} vs {best}", d[y * w + x]);
            }
        }
    }

    #[test]
    fn dilate_grows() {
        let mut m = vec![0f32; 11 * 11];
        m[5 * 11 + 5] = 1.0;
        let d = dilate(&m, 11, 11, 2.0);
        assert_eq!(d[5 * 11 + 7], 1.0);
        assert_eq!(d[7 * 11 + 5], 1.0);
        assert_eq!(d[5 * 11 + 9], 0.0);
        assert_eq!(d[0], 0.0);
    }
}
