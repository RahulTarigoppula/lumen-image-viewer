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
    use crate::edit::tone::percentile;
    let (llo, lhi) = (percentile(&hist.l, 0.003) as f32, percentile(&hist.l, 0.997) as f32);
    // Midtone correction from the stretched mean luminance.
    let mean: f64 = hist.l.iter().enumerate().map(|(i, &v)| i as f64 * v as f64).sum::<f64>() / total as f64;
    let span = (lhi - llo).max(32.0);
    let m = ((mean as f32 - llo) / span).clamp(0.02, 0.98) as f64;
    let p = ((0.46f64.ln() / m.ln()) as f32).clamp(0.78, 1.25);
    // Pull the gamma towards 1: an automatic tool shouldn't be heavy handed.
    let p = 1.0 + (p - 1.0) * 0.6;
    let mut lut = [[0u8; 256]; 3];
    for (c, h) in [&hist.r, &hist.g, &hist.b].iter().enumerate() {
        // Per-channel stretch pulled towards the luminance stretch so colour
        // casts are only partially corrected and results stay natural.
        let lo = llo + (percentile(h, 0.003) as f32 - llo) * 0.65;
        let hi = (lhi + (percentile(h, 0.997) as f32 - lhi) * 0.65).max(lo + 32.0);
        for i in 0..256 {
            let v = ((i as f32 - lo) / (hi - lo)).clamp(0.0, 1.0).powf(p);
            lut[c][i] = (v * 255.0 + 0.5) as u8;
        }
    }
    // LUT + a touch of saturation in a single pass.
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(4 * 4096).for_each(|chunk| {
        const SAT: f32 = 1.1;
        for px in chunk.chunks_exact_mut(4) {
            let r = lut[0][px[0] as usize] as f32;
            let g = lut[1][px[1] as usize] as f32;
            let b = lut[2][px[2] as usize] as f32;
            let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            px[0] = (l + (r - l) * SAT + 0.5).clamp(0.0, 255.0) as u8;
            px[1] = (l + (g - l) * SAT + 0.5).clamp(0.0, 255.0) as u8;
            px[2] = (l + (b - l) * SAT + 0.5).clamp(0.0, 255.0) as u8;
        }
    });
    out
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
    fn auto_levels_stretches() {
        let img = RgbaImage::from_fn(64, 64, |x, _| image::Rgba([60 + x as u8, 60 + x as u8, 60 + x as u8, 255]));
        let out = auto_levels(&img);
        assert_eq!(out.dimensions(), img.dimensions());
        let (mn, mx) = out.pixels().fold((255u8, 0u8), |(a, b), p| (a.min(p[0]), b.max(p[0])));
        assert!(mn < 10 && mx > 245, "{mn} {mx}");
        let one = RgbaImage::from_pixel(1, 1, image::Rgba([5, 5, 5, 255]));
        assert_eq!(auto_levels(&one).dimensions(), (1, 1));
    }
    #[test]
    fn crop_clamps() {
        let img = RgbaImage::new(10, 10);
        let c = crop(&img, 8, 8, 50, 50);
        assert_eq!(c.dimensions(), (2, 2));
    }
}
