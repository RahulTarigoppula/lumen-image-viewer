//! Difference maps. `diff.abs`: per-pixel absolute difference in 8-bit
//! levels (RGB: the largest of R, G, B; YUV: luma). `diff.signed`: luma
//! B - A in 8-bit levels (bias and spread), shown with a diverging colormap.

use crate::frame::{FrameCtx, FrameView, InputKind, Plane, PlaneView, RgbData, RowConverter, Sample};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{reference, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use crate::stats::Accum;
use rayon::prelude::*;

const BAND: usize = 8;

/// Fills `out` with |A - B| for row `y` in 8-bit levels.
struct AbsRow<'v> {
    a: FrameView<'v>,
    b: FrameView<'v>,
}

impl AbsRow<'_> {
    fn row(&self, y: usize, out: &mut [f32], conv: &mut Option<(RowConverter, RowConverter)>) {
        let w = out.len();
        match (&self.a, &self.b) {
            (FrameView::Rgb(p), FrameView::Rgb(q)) => {
                if let (RgbData::U8(da), RgbData::U8(db)) = (p.data, q.data) {
                    let ra = &da[y * p.stride..];
                    let rb = &db[y * q.stride..];
                    match (p.channels, q.channels) {
                        (4, 4) => rgb8_row::<4>(&ra[..w * 4], &rb[..w * 4], out),
                        (3, 3) => rgb8_row::<3>(&ra[..w * 3], &rb[..w * 3], out),
                        (ca, cb) => {
                            for (x, o) in out.iter_mut().enumerate() {
                                let (i, j) = (x * ca, x * cb);
                                *o = ra[i].abs_diff(rb[j]).max(ra[i + 1].abs_diff(rb[j + 1])).max(ra[i + 2].abs_diff(rb[j + 2])) as f32;
                            }
                        }
                    }
                    return;
                }
            }
            (FrameView::Yuv(p), FrameView::Yuv(q)) if p.y.is_u16() == q.y.is_u16() => {
                let s = 255.0 / p.max_value() as f32;
                match (p.y, q.y) {
                    (PlaneView::U8(pa), PlaneView::U8(pb)) => plane_row(&pa, &pb, y, s, out),
                    (PlaneView::U16(pa), PlaneView::U16(pb)) => plane_row(&pa, &pb, y, s, out),
                    _ => unreachable!(),
                }
                return;
            }
            _ => {}
        }
        let (ca, cb) = conv.get_or_insert_with(|| (RowConverter::new(w), RowConverter::new(w)));
        ca.encoded(&self.a, y);
        cb.encoded(&self.b, y);
        for x in 0..w {
            let d = (ca.r[x] - cb.r[x]).abs().max((ca.g[x] - cb.g[x]).abs()).max((ca.b[x] - cb.b[x]).abs());
            out[x] = d * 255.0;
        }
    }
}

/// Largest per-channel |A - B| of interleaved 8-bit rows with `CH` channels.
#[inline]
fn rgb8_row<const CH: usize>(a: &[u8], b: &[u8], out: &mut [f32]) {
    for ((pa, pb), o) in a.as_chunks::<CH>().0.iter().zip(b.as_chunks::<CH>().0).zip(out.iter_mut()) {
        *o = pa[0].abs_diff(pb[0]).max(pa[1].abs_diff(pb[1])).max(pa[2].abs_diff(pb[2])) as f32;
    }
}

fn plane_row<T: Sample>(a: &Plane<'_, T>, b: &Plane<'_, T>, y: usize, s: f32, out: &mut [f32]) {
    for ((o, p), q) in out.iter_mut().zip(a.row(y)).zip(b.row(y)) {
        *o = (p.to_i32() - q.to_i32()).unsigned_abs() as f32 * s;
    }
}

/// `diff.abs`: absolute difference map and statistics.
pub struct DiffAbs;

static DIFF_ABS: MetricInfo = MetricInfo {
    id: "diff.abs",
    name: "Abs difference",
    kind: MetricKind::FullReference,
    version: 1,
    inputs: &[InputKind::NativePlanes],
    min_size: (1, 1),
    cost_ms_4k: 4.0,
    heavy: false,
    has_map: true,
    direction: Direction::LowerIsBetter,
    description: "Absolute difference in 8-bit levels (RGB: largest channel; YUV: luma)",
};

