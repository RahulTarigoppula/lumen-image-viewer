//! Simple banding detector (v1; CAMBI comes with the VMAF feature).
//!
//! Luma is quantized to its native code levels. A pixel is *smooth* when its
//! 5x5 neighbourhood spans at most `SMOOTH_RANGE` levels, and a *band
//! contour* when it is smooth and the next pixel (right, or below) is exactly
//! one level away with flat runs of at least `RUN` pixels on both sides: wide
//! plateaus separated by single-level steps, the signature of posterized
//! gradients. Noise and dither break smoothness and runs; texture breaks
//! smoothness. The score is the share of smooth pixels that are contours.
//!
//! The map is a grid of [`MAP_TILE`] px tiles holding each tile's own score,
//! left clear where it is under [`MAP_MIN`] percent (no visible banding), so
//! "show where" colors only the banded parts of the gradients.

use crate::frame::{FrameCtx, InputKind};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use rayon::prelude::*;
use std::ops::Add;

/// Flat run length required on each side of a step.
const RUN: usize = 3;
/// Half size of the smoothness window (5x5).
const R: usize = 2;
const SMOOTH_RANGE: i32 = 2;
const BAND: usize = 16;
/// Map tile size in pixels (the no-reference tile maps' size: wide enough
/// to hold a step of a slow gradient, so banded areas read as one patch).
pub const MAP_TILE: usize = 64;
/// Tiles scoring less than this (percent) are left out of the map.
pub const MAP_MIN: f32 = 1.0;

/// `banding`
pub struct Banding;

static BANDING: MetricInfo = MetricInfo {
    id: "banding",
    name: "Banding",
    kind: MetricKind::NoReference,
    version: 2,
    inputs: &[InputKind::Luma],
    min_size: (8, 8),
    cost_ms_4k: 15.0,
    heavy: false,
    has_map: true,
    direction: Direction::LowerIsBetter,
    headline: &["score"],
    description: "Share of smooth area made of one-level step contours between flat runs, with a map of the banded tiles",
};

/// 5-wide horizontal min and max of `q` at x in R..w-R.
#[inline(always)]
fn minmax_row_impl(q: &[i32], lo: &mut [i32], hi: &mut [i32]) {
    let n = q.len().saturating_sub(2 * R);
    let (q0, q1, q2, q3, q4) = (&q[..n], &q[1..n + 1], &q[2..n + 2], &q[3..n + 3], &q[4..n + 4]);
    let (lo, hi) = (&mut lo[R..R + n], &mut hi[R..R + n]);
    for i in 0..n {
        lo[i] = q0[i].min(q1[i]).min(q2[i]).min(q3[i]).min(q4[i]);
        hi[i] = q0[i].max(q1[i]).max(q2[i]).max(q3[i]).max(q4[i]);
    }
}

crate::simd::avx2_dispatch!(fn minmax_row(q: &[i32], lo: &mut [i32], hi: &mut [i32]) => minmax_row_impl);

/// One output row: `rows` are q rows y-2..=y+3, `lo`/`hi` the horizontal
/// min/max rows y-2..=y+2. Counts smooth pixels and contours in x0..x1 and
/// marks them in `mask` (if any: 1 smooth, 2 contour). Branch-free per pixel.
#[inline(always)]
fn band_row_impl(rows: &[&[i32]; 6], lo: &[&[i32]; 5], hi: &[&[i32]; 5], x0: usize, x1: usize, mask: Option<&mut [u8]>) -> (u64, u64) {
    let n = x1 - x0;
    let c = &rows[2][x0..x1];
    let (hl2, hl1, hn, hn2, hn3) = (&rows[2][x0 - 2..x1 - 2], &rows[2][x0 - 1..x1 - 1], &rows[2][x0 + 1..x1 + 1], &rows[2][x0 + 2..x1 + 2], &rows[2][x0 + 3..x1 + 3]);
    let (vu2, vu1, vn, vn2, vn3) = (&rows[0][x0..x1], &rows[1][x0..x1], &rows[3][x0..x1], &rows[4][x0..x1], &rows[5][x0..x1]);
    let l: [&[i32]; 5] = std::array::from_fn(|k| &lo[k][x0..x1]);
    let h: [&[i32]; 5] = std::array::from_fn(|k| &hi[k][x0..x1]);
    let (mut smooth_n, mut contour_n) = (0u32, 0u32);
    let mut mask = mask.map(|m| &mut m[x0..x1]);
    for i in 0..n {
        let mn = l[0][i].min(l[1][i]).min(l[2][i]).min(l[3][i]).min(l[4][i]);
        let mx = h[0][i].max(h[1][i]).max(h[2][i]).max(h[3][i]).max(h[4][i]);
        let smooth = mx - mn <= SMOOTH_RANGE;
        let cv = c[i];
        // Step to the right / below of exactly one level between flat runs of RUN.
        let hc = (hn[i] - cv).abs() == 1 && hl1[i] == cv && hl2[i] == cv && hn2[i] == hn[i] && hn3[i] == hn[i];
        let vc = (vn[i] - cv).abs() == 1 && vu1[i] == cv && vu2[i] == cv && vn2[i] == vn[i] && vn3[i] == vn[i];
        let contour = smooth & (hc | vc);
        smooth_n += smooth as u32;
        contour_n += contour as u32;
        if let Some(m) = mask.as_deref_mut() {
            m[i] = smooth as u8 + contour as u8;
        }
    }
    (contour_n as u64, smooth_n as u64)
}

