//! v2 perceptual metrics wrapped from pure-Rust crates (each behind its own
//! cargo feature, all on by default):
//!
//! - `ssimulacra2`: SSIMULACRA2 via `fast-ssim2` (BSD-2-Clause). Higher is
//!   better, 100 = identical. No map (the crate's maps are not public API).
//! - `butteraugli`: libjxl butteraugli via `butteraugli` (BSD-3-Clause),
//!   max-norm score and 3-norm, with the diffmap.
//! - `flip`: NVIDIA FLIP (LDR) via `flip-rs` (BSD-3-Clause), mean error and
//!   the per-pixel error map.
//!
//! All three take the frame's linear-light RGB (the shared derived image), so
//! YUV, 16-bit and float sources work too. They are heavy: image jobs run
//! them on a 1/4-area preview first. Running inside the analysis pool, their
//! internal rayon work uses that pool. Cancellation is checked before each
//! call (one call of latency).

#[allow(unused_imports)]
use crate::frame::{FrameCtx, InputKind, RgbF32};
#[allow(unused_imports)]
use crate::heatmap::{Colormap, Heatmap, Pool};
#[allow(unused_imports)]
use crate::job::CancelToken;
#[allow(unused_imports)]
use crate::metrics::{reference, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};

/// Interleaved linear RGB as a flat `f32` slice.
#[allow(dead_code)]
fn flat(l: &RgbF32) -> &[f32] {
    l.data.as_flattened()
}

#[cfg(feature = "ssimulacra2")]
mod ssimulacra2 {
    use super::*;
    use fast_ssim2::{compute_ssimulacra2, PixelDescriptor, PixelSlice};

    /// `ssimulacra2`
    pub struct Ssimulacra2;

    pub(crate) static INFO: MetricInfo = MetricInfo {
        id: "ssimulacra2",
        name: "SSIMULACRA2",
        kind: MetricKind::FullReference,
        version: 1,
        inputs: &[InputKind::LinearRgb],
        min_size: (8, 8),
        cost_ms_4k: 1200.0,
        heavy: true,
        has_map: false,
        direction: Direction::HigherIsBetter,
        description: "SSIMULACRA2 perceptual score (100 = identical, 90 very high, 70 high, 50 medium quality)",
    };

    impl Metric for Ssimulacra2 {
        fn info(&self) -> &'static MetricInfo {
            &INFO
        }

        fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, _o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
            let b = reference(&INFO, a, b)?;
            cancel.check()?;
            let (la, lb) = (a.linear(), b.linear());
            fn slice(l: &RgbF32) -> Result<PixelSlice<'_>, MetricError> {
                let bytes: &[u8] = bytemuck::cast_slice(flat(l));
                PixelSlice::new(bytes, l.width as u32, l.height as u32, l.width * 12, PixelDescriptor::RGBF32_LINEAR).map_err(|e| MetricError::NotApplicable(format!("{e:?}")))
            }
            let s = compute_ssimulacra2(&slice(la)?, &slice(lb)?).map_err(|e| MetricError::NotApplicable(e.to_string()))?;
            let mut r = MetricResult::new(&INFO);
            r.push("score", s, Unit::Score, Direction::HigherIsBetter);
            Ok(r)
        }
    }
}

#[cfg(feature = "butteraugli")]
mod butter {
    use super::*;
    use butteraugli::{ButteraugliParams, ButteraugliReference};

    /// `butteraugli`
    pub struct Butteraugli;

    pub(crate) static INFO: MetricInfo = MetricInfo {
        id: "butteraugli",
        name: "Butteraugli",
        kind: MetricKind::FullReference,
        version: 1,
        inputs: &[InputKind::LinearRgb],
        min_size: (8, 8),
        cost_ms_4k: 900.0,
        heavy: true,
        has_map: true,
        direction: Direction::LowerIsBetter,
        description: "Butteraugli (libjxl) max-norm distance (below 1 good, above 2 bad) and 3-norm, with a diffmap",
    };

