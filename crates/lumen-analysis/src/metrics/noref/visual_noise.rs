//! Visual noise on the flat areas of a real scene (CPIQ-inspired).
//!
//! The picture is cut into tiles (`MetricOptions::tile`, 64 px). In each
//! tile a plane `c + p x + q y` is fitted to L*, a* and b* and removed, so a
//! smooth gradient such as a sky does not count as noise; the standard
//! deviations of what is left are the tile's noise. A tile is *flat* when
//! fewer than 1% of its pixels are clipped (L* below 1 or above 99), its L*
//! residual is at most twice that of the cleanest tiles (the 10th
//! percentile, never less than [`FLAT_FLOOR`]) and at most [`FLAT_MAX`] L*,
//! above which it is texture, not noise. The variances are pooled over the
//! flat tiles (weighted by pixels).
//!
//! Values are in CIELAB units: `l` is the standard deviation of L*, `a` and
//! `b` those of a* and b*, `chroma` = sqrt(var a* + var b*) and the headline
//! `index` = sqrt(var L* + 0.852 var a* + 0.323 var b*), the a*/b* weights of
//! the CPIQ / ISO 15739 visual noise formulas. There is no contrast
//! sensitivity filtering and no viewing-distance model, so the index ranks
//! noise the way those formulas do but is not a certified CPIQ value; use it
//! to compare shots of the same scene.
//!
//! A picture without flat tiles (all texture) is measured on its cleanest
//! tenth instead; `flat_pct` is then 0 and a note says the value includes
//! texture. The map holds each measured tile's index (no value elsewhere).

use crate::frame::{FrameCtx, InputKind, LabImage};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use rayon::prelude::*;

/// CPIQ / ISO 15739 visual noise weight of var(a*).
pub const W_A: f64 = 0.852;
/// CPIQ / ISO 15739 visual noise weight of var(b*).
pub const W_B: f64 = 0.323;
/// L* residual (std) above which a tile is texture, whatever the rest.
pub const FLAT_MAX: f64 = 6.0;
/// Smallest flatness threshold (std of L*): noise-free renders still have
/// quantization steps.
pub const FLAT_FLOOR: f64 = 0.3;
/// Most clipped pixels a flat tile may have, as a fraction.
const MAX_CLIPPED: f64 = 0.01;
/// Smallest tile side measured (edge slivers are skipped).
const MIN_SIDE: usize = 8;

/// `visual_noise`
pub struct VisualNoise;

static VISUAL_NOISE: MetricInfo = MetricInfo {
    id: "visual_noise",
    name: "Visual noise",
    kind: MetricKind::NoReference,
    version: 1,
    inputs: &[InputKind::Lab],
    min_size: (16, 16),
    cost_ms_4k: 12.0,
    heavy: false,
    has_map: true,
    direction: Direction::LowerIsBetter,
    headline: &["index"],
    description: "Noise of L*, a* and b* on the flat areas (gradients removed), combined with CPIQ-style weights, with a map of the measured tiles",
};

/// Residual variances (L*, a*, b*) of one tile after removing a plane, its
/// pixel count and how many of its pixels are clipped.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TileNoise {
    pub var: [f64; 3],
    pub n: u64,
    pub clipped: u64,
}

impl TileNoise {
    fn sd_l(&self) -> f64 {
        self.var[0].sqrt()
    }
}

