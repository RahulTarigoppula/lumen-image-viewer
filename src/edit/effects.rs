//! Artistic / corrective effects used by the Effects gallery.

use super::filters;
use super::ops::map_pixels;
use image::RgbaImage;
use rayon::prelude::*;

#[inline]
pub fn luma(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

#[inline]
fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Blend `fx` over `orig` by `t` (0..1). Alpha is blended too, so a partial
/// strength stays consistent with 100 % for effects that change alpha.
pub fn mix_images(orig: &RgbaImage, fx: &RgbaImage, t: f32) -> RgbaImage {
    let t = t.clamp(0.0, 1.0);
    if t >= 1.0 || orig.dimensions() != fx.dimensions() {
        return fx.clone();
    }
    let mut out = orig.clone();
    out.as_mut().par_chunks_mut(4 * 4096).zip(fx.as_raw().par_chunks(4 * 4096)).for_each(|(o, f)| {
        for (p, q) in o.chunks_exact_mut(4).zip(f.chunks_exact(4)) {
            for c in 0..4 {
                p[c] = (p[c] as f32 + (q[c] as f32 - p[c] as f32) * t).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    out
}

pub fn grayscale(img: &RgbaImage, t: f32) -> RgbaImage {
    map_pixels(img, |c| {
        let l = luma(c[0], c[1], c[2]);
        mix3(c, [l, l, l], t)
    })
}

/// Black & white through a coloured lens filter: weights for R, G, B.
pub fn bw_filter_weights(filter: usize) -> [f32; 3] {
    match filter {
        1 => [0.80, 0.20, 0.00],  // red: dark skies, light skin
        2 => [0.60, 0.35, 0.05],  // orange
        3 => [0.45, 0.50, 0.05],  // yellow
        4 => [0.15, 0.75, 0.10],  // green: foliage
        5 => [0.10, 0.25, 0.65],  // blue
        _ => [0.2126, 0.7152, 0.0722],
    }
}

pub fn bw_filter(img: &RgbaImage, filter: usize, t: f32) -> RgbaImage {
    let w = bw_filter_weights(filter);
    map_pixels(img, move |c| {
        let l = c[0] * w[0] + c[1] * w[1] + c[2] * w[2];
        mix3(c, [l, l, l], t)
    })
}

pub fn sepia(img: &RgbaImage, t: f32) -> RgbaImage {
    map_pixels(img, |c| {
        let s = [
            0.393 * c[0] + 0.769 * c[1] + 0.189 * c[2],
            0.349 * c[0] + 0.686 * c[1] + 0.168 * c[2],
            0.272 * c[0] + 0.534 * c[1] + 0.131 * c[2],
        ];
        mix3(c, s, t)
    })
}

pub fn invert(img: &RgbaImage) -> RgbaImage {
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(4 * 4096).for_each(|c| {
        for p in c.chunks_exact_mut(4) {
            p[0] = 255 - p[0];
            p[1] = 255 - p[1];
            p[2] = 255 - p[2];
        }
    });
    out
}

pub fn posterize(img: &RgbaImage, levels: u32) -> RgbaImage {
    let n = levels.clamp(2, 256) as f32 - 1.0;
    let lut: [u8; 256] = std::array::from_fn(|i| ((i as f32 / 255.0 * n).round() / n * 255.0).round() as u8);
    super::ops::apply_lut(img, &[lut, lut, lut])
}

pub fn solarize(img: &RgbaImage, threshold: u8) -> RgbaImage {
    let lut: [u8; 256] = std::array::from_fn(|i| if i as u8 >= threshold { 255 - i as u8 } else { i as u8 });
    super::ops::apply_lut(img, &[lut, lut, lut])
}

/// Map luminance onto a two-colour gradient (shadows → highlights).
pub fn duotone(img: &RgbaImage, dark: [u8; 3], light: [u8; 3], t: f32) -> RgbaImage {
    let d = [dark[0] as f32 / 255.0, dark[1] as f32 / 255.0, dark[2] as f32 / 255.0];
    let l = [light[0] as f32 / 255.0, light[1] as f32 / 255.0, light[2] as f32 / 255.0];
    map_pixels(img, move |c| {
        let y = luma(c[0], c[1], c[2]);
        mix3(c, mix3(d, l, y), t)
    })
}

fn luma_plane(img: &RgbaImage) -> Vec<f32> {
    img.as_raw().par_chunks(4).map(|p| luma(p[0] as f32, p[1] as f32, p[2] as f32)).collect()
}

/// Grey relief: difference of luminance across a diagonal at `depth` px.
pub fn emboss(img: &RgbaImage, depth: f32, t: f32) -> RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 {
        return img.clone();
    }
    let d = depth.max(0.5);
    let l = luma_plane(img);
    let lw = |x: f32, y: f32| -> f32 {
        let xi = (x.round() as i64).clamp(0, w as i64 - 1) as usize;
        let yi = (y.round() as i64).clamp(0, h as i64 - 1) as usize;
        l[yi * w + xi]
    };
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let (xf, yf) = (x as f32, y as f32);
            let v = (128.0 + (lw(xf + d, yf + d) - lw(xf - d, yf - d)) * 2.0).clamp(0.0, 255.0);
            for c in 0..3 {
                let o = row[x * 4 + c] as f32;
                row[x * 4 + c] = (o + (v - o) * t + 0.5) as u8;
            }
        }
    });
    out
}

/// Sobel edge magnitude, light edges on black.
pub fn edges(img: &RgbaImage, t: f32) -> RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 {
        return img.clone();
    }
    let l = luma_plane(img);
    let at = |x: i64, y: i64| l[(y.clamp(0, h as i64 - 1) as usize) * w + x.clamp(0, w as i64 - 1) as usize];
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let y = y as i64;
        for x in 0..w as i64 {
            let gx = at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1) - at(x - 1, y - 1) - 2.0 * at(x - 1, y) - at(x - 1, y + 1);
            let gy = at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1) - at(x - 1, y - 1) - 2.0 * at(x, y - 1) - at(x + 1, y - 1);
            let v = ((gx * gx + gy * gy).sqrt() * 0.5).min(255.0);
            let i = x as usize * 4;
            for c in 0..3 {
                let o = row[i + c] as f32;
                row[i + c] = (o + (v - o) * t + 0.5) as u8;
            }
        }
    });
    out
}

