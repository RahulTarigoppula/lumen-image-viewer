//! Pure pixel operations (no UI). All operate on straight-alpha sRGB RGBA8.

use image::RgbaImage;
use rayon::prelude::*;

pub fn crop(img: &RgbaImage, x: u32, y: u32, w: u32, h: u32) -> RgbaImage {
    let x = x.min(img.width().saturating_sub(1));
    let y = y.min(img.height().saturating_sub(1));
    let w = w.clamp(1, img.width() - x);
    let h = h.clamp(1, img.height() - y);
    image::imageops::crop_imm(img, x, y, w, h).to_image()
}

/// Apply a per-channel 256-entry lookup table (RGB), in parallel.
pub fn apply_lut(img: &RgbaImage, lut: &[[u8; 256]; 3]) -> RgbaImage {
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(4 * 4096).for_each(|chunk| {
        for p in chunk.chunks_exact_mut(4) {
            p[0] = lut[0][p[0] as usize];
            p[1] = lut[1][p[1] as usize];
            p[2] = lut[2][p[2] as usize];
        }
    });
    out
}

/// Run `f` on every pixel (RGB as 0..1 floats) in parallel.
pub fn map_pixels(img: &RgbaImage, f: impl Fn([f32; 3]) -> [f32; 3] + Sync) -> RgbaImage {
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(4 * 4096).for_each(|chunk| {
        for p in chunk.chunks_exact_mut(4) {
            let c = f([p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0]);
            p[0] = (c[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            p[1] = (c[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            p[2] = (c[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    });
    out
}

pub fn rgb_to_hsl([r, g, b]: [f32; 3]) -> [f32; 3] {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return [0.0, 0.0, l];
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    [h / 6.0, s, l]
}

pub fn hsl_to_rgb([h, s, l]: [f32; 3]) -> [f32; 3] {
    if s <= 0.0 {
        return [l, l, l];
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0)]
}

/// Auto levels: stretch each channel between its 0.5% and 99.5% percentiles,
/// keeping the color balance natural by limiting per-channel divergence.
pub fn auto_levels(img: &RgbaImage) -> RgbaImage {
    let hist = crate::edit::Histogram::compute(img);
    let total: u64 = hist.l.iter().map(|&v| v as u64).sum();
    if total == 0 {
        return img.clone();
    }
    let pct = |h: &Vec<u32>, p: f64| -> usize {
        let target = (total as f64 * p) as u64;
        let mut acc = 0u64;
        for (i, &v) in h.iter().enumerate() {
            acc += v as u64;
            if acc >= target {
                return i;
            }
        }
        255
    };
    let mut lut = [[0u8; 256]; 3];
    for (c, h) in [&hist.r, &hist.g, &hist.b].iter().enumerate() {
        let lo = pct(h, 0.005) as f32;
        let hi = (pct(h, 0.995) as f32).max(lo + 16.0);
        for i in 0..256 {
            let v = ((i as f32 - lo) / (hi - lo)).clamp(0.0, 1.0);
            lut[c][i] = (v * 255.0 + 0.5) as u8;
        }
    }
    apply_lut(img, &lut)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hsl_roundtrip() {
        for c in [[0.2, 0.4, 0.6], [1.0, 0.0, 0.0], [0.5, 0.5, 0.5], [0.9, 0.8, 0.1]] {
            let back = hsl_to_rgb(rgb_to_hsl(c));
            for i in 0..3 {
                assert!((back[i] - c[i]).abs() < 1e-4, "{c:?} -> {back:?}");
            }
        }
    }
    #[test]
    fn crop_clamps() {
        let img = RgbaImage::new(10, 10);
        let c = crop(&img, 8, 8, 50, 50);
        assert_eq!(c.dimensions(), (2, 2));
    }
}
