//! Sharpness: variance of the 3x3 Laplacian (Pech-Pacheco 2000) and
//! Tenengrad, the mean squared Sobel gradient magnitude above a threshold
//! (Krotkov 1987), both on luma in 8-bit levels with per-tile maps.

use super::{sobel, tile_stencil};
use crate::frame::{FrameCtx, InputKind};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};

/// `sharpness.laplacian`
pub struct LaplacianVar;

static LAPLACIAN: MetricInfo = MetricInfo {
    id: "sharpness.laplacian",
    name: "Sharpness (Laplacian variance)",
    kind: MetricKind::NoReference,
    version: 1,
    inputs: &[InputKind::Luma],
    min_size: (3, 3),
    cost_ms_4k: 5.0,
    heavy: false,
    has_map: true,
    direction: Direction::HigherIsBetter,
    description: "Variance of the 3x3 Laplacian of luma (8-bit levels squared), with a tile map",
};

impl Metric for LaplacianVar {
    fn info(&self) -> &'static MetricInfo {
        &LAPLACIAN
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&LAPLACIAN, a)?;
        let l = a.luma();
        let tiles = tile_stencil(l, o.tile as usize, |up, mid, dn, out| {
            let n = out.len();
            for x in 1..=n {
                out[x - 1] = (up[x] + dn[x] + mid[x - 1] + mid[x + 1] - 4.0 * mid[x]) * 255.0;
            }
        });
        cancel.check()?;
        let mut r = MetricResult::new(&LAPLACIAN);
        r.push("var", tiles.total().var(), Unit::Levels8Sq, Direction::HigherIsBetter);
        if o.want_map {
            r.map = Some(tiles.map(|t| t.var(), "Laplacian variance", Unit::Levels8Sq));
        }
        Ok(r)
    }
}

/// `sharpness.tenengrad`
pub struct Tenengrad;

static TENENGRAD: MetricInfo = MetricInfo {
    id: "sharpness.tenengrad",
    name: "Sharpness (Tenengrad)",
    kind: MetricKind::NoReference,
    version: 1,
    inputs: &[InputKind::Luma],
    min_size: (3, 3),
    cost_ms_4k: 10.0,
    heavy: false,
    has_map: true,
    direction: Direction::HigherIsBetter,
    description: "Mean squared Sobel gradient magnitude above the threshold (8-bit levels squared), with a tile map",
};

impl Metric for Tenengrad {
    fn info(&self) -> &'static MetricInfo {
        &TENENGRAD
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&TENENGRAD, a)?;
        let l = a.luma();
        let t2 = o.tenengrad_threshold * o.tenengrad_threshold;
        let tiles = tile_stencil(l, o.tile as usize, |up, mid, dn, out| {
            let n = out.len();
            for x in 1..=n {
                let (gx, gy) = sobel(up, mid, dn, x);
                let g2 = (gx * gx + gy * gy) * (255.0 * 255.0);
                out[x - 1] = if g2 > t2 { g2 } else { f32::NAN };
            }
        });
        cancel.check()?;
        let total = tiles.total();
        let interior = (l.width.saturating_sub(2) * l.height.saturating_sub(2)).max(1) as f64;
        let mut r = MetricResult::new(&TENENGRAD);
        r.push("mean_g2", if total.n == 0 { 0.0 } else { total.mean() }, Unit::Levels8Sq, Direction::HigherIsBetter);
        r.push("edge_pct", 100.0 * total.n as f64 / interior, Unit::Percent, Direction::Neutral);
        if o.want_map {
            r.map = Some(tiles.map(|t| if t.n == 0 { 0.0 } else { t.mean() }, "Tenengrad", Unit::Levels8Sq));
        }
        Ok(r)
    }
}
