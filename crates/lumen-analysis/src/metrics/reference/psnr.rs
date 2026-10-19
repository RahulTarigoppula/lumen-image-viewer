//! PSNR, MSE and max abs error on native YUV planes (FFmpeg `psnr`
//! weighting) and on RGB channels. Integer accumulation on typed slices:
//! fixed-width lanes so the inner loop autovectorizes, no per-sample match.

use crate::frame::{FrameCtx, FrameView, Plane, PlaneView, RgbData, RgbView, RowConverter};
use crate::job::CancelToken;
use crate::metrics::{psnr_db, reference, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use crate::frame::InputKind;
use rayon::prelude::*;

/// Squared error sum and max abs difference of one plane or channel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ErrStats {
    pub sse: f64,
    pub max_abs: f64,
    pub count: u64,
}

impl ErrStats {
    pub fn mse(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sse / self.count as f64
        }
    }
}

type Lanes = ([u64; 4], [u32; 4]);

fn merge(mut a: Lanes, b: Lanes) -> Lanes {
    for k in 0..4 {
        a.0[k] += b.0[k];
        a.1[k] = a.1[k].max(b.1[k]);
    }
    a
}

/// Per-channel SSE and max abs over an interleaved 8-bit row (`ch` = 1, 3 or 4;
/// `L` a multiple of `ch`). Lane sums fit `u32` for rows up to 2 million samples.
#[inline]
fn row_u8<const L: usize>(a: &[u8], b: &[u8], ch: usize, acc: &mut Lanes) {
    let n = a.len().min(b.len());
    let full = n / L * L;
    let mut sse = [0u32; L];
    let mut mx = [0u8; L];
    for (x, y) in a[..full].as_chunks::<L>().0.iter().zip(b[..full].as_chunks::<L>().0) {
        for i in 0..L {
            let d = x[i].abs_diff(y[i]);
            sse[i] += d as u32 * d as u32;
            mx[i] = mx[i].max(d);
        }
    }
    for i in 0..L {
        acc.0[i % ch] += sse[i] as u64;
        acc.1[i % ch] = acc.1[i % ch].max(mx[i] as u32);
    }
    for i in full..n {
        let d = a[i].abs_diff(b[i]) as u64;
        acc.0[i % ch] += d * d;
        acc.1[i % ch] = acc.1[i % ch].max(d as u32);
    }
}

/// 16-bit version of [`row_u8`] (`u64` lanes).
#[inline]
fn row_u16<const L: usize>(a: &[u16], b: &[u16], ch: usize, acc: &mut Lanes) {
    let n = a.len().min(b.len());
    let full = n / L * L;
    let mut sse = [0u64; L];
    let mut mx = [0u16; L];
    for (x, y) in a[..full].as_chunks::<L>().0.iter().zip(b[..full].as_chunks::<L>().0) {
        for i in 0..L {
            let d = x[i].abs_diff(y[i]) as u64;
            sse[i] += d * d;
            mx[i] = mx[i].max(d as u16);
        }
    }
    for i in 0..L {
        acc.0[i % ch] += sse[i];
        acc.1[i % ch] = acc.1[i % ch].max(mx[i] as u32);
    }
    for i in full..n {
        let d = a[i].abs_diff(b[i]) as u64;
        acc.0[i % ch] += d * d;
        acc.1[i % ch] = acc.1[i % ch].max(d as u32);
    }
}

fn rows_par(h: usize, row: impl Fn(usize, &mut Lanes) + Sync + Send) -> Lanes {
    (0..h)
        .into_par_iter()
        .with_min_len(8)
        .fold(|| ([0u64; 4], [0u32; 4]), |mut acc, y| {
            row(y, &mut acc);
            acc
        })
        .reduce(|| ([0u64; 4], [0u32; 4]), merge)
}

fn to_stats(l: &Lanes, k: usize, count: u64) -> ErrStats {
    ErrStats { sse: l.0[k] as f64, max_abs: l.1[k] as f64, count }
}

/// Error statistics of two planes of the same size and sample type.
pub fn plane_error(a: &PlaneView<'_>, b: &PlaneView<'_>) -> Option<ErrStats> {
    if a.width() != b.width() || a.height() != b.height() {
        return None;
    }
    let n = a.len() as u64;
    let l = match (a, b) {
        (PlaneView::U8(p), PlaneView::U8(q)) => plane_lanes_u8(p, q),
        (PlaneView::U16(p), PlaneView::U16(q)) => rows_par(p.height, |y, acc| row_u16::<32>(p.row(y), q.row(y), 1, acc)),
        _ => return None,
    };
    Some(to_stats(&l, 0, n))
}

fn plane_lanes_u8(p: &Plane<'_, u8>, q: &Plane<'_, u8>) -> Lanes {
    rows_par(p.height, |y, acc| row_u8::<32>(p.row(y), q.row(y), 1, acc))
}

