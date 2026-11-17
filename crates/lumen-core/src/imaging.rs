//! Fast resampling helpers (SIMD + multithreaded via fast_image_resize).

use fast_image_resize as fr;
use image::RgbaImage;
use rayon::prelude::*;

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
///
/// Each output pixel is the rounded mean of one 2x2 block (alpha-weighted
/// where the block is not fully opaque, so transparent pixels do not darken
/// their neighbours). An odd last row or column is left out. One pass over
/// the source, rows in parallel: several times faster than a general
/// resampler, which matters for every image opened in the viewer.
pub fn half(src: &RgbaImage) -> RgbaImage {
    let (w, h) = src.dimensions();
    if w < 2 || h < 2 {
        return half_resampled(src);
    }
    let nw = w / 2;
    let sstride = w as usize * 4;
    let dstride = nw as usize * 4;
    let s = src.as_raw();
    let mut dst = RgbaImage::new(nw, h / 2);
    // Bands of rows: enough work per task even for narrow images.
    let band = (64 * 1024 / dstride).max(1);
    dst.par_chunks_mut(dstride * band).enumerate().for_each(|(bi, out)| {
        for (r, orow) in out.chunks_exact_mut(dstride).enumerate() {
            let y = (bi * band + r) * 2;
            let r0 = &s[y * sstride..y * sstride + dstride * 2];
            let r1 = &s[(y + 1) * sstride..(y + 1) * sstride + dstride * 2];
            half_row(r0, r1, orow);
        }
    });
    dst
}

/// One output row of [`half`]: `r0` and `r1` are two source rows of exactly
/// twice the output width.
#[inline]
fn half_row(r0: &[u8], r1: &[u8], out: &mut [u8]) {
    let a = r0.as_chunks::<8>().0;
    let b = r1.as_chunks::<8>().0;
    let o = out.as_chunks_mut::<4>().0;
    // Plain average of every channel (vectorizes), then redo the blocks
    // that have any transparency, if the two rows have any.
    for ((o, p), q) in o.iter_mut().zip(a).zip(b) {
        for c in 0..4 {
            o[c] = ((p[c] as u16 + p[c + 4] as u16 + q[c] as u16 + q[c + 4] as u16 + 2) >> 2) as u8;
        }
    }
    let opaque = a.iter().chain(b).fold(255u8, |m, p| m & p[3] & p[7]) == 255;
    if opaque {
        return;
    }
    for ((o, p), q) in o.iter_mut().zip(a).zip(b) {
        if p[3] & p[7] & q[3] & q[7] == 255 {
            continue;
        }
        let w = [p[3] as u32, p[7] as u32, q[3] as u32, q[7] as u32];
        let sa = w[0] + w[1] + w[2] + w[3];
        if sa == 0 {
            *o = [0, 0, 0, 0];
            continue;
        }
        for c in 0..3 {
            let sum = p[c] as u32 * w[0] + p[c + 4] as u32 * w[1] + q[c] as u32 * w[2] + q[c + 4] as u32 * w[3];
            o[c] = ((sum + sa / 2) / sa) as u8;
        }
        o[3] = ((sa + 2) >> 2) as u8;
    }
}

