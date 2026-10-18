//! Tonal mapping: Levels and Curves lookup tables (per channel + master), and
//! Shadows / Highlights local tone mapping.

use image::RgbaImage;
use rayon::prelude::*;

/// Cells along the long side of the low-resolution luminance grid used by
/// [`shadows_highlights`]. Being relative to the image size, the downscaled
/// preview and the full-resolution render produce the same look.
const SH_GRID: f32 = 256.0;

/// Shadows / Highlights: lift dark areas (`shadows` > 0) and recover bright
/// ones (`highlights` > 0), each -1..1. Brightness is judged from a heavily
/// blurred luminance map (local, not per pixel), so local contrast and colour
/// ratios are kept. Cost: one pass over the image plus a tiny grid.
pub fn shadows_highlights(img: &RgbaImage, shadows: f32, highlights: f32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (sh, hi) = (shadows.clamp(-1.0, 1.0), highlights.clamp(-1.0, 1.0));
    if w == 0 || h == 0 || (sh == 0.0 && hi == 0.0) {
        return img.clone();
    }
    let long = w.max(h) as f32;
    let gw = ((w as f32 * SH_GRID / long).round() as usize).max(1);
    let gh = ((h as f32 * SH_GRID / long).round() as usize).max(1);
    let (cw, ch) = (w as f32 / gw as f32, h as f32 / gh as f32);
    let raw = img.as_raw();
    // Mean luma per grid cell (sampled sparsely inside big cells).
    let mut grid: Vec<f32> = (0..gw * gh)
        .into_par_iter()
        .map(|i| {
            let (gx, gy) = (i % gw, i / gw);
            let (x0, x1) = ((gx as f32 * cw) as u32, (((gx + 1) as f32 * cw).ceil() as u32).min(w).max((gx as f32 * cw) as u32 + 1));
            let (y0, y1) = ((gy as f32 * ch) as u32, (((gy + 1) as f32 * ch).ceil() as u32).min(h).max((gy as f32 * ch) as u32 + 1));
            let step = ((x1 - x0).max(y1 - y0) / 16).max(1);
            let (mut s, mut n) = (0f32, 0f32);
            let mut y = y0;
            while y < y1.min(h) {
                let mut x = x0;
                while x < x1.min(w) {
                    let j = (y as usize * w as usize + x as usize) * 4;
                    s += luma8(raw[j], raw[j + 1], raw[j + 2]);
                    n += 1.0;
                    x += step;
                }
                y += step;
            }
            if n > 0.0 {
                s / n
            } else {
                0.5
            }
        })
        .collect();
    super::mask::blur_plane(&mut grid, gw, gh, SH_GRID * 0.03);
    let stride = w as usize * 4;
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        let gyf = ((y as f32 + 0.5) / ch - 0.5).clamp(0.0, (gh - 1) as f32);
        let gy0 = gyf.floor() as usize;
        let gy1 = (gy0 + 1).min(gh - 1);
        let fy = gyf - gy0 as f32;
        for x in 0..w as usize {
            let gxf = ((x as f32 + 0.5) / cw - 0.5).clamp(0.0, (gw - 1) as f32);
            let gx0 = gxf.floor() as usize;
            let gx1 = (gx0 + 1).min(gw - 1);
            let fx = gxf - gx0 as f32;
            let top = grid[gy0 * gw + gx0] * (1.0 - fx) + grid[gy0 * gw + gx1] * fx;
            let bot = grid[gy1 * gw + gx0] * (1.0 - fx) + grid[gy1 * gw + gx1] * fx;
            let local = top * (1.0 - fy) + bot * fy;
            let p = &mut row[x * 4..x * 4 + 4];
            let own = luma8(p[0], p[1], p[2]);
            // Mostly the local average, a little of the pixel itself (fewer halos).
            let l = local * 0.6 + own * 0.4;
            let ws = (1.0 - l / 0.55).clamp(0.0, 1.0);
            let wh = ((l - 0.45) / 0.55).clamp(0.0, 1.0);
            let ev = sh * ws * ws * 1.6 - hi * wh * wh * 1.3;
            if ev.abs() < 1e-4 {
                continue;
            }
            let gain = ev.exp2();
            for c in &mut p[..3] {
                *c = (*c as f32 * gain).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    out
}

#[inline]
fn luma8(r: u8, g: u8, b: u8) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// Channel index used by Levels/Curves: 0 = composite RGB, 1..=3 = R, G, B.
pub const CHANNEL_NAMES: [&str; 4] = ["RGB", "Red", "Green", "Blue"];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelsParams {
    pub in_black: f32,
    pub in_white: f32,
    /// Midtone gamma (> 1 brightens).
    pub gamma: f32,
    pub out_black: f32,
    pub out_white: f32,
}

impl Default for LevelsParams {
    fn default() -> Self {
        LevelsParams { in_black: 0.0, in_white: 255.0, gamma: 1.0, out_black: 0.0, out_white: 255.0 }
    }
}

impl LevelsParams {
    pub fn is_identity(&self) -> bool {
        *self == LevelsParams::default()
    }
    pub fn map(&self, v: f32) -> f32 {
        let lo = self.in_black.clamp(0.0, 254.0);
        let hi = self.in_white.clamp(lo + 1.0, 255.0);
        let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
        let g = self.gamma.clamp(0.05, 20.0);
        let t = if g == 1.0 { t } else { t.powf(1.0 / g) };
        self.out_black + (self.out_white - self.out_black) * t
    }
    pub fn lut(&self) -> [u8; 256] {
        let mut l = [0u8; 256];
        for (i, v) in l.iter_mut().enumerate() {
            *v = self.map(i as f32).round().clamp(0.0, 255.0) as u8;
        }
        l
    }
}

/// Combine per-channel tables with the composite one: out = master(channel(x)).
pub fn compose_luts(master: &[u8; 256], chans: [&[u8; 256]; 3]) -> [[u8; 256]; 3] {
    let mut out = [[0u8; 256]; 3];
    for c in 0..3 {
        for i in 0..256 {
            out[c][i] = master[chans[c][i] as usize];
        }
    }
    out
}

pub fn apply_levels(img: &RgbaImage, p: &[LevelsParams; 4]) -> RgbaImage {
    if p.iter().all(|l| l.is_identity()) {
        return img.clone();
    }
    let luts = compose_luts(&p[0].lut(), [&p[1].lut(), &p[2].lut(), &p[3].lut()]);
    super::ops::apply_lut(img, &luts)
}

/// Value at `pct` (0..1) of a 256-bin histogram.
pub fn percentile(h: &[u32], pct: f64) -> usize {
    let total: u64 = h.iter().map(|&v| v as u64).sum();
    if total == 0 {
        return if pct < 0.5 { 0 } else { 255 };
    }
    let target = (total as f64 * pct).max(1.0) as u64;
    let mut acc = 0u64;
    for (i, &v) in h.iter().enumerate() {
        acc += v as u64;
        if acc >= target {
            return i;
        }
    }
    h.len().saturating_sub(1)
}

// ---------------------------------------------------------------- curves

/// Monotone cubic (Fritsch-Carlson) interpolation through `pts` (x sorted,
/// 0..1 coordinates) sampled into a 256-entry table. Outside the first/last
/// point the curve is flat.
pub fn curve_lut(pts: &[[f32; 2]]) -> [u8; 256] {
    let mut p: Vec<[f32; 2]> = pts.iter().map(|q| [q[0].clamp(0.0, 1.0), q[1].clamp(0.0, 1.0)]).collect();
    p.sort_by(|a, b| a[0].total_cmp(&b[0]));
    p.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-6);
    let mut lut = [0u8; 256];
    if p.is_empty() {
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as u8;
        }
        return lut;
    }
    if p.len() == 1 {
        return [(p[0][1] * 255.0).round() as u8; 256];
    }
    let n = p.len();
    let mut d = vec![0f32; n - 1];
    for i in 0..n - 1 {
        d[i] = (p[i + 1][1] - p[i][1]) / (p[i + 1][0] - p[i][0]);
    }
    let mut m = vec![0f32; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for i in 1..n - 1 {
        m[i] = if d[i - 1] * d[i] <= 0.0 { 0.0 } else { (d[i - 1] + d[i]) / 2.0 };
    }
    for i in 0..n - 1 {
        if d[i] == 0.0 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
        } else {
            let a = m[i] / d[i];
            let b = m[i + 1] / d[i];
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                m[i] = t * a * d[i];
                m[i + 1] = t * b * d[i];
            }
        }
    }
    let eval = |x: f32| -> f32 {
        if x <= p[0][0] {
            return p[0][1];
        }
        if x >= p[n - 1][0] {
            return p[n - 1][1];
        }
        let mut i = 0;
        while i + 1 < n - 1 && x > p[i + 1][0] {
            i += 1;
        }
        let h = p[i + 1][0] - p[i][0];
        let t = (x - p[i][0]) / h;
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * p[i][1] + h10 * h * m[i] + h01 * p[i + 1][1] + h11 * h * m[i + 1]
    };
    for (i, v) in lut.iter_mut().enumerate() {
        *v = (eval(i as f32 / 255.0).clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    lut
}

pub fn identity_curve() -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [1.0, 1.0]]
}

