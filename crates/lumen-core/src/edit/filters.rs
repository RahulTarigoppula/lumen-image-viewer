//! Spatial filters: Gaussian / box / motion / edge-preserving blur, unsharp
//! mask, median, plus small helpers (premultiplication, tiled processing,
//! bilinear sampling). Everything is multithreaded with rayon and uses
//! separable or running-sum kernels so large radii stay O(pixels).
//!
//! Images are straight-alpha RGBA8. Neighbourhood filters work on
//! premultiplied 16-bit copies (only when the image actually has transparency)
//! so transparent pixels don't bleed their hidden colour and semi-transparent
//! colours survive the round trip without 8-bit quantisation.

use image::RgbaImage;
use rayon::prelude::*;

// ---------------------------------------------------------------- alpha helpers

pub fn is_opaque(img: &RgbaImage) -> bool {
    img.as_raw().par_chunks(4 * 16384).all(|c| c.chunks_exact(4).all(|p| p[3] == 255))
}

/// A channel type the separable kernels can run on (8-bit for opaque images,
/// 16-bit premultiplied when there is transparency).
pub trait Px: Copy + Default + Send + Sync + 'static {
    /// Accumulator for 16.16 fixed-point kernels (weights summing to 65536).
    type Acc: Acc;
    fn get(self) -> u32;
    fn put(v: u32) -> Self;
}

/// Sum of `weight * value` products in a kernel: `u32` holds 65536 x 255
/// exactly (8-bit, and vectorizes far better), 16-bit channels need `u64`.
pub trait Acc: Copy + Default + Send + Sync + std::ops::AddAssign + 'static {
    fn mul(k: u32, v: u32) -> Self;
    /// `(self + 0.5) / 65536`, rounded down.
    fn round16(self) -> u32;
}

impl Acc for u32 {
    #[inline]
    fn mul(k: u32, v: u32) -> Self {
        k * v
    }
    #[inline]
    fn round16(self) -> u32 {
        (self + 32768) >> 16
    }
}

impl Acc for u64 {
    #[inline]
    fn mul(k: u32, v: u32) -> Self {
        k as u64 * v as u64
    }
    #[inline]
    fn round16(self) -> u32 {
        ((self + 32768) >> 16) as u32
    }
}

impl Px for u8 {
    type Acc = u32;
    #[inline]
    fn get(self) -> u32 {
        self as u32
    }
    #[inline]
    fn put(v: u32) -> Self {
        v.min(255) as u8
    }
}

impl Px for u16 {
    type Acc = u64;
    #[inline]
    fn get(self) -> u32 {
        self as u32
    }
    #[inline]
    fn put(v: u32) -> Self {
        v.min(65535) as u16
    }
}

/// Straight RGBA8 to premultiplied RGBA16 (alpha scaled to 0..65535).
pub fn premultiply16(img: &RgbaImage) -> Vec<u16> {
    let mut out = vec![0u16; img.as_raw().len()];
    out.par_chunks_mut(4 * 4096).zip(img.as_raw().par_chunks(4 * 4096)).for_each(|(o, i)| {
        for (q, p) in o.chunks_exact_mut(4).zip(i.chunks_exact(4)) {
            let a = p[3] as u32;
            for c in 0..3 {
                // c/255 * a/255 * 65535 == c * a * 257 / 255
                q[c] = ((p[c] as u32 * a * 257 + 127) / 255) as u16;
            }
            q[3] = (a * 257) as u16;
        }
    });
    out
}

