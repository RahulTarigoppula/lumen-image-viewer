//! MS-SSIM (Wang, Simoncelli, Bovik 2003): 5 scales, Gaussian SSIM kernel at
//! each scale, 2x2 average downsampling between scales, Wang weights;
//! negative terms are clamped to 0 before the power (as libvmaf and TF do).

use super::ssim::gauss_ssim;
use crate::frame::{FrameCtx, InputKind};
use crate::job::CancelToken;
use crate::metrics::{check_size, reference, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use rayon::prelude::*;

/// Wang 2003 exponents for scales 1..5.
pub const WEIGHTS: [f64; 5] = [0.0448, 0.2856, 0.3001, 0.2363, 0.1333];

/// 2x2 box average, dropping an odd last row/column.
pub(crate) fn half(d: &[f32], w: usize, h: usize) -> (Vec<f32>, usize, usize) {
    let (w2, h2) = (w / 2, h / 2);
    let mut out = vec![0f32; w2 * h2];
    out.par_chunks_mut(w2.max(1)).enumerate().for_each(|(y, o)| {
        let r0 = &d[2 * y * w..];
        let r1 = &d[(2 * y + 1) * w..];
        for (x, v) in o.iter_mut().enumerate() {
            *v = 0.25 * (r0[2 * x] + r0[2 * x + 1] + r1[2 * x] + r1[2 * x + 1]);
        }
    });
    (out, w2, h2)
}

/// MS-SSIM of two packed `f32` images (dynamic range `peak`) and the
/// per-scale (cs, ssim) values.
pub fn ms_ssim(a: &[f32], b: &[f32], w: usize, h: usize, peak: f32, cancel: &CancelToken) -> Result<(f64, Vec<(f64, f64)>), MetricError> {
    let mut scales = Vec::with_capacity(5);
    let o = gauss_ssim(a, b, w, h, peak, false, cancel)?;
    scales.push((o.cs, o.ssim));
    let (mut ca, mut cw, mut ch) = half(a, w, h);
    let (mut cb, ..) = half(b, w, h);
    for j in 1..5 {
        let o = gauss_ssim(&ca, &cb, cw, ch, peak, false, cancel)?;
        scales.push((o.cs, o.ssim));
        if j < 4 {
            let (na, nw, nh) = half(&ca, cw, ch);
            let (nb, ..) = half(&cb, cw, ch);
            (ca, cb, cw, ch) = (na, nb, nw, nh);
        }
    }
    let mut v = 1.0;
    for (j, (cs, ss)) in scales.iter().enumerate() {
        let term = if j < 4 { *cs } else { *ss };
        v *= term.max(0.0).powf(WEIGHTS[j]);
    }
    Ok((v, scales))
}

/// `ms_ssim`: multi-scale SSIM on luma.
pub struct MsSsim;

static MS_SSIM: MetricInfo = MetricInfo {
    id: "ms_ssim",
    name: "MS-SSIM",
    kind: MetricKind::FullReference,
    version: 1,
    inputs: &[InputKind::Luma],
    min_size: (176, 176),
    cost_ms_4k: 60.0,
    heavy: false,
    has_map: false,
    direction: Direction::HigherIsBetter,
    headline: &["y"],
    description: "Multi-scale SSIM, 5 scales with Wang 2003 weights",
};

impl Metric for MsSsim {
    fn info(&self) -> &'static MetricInfo {
        &MS_SSIM
    }

    fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, _o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        let b = reference(&MS_SSIM, a, b)?;
        check_size(&MS_SSIM, b)?;
        let (la, lb) = (a.luma(), b.luma());
        let (v, scales) = ms_ssim(&la.data, &lb.data, la.width, la.height, 1.0, cancel)?;
        let mut r = MetricResult::new(&MS_SSIM);
        r.push("y", v, Unit::Index, Direction::HigherIsBetter);
        for (j, (cs, _)) in scales.iter().enumerate().take(4) {
            r.push(format!("cs{}", j + 1), *cs, Unit::Index, Direction::HigherIsBetter);
        }
        r.push("ssim5", scales[4].1, Unit::Index, Direction::HigherIsBetter);
        Ok(r)
    }
}
