//! SSIM in two variants.
//!
//! - `ssim.fast`: FFmpeg / x264 SSIM, unweighted 8x8 windows at stride 4 built
//!   from integer 4x4 block sums (two block rows in flight per band).
//! - `ssim.gauss`: Wang et al. 2004, 11x11 Gaussian (sigma 1.5), "valid"
//!   windows only, as scikit-image `structural_similarity(gaussian_weights=True,
//!   sigma=1.5, use_sample_covariance=False)`. Separable `f32` filtering of the
//!   five moment images in strips of 64 rows plus a 10-row halo, with a map.

use crate::frame::{FrameCtx, FrameView, InputKind, Plane, PlaneView, RgbData, Sample};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{check_size, reference, ssim_db, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use rayon::prelude::*;
use std::ops::{Add, AddAssign, Mul};

// ---------------------------------------------------------------- fast SSIM

/// Integer accumulator wide enough for 4x4 / 8x8 block moments.
pub(crate) trait Wide: Copy + Default + Send + Sync + Add<Output = Self> + AddAssign + Mul<Output = Self> {
    fn f64(self) -> f64;
}
impl Wide for u32 {
    #[inline(always)]
    fn f64(self) -> f64 {
        self as f64
    }
}
impl Wide for u64 {
    #[inline(always)]
    fn f64(self) -> f64 {
        self as f64
    }
}

/// Sample types with their block-moment accumulator: `u8` fits `u32`
/// (8x8 sum of a^2 + b^2 is at most 8.3 million), `u16` needs `u64`.
pub(crate) trait SsimSample: Sample {
    type W: Wide;
    fn w(self) -> Self::W;
}
impl SsimSample for u8 {
    type W = u32;
    #[inline(always)]
    fn w(self) -> u32 {
        self as u32
    }
}
impl SsimSample for u16 {
    type W = u64;
    #[inline(always)]
    fn w(self) -> u64 {
        self as u64
    }
}

/// Column accumulators (structure of arrays) for one block row.
struct Cols<W> {
    s1: Vec<W>,
    s2: Vec<W>,
    ss: Vec<W>,
    s12: Vec<W>,
}

impl<W: Wide> Cols<W> {
    fn new(n: usize) -> Self {
        Cols { s1: vec![W::default(); n], s2: vec![W::default(); n], ss: vec![W::default(); n], s12: vec![W::default(); n] }
    }
}

/// 4x4 block sums (s1, s2, ss, s12) of block row `by` into `out[..bw]`.
#[inline(always)]
fn block_row<T: SsimSample>(a: &Plane<'_, T>, b: &Plane<'_, T>, by: usize, bw: usize, cols: &mut Cols<T::W>, out: &mut [[T::W; 4]]) {
    let n = bw * 4;
    for v in [&mut cols.s1, &mut cols.s2, &mut cols.ss, &mut cols.s12] {
        v[..n].fill(T::W::default());
    }
    for r in 0..4 {
        let ra = &a.row(by * 4 + r)[..n];
        let rb = &b.row(by * 4 + r)[..n];
        let (s1, s2, ss, s12) = (&mut cols.s1[..n], &mut cols.s2[..n], &mut cols.ss[..n], &mut cols.s12[..n]);
        for x in 0..n {
            let p = ra[x].w();
            let q = rb[x].w();
            s1[x] += p;
            s2[x] += q;
            ss[x] += p * p + q * q;
            s12[x] += p * q;
        }
    }
    for (bx, o) in out[..bw].iter_mut().enumerate() {
        let i = bx * 4;
        let sum = |v: &[T::W]| v[i] + v[i + 1] + v[i + 2] + v[i + 3];
        *o = [sum(&cols.s1), sum(&cols.s2), sum(&cols.ss), sum(&cols.s12)];
    }
}

#[inline(always)]
fn block_row_u8_impl(a: &Plane<'_, u8>, b: &Plane<'_, u8>, by: usize, bw: usize, cols: &mut Cols<u32>, out: &mut [[u32; 4]]) {
    block_row(a, b, by, bw, cols, out)
}

#[inline(always)]
fn block_row_u16_impl(a: &Plane<'_, u16>, b: &Plane<'_, u16>, by: usize, bw: usize, cols: &mut Cols<u64>, out: &mut [[u64; 4]]) {
    block_row(a, b, by, bw, cols, out)
}

crate::simd::avx2_dispatch!(fn block_row_u8(a: &Plane<'_, u8>, b: &Plane<'_, u8>, by: usize, bw: usize, cols: &mut Cols<u32>, out: &mut [[u32; 4]]) => block_row_u8_impl);
crate::simd::avx2_dispatch!(fn block_row_u16(a: &Plane<'_, u16>, b: &Plane<'_, u16>, by: usize, bw: usize, cols: &mut Cols<u64>, out: &mut [[u64; 4]]) => block_row_u16_impl);

/// Typed block-row kernel, picked once per plane.
trait BlockRow: SsimSample {
    fn block_row(a: &Plane<'_, Self>, b: &Plane<'_, Self>, by: usize, bw: usize, cols: &mut Cols<Self::W>, out: &mut [[Self::W; 4]]);
}
impl BlockRow for u8 {
    fn block_row(a: &Plane<'_, u8>, b: &Plane<'_, u8>, by: usize, bw: usize, cols: &mut Cols<u32>, out: &mut [[u32; 4]]) {
        block_row_u8(a, b, by, bw, cols, out)
    }
}
impl BlockRow for u16 {
    fn block_row(a: &Plane<'_, u16>, b: &Plane<'_, u16>, by: usize, bw: usize, cols: &mut Cols<u64>, out: &mut [[u64; 4]]) {
        block_row_u16(a, b, by, bw, cols, out)
    }
}

/// Sum of SSIM over the 8x8 windows formed by two adjacent block rows
/// (same arithmetic and operation order as FFmpeg's `ssim_end1`, in `f64`).
fn window_row<W: Wide>(top: &[[W; 4]], bot: &[[W; 4]], c1: f64, c2: f64) -> f64 {
    let mut acc = 0.0;
    for bx in 0..top.len() - 1 {
        let s = |k: usize| (top[bx][k] + top[bx + 1][k] + bot[bx][k] + bot[bx + 1][k]).f64();
        let (s1, s2, ss, s12) = (s(0), s(1), s(2), s(3));
        let vars = ss * 64.0 - s1 * s1 - s2 * s2;
        let covar = s12 * 64.0 - s1 * s2;
        acc += (2.0 * s1 * s2 + c1) * (2.0 * covar + c2) / ((s1 * s1 + s2 * s2 + c1) * (vars + c2));
    }
    acc
}

const BAND: usize = 16;

fn ssim_fast_t<T: BlockRow>(a: &Plane<'_, T>, b: &Plane<'_, T>, peak: f64, cancel: &CancelToken) -> Result<f64, MetricError> {
    let (bw, bh) = (a.width / 4, a.height / 4);
    let c1 = (0.01 * peak).powi(2) * 64.0;
    let c2 = (0.03 * peak).powi(2) * 64.0 * 63.0;
    let rows = bh - 1;
    let total: f64 = (0..rows.div_ceil(BAND))
        .into_par_iter()
        .map(|band| {
            if cancel.is_cancelled() {
                return 0.0;
            }
            let r0 = band * BAND;
            let r1 = (r0 + BAND).min(rows);
            let mut cols = Cols::<T::W>::new(bw * 4);
            let mut prev = vec![[T::W::default(); 4]; bw];
            let mut cur = vec![[T::W::default(); 4]; bw];
            T::block_row(a, b, r0, bw, &mut cols, &mut prev);
            let mut acc = 0.0;
            for by in r0..r1 {
                T::block_row(a, b, by + 1, bw, &mut cols, &mut cur);
                acc += window_row(&prev, &cur, c1, c2);
                std::mem::swap(&mut prev, &mut cur);
            }
            acc
        })
        .sum();
    cancel.check()?;
    Ok(total / ((bw - 1) * rows) as f64)
}

/// FFmpeg-style SSIM of two planes (same size and sample type). Planes
/// smaller than 8x8 read 1 when identical and 0 otherwise, like lumen-core.
pub fn ssim_fast_plane(a: &PlaneView<'_>, b: &PlaneView<'_>, peak: f64, cancel: &CancelToken) -> Result<f64, MetricError> {
    if a.width() != b.width() || a.height() != b.height() || a.is_u16() != b.is_u16() {
        return Err(MetricError::Mismatch("plane layout".into()));
    }
    if a.width() / 4 < 2 || a.height() / 4 < 2 {
        let same = super::psnr::plane_error(a, b).map(|e| e.sse == 0.0).unwrap_or(false);
        return Ok(if same { 1.0 } else { 0.0 });
    }
    match (a, b) {
        (PlaneView::U8(p), PlaneView::U8(q)) => ssim_fast_t(p, q, peak, cancel),
        (PlaneView::U16(p), PlaneView::U16(q)) => ssim_fast_t(p, q, peak, cancel),
        _ => unreachable!(),
    }
}

/// Integer BT.709 luma plane of an RGB view (u8 for 8-bit, u16 otherwise).
enum LumaPlane {
    U8(Vec<u8>),
    U16(Vec<u16>),
}

fn rgb_luma_plane(v: &FrameView<'_>) -> Option<(LumaPlane, usize, usize, f64)> {
    let FrameView::Rgb(r) = v else { return None };
    let (w, h, c) = (r.width, r.height, r.channels);
    // 0.2126, 0.7152, 0.0722 in 16-bit fixed point (sum 65536).
    const KR: u64 = 13_933;
    const KG: u64 = 46_871;
    const KB: u64 = 4_732;
    Some(match r.data {
        RgbData::U8(d) => {
            let mut out = vec![0u8; w * h];
            out.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
                let row = &d[y * r.stride..];
                for (x, p) in o.iter_mut().enumerate() {
                    let s = &row[x * c..x * c + 3];
                    *p = ((KR as u32 * s[0] as u32 + KG as u32 * s[1] as u32 + KB as u32 * s[2] as u32 + 32_768) >> 16) as u8;
                }
            });
            (LumaPlane::U8(out), w, h, 255.0)
        }
        RgbData::U16(d) => {
            let mut out = vec![0u16; w * h];
            out.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
                let row = &d[y * r.stride..];
                for (x, p) in o.iter_mut().enumerate() {
                    let s = &row[x * c..x * c + 3];
                    *p = ((KR * s[0] as u64 + KG * s[1] as u64 + KB * s[2] as u64 + 32_768) >> 16) as u16;
                }
            });
            (LumaPlane::U16(out), w, h, 65535.0)
        }
        RgbData::F32(d) => {
            let mut out = vec![0u16; w * h];
            out.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
                let row = &d[y * r.stride..];
                for (x, p) in o.iter_mut().enumerate() {
                    let s = &row[x * c..x * c + 3];
                    let l = 0.2126 * s[0] + 0.7152 * s[1] + 0.0722 * s[2];
                    *p = (l.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
                }
            });
            (LumaPlane::U16(out), w, h, 65535.0)
        }
    })
}

