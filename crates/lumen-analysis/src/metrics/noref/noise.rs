//! Noise: Immerkaer's fast noise variance estimate (CVIU 1996) per plane,
//! sigma = sqrt(pi / 2) / 6 * mean |I * N| with N = [1 -2 1; -2 4 -2; 1 -2 1].
//! Edges are masked (Sobel magnitude above the 90th percentile is skipped)
//! so texture does not read as noise. Reported in 8-bit levels.

use crate::frame::{FrameCtx, FrameView, InputKind, RowConverter};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use crate::stats::Accum;
use rayon::prelude::*;

const BAND: usize = 16;
const HIST_BINS: usize = 16384;

/// Sobel gradient magnitudes of the interior of row `mid` into `out[..w - 2]`.
#[inline(always)]
fn grad_row_impl(up: &[f32], mid: &[f32], dn: &[f32], out: &mut [f32]) {
    let n = out.len().min(mid.len().saturating_sub(2));
    let (u0, u1, u2) = (&up[..n], &up[1..n + 1], &up[2..n + 2]);
    let (m0, m2) = (&mid[..n], &mid[2..n + 2]);
    let (d0, d1, d2) = (&dn[..n], &dn[1..n + 1], &dn[2..n + 2]);
    for i in 0..n {
        let gx = (u2[i] + 2.0 * m2[i] + d2[i]) - (u0[i] + 2.0 * m0[i] + d0[i]);
        let gy = (d0[i] + 2.0 * d1[i] + d2[i]) - (u0[i] + 2.0 * u1[i] + u2[i]);
        out[i] = (gx * gx + gy * gy).sqrt();
    }
}

crate::simd::avx2_dispatch!(fn grad_row(up: &[f32], mid: &[f32], dn: &[f32], out: &mut [f32]) => grad_row_impl);

/// Sum of |I * N| and count over the interior of row `mid`, skipping pixels
/// whose squared Sobel magnitude exceeds `thr2` (branch-free).
#[inline(always)]
fn immerkaer_row_impl(up: &[f32], mid: &[f32], dn: &[f32], thr2: f32) -> (f64, u64) {
    let n = mid.len().saturating_sub(2);
    let (u0, u1, u2) = (&up[..n], &up[1..n + 1], &up[2..n + 2]);
    let (m0, m1, m2) = (&mid[..n], &mid[1..n + 1], &mid[2..n + 2]);
    let (d0, d1, d2) = (&dn[..n], &dn[1..n + 1], &dn[2..n + 2]);
    // Eight independent f32 lanes so the loop vectorizes (an in-order float
    // sum would not); per-chunk subslices let the bounds checks fold away.
    // One row is short enough for f32 lane sums.
    const L: usize = 8;
    #[inline(always)]
    fn px(u: [f32; 3], m: [f32; 3], d: [f32; 3], thr2: f32) -> (f32, u32) {
        let v = (u[0] + u[2] + d[0] + d[2]) - 2.0 * (u[1] + d[1] + m[0] + m[2]) + 4.0 * m[1];
        let gx = (u[2] + 2.0 * m[2] + d[2]) - (u[0] + 2.0 * m[0] + d[0]);
        let gy = (d[0] + 2.0 * d[1] + d[2]) - (u[0] + 2.0 * u[1] + u[2]);
        let keep = gx * gx + gy * gy <= thr2;
        (if keep { v.abs() } else { 0.0 }, keep as u32)
    }
    let mut ls = [0f32; L];
    let mut lc = [0u32; L];
    let mut k = 0;
    while k + L <= n {
        let (a0, a1, a2) = (&u0[k..k + L], &u1[k..k + L], &u2[k..k + L]);
        let (b0, b1, b2) = (&m0[k..k + L], &m1[k..k + L], &m2[k..k + L]);
        let (c0, c1, c2) = (&d0[k..k + L], &d1[k..k + L], &d2[k..k + L]);
        for j in 0..L {
            let (v, c) = px([a0[j], a1[j], a2[j]], [b0[j], b1[j], b2[j]], [c0[j], c1[j], c2[j]], thr2);
            ls[j] += v;
            lc[j] += c;
        }
        k += L;
    }
    let (mut s, mut c) = (ls.iter().map(|v| *v as f64).sum::<f64>(), lc.iter().map(|v| *v as u64).sum::<u64>());
    for i in k..n {
        let (v, kk) = px([u0[i], u1[i], u2[i]], [m0[i], m1[i], m2[i]], [d0[i], d1[i], d2[i]], thr2);
        s += v as f64;
        c += kk as u64;
    }
    (s, c)
}