/// Kuwahara filter (oil-paint look): each pixel takes the mean colour of the
/// least-varying of its four (r+1)² quadrants. Uses per-tile integral images.
pub fn oil_paint(img: &RgbaImage, radius: u32) -> RgbaImage {
    let r = radius.clamp(1, 24);
    filters::tiled(img, r, |t| {
        let (w, h) = (t.width() as usize, t.height() as usize);
        let raw = t.as_raw();
        let iw = w + 1;
        // Integral images: R, G, B, L, L² (f64 so large tiles never overflow).
        let mut ii = vec![[0f64; 5]; iw * (h + 1)];
        for y in 0..h {
            let mut row = [0f64; 5];
            for x in 0..w {
                let p = &raw[(y * w + x) * 4..];
                let l = luma(p[0] as f32, p[1] as f32, p[2] as f32) as f64;
                let v = [p[0] as f64, p[1] as f64, p[2] as f64, l, l * l];
                for k in 0..5 {
                    row[k] += v[k];
                    ii[(y + 1) * iw + x + 1][k] = ii[y * iw + x + 1][k] + row[k];
                }
            }
        }
        let rect = |x0: usize, y0: usize, x1: usize, y1: usize| -> [f64; 5] {
            // inclusive-exclusive [x0,x1) × [y0,y1)
            let mut s = [0f64; 5];
            for k in 0..5 {
                s[k] = ii[y1 * iw + x1][k] - ii[y0 * iw + x1][k] - ii[y1 * iw + x0][k] + ii[y0 * iw + x0][k];
            }
            s
        };
        let mut out = t.clone();
        let ri = r as usize;
        let n = ((ri + 1) * (ri + 1)) as f64;
        for y in ri..h.saturating_sub(ri) {
            for x in ri..w.saturating_sub(ri) {
                let quads = [(x - ri, y - ri), (x, y - ri), (x - ri, y), (x, y)];
                let mut best = f64::MAX;
                let mut col = [0f64; 3];
                for (qx, qy) in quads {
                    let s = rect(qx, qy, qx + ri + 1, qy + ri + 1);
                    let mean_l = s[3] / n;
                    let var = s[4] / n - mean_l * mean_l;
                    if var < best {
                        best = var;
                        col = [s[0] / n, s[1] / n, s[2] / n];
                    }
                }
                let o = &mut out.as_mut()[(y * w + x) * 4..];
                for c in 0..3 {
                    o[c] = (col[c] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        }
        out
    })
}

/// Mosaic of `block`×`block` cells (fractional sizes allowed for previews).
pub fn pixelate(img: &RgbaImage, block: f32) -> RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let bs = block.max(1.0);
    if w == 0 || h == 0 || bs <= 1.0 {
        return img.clone();
    }
    let cell = |i: usize| (i as f32 / bs) as usize;
    let start = |b: usize| (b as f32 * bs).ceil() as usize;
    let (nbx, nby) = (cell(w - 1) + 1, cell(h - 1) + 1);
    let raw = img.as_raw();
    // Average colour per cell (alpha-weighted).
    let cells: Vec<Vec<[u8; 4]>> = (0..nby)
        .into_par_iter()
        .map(|by| {
            let (y0, y1) = (start(by).saturating_sub(1).min(h), (start(by + 1) + 1).min(h));
            let mut acc = vec![[0f64; 5]; nbx];
            for y in (y0..y1).filter(|&y| cell(y) == by) {
                for x in 0..w {
                    let p = &raw[(y * w + x) * 4..];
                    let a = p[3] as f64;
                    let s = &mut acc[cell(x).min(nbx - 1)];
                    s[0] += p[0] as f64 * a;
                    s[1] += p[1] as f64 * a;
                    s[2] += p[2] as f64 * a;
                    s[3] += a;
                    s[4] += 1.0;
                }
            }
            acc.iter()
                .map(|s| {
                    if s[3] <= 0.0 {
                        [0, 0, 0, 0]
                    } else {
                        [(s[0] / s[3] + 0.5) as u8, (s[1] / s[3] + 0.5) as u8, (s[2] / s[3] + 0.5) as u8, (s[3] / s[4].max(1.0) + 0.5) as u8]
                    }
                })
                .collect()
        })
        .collect();
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let cr = &cells[cell(y).min(nby - 1)];
        for x in 0..w {
            row[x * 4..x * 4 + 4].copy_from_slice(&cr[cell(x).min(nbx - 1)]);
        }
    });
    out
}