/// `ssim.fast`: FFmpeg/x264 SSIM per plane (Y, U, V, and the sample-weighted
/// "all"); RGB inputs use integer BT.709 luma.
pub struct SsimFast;

static SSIM_FAST: MetricInfo = MetricInfo {
    id: "ssim.fast",
    name: "SSIM (fast)",
    kind: MetricKind::FullReference,
    version: 1,
    inputs: &[InputKind::NativePlanes],
    // Smaller planes read 1 when identical, else 0 (as lumen-core did).
    min_size: (1, 1),
    cost_ms_4k: 7.0,
    heavy: false,
    has_map: false,
    direction: Direction::HigherIsBetter,
    headline: &["y"],
    description: "FFmpeg/x264-compatible SSIM: 8x8 windows at stride 4",
};

impl Metric for SsimFast {
    fn info(&self) -> &'static MetricInfo {
        &SSIM_FAST
    }

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, _o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        let b = reference(&SSIM_FAST, a, b)?;
        let mut r = MetricResult::new(&SSIM_FAST);
        match (a.view(), b.view()) {
            (FrameView::Yuv(x), FrameView::Yuv(y)) if a.view().same_layout(b.view()) => {
                let peak = x.max_value() as f64;
                let (pa, pb) = (x.planes(), y.planes());
                let mut vals = Vec::new();
                for (p, q) in pa.iter().zip(&pb) {
                    vals.push((ssim_fast_plane(p, q, peak, cancel)?, p.len() as f64));
                }
                r.push("y", vals[0].0, Unit::Index, Direction::HigherIsBetter);
                if vals.len() == 3 {
                    r.push("u", vals[1].0, Unit::Index, Direction::HigherIsBetter);
                    r.push("v", vals[2].0, Unit::Index, Direction::HigherIsBetter);
                    let n: f64 = vals.iter().map(|v| v.1).sum();
                    r.push("all", vals.iter().map(|v| v.0 * v.1).sum::<f64>() / n, Unit::Index, Direction::HigherIsBetter);
                }
                r.push("y_db", ssim_db(vals[0].0), Unit::Db, Direction::HigherIsBetter);
            }
            (FrameView::Rgb(_), FrameView::Rgb(_)) => {
                let (la, w, h, peak) = rgb_luma_plane(a.view()).expect("rgb");
                let (lb, ..) = rgb_luma_plane(b.view()).expect("rgb");
                let s = match (&la, &lb) {
                    (LumaPlane::U8(p), LumaPlane::U8(q)) => ssim_fast_plane(&PlaneView::U8(Plane { data: p, width: w, height: h, stride: w }), &PlaneView::U8(Plane { data: q, width: w, height: h, stride: w }), peak, cancel)?,
                    (LumaPlane::U16(p), LumaPlane::U16(q)) => ssim_fast_plane(&PlaneView::U16(Plane { data: p, width: w, height: h, stride: w }), &PlaneView::U16(Plane { data: q, width: w, height: h, stride: w }), peak, cancel)?,
                    _ => return Err(MetricError::Mismatch("RGB sample types differ".into())),
                };
                r.push("y", s, Unit::Index, Direction::HigherIsBetter);
                r.push("y_db", ssim_db(s), Unit::Db, Direction::HigherIsBetter);
            }
            _ => return Err(MetricError::Mismatch("YUV layouts or kinds differ".into())),
        }
        Ok(r)
    }
}