/// Per-channel (R, G, B) error statistics of two RGB views of the same size
/// and sample type. Float data is compared in its own scale.
pub fn rgb_error(a: &RgbView<'_>, b: &RgbView<'_>) -> Option<[ErrStats; 3]> {
    if a.width != b.width || a.height != b.height {
        return None;
    }
    let (w, h) = (a.width, a.height);
    let n = (w * h) as u64;
    let lanes = match (a.data, b.data) {
        (RgbData::U8(p), RgbData::U8(q)) => {
            let row = |y: usize, acc: &mut Lanes| {
                let ra = &p[y * a.stride..y * a.stride + w * a.channels];
                let rb = &q[y * b.stride..y * b.stride + w * b.channels];
                match (a.channels, b.channels) {
                    (4, 4) => row_u8::<32>(ra, rb, 4, acc),
                    (3, 3) => row_u8::<48>(ra, rb, 3, acc),
                    _ => mixed_row(ra, rb, a.channels, b.channels, w, acc),
                }
            };
            rows_par(h, row)
        }
        (RgbData::U16(p), RgbData::U16(q)) => {
            let row = |y: usize, acc: &mut Lanes| {
                let ra = &p[y * a.stride..y * a.stride + w * a.channels];
                let rb = &q[y * b.stride..y * b.stride + w * b.channels];
                match (a.channels, b.channels) {
                    (4, 4) => row_u16::<32>(ra, rb, 4, acc),
                    (3, 3) => row_u16::<48>(ra, rb, 3, acc),
                    _ => mixed_row(ra, rb, a.channels, b.channels, w, acc),
                }
            };
            rows_par(h, row)
        }
        (RgbData::F32(p), RgbData::F32(q)) => return Some(rgb_error_f32(p, q, a, b)),
        _ => return None,
    };
    Some([to_stats(&lanes, 0, n), to_stats(&lanes, 1, n), to_stats(&lanes, 2, n)])
}

/// RGB vs RGBA rows (rare): plain per-pixel loop.
fn mixed_row<T: Copy + Into<u32>>(a: &[T], b: &[T], ca: usize, cb: usize, w: usize, acc: &mut Lanes) {
    for x in 0..w {
        for k in 0..3 {
            let d = (a[x * ca + k].into() as i64 - b[x * cb + k].into() as i64).unsigned_abs();
            acc.0[k] += d * d;
            acc.1[k] = acc.1[k].max(d as u32);
        }
    }
}

fn rgb_error_f32(p: &[f32], q: &[f32], a: &RgbView<'_>, b: &RgbView<'_>) -> [ErrStats; 3] {
    let (w, h) = (a.width, a.height);
    let (sse, mx) = (0..h)
        .into_par_iter()
        .map(|y| {
            let ra = &p[y * a.stride..];
            let rb = &q[y * b.stride..];
            let mut s = [0f64; 3];
            let mut m = [0f32; 3];
            for x in 0..w {
                for k in 0..3 {
                    let d = ra[x * a.channels + k] - rb[x * b.channels + k];
                    s[k] += (d * d) as f64;
                    m[k] = m[k].max(d.abs());
                }
            }
            (s, m)
        })
        .reduce(|| ([0f64; 3], [0f32; 3]), |a, b| ([a.0[0] + b.0[0], a.0[1] + b.0[1], a.0[2] + b.0[2]], [a.1[0].max(b.1[0]), a.1[1].max(b.1[1]), a.1[2].max(b.1[2])]));
    let n = (w * h) as u64;
    std::array::from_fn(|k| ErrStats { sse: sse[k], max_abs: mx[k] as f64, count: n })
}

/// Peak signal value used for PSNR on a view's native samples.
pub fn peak(view: &FrameView<'_>) -> f64 {
    match view {
        FrameView::Yuv(v) => v.max_value() as f64,
        FrameView::Rgb(v) => v.max_value() as f64,
    }
}

fn push_set(r: &mut MetricResult, names: &[&str], errs: &[ErrStats], peak: f64, code: Unit) {
    let total: u64 = errs.iter().map(|e| e.count).sum();
    let mse_avg: f64 = errs.iter().map(|e| e.mse() * e.count as f64 / total.max(1) as f64).sum();
    for (n, e) in names.iter().zip(errs) {
        r.push(*n, psnr_db(e.mse(), peak), Unit::Db, Direction::HigherIsBetter);
    }
    r.push("avg", psnr_db(mse_avg, peak), Unit::Db, Direction::HigherIsBetter);
    let sq = if code == Unit::Code { Unit::CodeSq } else { Unit::Levels8Sq };
    for (n, e) in names.iter().zip(errs) {
        r.push(format!("mse_{n}"), e.mse(), sq, Direction::LowerIsBetter);
    }
    r.push("mse_avg", mse_avg, sq, Direction::LowerIsBetter);
    for (n, e) in names.iter().zip(errs) {
        r.push(format!("maxabs_{n}"), e.max_abs, code, Direction::LowerIsBetter);
    }
}