    impl Metric for Butteraugli {
        fn info(&self) -> &'static MetricInfo {
            &INFO
        }

        fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
            let b = reference(&INFO, a, b)?;
            cancel.check()?;
            let (la, lb) = (a.linear(), b.linear());
            let err = |e: butteraugli::ButteraugliError| MetricError::NotApplicable(e.to_string());
            let params = ButteraugliParams::new().with_compute_diffmap(o.want_map);
            let rf = ButteraugliReference::new_linear(flat(la), la.width, la.height, params).map_err(err)?;
            let res = rf.compare_linear(flat(lb)).map_err(err)?;
            let mut r = MetricResult::new(&INFO);
            r.push("score", res.score, Unit::Score, Direction::LowerIsBetter);
            r.push("pnorm3", res.pnorm_3, Unit::Score, Direction::LowerIsBetter);
            if let Some(m) = res.diffmap {
                let data: Vec<f32> = m.rows().flat_map(|row| row.iter().copied()).collect();
                r.map = Some(Heatmap::new(m.width(), m.height(), data, "Butteraugli").with_unit(Unit::Score).with_range(0.0, 3.0).with_style(Colormap::Magma, false, Pool::Max));
            }
            Ok(r)
        }
    }
}

#[cfg(feature = "flip")]
mod flip {
    use super::*;
    use crate::color::linear_to_srgb;
    use flip_rs::{ldr_flip, RgbImage, DEFAULT_PPD};
    use rayon::prelude::*;

    /// `flip`
    pub struct Flip;

    pub(crate) static INFO: MetricInfo = MetricInfo {
        id: "flip",
        name: "FLIP",
        kind: MetricKind::FullReference,
        version: 1,
        inputs: &[InputKind::LinearRgb],
        min_size: (1, 1),
        cost_ms_4k: 350.0,
        heavy: true,
        has_map: true,
        direction: Direction::LowerIsBetter,
        description: "NVIDIA FLIP (LDR, 67 pixels per degree) mean error 0..1, with the error map",
    };

    /// sRGB-encoded interleaved copy (FLIP LDR reads sRGB values).
    fn encoded(l: &RgbF32) -> Vec<f32> {
        flat(l).par_iter().map(|v| linear_to_srgb(v.clamp(0.0, 1.0))).collect()
    }

    impl Metric for Flip {
        fn info(&self) -> &'static MetricInfo {
            &INFO
        }

        fn compute(&self, a: &FrameCtx<'_>, b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
            let b = reference(&INFO, a, b)?;
            cancel.check()?;
            let (la, lb) = (a.linear(), b.linear());
            let (w, h) = (la.width, la.height);
            let err = |e: flip_rs::FlipError| MetricError::NotApplicable(e.to_string());
            let ra = RgbImage::new(w, h, encoded(la)).map_err(err)?;
            let rb = RgbImage::new(w, h, encoded(lb)).map_err(err)?;
            let map = ldr_flip(&ra, &rb, DEFAULT_PPD).map_err(err)?;
            let st = map.statistics();
            let mut r = MetricResult::new(&INFO);
            r.push("mean", st.mean as f64, Unit::Score, Direction::LowerIsBetter);
            r.push("median", st.weighted_median as f64, Unit::Score, Direction::LowerIsBetter);
            r.push("max", st.max as f64, Unit::Score, Direction::LowerIsBetter);
            if o.want_map {
                r.map = Some(Heatmap::new(w, h, map.into_pixels(), "FLIP").with_unit(Unit::Score).with_range(0.0, 1.0).with_style(Colormap::Magma, false, Pool::Max));
            }
            Ok(r)
        }
    }
}

#[cfg(feature = "butteraugli")]
pub use butter::Butteraugli;
#[cfg(feature = "flip")]
pub use flip::Flip;
#[cfg(feature = "ssimulacra2")]
pub use ssimulacra2::Ssimulacra2;