// ------------------------------------------------------------ Gaussian SSIM

const RADIUS: usize = 5;
const TAPS: usize = 2 * RADIUS + 1;
const STRIP: usize = 64;

/// Normalized 11-tap Gaussian, sigma 1.5 (scipy `gaussian_filter` with truncate 3.5).
fn kernel() -> [f32; TAPS] {
    let sigma = 1.5f64;
    let raw: [f64; TAPS] = std::array::from_fn(|i| {
        let x = i as f64 - RADIUS as f64;
        (-0.5 * x * x / (sigma * sigma)).exp()
    });
    let s: f64 = raw.iter().sum();
    raw.map(|v| (v / s) as f32)
}

/// Vector width of the register-blocked loops (one AVX register, two SSE).
const LANES: usize = 8;

/// out[x] = sum_k g[k] * src[x + k] for x in 0..out.len(), accumulating
/// `LANES` outputs in registers across all taps.
#[inline(always)]
fn conv_row(src: &[f32], g: &[f32; TAPS], out: &mut [f32]) {
    let n = out.len();
    let full = n / LANES * LANES;
    for x0 in (0..full).step_by(LANES) {
        let mut acc = [0f32; LANES];
        for (k, &gk) in g.iter().enumerate() {
            let s = &src[x0 + k..x0 + k + LANES];
            for i in 0..LANES {
                acc[i] += gk * s[i];
            }
        }
        out[x0..x0 + LANES].copy_from_slice(&acc);
    }
    for x in full..n {
        let mut acc = 0f32;
        for (k, &gk) in g.iter().enumerate() {
            acc += gk * src[x + k];
        }
        out[x] = acc;
    }
}

