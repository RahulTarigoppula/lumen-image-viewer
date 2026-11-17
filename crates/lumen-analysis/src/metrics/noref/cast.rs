//! Color cast, i.e. white balance measured on the neutral areas: the mean
//! a*, b* of near-neutral pixels (chroma under 10, mid luminance 20 < L* <
//! 90), falling back to gray world (all pixels) when fewer than 2% are near
//! neutral. Those limits also keep clipped pixels out: an sRGB color with a
//! channel at 0 or 255 is either outside 20 < L* < 90 or far more than 10
//! from gray (a blown red channel at L* 89 is already 14 a* away).
//! `delta_e` is the distance of the mean from neutral, which at equal L* is
//! both Delta-E 76 and Delta C*; `hue` is its direction in degrees (0 red,
//! 90 yellow, 180 green, 270 blue); `neutral_pct` is the share of pixels
//! used and `gray_world` is 1 when the fallback was used.
//!
//! The map shows where the neutrals are and how far each leans: per tile
//! (`MetricOptions::tile`) the Delta C* of the mean of its near-neutral
//! pixels, no value for tiles with fewer than 5% of them. A cast that
//! changes across the picture (mixed light) shows up there.

use crate::frame::{FrameCtx, InputKind};
use crate::heatmap::{Colormap, Heatmap, Pool};
use crate::job::CancelToken;
use crate::metrics::{check_size, Direction, Metric, MetricError, MetricInfo, MetricKind, MetricOptions, MetricResult, Unit};
use rayon::prelude::*;

const NEUTRAL_CHROMA: f32 = 10.0;
const MIN_NEUTRAL: f64 = 0.02;
/// Least share of near-neutral pixels for a tile of the map to get a value.
const MAP_MIN_NEUTRAL: f64 = 0.05;

/// `color_cast`
pub struct ColorCast;