crate::simd::avx2_dispatch!(
    fn band_row(rows: &[&[i32]; 6], lo: &[&[i32]; 5], hi: &[&[i32]; 5], x0: usize, x1: usize, mask: Option<&mut [u8]>) -> (u64, u64) => band_row_impl
);

/// The score of each [`MAP_TILE`] tile of a smooth / contour mask (NaN
/// below [`MAP_MIN`]).
fn tile_map(mask: &[u8], w: usize, h: usize) -> Heatmap {
    let (tw, th) = (w.div_ceil(MAP_TILE), h.div_ceil(MAP_TILE));
    let data: Vec<f32> = (0..th)
        .into_par_iter()
        .flat_map_iter(|ty| {
            let mut n = vec![(0u32, 0u32); tw];
            for y in ty * MAP_TILE..((ty + 1) * MAP_TILE).min(h) {
                for (x, &m) in mask[y * w..(y + 1) * w].iter().enumerate() {
                    let t = &mut n[x / MAP_TILE];
                    t.0 += (m >= 1) as u32;
                    t.1 += (m >= 2) as u32;
                }
            }
            n.into_iter().map(|(smooth, contour)| {
                let v = if smooth == 0 { 0.0 } else { 100.0 * contour as f32 / smooth as f32 };
                if v < MAP_MIN { f32::NAN } else { v }
            })
        })
        .collect();
    let t = MAP_TILE as f32;
    Heatmap::new(tw, th, data, "Banding").with_unit(Unit::Percent).with_cell(t, t).with_range(0.0, 20.0).with_min_span(5.0).with_style(Colormap::Magma, false, Pool::Max)
}

impl Metric for Banding {
    fn info(&self) -> &'static MetricInfo {
        &BANDING
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&BANDING, a)?;
        let l = a.luma();
        let (w, h) = (l.width, l.height);
        let levels = l.levels;
        // Round half up without libm (`round` is a call on baseline x86-64).
        let q: Vec<i32> = l.data.par_iter().map(|v| (v * levels).max(0.0).add(0.5) as i32).collect();
        let mut mask = if o.want_map { vec![0u8; w * h] } else { Vec::new() };
        // Pixels far enough from the border for the window and the runs.
        let m = RUN.max(R);
        let (x0, x1) = (m, w.saturating_sub(RUN + 1));
        let (y0, y1) = (m, h.saturating_sub(RUN + 1));
        let band = |b: usize, mut out: Option<&mut [u8]>| {
            let (r0, r1) = ((b * BAND).max(y0), ((b + 1) * BAND).min(y1));
            if cancel.is_cancelled() || r0 >= r1 || x0 >= x1 {
                return (0, 0);
            }
            // Horizontal 5-wide min / max for rows r0 - R .. r1 + R.
            let rows = r1 - r0 + 2 * R;
            let mut hmin = vec![0i32; rows * w];
            let mut hmax = vec![0i32; rows * w];
            for (r, (lo, hi)) in hmin.chunks_mut(w).zip(hmax.chunks_mut(w)).enumerate() {
                let y = r0 + r - R;
                minmax_row(&q[y * w..(y + 1) * w], lo, hi);
            }
            let (mut contours, mut smooth_n) = (0u64, 0u64);
            for y in r0..r1 {
                let qr: [&[i32]; 6] = std::array::from_fn(|k| &q[(y + k - 2) * w..(y + k - 1) * w]);
                let rr = y - r0;
                let lo: [&[i32]; 5] = std::array::from_fn(|k| &hmin[(rr + k) * w..(rr + k + 1) * w]);
                let hi: [&[i32]; 5] = std::array::from_fn(|k| &hmax[(rr + k) * w..(rr + k + 1) * w]);
                let m = out.as_deref_mut().map(|m| &mut m[(y - b * BAND) * w..(y - b * BAND + 1) * w]);
                let (c, s) = band_row(&qr, &lo, &hi, x0, x1, m);
                contours += c;
                smooth_n += s;
            }
            (contours, smooth_n)
        };
        let add = |a: (u64, u64), b: (u64, u64)| (a.0 + b.0, a.1 + b.1);
        let (contours, smooth_n) = if o.want_map {
            mask.par_chunks_mut(w * BAND).enumerate().map(|(b, m)| band(b, Some(m))).reduce(|| (0, 0), add)
        } else {
            (0..h.div_ceil(BAND)).into_par_iter().map(|b| band(b, None)).reduce(|| (0, 0), add)
        };
        cancel.check()?;
        let mut r = MetricResult::new(&BANDING);
        let pct = |n: u64, d: u64| if d == 0 { 0.0 } else { 100.0 * n as f64 / d as f64 };
        r.push("score", pct(contours, smooth_n), Unit::Percent, Direction::LowerIsBetter);
        r.push("smooth_pct", pct(smooth_n, (w * h) as u64), Unit::Percent, Direction::Neutral);
        r.push("contour_pct", pct(contours, (w * h) as u64), Unit::Percent, Direction::LowerIsBetter);
        if o.want_map {
            r.map = Some(tile_map(&mask, w, h));
        }
        Ok(r)
    }
}