/// The general resampler's 2x halving (also used for images 1 pixel wide or high).
fn half_resampled(src: &RgbaImage) -> RgbaImage {
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

/// Apply EXIF orientation `o` (2..=8; any other value returns `img`
/// unchanged). Gives the same pixels as the image crate's rotate / flip
/// combinations (5 = transpose, 6 = rotate 90, 7 = transverse, 8 = rotate
/// 270), but flips work in place and the rotations copy in cache-sized tiles
/// on all cores instead of one bounds-checked pixel at a time. Portrait
/// photos from phones (orientation 6) take this path on every open.
pub fn orient(mut img: RgbaImage, o: u8) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    if !(2..=8).contains(&o) {
        return img;
    }
    if w == 0 || h == 0 {
        return if o >= 5 { RgbaImage::new(h, w) } else { img };
    }
    let stride = wu * 4;
    match o {
        2 => {
            img.par_chunks_mut(stride).for_each(|row| row.as_chunks_mut::<4>().0.reverse());
            img
        }
        3 | 4 => {
            let (top, rest) = img.split_at_mut(hu / 2 * stride);
            let (mid, bottom) = rest.split_at_mut((hu % 2) * stride);
            if o == 3 {
                mid.as_chunks_mut::<4>().0.reverse();
            }
            top.par_chunks_mut(stride).zip(bottom.par_chunks_mut(stride).rev()).for_each(|(a, b)| {
                a.swap_with_slice(b);
                if o == 3 {
                    a.as_chunks_mut::<4>().0.reverse();
                    b.as_chunks_mut::<4>().0.reverse();
                }
            });
            img
        }
        _ => {
            // Output pixel (xo, yo) = source (fx(yo), fy(xo)).
            let flip_x = matches!(o, 7 | 8);
            let flip_y = matches!(o, 6 | 7);
            let src = img.as_raw().as_chunks::<4>().0;
            let ow = hu;
            let mut out = RgbaImage::new(h, w);
            const B: usize = 64;
            out.par_chunks_mut(ow * 4 * B).enumerate().for_each(|(band, chunk)| {
                let px = chunk.as_chunks_mut::<4>().0;
                let rows = px.len() / ow;
                for x0 in (0..ow).step_by(B) {
                    let x1 = (x0 + B).min(ow);
                    for r in 0..rows {
                        let yo = band * B + r;
                        let sx = if flip_x { wu - 1 - yo } else { yo };
                        for (xo, p) in (x0..x1).zip(&mut px[r * ow + x0..r * ow + x1]) {
                            let sy = if flip_y { hu - 1 - xo } else { xo };
                            *p = src[sy * wu + sx];
                        }
                    }
                }
            });
            out
        }
    }
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
                std::hint::black_box(half_resampled(&img));
            });
            eprintln!("  resampler, alpha-aware {w}x{h} alpha={alpha}: {t:.2} ms");
            let t = best_ms(5, || {
                std::hint::black_box(half_resampler_opaque(&img));
            });
            eprintln!("  resampler, use_alpha off {w}x{h} alpha={alpha}: {t:.2} ms");
            let t = best_ms(5, || {
                std::hint::black_box(resize_to_fit(&img, 384, 384));
            });
            eprintln!("resize_to_fit 384 {w}x{h} alpha={alpha}: {t:.2} ms");
        }
        let img = synth(6000, 4000, false);
        for o in [3u8, 6] {
            let mut best = f64::INFINITY;
            for _ in 0..3 {
                let c = img.clone();
                let s = std::time::Instant::now();
                std::hint::black_box(old_orient(c, o));
                best = best.min(s.elapsed().as_secs_f64() * 1000.0);
            }
            eprintln!("  image crate orientation {o} 24 MP: {best:.2} ms");
        }
    }

    fn half_resampler_opaque(src: &RgbaImage) -> RgbaImage {
        let mut dst = RgbaImage::new(src.width() / 2, src.height() / 2);
        let opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Box)).use_alpha(false);
        fr::Resizer::new().resize(src, &mut dst, &opts).unwrap();
        dst
    }

    /// The image crate operations `decode::apply_orientation` used before `orient`.
    fn old_orient(img: RgbaImage, orientation: u8) -> RgbaImage {
        use image::imageops;
        match orientation {
            2 => imageops::flip_horizontal(&img),
            3 => imageops::rotate180(&img),
            4 => imageops::flip_vertical(&img),
            5 => imageops::flip_horizontal(&imageops::rotate90(&img)),
            6 => imageops::rotate90(&img),
            7 => imageops::flip_horizontal(&imageops::rotate270(&img)),
            8 => imageops::rotate270(&img),
            _ => img,
        }
    }

    #[test]
    fn orient_matches_image_crate() {
        for (w, h) in [(1, 1), (1, 7), (7, 1), (2, 3), (5, 4), (63, 65), (130, 67), (64, 128)] {
            let img = synth(w, h, true);
            for o in 0..=9u8 {
                let a = orient(img.clone(), o);
                let b = old_orient(img.clone(), o);
                assert_eq!(a.dimensions(), b.dimensions(), "{w}x{h} o={o}");
                assert!(a.as_raw() == b.as_raw(), "{w}x{h} o={o}");
            }
        }
        assert_eq!(orient(RgbaImage::new(0, 3), 6).dimensions(), (3, 0));
    }

    #[test]
    fn half_is_the_2x2_mean() {
        // Opaque: within 1 of the exact mean, and of the resampler it replaced.
        for (w, h) in [(2, 2), (64, 48), (65, 47), (301, 3)] {
            let img = synth(w, h, false);
            let out = half(&img);
            assert_eq!(out.dimensions(), (w / 2, h / 2));
            for (x, y, p) in out.enumerate_pixels() {
                for c in 0..4 {
                    let s: u32 = [(0, 0), (1, 0), (0, 1), (1, 1)].iter().map(|(dx, dy)| img.get_pixel(2 * x + dx, 2 * y + dy).0[c] as u32).sum();
                    assert!((p.0[c] as f64 - s as f64 / 4.0).abs() <= 0.5, "{w}x{h} at {x},{y}");
                }
            }
            if w % 2 == 0 && h % 2 == 0 {
                let old = half_resampled(&img);
                let worst = out.as_raw().iter().zip(old.as_raw()).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
                assert!(worst <= 1, "{w}x{h}: {worst}");
            }
        }
        // Transparency: colours are alpha-weighted, fully transparent blocks are zero.
        let mut img = RgbaImage::from_pixel(4, 2, image::Rgba([10, 20, 30, 0]));
        img.put_pixel(0, 0, image::Rgba([200, 100, 50, 255]));
        img.put_pixel(2, 0, image::Rgba([100, 100, 100, 128]));
        img.put_pixel(3, 1, image::Rgba([0, 0, 0, 128]));
        let out = half(&img);
        assert_eq!(out.get_pixel(0, 0).0, [200, 100, 50, 64]);
        assert_eq!(out.get_pixel(1, 0).0, [50, 50, 50, 64]);
        let alpha = synth(66, 40, true);
        for (x, y, p) in half(&alpha).enumerate_pixels() {
            let px: Vec<[u8; 4]> = [(0, 0), (1, 0), (0, 1), (1, 1)].iter().map(|(dx, dy)| alpha.get_pixel(2 * x + dx, 2 * y + dy).0).collect();
            let sa: f64 = px.iter().map(|q| q[3] as f64).sum();
            assert!((p.0[3] as f64 - sa / 4.0).abs() <= 0.5);
            if sa > 0.0 {
                for c in 0..3 {
                    let v: f64 = px.iter().map(|q| q[c] as f64 * q[3] as f64).sum::<f64>() / sa;
                    assert!((p.0[c] as f64 - v).abs() <= 0.5, "at {x},{y}");
                }
            }
        }
        // 1-pixel-high and wide images still halve the other side.
        assert_eq!(half(&synth(9, 1, false)).dimensions(), (4, 1));
        assert_eq!(half(&synth(1, 9, true)).dimensions(), (1, 4));
    }
}