/// `psnr`: PSNR per native plane (Y, U, V) or channel (R, G, B) plus the
/// sample-weighted average, MSE and max abs error.
pub struct Psnr;

static PSNR: MetricInfo = MetricInfo {
    id: "psnr",
    name: "PSNR",
    kind: MetricKind::FullReference,
    version: 1,
    inputs: &[InputKind::NativePlanes],
    min_size: (1, 1),
    cost_ms_4k: 3.0,
    heavy: false,
    has_map: false,
    direction: Direction::HigherIsBetter,
    headline: &["y", "avg"],
    description: "Peak signal-to-noise ratio per plane and the FFmpeg-style weighted average",
};

impl Metric for Psnr {
    fn info(&self) -> &'static MetricInfo {
        &PSNR
    }

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, _o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        let b = reference(&PSNR, a, b)?;
        let mut r = MetricResult::new(&PSNR);
        match (a.view(), b.view()) {
            (FrameView::Yuv(x), FrameView::Yuv(_)) if a.view().same_layout(b.view()) => {
                let FrameView::Yuv(y) = b.view() else { unreachable!() };
                let (pa, pb) = (x.planes(), y.planes());
                let mut errs = Vec::new();
                for (p, q) in pa.iter().zip(&pb) {
                    cancel.check()?;
                    errs.push(plane_error(p, q).ok_or_else(|| MetricError::Mismatch("plane layout".into()))?);
                }
                let names: &[&str] = if errs.len() == 3 { &["y", "u", "v"] } else { &["y"] };
                push_set(&mut r, names, &errs, x.max_value() as f64, Unit::Code);
            }
            (FrameView::Rgb(x), FrameView::Rgb(y)) => {
                let e = rgb_error(x, y).ok_or_else(|| MetricError::Mismatch("RGB sample types differ".into()))?;
                push_set(&mut r, &["r", "g", "b"], &e, x.max_value() as f64, Unit::Code);
            }
            _ => return Err(MetricError::Mismatch("YUV layouts or kinds differ; use psnr.rgb".into())),
        }
        cancel.check()?;
        Ok(r)
    }
}

/// `psnr.rgb`: PSNR on R, G, B. RGB inputs use the integer path; YUV inputs
/// are converted (matrix and range aware) and measured in 8-bit-equivalent levels.
pub struct PsnrRgb;

static PSNR_RGB: MetricInfo = MetricInfo {
    id: "psnr.rgb",
    name: "PSNR (RGB)",
    kind: MetricKind::FullReference,
    version: 1,
    inputs: &[InputKind::NativePlanes],
    min_size: (1, 1),
    cost_ms_4k: 12.0,
    heavy: false,
    has_map: false,
    direction: Direction::HigherIsBetter,
    headline: &["avg"],
    description: "PSNR on R, G and B (converted from YUV when needed)",
};

impl Metric for PsnrRgb {
    fn info(&self) -> &'static MetricInfo {
        &PSNR_RGB
    }

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        let b = reference(&PSNR_RGB, a, b)?;
        if let (FrameView::Rgb(_), FrameView::Rgb(_)) = (a.view(), b.view()) {
            if a.view().same_layout(b.view()) {
                let mut r = Psnr.compute(a, Some(b), o, cancel)?;
                r.id = PSNR_RGB.id.into();
                return Ok(r);
            }
        }
        let (va, vb) = (*a.view(), *b.view());
        let w = va.width();
        let (sse, mx) = (0..va.height())
            .into_par_iter()
            .with_min_len(4)
            .fold(
                || (RowConverter::new(w), RowConverter::new(w), [0f64; 3], [0f32; 3]),
                |(mut ca, mut cb, mut s, mut m), y| {
                    ca.encoded(&va, y);
                    cb.encoded(&vb, y);
                    for (k, (pa, pb)) in [(&ca.r, &cb.r), (&ca.g, &cb.g), (&ca.b, &cb.b)].into_iter().enumerate() {
                        let mut acc = 0f64;
                        for (p, q) in pa[..w].iter().zip(&pb[..w]) {
                            let d = (p - q) * 255.0;
                            acc += (d * d) as f64;
                            m[k] = m[k].max(d.abs());
                        }
                        s[k] += acc;
                    }
                    (ca, cb, s, m)
                },
            )
            .map(|(_, _, s, m)| (s, m))
            .reduce(|| ([0f64; 3], [0f32; 3]), |a, b| ([a.0[0] + b.0[0], a.0[1] + b.0[1], a.0[2] + b.0[2]], [a.1[0].max(b.1[0]), a.1[1].max(b.1[1]), a.1[2].max(b.1[2])]));
        cancel.check()?;
        let n = (w * va.height()) as u64;
        let errs: [ErrStats; 3] = std::array::from_fn(|k| ErrStats { sse: sse[k], max_abs: mx[k] as f64, count: n });
        let mut r = MetricResult::new(&PSNR_RGB);
        push_set(&mut r, &["r", "g", "b"], &errs, 255.0, Unit::Levels8);
        Ok(r)
    }
}