impl Metric for DiffAbs {
    fn info(&self) -> &'static MetricInfo {
        &DIFF_ABS
    }

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        let b = reference(&DIFF_ABS, a, b)?;
        let (w, h) = (a.width(), a.height());
        let src = AbsRow { a: *a.view(), b: *b.view() };
        let thr = o.diff_threshold;
        let new = || (Accum::new(0.0, 256.0, 4096), 0u64);
        let merge = |x: (Accum, u64), y: (Accum, u64)| (x.0.merge(y.0), x.1 + y.1);
        // `buf` holds the band's rows: the map itself, or scratch without a map.
        let band = |i: usize, buf: &mut [f32]| {
            let mut acc = new();
            if cancel.is_cancelled() {
                return acc;
            }
            let mut conv = None;
            for (r, out) in buf.chunks_mut(w).enumerate() {
                src.row(i * BAND + r, out, &mut conv);
                acc.0.push(out);
                acc.1 += out.iter().filter(|v| **v > thr).count() as u64;
            }
            acc
        };
        let (acc, map) = if o.want_map {
            let mut map = vec![0f32; w * h];
            let acc = map.par_chunks_mut(w * BAND).enumerate().map(|(i, m)| band(i, m)).reduce(new, merge);
            (acc, Some(map))
        } else {
            let acc = (0..h.div_ceil(BAND))
                .into_par_iter()
                .map(|i| {
                    let rows = ((i + 1) * BAND).min(h) - i * BAND;
                    band(i, &mut vec![0f32; rows * w])
                })
                .reduce(new, merge);
            (acc, None)
        };
        cancel.check()?;
        let (s, changed) = (acc.0.finish(), acc.1);
        let mut r = MetricResult::new(&DIFF_ABS);
        let d = Direction::LowerIsBetter;
        r.push("mean", s.mean, Unit::Levels8, d);
        r.push("p99", s.percentile(99.0), Unit::Levels8, d);
        r.push("max", s.max as f64, Unit::Levels8, d);
        r.push("changed_pct", 100.0 * changed as f64 / (w * h) as f64, Unit::Percent, d);
        r.map = map.map(|m| Heatmap::new(w, h, m, "Abs difference").with_unit(Unit::Levels8).with_range(0.0, 32.0).with_style(Colormap::Magma, false, Pool::Max));
        Ok(r)
    }
}

/// `diff.signed`: luma B - A.
pub struct DiffSigned;

static DIFF_SIGNED: MetricInfo = MetricInfo {
    id: "diff.signed",
    name: "Signed difference",
    kind: MetricKind::FullReference,
    version: 1,
    inputs: &[InputKind::Luma],
    min_size: (1, 1),
    cost_ms_4k: 4.0,
    heavy: false,
    has_map: true,
    direction: Direction::Neutral,
    description: "Luma B minus A in 8-bit levels: bias (mean) and spread",
};

impl Metric for DiffSigned {
    fn info(&self) -> &'static MetricInfo {
        &DIFF_SIGNED
    }

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        let b = reference(&DIFF_SIGNED, a, b)?;
        let (la, lb) = (a.luma(), b.luma());
        let (w, h) = (la.width, la.height);
        let mut map = if o.want_map { vec![0f32; w * h] } else { Vec::new() };
        let stats = |ra: &[f32], rb: &[f32], out: Option<&mut [f32]>| {
            let (mut s, mut s2, mut mn, mut mx) = (0f64, 0f64, f32::INFINITY, f32::NEG_INFINITY);
            let mut buf = [0f32; 256];
            let mut out = out;
            for (k, (ca, cb)) in ra.chunks(256).zip(rb.chunks(256)).enumerate() {
                let n = ca.len();
                for i in 0..n {
                    buf[i] = (cb[i] - ca[i]) * 255.0;
                }
                // Eight lanes so the sums vectorize.
                let (mut ls, mut ls2) = ([0f32; 8], [0f32; 8]);
                let full = n / 8 * 8;
                for c in buf[..full].as_chunks::<8>().0 {
                    for j in 0..8 {
                        ls[j] += c[j];
                        ls2[j] += c[j] * c[j];
                    }
                }
                for &d in &buf[full..n] {
                    ls[0] += d;
                    ls2[0] += d * d;
                }
                for &d in &buf[..n] {
                    mn = mn.min(d);
                    mx = mx.max(d);
                }
                s += ls.iter().map(|v| *v as f64).sum::<f64>();
                s2 += ls2.iter().map(|v| *v as f64).sum::<f64>();
                if let Some(o) = out.as_deref_mut() {
                    o[k * 256..k * 256 + n].copy_from_slice(&buf[..n]);
                }
            }
            (s, s2, mn, mx)
        };
        let red = |a: (f64, f64, f32, f32), b: (f64, f64, f32, f32)| (a.0 + b.0, a.1 + b.1, a.2.min(b.2), a.3.max(b.3));
        let id = || (0.0, 0.0, f32::INFINITY, f32::NEG_INFINITY);
        let tot = if o.want_map {
            map.par_chunks_mut(w).enumerate().map(|(y, out)| stats(la.row(y), lb.row(y), Some(out))).reduce(id, red)
        } else {
            (0..h).into_par_iter().map(|y| stats(la.row(y), lb.row(y), None)).reduce(id, red)
        };
        cancel.check()?;
        let n = (w * h) as f64;
        let mean = tot.0 / n;
        let var = (tot.1 / n - mean * mean).max(0.0);
        let mut r = MetricResult::new(&DIFF_SIGNED);
        r.push("mean", mean, Unit::Levels8, Direction::Neutral);
        r.push("std", var.sqrt(), Unit::Levels8, Direction::LowerIsBetter);
        r.push("min", tot.2 as f64, Unit::Levels8, Direction::Neutral);
        r.push("max", tot.3 as f64, Unit::Levels8, Direction::Neutral);
        if o.want_map {
            r.map = Some(Heatmap::new(w, h, map, "Signed difference (B - A)").with_unit(Unit::Levels8).with_range(-16.0, 16.0).with_style(Colormap::RdBu, false, Pool::MaxAbs));
        }
        Ok(r)
    }
}
