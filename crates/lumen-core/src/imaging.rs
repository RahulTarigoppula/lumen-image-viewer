//! Fast resampling helpers (SIMD + multithreaded via fast_image_resize).

use fast_image_resize as fr;
use image::RgbaImage;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Filter {
    Nearest,
    Bilinear,
    Bicubic,
    Lanczos3,
    Mitchell,
}

impl Filter {
    pub const ALL: [Filter; 5] = [Filter::Lanczos3, Filter::Bicubic, Filter::Mitchell, Filter::Bilinear, Filter::Nearest];
    pub fn label(self) -> &'static str {
        match self {
            Filter::Nearest => "Nearest neighbour (pixel art)",
            Filter::Bilinear => "Bilinear",
            Filter::Bicubic => "Bicubic (Catmull-Rom)",
            Filter::Lanczos3 => "Lanczos 3 (sharpest)",
            Filter::Mitchell => "Mitchell (smooth)",
        }
    }
    fn alg(self) -> fr::ResizeAlg {
        match self {
            Filter::Nearest => fr::ResizeAlg::Nearest,
            Filter::Bilinear => fr::ResizeAlg::Convolution(fr::FilterType::Bilinear),
            Filter::Bicubic => fr::ResizeAlg::Convolution(fr::FilterType::CatmullRom),
            Filter::Lanczos3 => fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3),
            Filter::Mitchell => fr::ResizeAlg::Convolution(fr::FilterType::Mitchell),
        }
    }
}

pub fn resize_exact(src: &RgbaImage, w: u32, h: u32, filter: Filter) -> RgbaImage {
    let w = w.max(1);
    let h = h.max(1);
    if src.width() == w && src.height() == h {
        return src.clone();
    }
    let mut dst = RgbaImage::new(w, h);
    let mut resizer = fr::Resizer::new();
    let opts = fr::ResizeOptions::new().resize_alg(filter.alg()).use_alpha(true);
    if resizer.resize(src, &mut dst, &opts).is_err() {
        // Fall back to the image crate if SIMD path refuses the input.
        return image::imageops::resize(src, w, h, image::imageops::FilterType::Triangle);
    }
    dst
}

/// Scale down (never up) so the image fits within `max_w` x `max_h`.
pub fn resize_to_fit(src: &RgbaImage, max_w: u32, max_h: u32) -> RgbaImage {
    let (w, h) = src.dimensions();
    if w <= max_w && h <= max_h {
        return src.clone();
    }
    let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let nw = ((w as f64 * s).round() as u32).max(1);
    let nh = ((h as f64 * s).round() as u32).max(1);
    // For big reductions, a cheap pre-shrink then a quality pass keeps it fast.
    let filter = if s < 0.25 { Filter::Bilinear } else { Filter::Lanczos3 };
    resize_exact(src, nw, nh, filter)
}

/// Halve each dimension (2x2 box average) - used to build display pyramids.
pub fn half(src: &RgbaImage) -> RgbaImage {
    let nw = (src.width() / 2).max(1);
    let nh = (src.height() / 2).max(1);
    let mut dst = RgbaImage::new(nw, nh);
    let mut resizer = fr::Resizer::new();
    let opts = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Box))
        .use_alpha(true);
    if resizer.resize(src, &mut dst, &opts).is_err() {
        return image::imageops::resize(src, nw, nh, image::imageops::FilterType::Triangle);
    }
    dst
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Deterministic photo-like content (smooth gradients plus noise).
    pub(crate) fn synth(w: u32, h: u32, alpha: bool) -> RgbaImage {
        let mut s = 0x9E37_79B9u32;
        let mut v = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h {
            for x in 0..w {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                let n = (s & 15) as u8;
                let a = if alpha { ((x * 7 + y * 3) % 256) as u8 } else { 255 };
                v.extend_from_slice(&[((x * 255 / w) as u8).wrapping_add(n), ((y * 255 / h) as u8).wrapping_add(n), ((x ^ y) as u8) / 2 + n, a]);
            }
        }
        RgbaImage::from_raw(w, h, v).unwrap()
    }

    pub(crate) fn best_ms(runs: usize, mut f: impl FnMut()) -> f64 {
        (0..runs)
            .map(|_| {
                let s = std::time::Instant::now();
                f();
                s.elapsed().as_secs_f64() * 1000.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// `cargo test --release -p lumen-core -- --ignored --nocapture bench_perf_`
    #[test]
    #[ignore]
    fn bench_perf_imaging() {
        for (w, h, alpha) in [(6000, 4000, false), (6000, 4000, true), (12000, 8000, false)] {
            let img = synth(w, h, alpha);
            let t = best_ms(5, || {
                std::hint::black_box(half(&img));
            });
            eprintln!("half {w}x{h} alpha={alpha}: {t:.2} ms");
            let t = best_ms(5, || {
                std::hint::black_box(resize_to_fit(&img, 384, 384));
            });
            eprintln!("resize_to_fit 384 {w}x{h} alpha={alpha}: {t:.2} ms");
        }
    }
}