/// Residual variance of `plane` over the tile x0..x1, y0..y1 after a least
/// squares plane fit (coordinates centered, so the fit terms separate).
fn plane_residual_var(plane: &[f32], w: usize, x0: usize, x1: usize, y0: usize, y1: usize) -> f64 {
    let (tw, th) = (x1 - x0, y1 - y0);
    let n = (tw * th) as f64;
    let mut sum = 0f64;
    for y in y0..y1 {
        sum += plane[y * w + x0..y * w + x1].iter().map(|v| *v as f64).sum::<f64>();
    }
    let mean = (sum / n) as f32;
    let cx = (tw as f32 - 1.0) / 2.0;
    let cy = (th as f32 - 1.0) / 2.0;
    let (mut s2, mut sxv, mut syv) = (0f64, 0f64, 0f64);
    for y in y0..y1 {
        // One row is short: f32 sums are exact enough, rows add up in f64.
        let (mut r2, mut rx, mut rs) = (0f32, 0f32, 0f32);
        for (i, v) in plane[y * w + x0..y * w + x1].iter().enumerate() {
            let d = *v - mean;
            r2 += d * d;
            rx += (i as f32 - cx) * d;
            rs += d;
        }
        s2 += r2 as f64;
        sxv += rx as f64;
        // Every pixel of the row has the same y offset.
        syv += (((y - y0) as f32 - cy) * rs) as f64;
    }
    // Sum of squared centered coordinates over the rectangle.
    let sxx = th as f64 * tw as f64 * ((tw * tw) as f64 - 1.0) / 12.0;
    let syy = tw as f64 * th as f64 * ((th * th) as f64 - 1.0) / 12.0;
    let mut res = s2;
    if sxx > 0.0 {
        res -= sxv * sxv / sxx;
    }
    if syy > 0.0 {
        res -= syv * syv / syy;
    }
    (res / (n - 3.0).max(1.0)).max(0.0)
}

/// Noise of every `tile` x `tile` tile of `lab`, row-major (`None` for
/// slivers under [`MIN_SIDE`]).
pub(crate) fn tile_noise(lab: &LabImage, tile: usize) -> (usize, usize, Vec<Option<TileNoise>>) {
    let (w, h) = (lab.width, lab.height);
    let tile = tile.max(16);
    let (tw, th) = (w.div_ceil(tile), h.div_ceil(tile));
    let rows: Vec<Vec<Option<TileNoise>>> = (0..th)
        .into_par_iter()
        .map(|ty| {
            let (y0, y1) = (ty * tile, ((ty + 1) * tile).min(h));
            (0..tw)
                .map(|tx| {
                    let (x0, x1) = (tx * tile, ((tx + 1) * tile).min(w));
                    if x1 - x0 < MIN_SIDE || y1 - y0 < MIN_SIDE {
                        return None;
                    }
                    let mut clipped = 0u64;
                    for y in y0..y1 {
                        clipped += lab.l[y * w + x0..y * w + x1].iter().filter(|l| **l < 1.0 || **l > 99.0).count() as u64;
                    }
                    let var = [&lab.l, &lab.a, &lab.b].map(|p| plane_residual_var(p, w, x0, x1, y0, y1));
                    Some(TileNoise { var, n: ((x1 - x0) * (y1 - y0)) as u64, clipped })
                })
                .collect()
        })
        .collect();
    (tw, th, rows.into_iter().flatten().collect())
}

/// Which tiles are measured: `(selected, flat)`. Flat tiles follow the
/// module rules; without any, the cleanest tenth is selected and `flat` is
/// false.
pub(crate) fn select_flat(tiles: &[Option<TileNoise>]) -> (Vec<bool>, bool) {
    let ok = |t: &TileNoise| (t.clipped as f64) <= MAX_CLIPPED * t.n as f64;
    let mut sds: Vec<f64> = tiles.iter().flatten().filter(|t| ok(*t)).map(|t| t.sd_l()).collect();
    if sds.is_empty() {
        return (vec![false; tiles.len()], false);
    }
    sds.sort_by(|a, b| a.total_cmp(b));
    let p10 = sds[(sds.len() - 1) / 10];
    let thr = (2.0 * p10).max(FLAT_FLOOR).min(FLAT_MAX);
    let pick = |limit: f64| tiles.iter().map(|t| t.is_some_and(|t| ok(&t) && t.sd_l() <= limit)).collect::<Vec<bool>>();
    let flat = pick(thr);
    if flat.iter().any(|f| *f) {
        (flat, true)
    } else {
        (pick(p10), false)
    }
}

