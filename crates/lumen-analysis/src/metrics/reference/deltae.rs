//! Delta-E maps between A and B in CIE Lab (D65): CIE76, CIE94 (graphic
//! arts, A is the reference) and CIEDE2000, with mean, median, p95 and max.

use crate::color::{delta_e2000_row, delta_e76_row, delta_e94_row};
use crate::frame::{FrameCtx, InputKind, LabImage};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{reference, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use crate::stats::{Accum, Summary};
use rayon::prelude::*;

const BAND: usize = 8;
const HIST_HI: f32 = 128.0;
const HIST_BINS: usize = 8192;

/// A row kernel: Lab A (L, a, b), Lab B (L, a, b) to Delta-E.
type RowFn = fn(&[f32], &[f32], &[f32], &[f32], &[f32], &[f32], &mut [f32]);

/// Runs `f` over every row in bands; returns the summary and, when asked,
/// the full-resolution map.
fn run(a: &LabImage, b: &LabImage, f: RowFn, want_map: bool, cancel: &CancelToken) -> Result<(Summary, Option<Vec<f32>>), MetricError> {
    let (w, h) = (a.width, a.height);
    let row = |y: usize, out: &mut [f32]| {
        let s = y * w..(y + 1) * w;
        f(&a.l[s.clone()], &a.a[s.clone()], &a.b[s.clone()], &b.l[s.clone()], &b.a[s.clone()], &b.b[s], out);
    };
    let new_acc = || Accum::new(0.0, HIST_HI, HIST_BINS);
    if want_map {
        let mut map = vec![0f32; w * h];
        let acc = map
            .par_chunks_mut(w * BAND)
            .enumerate()
            .map(|(i, m)| {
                let mut acc = new_acc();
                if cancel.is_cancelled() {
                    return acc;
                }
                for (r, out) in m.chunks_mut(w).enumerate() {
                    row(i * BAND + r, out);
                    acc.push(out);
                }
                acc
            })
            .reduce(new_acc, Accum::merge);
        cancel.check()?;
        Ok((acc.finish(), Some(map)))
    } else {
        let acc = (0..h.div_ceil(BAND))
            .into_par_iter()
            .map(|i| {
                let mut acc = new_acc();
                if cancel.is_cancelled() {
                    return acc;
                }
                let mut out = vec![0f32; w];
                for y in i * BAND..((i + 1) * BAND).min(h) {
                    row(y, &mut out);
                    acc.push(&out);
                }
                acc
            })
            .reduce(new_acc, Accum::merge);
        cancel.check()?;
        Ok((acc.finish(), None))
    }
}

fn finish(info: &'static MetricInfo, s: Summary, map: Option<Vec<f32>>, w: usize, h: usize, label: &str) -> MetricResult {
    let mut r = MetricResult::new(info);
    let d = Direction::LowerIsBetter;
    r.push("mean", s.mean, Unit::DeltaE, d);
    r.push("median", s.percentile(50.0), Unit::DeltaE, d);
    r.push("p95", s.percentile(95.0), Unit::DeltaE, d);
    r.push("max", s.max as f64, Unit::DeltaE, d);
    r.push("above_jnd_pct", 100.0 * (1.0 - s.fraction_between(0.0, 2.3)), Unit::Percent, d);
    r.map = map.map(|m| Heatmap::new(w, h, m, label).with_unit(Unit::DeltaE).with_range(0.0, 10.0).with_min_span(5.0).with_style(Colormap::Magma, false, Pool::Max));
    r
}

macro_rules! delta_e_metric {
    ($ty:ident, $info:ident, $id:literal, $name:literal, $cost:literal, $version:literal, $desc:literal, $f:expr) => {
        #[doc = $desc]
        pub struct $ty;

        static $info: MetricInfo = MetricInfo {
            id: $id,
            name: $name,
            kind: MetricKind::FullReference,
            version: $version,
            inputs: &[InputKind::Lab],
            min_size: (1, 1),
            cost_ms_4k: $cost,
            heavy: false,
            has_map: true,
            direction: Direction::LowerIsBetter,
            headline: &["mean"],
            description: $desc,
        };

        impl Metric for $ty {
            fn info(&self) -> &'static MetricInfo {
                &$info
            }

            fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
                let b = reference(&$info, a, b)?;
                let (la, lb) = (a.lab(), b.lab());
                let (s, map) = run(la, lb, $f, o.want_map, cancel)?;
                Ok(finish(&$info, s, map, la.width, la.height, $name))
            }
        }
    };
}

delta_e_metric!(DeltaE76, DE76, "deltae.76", "Delta-E 76", 10.0, 1, "CIE76 color difference (Euclidean in Lab)", delta_e76_row);
delta_e_metric!(DeltaE94, DE94, "deltae.94", "Delta-E 94", 20.0, 1, "CIE94 color difference (graphic arts weights, A is the reference)", delta_e94_row);
delta_e_metric!(DeltaE2000, DE2000, "deltae.2000", "Delta-E 2000", 70.0, 1, "CIEDE2000 color difference (Sharma 2005)", delta_e2000_row);
