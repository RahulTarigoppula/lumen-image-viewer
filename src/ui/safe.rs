//! Guarded math for layout and zoom. Sizes in a frame can be 0, negative or
//! NaN for a moment (window minimized, moved to a display with another
//! scale, a panel squeezed to nothing), and `f32::clamp` panics on an
//! inverted or NaN range. Use these instead of `clamp` whenever a bound is
//! computed from a size, and `color_image` for any texture whose size comes
//! from an image (the GPU has a maximum texture side and egui asserts it).

use egui::ColorImage;
use image::RgbaImage;

/// `v` limited to `lo..=hi` that never panics: an inverted range collapses
/// to `lo`, NaN bounds are ignored and a NaN `v` becomes `lo` (or `hi`).
pub fn safe_clamp(v: f32, lo: f32, hi: f32) -> f32 {
    let r = v.max(lo).min(hi.max(lo));
    if r.is_nan() {
        if lo.is_finite() {
            lo
        } else if hi.is_finite() {
            hi
        } else {
            0.0
        }
    } else {
        r
    }
}

/// Zoom factors must stay finite and positive (a collapsed view gives 0 or NaN).
pub fn sane_zoom(z: f32) -> f32 {
    if z.is_finite() && z > 0.0 {
        z.clamp(0.001, 64.0)
    } else {
        1.0
    }
}

/// `a / b`, or `fallback` when `b` is 0 or the result is not finite.
pub fn safe_div(a: f32, b: f32, fallback: f32) -> f32 {
    let r = a / b;
    if b != 0.0 && r.is_finite() {
        r
    } else {
        fallback
    }
}

/// `v` when finite, else `fallback`.
pub fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

/// Size `(w, h)` scaled down to fit `max_side` on its longer side (at least 1x1).
pub fn fit_side(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let (w, h, m) = (w.max(1), h.max(1), max_side.max(1));
    if w <= m && h <= m {
        return (w, h);
    }
    let s = m as f64 / w.max(h) as f64;
    (((w as f64 * s).round() as u32).clamp(1, m), ((h as f64 * s).round() as u32).clamp(1, m))
}

/// The largest texture side this GPU accepts (2048 until the backend reports it).
pub fn max_texture_side(ctx: &egui::Context) -> u32 {
    let m = ctx.input(|i| i.max_texture_side);
    if m == 0 { 2048 } else { m.min(u32::MAX as usize) as u32 }
}

/// Texture data for `img` that the GPU can take: an empty image becomes one
/// transparent pixel and an image larger than the maximum texture side is
/// scaled down (so the caller must not rely on texel = pixel then).
pub fn color_image(ctx: &egui::Context, img: &RgbaImage) -> ColorImage {
    if img.width() == 0 || img.height() == 0 {
        return ColorImage::filled([1, 1], egui::Color32::TRANSPARENT);
    }
    let max = max_texture_side(ctx);
    if img.width() <= max && img.height() <= max {
        return crate::render::color_image_from_rgba(img);
    }
    let (w, h) = fit_side(img.width(), img.height(), max);
    log::warn!("image of {}x{} scaled to {w}x{h} for the GPU (largest texture side {max})", img.width(), img.height());
    crate::render::color_image_from_rgba(&crate::imaging::resize_to_fit(img, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_never_panics() {
        assert_eq!(safe_clamp(5.0, 0.0, -3.0), 0.0);
        assert_eq!(safe_clamp(5.0, 0.0, 2.0), 2.0);
        assert_eq!(safe_clamp(-1.0, 0.0, 2.0), 0.0);
        assert_eq!(safe_clamp(1.0, 0.0, 2.0), 1.0);
        assert!(safe_clamp(1.0, f32::NAN, f32::NAN).is_finite());
        assert_eq!(safe_clamp(f32::NAN, 3.0, 9.0), 3.0);
        assert_eq!(safe_clamp(f32::NAN, f32::NAN, 9.0), 9.0);
        assert_eq!(safe_clamp(f32::NAN, f32::NAN, f32::NAN), 0.0);
        assert_eq!(safe_clamp(f32::INFINITY, 0.0, 4.0), 4.0);
    }

    #[test]
    fn zoom_is_always_usable() {
        assert_eq!(sane_zoom(f32::NAN), 1.0);
        assert_eq!(sane_zoom(0.0), 1.0);
        assert_eq!(sane_zoom(-2.0), 1.0);
        assert_eq!(sane_zoom(f32::INFINITY), 1.0);
        assert_eq!(sane_zoom(1e-9), 0.001);
        assert_eq!(sane_zoom(1000.0), 64.0);
        assert_eq!(sane_zoom(0.5), 0.5);
    }

    #[test]
    fn division_guards() {
        assert_eq!(safe_div(1.0, 0.0, 7.0), 7.0);
        assert_eq!(safe_div(0.0, 0.0, 7.0), 7.0);
        assert_eq!(safe_div(1.0, f32::NAN, 7.0), 7.0);
        assert_eq!(safe_div(6.0, 3.0, 7.0), 2.0);
        assert_eq!(finite_or(f32::NAN, 2.0), 2.0);
        assert_eq!(finite_or(-f32::INFINITY, 2.0), 2.0);
        assert_eq!(finite_or(3.0, 2.0), 3.0);
    }

    #[test]
    fn fit_side_keeps_aspect_and_bounds() {
        assert_eq!(fit_side(100, 50, 200), (100, 50));
        assert_eq!(fit_side(40000, 20000, 16384), (16384, 8192));
        assert_eq!(fit_side(1, 100000, 16384), (1, 16384));
        assert_eq!(fit_side(0, 0, 0), (1, 1));
    }
}