/// Mean SSIM and mean contrast-structure term over the valid windows, with
/// the optional SSIM map (`(w - 10) x (h - 10)`, centered: origin (5, 5)).
pub(crate) struct GaussOut {
    pub ssim: f64,
    pub cs: f64,
    pub map: Option<Vec<f32>>,
    pub w: usize,
    pub h: usize,
}

/// SSIM and cs at one pixel from offset moments.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn ssim_px(mx: f32, my: f32, mxx: f32, myy: f32, mxy: f32, off: f32, c1: f32, c2: f32) -> (f32, f32) {
    let vx = mxx - mx * mx;
    let vy = myy - my * my;
    let vxy = mxy - mx * my;
    let (ux, uy) = (mx + off, my + off);
    let cs = (2.0 * vxy + c2) / (vx + vy + c2);
    let l = (2.0 * ux * uy + c1) / (ux * ux + uy * uy + c1);
    (l * cs, cs)
}

/// Horizontally filtered moments (x, y, x^2, y^2, xy around `off`) of input
/// row `iy` into its ring slot.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn fill(a: &[f32], b: &[f32], w: usize, iy: usize, off: f32, g: &[f32; TAPS], tmp: &mut [f32], ring: &mut [f32]) {
    let wo = w - 2 * RADIUS;
    let (ra, rb) = (&a[iy * w..(iy + 1) * w], &b[iy * w..(iy + 1) * w]);
    let (tx, rest) = tmp.split_at_mut(w);
    let (ty, rest) = rest.split_at_mut(w);
    let (txx, rest) = rest.split_at_mut(w);
    let (tyy, txy) = rest.split_at_mut(w);
    for x in 0..w {
        let p = ra[x] - off;
        let q = rb[x] - off;
        tx[x] = p;
        ty[x] = q;
        txx[x] = p * p;
        tyy[x] = q * q;
        txy[x] = p * q;
    }
    let slot = iy % TAPS;
    for (k, src) in [&*tx, &*ty, &*txx, &*tyy, &*txy].into_iter().enumerate() {
        let o = (slot * 5 + k) * wo;
        conv_row(src, g, &mut ring[o..o + wo]);
    }
}