impl Metric for VisualNoise {
    fn info(&self) -> &'static MetricInfo {
        &VISUAL_NOISE
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&VISUAL_NOISE, a)?;
        let lab = a.lab();
        let tile = (o.tile as usize).max(16);
        let (tw, th, tiles) = tile_noise(lab, tile);
        cancel.check()?;
        let (selected, flat) = select_flat(&tiles);
        let (mut v, mut n) = ([0f64; 3], 0u64);
        for (t, s) in tiles.iter().zip(&selected) {
            if let (Some(t), true) = (t, *s) {
                for k in 0..3 {
                    v[k] += t.var[k] * t.n as f64;
                }
                n += t.n;
            }
        }
        let var = if n == 0 { [f64::NAN; 3] } else { v.map(|x| x / n as f64) };
        let index = (var[0] + W_A * var[1] + W_B * var[2]).sqrt();
        let total = (lab.width * lab.height) as f64;
        let flat_pct = if flat { 100.0 * n as f64 / total } else { 0.0 };
        let lower = Direction::LowerIsBetter;
        let mut r = MetricResult::new(&VISUAL_NOISE);
        r.push("index", index, Unit::DeltaE, lower);
        r.push("l", var[0].sqrt(), Unit::LStar, lower);
        r.push("chroma", (var[1] + var[2]).sqrt(), Unit::DeltaE, lower);
        r.push("a", var[1].sqrt(), Unit::Score, lower);
        r.push("b", var[2].sqrt(), Unit::Score, lower);
        r.push("flat_pct", flat_pct, Unit::Percent, Direction::Neutral);
        if n == 0 {
            r.notes.push("no measurable area: every tile is clipped or too small".into());
        } else if flat {
            r.notes.push(format!("measured on the flat areas, {flat_pct:.1}% of the picture, gradients removed"));
        } else {
            r.notes.push("no flat areas: measured on the least textured tenth of the picture, so texture adds to the value".into());
        }
        if o.want_map {
            let data = tiles
                .iter()
                .zip(&selected)
                .map(|(t, s)| match (t, *s) {
                    (Some(t), true) => (t.var[0] + W_A * t.var[1] + W_B * t.var[2]).sqrt() as f32,
                    _ => f32::NAN,
                })
                .collect();
            let c = tile as f32;
            r.map = Some(Heatmap::new(tw, th, data, "Visual noise on flat areas").with_unit(Unit::DeltaE).with_cell(c, c).with_range(0.0, 5.0).with_min_span(2.0).with_style(Colormap::Magma, false, Pool::Max));
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::srgb8_to_lab;
    use crate::frame::OwnedFrame;
    use crate::metrics::noref::test_util::{rgb_frame, run, Rng};

    fn level(v: f64) -> u8 {
        v.round().clamp(0.0, 255.0) as u8
    }

    /// Gray `base` plus noise: `f(rng)` gives the (R, G, B) offsets.
    fn noisy(w: usize, h: usize, base: f64, seed: u64, f: impl Fn(&mut Rng) -> [f64; 3]) -> OwnedFrame {
        let mut rng = Rng::new(seed);
        rgb_frame(w, h, |_, _| f(&mut rng).map(|n| level(base + n)))
    }

    #[test]
    fn gray_noise_of_known_sigma() {
        let (w, h, sigma) = (256, 256, 4.0);
        let f = noisy(w, h, 128.0, 3, |r| [sigma * r.normal(); 3]);
        let r = run(&VisualNoise, &f.view(), &MetricOptions::default());
        // L* per 8-bit level around 128; rounding adds 1/12 level^2.
        let slope = (srgb8_to_lab([136; 3])[0] - srgb8_to_lab([120; 3])[0]) / 16.0;
        let want = slope * (sigma * sigma + 1.0 / 12.0).sqrt();
        let l = r.get("l").unwrap();
        assert!((l - want).abs() < 0.05 * want, "L* std {l}, expected {want}");
        // Equal R, G and B: no color noise, and every tile is flat.
        assert!(r.get("chroma").unwrap() < 0.05, "{:?}", r.values);
        assert!((r.get("index").unwrap() - l).abs() < 0.02);
        assert_eq!(r.get("flat_pct"), Some(100.0));
        let m = r.map.unwrap();
        assert_eq!((m.width, m.height), (4, 4));
        assert!(m.data.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn colored_noise_is_chroma_not_luminance() {
        let (w, h, sigma) = (192, 192, 6.0);
        let o = MetricOptions::default();
        let gray = run(&VisualNoise, &noisy(w, h, 128.0, 5, |r| [sigma * r.normal(); 3]).view(), &o);
        // Red against green at constant luminance (BT.709 weights, equal
        // slopes at mid gray): the color moves, the brightness hardly.
        let color = run(
            &VisualNoise,
            &noisy(w, h, 128.0, 5, |r| {
                let n = sigma * r.normal();
                [n, -n * 0.2126 / 0.7152, 0.0]
            })
            .view(),
            &o,
        );
        let (gl, gc) = (gray.get("l").unwrap(), gray.get("chroma").unwrap());
        let (cl, cc) = (color.get("l").unwrap(), color.get("chroma").unwrap());
        assert!(cc > 1.5 && cc > 10.0 * gc, "chroma: color {cc}, gray {gc}");
        assert!(cl < 0.3 * gl, "L*: color {cl}, gray {gl}");
        assert!(color.get("a").unwrap() > color.get("b").unwrap());
        assert!(color.get("index").unwrap() > color.get("l").unwrap());
    }

    #[test]
    fn gradients_and_texture_are_not_noise() {
        let (w, h) = (256, 128);
        let o = MetricOptions::default();
        // A smooth ramp with light noise: the plane fit removes the ramp.
        let mut rng = Rng::new(9);
        let ramp = rgb_frame(w, h, |x, y| [level(60.0 + 120.0 * x as f64 / w as f64 + 20.0 * y as f64 / h as f64 + 2.0 * rng.normal()); 3]);
        let ramp = run(&VisualNoise, &ramp.view(), &o);
        assert!(ramp.get("l").unwrap() < 1.2, "{:?}", ramp.values);
        assert!(ramp.get("flat_pct").unwrap() > 90.0);
        // Left half flat and clean, right half a strong pattern: only the
        // flat half is measured.
        let pattern = |x: usize, y: usize| if (x / 3 + y / 3) % 2 == 0 { 40u8 } else { 200 };
        let half = rgb_frame(w, h, |x, y| [if x < w / 2 { 128 } else { pattern(x, y) }; 3]);
        let half = run(&VisualNoise, &half.view(), &o);
        assert!(half.get("l").unwrap() < 0.01, "{:?}", half.values);
        assert!((half.get("flat_pct").unwrap() - 50.0).abs() < 1e-9);
        // All texture: still a value, flagged as not flat.
        let busy = run(&VisualNoise, &rgb_frame(w, h, |x, y| [pattern(x, y); 3]).view(), &o);
        assert_eq!(busy.get("flat_pct"), Some(0.0));
        assert!(busy.get("index").unwrap().is_finite());
        assert!(busy.notes[0].contains("no flat areas"), "{:?}", busy.notes);
    }

    #[test]
    fn clipped_tiles_are_skipped() {
        let (w, h) = (128, 64);
        // Left half blown to white (no noise), right half noisy gray.
        let mut rng = Rng::new(4);
        let f = rgb_frame(w, h, |x, _| [if x < w / 2 { 255 } else { level(128.0 + 3.0 * rng.normal()) }; 3]);
        let r = run(&VisualNoise, &f.view(), &MetricOptions::default());
        assert!(r.get("l").unwrap() > 0.8, "the white half must not pull it to zero: {:?}", r.values);
        assert!((r.get("flat_pct").unwrap() - 50.0).abs() < 1e-9);
    }
}
