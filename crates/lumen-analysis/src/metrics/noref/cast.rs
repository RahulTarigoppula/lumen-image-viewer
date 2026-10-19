//! Color cast: mean a*, b* over near-neutral pixels (chroma < 10, 20 < L* < 90),
//! falling back to gray world (all pixels) when fewer than 2% are near
//! neutral. Magnitude is the Delta-E 76 distance from neutral.

use crate::frame::{FrameCtx, InputKind};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use rayon::prelude::*;

const NEUTRAL_CHROMA: f32 = 10.0;
const MIN_NEUTRAL: f64 = 0.02;

/// `color_cast`
pub struct ColorCast;

static CAST: MetricInfo = MetricInfo {
    id: "color_cast",
    name: "Color cast",
    kind: MetricKind::NoReference,
    version: 1,
    inputs: &[InputKind::Lab],
    min_size: (1, 1),
    cost_ms_4k: 3.0,
    heavy: false,
    has_map: false,
    direction: Direction::LowerIsBetter,
    description: "Mean a*/b* of near-neutral pixels (gray-world fallback), magnitude and hue",
};

/// Words for a cast direction (hue angle of the a*/b* vector, degrees).
pub fn describe(hue: f64) -> &'static str {
    let h = hue.rem_euclid(360.0);
    match h {
        h if !(22.5..337.5).contains(&h) => "magenta-red",
        h if h < 67.5 => "warm (orange)",
        h if h < 112.5 => "yellow",
        h if h < 157.5 => "yellow-green",
        h if h < 202.5 => "green",
        h if h < 247.5 => "cyan",
        h if h < 292.5 => "blue (cool)",
        _ => "magenta",
    }
}

impl Metric for ColorCast {
    fn info(&self) -> &'static MetricInfo {
        &CAST
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, _o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&CAST, a)?;
        let lab = a.lab();
        let w = lab.width;
        // (neutral a, neutral b, neutral n, all a, all b)
        let s = (0..lab.height)
            .into_par_iter()
            .map(|y| {
                let r = y * w..(y + 1) * w;
                let (l, ca, cb) = (&lab.l[r.clone()], &lab.a[r.clone()], &lab.b[r]);
                // Lanes: [neutral a, neutral b, neutral count, all a, all b] x 8.
                const L: usize = 8;
                let mut acc = [[0f32; L]; 5];
                let px = |x: usize| {
                    let (av, bv) = (ca[x], cb[x]);
                    let k = if av * av + bv * bv < NEUTRAL_CHROMA * NEUTRAL_CHROMA && l[x] > 20.0 && l[x] < 90.0 { 1.0 } else { 0.0 };
                    [av * k, bv * k, k, av, bv]
                };
                let full = w / L * L;
                for x0 in (0..full).step_by(L) {
                    for j in 0..L {
                        let v = px(x0 + j);
                        for (q, a) in acc.iter_mut().enumerate() {
                            a[j] += v[q];
                        }
                    }
                }
                let mut t: [f64; 5] = std::array::from_fn(|q| acc[q].iter().map(|v| *v as f64).sum());
                for x in full..w {
                    for (q, v) in px(x).into_iter().enumerate() {
                        t[q] += v as f64;
                    }
                }
                (t[0], t[1], t[2] as u64, t[3], t[4])
            })
            .reduce(|| (0.0, 0.0, 0, 0.0, 0.0), |p, q| (p.0 + q.0, p.1 + q.1, p.2 + q.2, p.3 + q.3, p.4 + q.4));
        cancel.check()?;
        let n = (w * lab.height) as f64;
        let frac = s.2 as f64 / n;
        let neutral = frac >= MIN_NEUTRAL && s.2 >= 64;
        let (ma, mb) = if neutral { (s.0 / s.2 as f64, s.1 / s.2 as f64) } else { (s.3 / n, s.4 / n) };
        let de = ma.hypot(mb);
        let hue = mb.atan2(ma).to_degrees().rem_euclid(360.0);
        let mut r = MetricResult::new(&CAST);
        r.push("delta_e", de, Unit::DeltaE, Direction::LowerIsBetter);
        r.push("a", ma, Unit::Score, Direction::Neutral);
        r.push("b", mb, Unit::Score, Direction::Neutral);
        r.push("hue", hue, Unit::Degrees, Direction::Neutral);
        r.push("neutral_pct", 100.0 * frac, Unit::Percent, Direction::Neutral);
        r.notes.push(format!(
            "cast {:+.1} a*, {:+.1} b* ({}), Delta-E {:.1}; {}",
            ma,
            mb,
            describe(hue),
            de,
            if neutral { "measured on near-neutral pixels" } else { "gray-world estimate (few neutral pixels)" }
        ));
        Ok(r)
    }
}