/// Premultiplied RGBA16 back to straight RGBA8.
pub fn unpremultiply16(w: u32, h: u32, buf: &[u16]) -> RgbaImage {
    let mut out = vec![0u8; buf.len()];
    out.par_chunks_mut(4 * 4096).zip(buf.par_chunks(4 * 4096)).for_each(|(o, i)| {
        for (p, q) in o.chunks_exact_mut(4).zip(i.chunks_exact(4)) {
            let a16 = q[3] as u32;
            let a8 = (a16 + 128) / 257;
            if a8 == 0 {
                p.copy_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            for c in 0..3 {
                p[c] = ((q[c] as u32 * 255 + a16 / 2) / a16).min(255) as u8;
            }
            p[3] = a8.min(255) as u8;
        }
    });
    RgbaImage::from_raw(w, h, out).expect("buffer size")
}

/// Run a neighbourhood filter on the raw channels: directly on 8-bit data when
/// the image is opaque, otherwise on a premultiplied 16-bit copy. The closures
/// get `(src, width, height)` and must return a buffer of the same length.
pub fn with_premul(img: &RgbaImage, f8: impl FnOnce(&[u8], usize, usize) -> Vec<u8>, f16: impl FnOnce(&[u16], usize, usize) -> Vec<u16>) -> RgbaImage {
    let (w, h) = img.dimensions();
    if is_opaque(img) {
        let out = f8(img.as_raw(), w as usize, h as usize);
        RgbaImage::from_raw(w, h, out).unwrap_or_else(|| img.clone())
    } else {
        let pre = premultiply16(img);
        let out = f16(&pre, w as usize, h as usize);
        if out.len() != pre.len() {
            return img.clone();
        }
        unpremultiply16(w, h, &out)
    }
}

#[inline]
fn clampi(v: i64, max: usize) -> usize {
    v.clamp(0, max as i64 - 1) as usize
}

// ---------------------------------------------------------------- Gaussian / box blur

/// Gaussian blur with standard deviation `sigma` (pixels). Small sigmas use an
/// exact separable kernel; large ones three running-sum box passes.
pub fn gaussian_blur(img: &RgbaImage, sigma: f32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if !(sigma >= 0.2) || w == 0 || h == 0 {
        return img.clone();
    }
    let sigma = sigma.min(w.max(h) as f32).min(4000.0);
    with_premul(img, |s, w, h| gaussian_raw(s, w, h, sigma), |s, w, h| gaussian_raw(s, w, h, sigma))
}

/// Gaussian blur of a raw RGBA buffer (any channel depth).
pub fn gaussian_raw<P: Px>(src: &[P], w: usize, h: usize, sigma: f32) -> Vec<P> {
    if sigma <= 3.0 {
        let kernel = gauss_kernel(sigma);
        let mut tmp = vec![P::default(); w * h * 4];
        conv_h(src, &mut tmp, w, h, &kernel);
        let mut out = vec![P::default(); w * h * 4];
        conv_v(&tmp, &mut out, w, h, &kernel);
        out
    } else {
        let mut a = src.to_vec();
        let mut b = vec![P::default(); a.len()];
        for r in boxes_for_gauss(sigma, 3) {
            box_h(&a, &mut b, w, h, r);
            box_v(&b, &mut a, w, h, r);
        }
        a
    }
}

/// Plain box blur (one horizontal + one vertical pass) with the given radius.
pub fn box_blur(img: &RgbaImage, radius: usize) -> RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if radius == 0 || w == 0 || h == 0 {
        return img.clone();
    }
    let radius = radius.min(w.max(h)).min(8000);
    fn run<P: Px>(src: &[P], w: usize, h: usize, radius: usize) -> Vec<P> {
        let mut b = vec![P::default(); w * h * 4];
        box_h(src, &mut b, w, h, radius);
        let mut out = vec![P::default(); w * h * 4];
        box_v(&b, &mut out, w, h, radius);
        out
    }
    with_premul(img, |s, w, h| run(s, w, h, radius), |s, w, h| run(s, w, h, radius))
}

/// Integer kernel (weights sum to exactly 65536 so flat areas stay exact).
fn gauss_kernel(sigma: f32) -> Vec<u32> {
    let r = (sigma * 3.0).ceil().max(1.0) as i32;
    let ws: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f32 = ws.iter().sum();
    let mut k: Vec<u32> = ws.iter().map(|w| (w / sum * 65536.0).round() as u32).collect();
    let total: u32 = k.iter().sum();
    let c = r as usize;
    k[c] = (k[c] as i64 + 65536 - total as i64).max(0) as u32;
    k
}

fn conv_h<P: Px>(src: &[P], dst: &mut [P], w: usize, _h: usize, k: &[u32]) {
    let r = k.len() / 2;
    let stride = w * 4;
    // Pixels [x0, x1) have every tap inside the row: there each tap is one
    // pass over contiguous values (vectorizes). Border pixels clamp per tap.
    let x0 = r.min(w);
    let x1 = w.saturating_sub(r).max(x0);
    dst.par_chunks_mut(stride).zip(src.par_chunks(stride)).for_each_init(
        || vec![P::Acc::default(); (x1 - x0) * 4],
        |acc, (out, row)| {
            if x1 > x0 {
                // Here x0 == r, so every tap offset is in range.
                acc.fill(P::Acc::default());
                for (j, &kw) in k.iter().enumerate() {
                    let s = &row[(x0 + j - r) * 4..(x1 + j - r) * 4];
                    for (a, &v) in acc.iter_mut().zip(s) {
                        *a += P::Acc::mul(kw, v.get());
                    }
                }
                for (o, &a) in out[x0 * 4..x1 * 4].iter_mut().zip(acc.iter()) {
                    *o = P::put(a.round16());
                }
            }
            for x in (0..x0).chain(x1..w) {
                let mut sum = [P::Acc::default(); 4];
                for (j, &kw) in k.iter().enumerate() {
                    let sx = clampi(x as i64 + j as i64 - r as i64, w) * 4;
                    for c in 0..4 {
                        sum[c] += P::Acc::mul(kw, row[sx + c].get());
                    }
                }
                for c in 0..4 {
                    out[x * 4 + c] = P::put(sum[c].round16());
                }
            }
        },
    );
}

fn conv_v<P: Px>(src: &[P], dst: &mut [P], w: usize, h: usize, k: &[u32]) {
    let r = (k.len() / 2) as i64;
    let stride = w * 4;
    dst.par_chunks_mut(stride).enumerate().for_each_init(
        || vec![P::Acc::default(); stride],
        |acc, (y, out)| {
            acc.fill(P::Acc::default());
            for (j, &kw) in k.iter().enumerate() {
                let sy = clampi(y as i64 + j as i64 - r, h);
                let row = &src[sy * stride..(sy + 1) * stride];
                for (a, &v) in acc.iter_mut().zip(row) {
                    *a += P::Acc::mul(kw, v.get());
                }
            }
            for (o, &a) in out.iter_mut().zip(acc.iter()) {
                *o = P::put(a.round16());
            }
        },
    );
}