/// Output rows `y0..y1` of the SSIM map. Horizontally filtered moment rows
/// live in a ring of 11 rows (about 0.8 MB at 4K, L2 resident); each output
/// chunk accumulates the 11 vertical taps of all five moments in registers
/// and evaluates SSIM directly. Returns (sum of SSIM, sum of cs).
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn strip_impl(a: &[f32], b: &[f32], w: usize, y0: usize, y1: usize, off: f32, c1: f32, c2: f32, mut map: Option<&mut [f32]>) -> (f64, f64) {
    let g = kernel();
    let wo = w - 2 * RADIUS;
    let mut ring = vec![0f32; TAPS * 5 * wo];
    let mut tmp = vec![0f32; 5 * w];
    for iy in y0..y0 + 2 * RADIUS {
        fill(a, b, w, iy, off, &g, &mut tmp, &mut ring);
    }
    let (mut ss, mut cs) = (0f64, 0f64);
    let full = wo / LANES * LANES;
    for oy in y0..y1 {
        fill(a, b, w, oy + 2 * RADIUS, off, &g, &mut tmp, &mut ring);
        let base: [usize; TAPS] = std::array::from_fn(|t| ((oy + t) % TAPS) * 5 * wo);
        let mut row_map = map.as_deref_mut().map(|m| &mut m[(oy - y0) * wo..(oy - y0 + 1) * wo]);
        let mut lane_s = [0f64; LANES];
        let mut lane_c = [0f64; LANES];
        for x0 in (0..full).step_by(LANES) {
            let mut acc = [[0f32; LANES]; 5];
            for t in 0..TAPS {
                let gt = g[t];
                for (k, ak) in acc.iter_mut().enumerate() {
                    let r = &ring[base[t] + k * wo + x0..base[t] + k * wo + x0 + LANES];
                    for i in 0..LANES {
                        ak[i] += gt * r[i];
                    }
                }
            }
            let mut sv = [0f32; LANES];
            for i in 0..LANES {
                let (s, c) = ssim_px(acc[0][i], acc[1][i], acc[2][i], acc[3][i], acc[4][i], off, c1, c2);
                sv[i] = s;
                lane_s[i] += s as f64;
                lane_c[i] += c as f64;
            }
            if let Some(m) = row_map.as_deref_mut() {
                m[x0..x0 + LANES].copy_from_slice(&sv);
            }
        }
        for x in full..wo {
            let mut v = [0f32; 5];
            for t in 0..TAPS {
                for (k, vk) in v.iter_mut().enumerate() {
                    *vk += g[t] * ring[base[t] + k * wo + x];
                }
            }
            let (s, c) = ssim_px(v[0], v[1], v[2], v[3], v[4], off, c1, c2);
            if let Some(m) = row_map.as_deref_mut() {
                m[x] = s;
            }
            ss += s as f64;
            cs += c as f64;
        }
        ss += lane_s.iter().sum::<f64>();
        cs += lane_c.iter().sum::<f64>();
    }
    (ss, cs)
}

crate::simd::avx2_dispatch!(
    fn strip(a: &[f32], b: &[f32], w: usize, y0: usize, y1: usize, off: f32, c1: f32, c2: f32, map: Option<&mut [f32]>) -> (f64, f64) => strip_impl
);