crate::simd::avx2_dispatch!(fn immerkaer_row(up: &[f32], mid: &[f32], dn: &[f32], thr2: f32) -> (f64, u64) => immerkaer_row_impl);

/// Immerkaer sigma (in the plane's own units) of a packed `f32` plane. The
/// edge threshold (90th percentile of the Sobel magnitude) is estimated on
/// every 4th row.
pub fn immerkaer(d: &[f32], w: usize, h: usize, mask_edges: bool, cancel: &CancelToken) -> Result<f64, MetricError> {
    if w < 3 || h < 3 {
        return Ok(f64::NAN);
    }
    let rows = |y: usize| (&d[(y - 1) * w..y * w], &d[y * w..(y + 1) * w], &d[(y + 1) * w..(y + 2) * w]);
    let thr2 = if mask_edges {
        // Planes span at most 1.0 (normalized), so |grad| <= 4 sqrt(2).
        let max_mag = 4.0 * std::f32::consts::SQRT_2;
        let sampled: Vec<usize> = (1..h - 1).step_by(4).collect();
        let acc = sampled
            .par_chunks(16)
            .map(|ys| {
                let mut acc = Accum::new(0.0, max_mag, HIST_BINS);
                let mut buf = vec![0f32; w - 2];
                for &y in ys {
                    let (up, mid, dn) = rows(y);
                    grad_row(up, mid, dn, &mut buf);
                    acc.push(&buf);
                }
                acc
            })
            .reduce(|| Accum::new(0.0, max_mag, HIST_BINS), Accum::merge);
        let t = acc.finish().percentile(90.0) as f32;
        t * t
    } else {
        f32::INFINITY
    };
    cancel.check()?;
    let (sum, n) = (1..h - 1)
        .into_par_iter()
        .with_min_len(BAND)
        .map(|y| {
            let (up, mid, dn) = rows(y);
            immerkaer_row(up, mid, dn, thr2)
        })
        .reduce(|| (0.0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    cancel.check()?;
    if n == 0 {
        return Ok(0.0);
    }
    Ok((std::f64::consts::PI / 2.0).sqrt() / 6.0 * sum / n as f64)
}

/// `noise`: Immerkaer sigma for Y, Cb and Cr.
pub struct Noise;

static NOISE: MetricInfo = MetricInfo {
    id: "noise",
    name: "Noise (Immerkaer)",
    kind: MetricKind::NoReference,
    version: 1,
    inputs: &[InputKind::Luma],
    min_size: (3, 3),
    cost_ms_4k: 15.0,
    heavy: false,
    has_map: false,
    direction: Direction::LowerIsBetter,
    description: "Immerkaer noise sigma per plane (Y, Cb, Cr) in 8-bit levels, edges masked",
};

impl Metric for Noise {
    fn info(&self) -> &'static MetricInfo {
        &NOISE
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&NOISE, a)?;
        let l = a.luma();
        let mut r = MetricResult::new(&NOISE);
        let d = Direction::LowerIsBetter;
        r.push("y", 255.0 * immerkaer(&l.data, l.width, l.height, o.noise_mask_edges, cancel)?, Unit::Levels8, d);
        let chroma: Option<(Vec<f32>, Vec<f32>, usize, usize)> = match a.view() {
            FrameView::Yuv(v) => v.uv.map(|(u, q)| {
                let s = 1.0 / v.max_value() as f32;
                (u.to_f32(s), q.to_f32(s), u.width(), u.height())
            }),
            FrameView::Rgb(_) => {
                // BT.709 Cb / Cr from the encoded RGB.
                let view = *a.view();
                let (w, h) = (l.width, l.height);
                let mut cb = vec![0f32; w * h];
                let mut cr = vec![0f32; w * h];
                cb.par_chunks_mut(w).zip(cr.par_chunks_mut(w)).enumerate().for_each_init(
                    || RowConverter::new(w),
                    |conv, (y, (ob, or))| {
                        conv.encoded(&view, y);
                        let ly = l.row(y);
                        for x in 0..w {
                            ob[x] = (conv.b[x] - ly[x]) / 1.8556;
                            or[x] = (conv.r[x] - ly[x]) / 1.5748;
                        }
                    },
                );
                Some((cb, cr, w, h))
            }
        };
        if let Some((cb, cr, w, h)) = chroma {
            r.push("cb", 255.0 * immerkaer(&cb, w, h, o.noise_mask_edges, cancel)?, Unit::Levels8, d);
            r.push("cr", 255.0 * immerkaer(&cr, w, h, o.noise_mask_edges, cancel)?, Unit::Levels8, d);
        }
        Ok(r)
    }
}