/// Box sizes approximating a Gaussian (Kovesi / Ivan Kutskir). Returns radii.
fn boxes_for_gauss(sigma: f32, n: usize) -> Vec<usize> {
    let nf = n as f32;
    let w_ideal = (12.0 * sigma * sigma / nf + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let m_ideal = (12.0 * sigma * sigma - nf * (wl * wl) as f32 - 4.0 * nf * wl as f32 - 3.0 * nf) / (-4.0 * wl as f32 - 4.0);
    let m = m_ideal.round() as i32;
    (0..n as i32).map(|i| (((if i < m { wl } else { wu }) - 1) / 2).max(0) as usize).collect()
}

#[inline]
fn div_round<P: Px>(sum: u32, inv: u64) -> P {
    P::put(((sum as u64 * inv + (1 << 31)) >> 32) as u32)
}

fn box_h<P: Px>(src: &[P], dst: &mut [P], w: usize, _h: usize, r: usize) {
    let stride = w * 4;
    let d = (2 * r + 1) as u64;
    let inv = ((1u64 << 32) + d / 2) / d;
    let r = r as i64;
    dst.par_chunks_mut(stride).zip(src.par_chunks(stride)).for_each(|(out, row)| {
        let mut s = [0u32; 4];
        for k in -r..=r {
            let i = clampi(k, w) * 4;
            for c in 0..4 {
                s[c] += row[i + c].get();
            }
        }
        for x in 0..w as i64 {
            let o = x as usize * 4;
            for c in 0..4 {
                out[o + c] = div_round(s[c], inv);
            }
            let add = clampi(x + r + 1, w) * 4;
            let sub = clampi(x - r, w) * 4;
            for c in 0..4 {
                s[c] = s[c] + row[add + c].get() - row[sub + c].get();
            }
        }
    });
}

fn box_v<P: Px>(src: &[P], dst: &mut [P], w: usize, h: usize, r: usize) {
    let stride = w * 4;
    let d = (2 * r + 1) as u64;
    let inv = ((1u64 << 32) + d / 2) / d;
    let bands = (rayon::current_num_threads() * 2).max(1);
    let band_h = h.div_ceil(bands).max(1);
    let r = r as i64;
    dst.par_chunks_mut(band_h * stride).enumerate().for_each(|(bi, out)| {
        let y0 = (bi * band_h) as i64;
        let rows = out.len() / stride;
        let mut sums = vec![0u32; stride];
        for k in -r..=r {
            let sy = clampi(y0 + k, h);
            for (s, &v) in sums.iter_mut().zip(&src[sy * stride..(sy + 1) * stride]) {
                *s += v.get();
            }
        }
        for yy in 0..rows {
            let y = y0 + yy as i64;
            let orow = &mut out[yy * stride..(yy + 1) * stride];
            for (o, &s) in orow.iter_mut().zip(&sums) {
                *o = div_round(s, inv);
            }
            if yy + 1 < rows {
                let add = clampi(y + r + 1, h) * stride;
                let sub = clampi(y - r, h) * stride;
                let (ra, rs) = (&src[add..add + stride], &src[sub..sub + stride]);
                for i in 0..stride {
                    sums[i] = sums[i] + ra[i].get() - rs[i].get();
                }
            }
        }
    });
}

// ---------------------------------------------------------------- sharpen

/// Unsharp mask. `amount` 1.0 = 100 %, `sigma` blur radius, `threshold` 0..255.
pub fn unsharp_mask(img: &RgbaImage, amount: f32, sigma: f32, threshold: u8) -> RgbaImage {
    if amount <= 0.0 || sigma < 0.2 {
        return img.clone();
    }
    let blur = gaussian_blur(img, sigma);
    let mut out = img.clone();
    let t = threshold as i32;
    out.as_mut().par_chunks_mut(4 * 4096).zip(blur.as_raw().par_chunks(4 * 4096)).for_each(|(o, b)| {
        for (p, q) in o.chunks_exact_mut(4).zip(b.chunks_exact(4)) {
            for c in 0..3 {
                let d = p[c] as i32 - q[c] as i32;
                if d.abs() > t || (t == 0 && d != 0) {
                    let v = p[c] as f32 + amount * d as f32;
                    p[c] = v.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    });
    out
}

// ---------------------------------------------------------------- motion blur

#[inline]
pub fn sample_bilinear(raw: &[u8], w: usize, h: usize, x: f32, y: f32) -> [f32; 4] {
    sample_bilinear_px(raw, w, h, x, y)
}

#[inline]
fn sample_bilinear_px<P: Px>(raw: &[P], w: usize, h: usize, x: f32, y: f32) -> [f32; 4] {
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    // Both are >= 0 here, so truncation is floor (and NaN still maps to 0)
    // without a libm call per sample.
    let x0 = x as usize;
    let y0 = y as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let px = |xx: usize, yy: usize| {
        let i = (yy * w + xx) * 4;
        [raw[i].get() as f32, raw[i + 1].get() as f32, raw[i + 2].get() as f32, raw[i + 3].get() as f32]
    };
    let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
    let mut out = [0.0; 4];
    for i in 0..4 {
        let top = a[i] + (b[i] - a[i]) * fx;
        let bot = c[i] + (d[i] - c[i]) * fx;
        out[i] = top + (bot - top) * fy;
    }
    out
}

/// The two source rows and vertical weight `sample_bilinear_px` uses for `y`.
#[inline]
fn source_rows<P: Px>(cur: &[P], w: usize, h: usize, y: f32) -> (&[P], &[P], f32) {
    let y = y.clamp(0.0, (h - 1) as f32);
    let y0 = y as usize;
    let y1 = (y0 + 1).min(h - 1);
    (&cur[y0 * w * 4..(y0 + 1) * w * 4], &cur[y1 * w * 4..(y1 + 1) * w * 4], y - y0 as f32)
}

/// Linear motion blur along `angle_deg` spread over `distance` pixels.
/// Built from log2(distance) shift-and-average passes, so cost is independent
/// of the distance (beyond the pass count).
pub fn motion_blur(img: &RgbaImage, angle_deg: f32, distance: f32) -> RgbaImage {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if !(distance >= 1.0) || w == 0 || h == 0 {
        return img.clone();
    }
    let distance = distance.min(4.0 * w.max(h) as f32);
    let (s, c) = angle_deg.to_radians().sin_cos();
    fn run<P: Px>(src: &[P], w: usize, h: usize, distance: f32, s: f32, c: f32) -> Vec<P> {
        let mut cur = src.to_vec();
        let mut shift = distance / 2.0;
        while shift >= 0.5 {
            let (dx, dy) = (c * shift / 2.0, -s * shift / 2.0);
            let mut next = vec![P::default(); cur.len()];
            next.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
                // The same arithmetic as two `sample_bilinear_px` calls per
                // pixel, with the row-only part (source rows and vertical
                // weight) done once per row.
                let (a0, a1, afy) = source_rows(&cur, w, h, y as f32 - dy);
                let (b0, b1, bfy) = source_rows(&cur, w, h, y as f32 + dy);
                let sample = |r0: &[P], r1: &[P], fy: f32, sx: f32| {
                    let sx = sx.clamp(0.0, (w - 1) as f32);
                    let x0 = sx as usize;
                    let x1 = (x0 + 1).min(w - 1);
                    let fx = sx - x0 as f32;
                    let mut out = [0.0f32; 4];
                    for i in 0..4 {
                        let (a, b) = (r0[x0 * 4 + i].get() as f32, r0[x1 * 4 + i].get() as f32);
                        let (c, d) = (r1[x0 * 4 + i].get() as f32, r1[x1 * 4 + i].get() as f32);
                        let top = a + (b - a) * fx;
                        let bot = c + (d - c) * fx;
                        out[i] = top + (bot - top) * fy;
                    }
                    out
                };
                for (x, o) in row.chunks_exact_mut(4).enumerate() {
                    let a = sample(a0, a1, afy, x as f32 - dx);
                    let b = sample(b0, b1, bfy, x as f32 + dx);
                    for i in 0..4 {
                        o[i] = P::put(((a[i] + b[i]) * 0.5 + 0.5) as u32);
                    }
                }
            });
            cur = next;
            shift /= 2.0;
        }
        cur
    }
    with_premul(img, |src, w, h| run(src, w, h, distance, s, c), |src, w, h| run(src, w, h, distance, s, c))
}

/// Edge-preserving "surface" blur: a Gaussian blur that is faded out where it
/// would change a pixel by more than `threshold` (0..255).
pub fn surface_blur(img: &RgbaImage, sigma: f32, threshold: f32) -> RgbaImage {
    if sigma < 0.2 {
        return img.clone();
    }
    let blur = gaussian_blur(img, sigma);
    let t = threshold.max(1.0);
    let mut out = img.clone();
    out.as_mut().par_chunks_mut(4 * 4096).zip(blur.as_raw().par_chunks(4 * 4096)).for_each(|(o, b)| {
        for (p, q) in o.chunks_exact_mut(4).zip(b.chunks_exact(4)) {
            let d = (0..3).map(|c| (p[c] as f32 - q[c] as f32).abs()).fold(0.0f32, f32::max);
            let wgt = (-(d / t) * (d / t)).exp();
            for c in 0..4 {
                p[c] = (p[c] as f32 + (q[c] as f32 - p[c] as f32) * wgt).round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    out
}

// ---------------------------------------------------------------- tiled processing

/// Process `img` in independent 256 px tiles, each padded by `margin` pixels
/// (edge-clamped). `f` gets the padded tile and must return an image of the same
/// size; only its inner part is kept. Bounds memory for per-tile buffers.
pub fn tiled(img: &RgbaImage, margin: u32, f: impl Fn(&RgbaImage) -> RgbaImage + Sync) -> RgbaImage {
    const T: u32 = 256;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let m = margin.min(4096);
    let tiles: Vec<(u32, u32)> = (0..h.div_ceil(T)).flat_map(|ty| (0..w.div_ceil(T)).map(move |tx| (tx * T, ty * T))).collect();
    let results: Vec<(u32, u32, RgbaImage)> = tiles
        .par_iter()
        .map(|&(x0, y0)| {
            let tw = T.min(w - x0);
            let th = T.min(h - y0);
            let pw = tw + 2 * m;
            let ph = th + 2 * m;
            let mut pad = RgbaImage::new(pw, ph);
            let raw = img.as_raw();
            for py in 0..ph {
                let sy = (y0 as i64 + py as i64 - m as i64).clamp(0, h as i64 - 1) as usize;
                let drow = &mut pad.as_mut()[(py * pw * 4) as usize..((py + 1) * pw * 4) as usize];
                for px in 0..pw {
                    let sx = (x0 as i64 + px as i64 - m as i64).clamp(0, w as i64 - 1) as usize;
                    let si = (sy * w as usize + sx) * 4;
                    drow[px as usize * 4..px as usize * 4 + 4].copy_from_slice(&raw[si..si + 4]);
                }
            }
            let res = f(&pad);
            let mut inner = RgbaImage::new(tw, th);
            if res.dimensions() == (pw, ph) {
                for y in 0..th {
                    let s = (((y + m) * pw + m) * 4) as usize;
                    let d = (y * tw * 4) as usize;
                    inner.as_mut()[d..d + tw as usize * 4].copy_from_slice(&res.as_raw()[s..s + tw as usize * 4]);
                }
            }
            (x0, y0, inner)
        })
        .collect();
    let mut out = RgbaImage::new(w, h);
    let stride = w as usize * 4;
    for (x0, y0, t) in results {
        let tw = t.width() as usize * 4;
        for y in 0..t.height() as usize {
            let d = (y0 as usize + y) * stride + x0 as usize * 4;
            out.as_mut()[d..d + tw].copy_from_slice(&t.as_raw()[y * tw..(y + 1) * tw]);
        }
    }
    out
}

/// Median filter with a (2r+1)² window, per channel (alpha untouched).
pub fn median(img: &RgbaImage, r: u32) -> RgbaImage {
    let r = r.clamp(0, 4);
    if r == 0 {
        return img.clone();
    }
    let n = ((2 * r + 1) * (2 * r + 1)) as usize;
    let net = median_network(n);
    tiled(img, r, |t| median_tile(t, r as usize, &net))
}

/// Pixels per block of [`median_tile`] (64 bytes of RGBA).
const MEDIAN_BLOCK: usize = 16;

/// Median of every interior pixel of a padded tile (the outer `r` pixels
/// keep their values). Works on blocks of 16 pixels at once: each window
/// position is one 64-byte vector and a compare-exchange network (byte-wise
/// min / max, which vectorize) sorts all 64 channel values together, so the
/// result is exactly the per-channel median of the scalar version.
fn median_tile(t: &RgbaImage, r: usize, net: &(usize, Vec<(usize, usize)>)) -> RgbaImage {
    let (w, h) = (t.width() as usize, t.height() as usize);
    if h <= 2 * r || w < 2 * r + MEDIAN_BLOCK {
        return median_tile_scalar(t, r);
    }
    const L: usize = MEDIAN_BLOCK * 4;
    let k = 2 * r + 1;
    let n = k * k;
    let (p, ces) = net;
    let raw = t.as_raw();
    let mut out = t.clone();
    let stride = w * 4;
    let x_end = w - r;
    let mut v = vec![[255u8; L]; *p];
    for y in r..h - r {
        let mut x0 = r;
        loop {
            // The last block is moved back to end at the last pixel (it
            // recomputes a few pixels, with the same result).
            let xb = x0.min(x_end - MEDIAN_BLOCK);
            for (i, vi) in v[..n].iter_mut().enumerate() {
                let (dy, dx) = (i / k, i % k);
                let s = (y + dy - r) * stride + (xb + dx - r) * 4;
                vi.copy_from_slice(&raw[s..s + L]);
            }
            for vi in &mut v[n..] {
                *vi = [255; L];
            }
            for &(a, b) in ces {
                let (lo, hi) = v.split_at_mut(b);
                let (x, z) = (&mut lo[a], &mut hi[0]);
                for j in 0..L {
                    let (q, u) = (x[j], z[j]);
                    x[j] = q.min(u);
                    z[j] = q.max(u);
                }
            }
            let m = &v[n / 2];
            let o = &mut out.as_mut()[y * stride + xb * 4..y * stride + xb * 4 + L];
            for (oc, mc) in o.chunks_exact_mut(4).zip(m.chunks_exact(4)) {
                oc[..3].copy_from_slice(&mc[..3]);
            }
            if xb + MEDIAN_BLOCK >= x_end {
                break;
            }
            x0 = xb + MEDIAN_BLOCK;
        }
    }
    out
}

/// Compare-exchange network (pairs `(a, b)`, `a < b`, min goes to `a`) that
/// leaves the median of `n` values at index `n / 2`: Batcher's odd-even merge
/// sort on the next power of two, keeping only the comparators that can
/// affect that index. The inputs from `n` up to the returned size must be 255.
fn median_network(n: usize) -> (usize, Vec<(usize, usize)>) {
    let p = n.next_power_of_two();
    let mut ces = Vec::new();
    let mut q = 1;
    while q < p {
        let mut k = q;
        while k > 0 {
            let mut j = k % q;
            while j + k < p {
                for i in 0..k.min(p - j - k) {
                    if (i + j) / (2 * q) == (i + j + k) / (2 * q) {
                        ces.push((i + j, i + j + k));
                    }
                }
                j += 2 * k;
            }
            k /= 2;
        }
        q *= 2;
    }
    let mut need = vec![false; p];
    need[n / 2] = true;
    let mut kept: Vec<(usize, usize)> = ces
        .into_iter()
        .rev()
        .filter(|&(a, b)| {
            let keep = need[a] || need[b];
            if keep {
                need[a] = true;
                need[b] = true;
            }
            keep
        })
        .collect();
    kept.reverse();
    (p, kept)
}

/// Per-pixel median of a padded tile (tiles too narrow for whole blocks).
fn median_tile_scalar(t: &RgbaImage, r: usize) -> RgbaImage {
    let (w, h) = (t.width() as i64, t.height() as i64);
    let raw = t.as_raw();
    let mut out = t.clone();
    let ri = r as i64;
    let mut buf: Vec<u8> = Vec::with_capacity((2 * r + 1) * (2 * r + 1));
    for y in ri..h - ri {
        for x in ri..w - ri {
            for c in 0..3 {
                buf.clear();
                for dy in -ri..=ri {
                    for dx in -ri..=ri {
                        buf.push(raw[(((y + dy) * w + x + dx) * 4 + c as i64) as usize]);
                    }
                }
                let mid = buf.len() / 2;
                let (_, m, _) = buf.select_nth_unstable(mid);
                out.as_mut()[((y * w + x) * 4 + c as i64) as usize] = *m;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noisy(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let v = ((x * 7919 + y * 104729) % 251) as u8;
            image::Rgba([v, v.wrapping_mul(3), 255 - v, 255])
        })
    }

    /// `motion_blur` before the per-row hoisting (the reference).
    fn old_motion_blur(img: &RgbaImage, angle_deg: f32, distance: f32) -> RgbaImage {
        let (w, h) = (img.width() as usize, img.height() as usize);
        if !(distance >= 1.0) || w == 0 || h == 0 {
            return img.clone();
        }
        let distance = distance.min(4.0 * w.max(h) as f32);
        let (s, c) = angle_deg.to_radians().sin_cos();
        fn run<P: Px>(src: &[P], w: usize, h: usize, distance: f32, s: f32, c: f32) -> Vec<P> {
            let mut cur = src.to_vec();
            let mut shift = distance / 2.0;
            while shift >= 0.5 {
                let (dx, dy) = (c * shift / 2.0, -s * shift / 2.0);
                let mut next = vec![P::default(); cur.len()];
                for (y, row) in next.chunks_mut(w * 4).enumerate() {
                    for x in 0..w {
                        let a = sample_bilinear_px(&cur, w, h, x as f32 - dx, y as f32 - dy);
                        let b = sample_bilinear_px(&cur, w, h, x as f32 + dx, y as f32 + dy);
                        for i in 0..4 {
                            row[x * 4 + i] = P::put(((a[i] + b[i]) * 0.5 + 0.5) as u32);
                        }
                    }
                }
                cur = next;
                shift /= 2.0;
            }
            cur
        }
        with_premul(img, |src, w, h| run(src, w, h, distance, s, c), |src, w, h| run(src, w, h, distance, s, c))
    }

    /// The separable Gaussian passes before the interior fast path (the reference).
    fn old_gauss_small<P: Px>(src: &[P], w: usize, h: usize, sigma: f32) -> Vec<P> {
        let k = gauss_kernel(sigma);
        let r = (k.len() / 2) as i64;
        let mut tmp = vec![P::default(); w * h * 4];
        for (y, out) in tmp.chunks_mut(w * 4).enumerate() {
            let row = &src[y * w * 4..(y + 1) * w * 4];
            for x in 0..w {
                let mut acc = [0u64; 4];
                for (j, &kw) in k.iter().enumerate() {
                    let sx = clampi(x as i64 + j as i64 - r, w) * 4;
                    for c in 0..4 {
                        acc[c] += kw as u64 * row[sx + c].get() as u64;
                    }
                }
                for c in 0..4 {
                    out[x * 4 + c] = P::put(((acc[c] + 32768) >> 16) as u32);
                }
            }
        }
        let mut out = vec![P::default(); w * h * 4];
        for (y, o) in out.chunks_mut(w * 4).enumerate() {
            let mut acc = vec![0u64; w * 4];
            for (j, &kw) in k.iter().enumerate() {
                let sy = clampi(y as i64 + j as i64 - r, h);
                for (a, &v) in acc.iter_mut().zip(&tmp[sy * w * 4..(sy + 1) * w * 4]) {
                    *a += kw as u64 * v.get() as u64;
                }
            }
            for (p, a) in o.iter_mut().zip(acc) {
                *p = P::put(((a + 32768) >> 16) as u32);
            }
        }
        out
    }

    /// Old against new kernels on one thread, in the same process (less
    /// sensitive to the machine than the parallel timings):
    /// `cargo test --release -p lumen-core -- --ignored --nocapture bench_perf_`
    #[test]
    #[ignore]
    fn bench_perf_filters_old_vs_new() {
        let img = RgbaImage::from_fn(4000, 3000, |x, y| image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 256) as u8, 255]));
        let (w, h) = (4000usize, 3000usize);
        let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let time = |f: &dyn Fn()| {
            (0..3)
                .map(|_| {
                    let s = std::time::Instant::now();
                    f();
                    s.elapsed().as_secs_f64() * 1000.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        pool.install(|| {
            let pair = |name: &str, old: &dyn Fn(), new: &dyn Fn()| eprintln!("1 thread 12 MP {name:>10}: old {:8.1} ms, new {:8.1} ms", time(old), time(new));
            pair("motion 40", &|| drop(old_motion_blur(&img, 30.0, 40.0)), &|| drop(motion_blur(&img, 30.0, 40.0)));
            pair("gauss 2", &|| drop(old_gauss_small(img.as_raw(), w, h, 2.0)), &|| drop(gaussian_raw(img.as_raw(), w, h, 2.0)));
            pair("median 3", &|| drop(tiled(&img, 1, |t| median_tile_scalar(t, 1))), &|| drop(median(&img, 1)));
            pair("median 5", &|| drop(tiled(&img, 2, |t| median_tile_scalar(t, 2))), &|| drop(median(&img, 2)));
        });
    }

    #[test]
    fn gaussian_matches_reference() {
        for (w, h) in [(1usize, 1usize), (3, 40), (13, 9), (64, 33), (130, 70)] {
            let img = noisy(w as u32, h as u32);
            let pre = premultiply16(&img);
            for sigma in [0.2f32, 0.7, 1.5, 3.0] {
                assert!(gaussian_raw(img.as_raw(), w, h, sigma) == old_gauss_small(img.as_raw(), w, h, sigma), "u8 {w}x{h} {sigma}");
                assert!(gaussian_raw(&pre, w, h, sigma) == old_gauss_small(&pre, w, h, sigma), "u16 {w}x{h} {sigma}");
            }
        }
    }

    #[test]
    fn motion_blur_matches_reference() {
        let mut alpha = noisy(61, 37);
        for (i, p) in alpha.pixels_mut().enumerate() {
            p.0[3] = (i * 37 % 256) as u8;
        }
        for img in [noisy(61, 37), noisy(5, 90), alpha] {
            for (angle, dist) in [(0.0, 1.0), (30.0, 12.0), (-75.0, 7.5), (90.0, 40.0), (200.0, 300.0)] {
                assert!(motion_blur(&img, angle, dist) == old_motion_blur(&img, angle, dist), "{angle} {dist}");
            }
        }
    }

    #[test]
    fn median_matches_per_pixel_select() {
        let mut img = noisy(300, 270);
        for (i, p) in img.pixels_mut().enumerate() {
            p.0[3] = (i % 256) as u8;
            p.0[1] = (i.wrapping_mul(2_654_435_761) >> 13) as u8;
        }
        for r in 1..=4u32 {
            let fast = median(&img, r);
            let slow = tiled(&img, r, |t| median_tile_scalar(t, r as usize));
            assert!(fast == slow, "r {r}");
            let small = noisy(19, 7);
            assert!(median(&small, r) == tiled(&small, r, |t| median_tile_scalar(t, r as usize)), "small r {r}");
        }
    }

    #[test]
    fn median_network_sorts_the_middle() {
        for n in [9usize, 25, 49, 81] {
            let (p, ces) = median_network(n);
            let mut seed = 12345u32;
            for _ in 0..200 {
                let mut v = vec![255u8; p];
                for x in &mut v[..n] {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    *x = (seed >> 16) as u8 % 7 * 40;
                }
                let mut sorted = v[..n].to_vec();
                sorted.sort_unstable();
                for &(a, b) in &ces {
                    if v[a] > v[b] {
                        v.swap(a, b);
                    }
                }
                assert_eq!(v[n / 2], sorted[n / 2], "n {n}");
            }
        }
    }

    #[test]
    fn gaussian_flat_unchanged() {
        let img = RgbaImage::from_pixel(40, 30, image::Rgba([90, 140, 200, 255]));
        for s in [0.5, 2.0, 8.0, 500.0] {
            assert_eq!(gaussian_blur(&img, s), img, "sigma {s}");
        }
        assert_eq!(box_blur(&img, 7), img);
        assert_eq!(motion_blur(&img, 30.0, 25.0), img);
    }

    #[test]
    fn gaussian_zero_identity_and_smooths() {
        let img = noisy(33, 21);
        assert_eq!(gaussian_blur(&img, 0.0), img);
        let b = gaussian_blur(&img, 4.0);
        assert_eq!(b.dimensions(), img.dimensions());
        let var = |i: &RgbaImage| {
            let m = i.pixels().map(|p| p[0] as f64).sum::<f64>() / (i.len() as f64 / 4.0);
            i.pixels().map(|p| (p[0] as f64 - m).powi(2)).sum::<f64>()
        };
        assert!(var(&b) < var(&img) * 0.5);
    }

    #[test]
    fn tiny_and_transparent() {
        let one = RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 0]));
        assert_eq!(gaussian_blur(&one, 50.0).dimensions(), (1, 1));
        assert_eq!(motion_blur(&one, 10.0, 100.0).dimensions(), (1, 1));
        assert_eq!(median(&one, 2).dimensions(), (1, 1));
        assert_eq!(unsharp_mask(&one, 2.0, 3.0, 0).dimensions(), (1, 1));
        // Transparent pixels must not bleed colour into opaque ones.
        let mut img = RgbaImage::from_pixel(20, 1, image::Rgba([255, 0, 0, 0]));
        for x in 10..20 {
            img.put_pixel(x, 0, image::Rgba([0, 0, 255, 255]));
        }
        let b = gaussian_blur(&img, 3.0);
        let p = b.get_pixel(9, 0);
        assert!(p[0] < 10 && p[2] > 240, "{p:?}");
    }

    #[test]
    fn unsharp_identity_and_effect() {
        let img = noisy(16, 16);
        assert_eq!(unsharp_mask(&img, 0.0, 2.0, 0), img);
        let flat = RgbaImage::from_pixel(8, 8, image::Rgba([100, 100, 100, 255]));
        assert_eq!(unsharp_mask(&flat, 3.0, 2.0, 0), flat);
        assert_ne!(unsharp_mask(&img, 1.5, 1.0, 0), img);
    }

    #[test]
    fn median_removes_speck() {
        let mut img = RgbaImage::from_pixel(300, 270, image::Rgba([50, 50, 50, 255]));
        img.put_pixel(280, 260, image::Rgba([255, 255, 255, 255]));
        let m = median(&img, 1);
        assert_eq!(m, RgbaImage::from_pixel(300, 270, image::Rgba([50, 50, 50, 255])));
    }

    /// Timing check on a 50 MP image: `cargo test bench_50mp -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_50mp() {
        use crate::edit::{effects, geom, tone};
        let img = RgbaImage::from_fn(8660, 5774, |x, y| image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 256) as u8, 255]));
        let t = |name: &str, f: &dyn Fn() -> RgbaImage| {
            let s = std::time::Instant::now();
            let o = f();
            eprintln!("{name:>14}: {:>6} ms  ({}x{})", s.elapsed().as_millis(), o.width(), o.height());
        };
        t("gauss 2", &|| gaussian_blur(&img, 2.0));
        t("gauss 50", &|| gaussian_blur(&img, 50.0));
        t("box 20", &|| box_blur(&img, 20));
        t("motion 80", &|| motion_blur(&img, 30.0, 80.0));
        t("unsharp", &|| unsharp_mask(&img, 1.0, 1.0, 2));
        t("surface", &|| surface_blur(&img, 6.0, 20.0));
        t("median 3", &|| median(&img, 1));
        t("median 5", &|| median(&img, 2));
        t("median 9", &|| median(&img, 4));
        t("shadows/hi", &|| tone::shadows_highlights(&img, 0.5, 0.3));
        t("oil 4", &|| effects::oil_paint(&img, 4));
        t("edges", &|| effects::edges(&img, 1.0));
        t("emboss", &|| effects::emboss(&img, 2.0, 1.0));
        t("pixelate", &|| effects::pixelate(&img, 24.0));
        t("grain", &|| effects::film_grain(&img, 0.4, 1.5, 1));
        t("vignette", &|| effects::vignette(&img, 0.5, 0.3, 0.6));
        t("rotate 7", &|| geom::rotate(&img, 7.0, true, [0; 4]));
        t("levels", &|| tone::apply_levels(&img, &[tone::LevelsParams { gamma: 1.3, ..Default::default() }; 4]));
        t("auto enh", &|| crate::edit::ops::auto_levels(&img));
    }

    #[test]
    fn semi_transparent_colour_survives_blur() {
        // Flat colour at low alpha: an 8-bit premultiplied round trip shifts it.
        let img = RgbaImage::from_pixel(24, 24, image::Rgba([201, 117, 33, 9]));
        for out in [gaussian_blur(&img, 2.0), gaussian_blur(&img, 9.0), box_blur(&img, 3), motion_blur(&img, 20.0, 12.0)] {
            assert_eq!(out, img);
        }
        let mut grad = RgbaImage::from_pixel(32, 4, image::Rgba([0, 0, 0, 0]));
        for x in 0..32 {
            for y in 0..4 {
                grad.put_pixel(x, y, image::Rgba([250, 140, 20, (x * 8) as u8]));
            }
        }
        let b = gaussian_blur(&grad, 1.5);
        for x in 2..30 {
            let p = b.get_pixel(x, 1);
            assert!((p[0] as i32 - 250).abs() <= 1 && (p[1] as i32 - 140).abs() <= 1 && (p[2] as i32 - 20).abs() <= 1, "{x}: {p:?}");
        }
    }

    #[test]
    fn surface_blur_flat() {
        let img = RgbaImage::from_pixel(10, 10, image::Rgba([1, 2, 3, 255]));
        assert_eq!(surface_blur(&img, 5.0, 20.0), img);
    }
}