/// Gaussian SSIM of two packed `f32` images with dynamic range `peak`.
pub(crate) fn gauss_ssim(a: &[f32], b: &[f32], w: usize, h: usize, peak: f32, want_map: bool, cancel: &CancelToken) -> Result<GaussOut, MetricError> {
    if w < TAPS || h < TAPS {
        return Err(MetricError::TooSmall { min: (TAPS as u32, TAPS as u32), got: (w as u32, h as u32) });
    }
    let (wo, ho) = (w - 2 * RADIUS, h - 2 * RADIUS);
    let c1 = (0.01 * peak) * (0.01 * peak);
    let c2 = (0.03 * peak) * (0.03 * peak);
    // Moments are taken around half scale so E[x^2] - E[x]^2 cancels less in f32.
    let off = 0.5 * peak;
    let mut map = if want_map { vec![0f32; wo * ho] } else { Vec::new() };
    let strips = ho.div_ceil(STRIP);
    let chunks: Vec<Option<&mut [f32]>> = if want_map { map.chunks_mut(STRIP * wo).map(Some).collect() } else { (0..strips).map(|_| None).collect() };
    let (ssim_sum, cs_sum) = chunks
        .into_par_iter()
        .enumerate()
        .map(|(s, m)| {
            if cancel.is_cancelled() {
                return (0.0, 0.0);
            }
            let y0 = s * STRIP;
            strip(a, b, w, y0, (y0 + STRIP).min(ho), off, c1, c2, m)
        })
        .reduce(|| (0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
    cancel.check()?;
    let n = (wo * ho) as f64;
    Ok(GaussOut { ssim: ssim_sum / n, cs: cs_sum / n, map: want_map.then_some(map), w: wo, h: ho })
}

/// Wang's automatic downsampling: `f = max(1, round(min(w, h) / 256))`,
/// f x f block means (non-overlapping).
pub(crate) fn wang_downsample(d: &[f32], w: usize, h: usize) -> (Vec<f32>, usize, usize, usize) {
    let f = ((w.min(h) as f64 / 256.0).round() as usize).max(1);
    if f == 1 {
        return (d.to_vec(), w, h, 1);
    }
    let (w2, h2) = (w / f, h / f);
    let mut out = vec![0f32; w2 * h2];
    let inv = 1.0 / (f * f) as f32;
    out.par_chunks_mut(w2).enumerate().for_each(|(y, o)| {
        for (x, v) in o.iter_mut().enumerate() {
            let mut s = 0f32;
            for yy in y * f..(y + 1) * f {
                s += d[yy * w + x * f..yy * w + (x + 1) * f].iter().sum::<f32>();
            }
            *v = s * inv;
        }
    });
    (out, w2, h2, f)
}

/// `ssim.gauss`: Wang SSIM with an 11x11 Gaussian window on luma, plus the map.
pub struct SsimGauss;

static SSIM_GAUSS: MetricInfo = MetricInfo {
    id: "ssim.gauss",
    name: "SSIM (Gaussian)",
    kind: MetricKind::FullReference,
    version: 1,
    inputs: &[InputKind::Luma],
    min_size: (11, 11),
    cost_ms_4k: 45.0,
    heavy: false,
    has_map: true,
    direction: Direction::HigherIsBetter,
    headline: &["y"],
    description: "SSIM with an 11x11 Gaussian window (sigma 1.5) on luma, with an SSIM map",
};

impl Metric for SsimGauss {
    fn info(&self) -> &'static MetricInfo {
        &SSIM_GAUSS
    }

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        let b = reference(&SSIM_GAUSS, a, b)?;
        check_size(&SSIM_GAUSS, b)?;
        let (la, lb) = (a.luma(), b.luma());
        let (w, h) = (la.width, la.height);
        let (out, f) = if o.ssim_downsample {
            let (da, w2, h2, f) = wang_downsample(&la.data, w, h);
            let (db, ..) = wang_downsample(&lb.data, w, h);
            (gauss_ssim(&da, &db, w2, h2, 1.0, o.want_map, cancel)?, f)
        } else {
            (gauss_ssim(&la.data, &lb.data, w, h, 1.0, o.want_map, cancel)?, 1)
        };
        let mut r = MetricResult::new(&SSIM_GAUSS);
        r.push("y", out.ssim, Unit::Index, Direction::HigherIsBetter);
        r.push("db", ssim_db(out.ssim), Unit::Db, Direction::HigherIsBetter);
        r.push("cs", out.cs, Unit::Index, Direction::HigherIsBetter);
        if f > 1 {
            r.notes.push(format!("downsampled {f}x before SSIM (Wang)"));
        }
        if let Some(m) = out.map {
            let f = f as f32;
            r.map = Some(
                Heatmap::new(out.w, out.h, m, "SSIM")
                    .with_unit(Unit::Index)
                    .with_range(0.7, 1.0)
                    .with_min_span(0.1)
                    .with_style(Colormap::Magma, true, Pool::Min)
                    .with_origin(RADIUS as f32 * f, RADIUS as f32 * f)
                    .with_cell(f, f),
            );
        }
        Ok(r)
    }
}