pub fn apply_curves(img: &RgbaImage, curves: &[Vec<[f32; 2]>; 4]) -> RgbaImage {
    let luts: Vec<[u8; 256]> = curves.iter().map(|c| curve_lut(c)).collect();
    let ident: [u8; 256] = std::array::from_fn(|i| i as u8);
    if luts.iter().all(|l| *l == ident) {
        return img.clone();
    }
    let t = compose_luts(&luts[0], [&luts[1], &luts[2], &luts[3]]);
    super::ops::apply_lut(img, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadows_highlights_basic() {
        // Left half dark, right half bright.
        let img = RgbaImage::from_fn(200, 100, |x, _| if x < 100 { image::Rgba([30, 25, 20, 255]) } else { image::Rgba([235, 230, 220, 255]) });
        assert_eq!(shadows_highlights(&img, 0.0, 0.0), img);
        let lifted = shadows_highlights(&img, 1.0, 0.0);
        assert!(lifted.get_pixel(20, 50)[0] > 45, "shadows lifted");
        assert!((lifted.get_pixel(180, 50)[0] as i32 - 235).abs() <= 2, "highlights untouched");
        // Colour ratios are kept (gain multiplies all channels).
        let p = lifted.get_pixel(20, 50);
        assert!(p[0] > p[1] && p[1] > p[2]);
        let rec = shadows_highlights(&img, 0.0, 1.0);
        assert!(rec.get_pixel(180, 50)[0] < 225, "highlights recovered");
        assert!((rec.get_pixel(20, 50)[0] as i32 - 30).abs() <= 1);
        // Preview (downscaled) and full render agree on the look.
        let small = crate::imaging::resize_to_fit(&img, 50, 50);
        let ls = shadows_highlights(&small, 1.0, 0.0);
        assert!((ls.get_pixel(5, 12)[0] as i32 - lifted.get_pixel(20, 50)[0] as i32).abs() <= 4);
        // Tiny images are fine.
        let one = RgbaImage::from_pixel(1, 1, image::Rgba([10, 10, 10, 255]));
        assert_eq!(shadows_highlights(&one, 1.0, 1.0).dimensions(), (1, 1));
    }

    fn sample() -> RgbaImage {
        RgbaImage::from_fn(17, 9, |x, y| image::Rgba([(x * 15) as u8, (y * 28) as u8, (x * y) as u8, 200]))
    }

    #[test]
    fn levels_identity() {
        let img = sample();
        assert_eq!(apply_levels(&img, &[LevelsParams::default(); 4]), img);
        let l = LevelsParams::default().lut();
        assert!(l.iter().enumerate().all(|(i, &v)| v == i as u8));
    }

    #[test]
    fn levels_values() {
        let p = LevelsParams { in_black: 50.0, in_white: 200.0, ..Default::default() };
        let l = p.lut();
        assert_eq!(l[0], 0);
        assert_eq!(l[50], 0);
        assert_eq!(l[200], 255);
        assert_eq!(l[125], 128);
        let inv = LevelsParams { out_black: 255.0, out_white: 0.0, ..Default::default() };
        assert_eq!(inv.lut()[0], 255);
        // Degenerate inputs don't panic.
        let bad = LevelsParams { in_black: 255.0, in_white: 0.0, gamma: 0.0, ..Default::default() };
        let _ = bad.lut();
    }

    #[test]
    fn curves_identity_and_monotone() {
        let img = sample();
        let id = [identity_curve(), identity_curve(), identity_curve(), identity_curve()];
        assert_eq!(apply_curves(&img, &id), img);
        let l = curve_lut(&[[0.0, 0.0], [0.25, 0.15], [0.75, 0.85], [1.0, 1.0]]);
        assert!(l.windows(2).all(|w| w[1] >= w[0]));
        assert!(l[64] < 64 && l[191] > 191);
        assert_eq!(curve_lut(&[[0.5, 0.5]]), [128u8; 256]);
        let flat = curve_lut(&[[0.2, 0.3], [0.8, 0.6]]);
        assert_eq!(flat[0], 77);
        assert_eq!(flat[255], 153);
    }

    #[test]
    fn percentile_basic() {
        let mut h = vec![0u32; 256];
        h[10] = 50;
        h[200] = 50;
        assert_eq!(percentile(&h, 0.01), 10);
        assert_eq!(percentile(&h, 0.99), 200);
        assert_eq!(percentile(&[0; 256], 0.99), 255);
    }
}