static CAST: MetricInfo = MetricInfo {
    id: "color_cast",
    name: "Color cast",
    kind: MetricKind::NoReference,
    version: 2,
    inputs: &[InputKind::Lab],
    min_size: (1, 1),
    cost_ms_4k: 6.0,
    heavy: false,
    has_map: true,
    direction: Direction::LowerIsBetter,
    headline: &["delta_e"],
    description: "White balance: mean a*/b* of near-neutral mid-tone pixels (gray-world fallback), Delta C* and hue, with a map of the neutral areas' cast",
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

/// Sums of a band of rows: neutral a*, b* and count, all a*, b*, per tile
/// (neutral a*, b*, count, pixels).
#[derive(Default)]
struct Band {
    na: f64,
    nb: f64,
    nn: u64,
    aa: f64,
    ab: f64,
    tiles: Vec<(f64, f64, u32, u32)>,
}

impl Metric for ColorCast {
    fn info(&self) -> &'static MetricInfo {
        &CAST
    }

    fn compute(&self, a: &FrameCtx<'_>, _b: Option<&FrameCtx<'_>>, o: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError> {
        check_size(&CAST, a)?;
        let lab = a.lab();
        let (w, h) = (lab.width, lab.height);
        let tile = (o.tile as usize).max(8);
        let (tw, th) = (w.div_ceil(tile), h.div_ceil(tile));
        // One band of rows per tile row, summed in order (deterministic).
        let bands: Vec<Band> = (0..th)
            .into_par_iter()
            .map(|ty| {
                let mut b = Band { tiles: vec![(0.0, 0.0, 0, 0); tw], ..Default::default() };
                if cancel.is_cancelled() {
                    return b;
                }
                for y in ty * tile..((ty + 1) * tile).min(h) {
                    let r = y * w..(y + 1) * w;
                    let (l, ca, cb) = (&lab.l[r.clone()], &lab.a[r.clone()], &lab.b[r]);
                    let (mut na, mut nb, mut aa, mut ab) = (0f64, 0f64, 0f64, 0f64);
                    for x in 0..w {
                        let (av, bv) = (ca[x], cb[x]);
                        aa += av as f64;
                        ab += bv as f64;
                        let neutral = av * av + bv * bv < NEUTRAL_CHROMA * NEUTRAL_CHROMA && l[x] > 20.0 && l[x] < 90.0;
                        let t = &mut b.tiles[x / tile];
                        t.3 += 1;
                        if neutral {
                            na += av as f64;
                            nb += bv as f64;
                            b.nn += 1;
                            t.0 += av as f64;
                            t.1 += bv as f64;
                            t.2 += 1;
                        }
                    }
                    b.na += na;
                    b.nb += nb;
                    b.aa += aa;
                    b.ab += ab;
                }
                b
            })
            .collect();
        cancel.check()?;
        let (mut s, mut tiles) = (Band::default(), Vec::with_capacity(tw * th));
        for b in bands {
            s.na += b.na;
            s.nb += b.nb;
            s.nn += b.nn;
            s.aa += b.aa;
            s.ab += b.ab;
            tiles.extend(b.tiles);
        }
        let n = (w * h) as f64;
        let frac = s.nn as f64 / n;
        let neutral = frac >= MIN_NEUTRAL && s.nn >= 64;
        let (ma, mb) = if neutral { (s.na / s.nn as f64, s.nb / s.nn as f64) } else { (s.aa / n, s.ab / n) };
        let de = ma.hypot(mb);
        let hue = mb.atan2(ma).to_degrees().rem_euclid(360.0);
        let mut r = MetricResult::new(&CAST);
        r.push("delta_e", de, Unit::DeltaE, Direction::LowerIsBetter);
        r.push("a", ma, Unit::Score, Direction::Neutral);
        r.push("b", mb, Unit::Score, Direction::Neutral);
        r.push("hue", hue, Unit::Degrees, Direction::Neutral);
        r.push("neutral_pct", 100.0 * frac, Unit::Percent, Direction::Neutral);
        r.push("gray_world", if neutral { 0.0 } else { 1.0 }, Unit::Count, Direction::Neutral);
        r.notes.push(format!(
            "cast {:+.1} a*, {:+.1} b* ({}), Delta C* {:.1}; {}",
            ma,
            mb,
            describe(hue),
            de,
            if neutral { "white balance measured on near-neutral pixels" } else { "gray-world estimate (few neutral pixels)" }
        ));
        if o.want_map {
            let data = tiles
                .iter()
                .map(|&(ta, tb, tn, px)| if tn > 0 && tn as f64 >= MAP_MIN_NEUTRAL * px as f64 { (ta / tn as f64).hypot(tb / tn as f64) as f32 } else { f32::NAN })
                .collect();
            let c = tile as f32;
            r.map = Some(Heatmap::new(tw, th, data, "Cast of the near-neutral areas (Delta C*)").with_unit(Unit::DeltaE).with_cell(c, c).with_range(0.0, 10.0).with_min_span(3.0).with_style(Colormap::Magma, false, Pool::Max));
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::srgb8_to_lab;
    use crate::metrics::noref::test_util::{rgb_frame, run};

    #[test]
    fn gray_card_with_a_known_tint() {
        let o = MetricOptions::default();
        // A warm gray card: R up, B down. Its own Lab is the expected cast.
        let card = [138u8, 128, 116];
        let want = srgb8_to_lab(card);
        let r = run(&ColorCast, &rgb_frame(64, 64, |_, _| card).view(), &o);
        let (a, b) = (r.get("a").unwrap(), r.get("b").unwrap());
        assert!(a > 0.0 && b > 0.0, "warm means +a*, +b*: {a}, {b}");
        assert!((a - want[1]).abs() < 0.05 && (b - want[2]).abs() < 0.05, "{a}, {b} vs {want:?}");
        assert!((r.get("delta_e").unwrap() - want[1].hypot(want[2])).abs() < 0.05);
        let hue = r.get("hue").unwrap();
        assert!(hue > 45.0 && hue < 90.0, "{hue}");
        assert!(r.notes[0].contains("near-neutral"), "{}", r.notes[0]);
        assert_eq!((r.get("neutral_pct"), r.get("gray_world")), (Some(100.0), Some(0.0)));
        // A cool card: negative b*.
        let r = run(&ColorCast, &rgb_frame(64, 64, |_, _| [118, 128, 140]).view(), &o);
        assert!(r.get("b").unwrap() < -3.0, "{:?}", r.values);
        // The map: every 64 px tile is neutral and has the card's cast.
        let r = run(&ColorCast, &rgb_frame(128, 64, |_, _| card).view(), &o);
        let m = r.map.unwrap();
        assert_eq!((m.width, m.height), (2, 1));
        assert!(m.data.iter().all(|v| (*v as f64 - want[1].hypot(want[2])).abs() < 0.05), "{:?}", m.data);
    }

    #[test]
    fn clipped_and_colorful_pixels_are_not_neutral() {
        let o = MetricOptions::default();
        // Half a neutral gray, half a near-white tinted patch with R clipped
        // (chroma under 10, L* above 90): only the gray half counts.
        let f = rgb_frame(64, 32, |x, _| if x < 32 { [128, 128, 128] } else { [255, 240, 235] });
        let r = run(&ColorCast, &f.view(), &o);
        assert!((r.get("neutral_pct").unwrap() - 50.0).abs() < 1e-9, "{:?}", r.values);
        assert!(r.get("delta_e").unwrap() < 0.05);
        // Strong color everywhere: gray-world fallback, flagged.
        let r = run(&ColorCast, &rgb_frame(32, 32, |_, _| [100, 125, 170]).view(), &o);
        assert_eq!(r.get("gray_world"), Some(1.0));
        assert!(r.notes[0].contains("gray-world"));
        assert!(r.map.unwrap().data.iter().all(|v| v.is_nan()));
    }
}