/// Darken (amount > 0) or lighten (amount < 0) towards the edges.
/// `midpoint` 0..1 = where the falloff starts, `feather` 0..1 = its softness.
pub fn vignette(img: &RgbaImage, amount: f32, midpoint: f32, feather: f32) -> RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 || amount == 0.0 {
        return img.clone();
    }
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let mid = midpoint.clamp(0.0, 1.0);
    let fe = feather.clamp(0.02, 1.0);
    let amt = amount.clamp(-1.0, 1.0);
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let ny = (y as f32 + 0.5 - cy) / cy;
        for x in 0..w {
            let nx = (x as f32 + 0.5 - cx) / cx;
            let d = ((nx * nx + ny * ny) / 2.0).sqrt();
            let t = ((d - mid) / fe).clamp(0.0, 1.0);
            let f = t * t * (3.0 - 2.0 * t);
            if f <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let v = row[x * 4 + c] as f32;
                let nv = if amt > 0.0 { v * (1.0 - amt * f) } else { v + (255.0 - v) * (-amt) * f };
                row[x * 4 + c] = (nv + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
    });
    out
}

#[inline]
fn hash2(x: i64, y: i64, seed: u32) -> f32 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ (seed as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 31;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 29;
    (h >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
}

/// Deterministic monochrome film grain. `amount` 0..1, `size` cell size in px.
pub fn film_grain(img: &RgbaImage, amount: f32, size: f32, seed: u32) -> RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 || amount <= 0.0 {
        return img.clone();
    }
    let s = size.max(1.0);
    let amp = amount.clamp(0.0, 1.0) * 80.0;
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let fy = y as f32 / s;
        let (y0, ty) = (fy.floor() as i64, fy - fy.floor());
        for x in 0..w {
            let fx = x as f32 / s;
            let (x0, tx) = (fx.floor() as i64, fx - fx.floor());
            let n = if s <= 1.0 {
                hash2(x as i64, y as i64, seed)
            } else {
                let a = hash2(x0, y0, seed) + (hash2(x0 + 1, y0, seed) - hash2(x0, y0, seed)) * tx;
                let b = hash2(x0, y0 + 1, seed) + (hash2(x0 + 1, y0 + 1, seed) - hash2(x0, y0 + 1, seed)) * tx;
                (a + (b - a) * ty) * 1.4
            };
            let p = &mut row[x * 4..x * 4 + 4];
            let l = luma(p[0] as f32, p[1] as f32, p[2] as f32) / 255.0;
            // Grain is strongest in the midtones, like real film.
            let k = amp * n * (0.35 + 2.6 * l * (1.0 - l));
            for c in 0..3 {
                p[c] = (p[c] as f32 + k + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
    });
    out
}

/// Noise reduction: median (removes specks) then an edge-preserving blur.
pub fn denoise(img: &RgbaImage, radius: u32, smooth: f32) -> RgbaImage {
    let m = filters::median(img, radius);
    if smooth > 0.0 {
        filters::surface_blur(&m, smooth, 14.0)
    } else {
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RgbaImage {
        RgbaImage::from_fn(37, 23, |x, y| image::Rgba([(x * 7) as u8, (y * 11) as u8, ((x * y) % 256) as u8, 255]))
    }

    #[test]
    fn invert_twice_is_identity() {
        let img = sample();
        assert_eq!(invert(&invert(&img)), img);
    }

    #[test]
    fn strength_zero_is_identity() {
        let img = sample();
        assert_eq!(grayscale(&img, 0.0), img);
        assert_eq!(sepia(&img, 0.0), img);
        assert_eq!(bw_filter(&img, 2, 0.0), img);
        assert_eq!(duotone(&img, [0, 0, 0], [255, 255, 255], 0.0), img);
        assert_eq!(emboss(&img, 2.0, 0.0), img);
        assert_eq!(edges(&img, 0.0), img);
        assert_eq!(vignette(&img, 0.0, 0.5, 0.5), img);
        assert_eq!(film_grain(&img, 0.0, 1.0, 1), img);
        assert_eq!(pixelate(&img, 1.0), img);
        assert_eq!(posterize(&img, 256), img);
        assert_eq!(mix_images(&img, &invert(&img), 0.0), img);
    }

    #[test]
    fn grayscale_is_gray() {
        let g = grayscale(&sample(), 1.0);
        assert!(g.pixels().all(|p| p[0] == p[1] && p[1] == p[2]));
    }

    #[test]
    fn posterize_levels() {
        let p = posterize(&sample(), 2);
        assert!(p.pixels().all(|q| (0..3).all(|c| q[c] == 0 || q[c] == 255)));
    }

    #[test]
    fn spatial_effects_keep_size_and_flat() {
        let img = sample();
        for out in [oil_paint(&img, 3), pixelate(&img, 4.5), edges(&img, 1.0), emboss(&img, 2.0, 1.0), film_grain(&img, 0.5, 2.0, 7), denoise(&img, 2, 1.0), vignette(&img, 0.5, 0.3, 0.5)] {
            assert_eq!(out.dimensions(), img.dimensions());
        }
        let flat = RgbaImage::from_pixel(30, 20, image::Rgba([120, 60, 30, 255]));
        assert_eq!(oil_paint(&flat, 4), flat);
        assert_eq!(pixelate(&flat, 7.0), flat);
        // Grain is deterministic.
        assert_eq!(film_grain(&img, 0.5, 1.5, 3), film_grain(&img, 0.5, 1.5, 3));
        let one = RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 4]));
        for out in [oil_paint(&one, 8), pixelate(&one, 50.0), edges(&one, 1.0), emboss(&one, 3.0, 1.0), vignette(&one, 1.0, 0.0, 0.1)] {
            assert_eq!(out.dimensions(), (1, 1));
        }
    }

    #[test]
    fn pixelate_blocks() {
        let p = pixelate(&sample(), 5.0);
        assert_eq!(p.get_pixel(0, 0), p.get_pixel(4, 4));
        assert_eq!(p.get_pixel(5, 0), p.get_pixel(9, 4));
    }
}
