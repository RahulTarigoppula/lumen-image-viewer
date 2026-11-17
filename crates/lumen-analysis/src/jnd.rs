//! Approximate just-noticeable differences (JND) for the gap between two
//! images' values of one metric ("B is about 2 JND noisier than A").
//!
//! These are rules of thumb, not a calibrated observer model: a JND depends
//! on content, viewing distance, display and the observer, and none of the
//! metrics here was fitted to JND data on this app's inputs. The mappings
//! are deliberately conservative (a larger metric step per JND than the
//! smallest values reported in the literature), so a gap called visible
//! usually is, and small gaps are called "within 1 JND" rather than
//! overclaimed. Use them to word comparisons, never as pass / fail limits.
//!
//! | metric family | one JND is about | basis |
//! |---|---|---|
//! | `deltae.2000`, `deltae.94` (mean, median, p95, max) | 2.0 Delta-E | Delta-E 2000 JND is quoted between 1.0 (uniform patches side by side) and 2.3; images hide small differences, so the upper half of the range |
//! | `deltae.76`, `color_cast.delta_e` | 2.3 Delta-E / Delta C* | the classic Delta-E 76 JND (Sharma); a cast on a whole picture is seen like a patch difference |
//! | `psnr` (any dB value) | 1.5 dB, nothing above 45 dB | heuristic: under about 0.5 dB is not seen at all, a few dB is plain at 30 to 40 dB, above about 45 dB distortions are invisible |
//! | `visual_noise` (`index`, `l`, `chroma`, `a`, `b`) | 25% more noise, on (value + 0.5) | heuristic, Weber-like: noise is judged relative to the noise already there; the 0.5 CIELAB offset keeps near-zero noise from inflating ratios |
//! | `noise` (levels) | 25% more noise, on (value + 1 level) | same rule in 8-bit levels |
//!
//! Signs: [`jnds`] is positive when B is worse than A.

use crate::metrics;

/// How a family's values map to JNDs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    /// One JND per `step` units (color differences, dB).
    Linear { step: f64 },
    /// One JND per relative change `weber` of `value + offset` (noise).
    Ratio { weber: f64, offset: f64 },
}

/// The JND rule of one metric value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Family {
    pub scale: Scale,
    pub higher_is_better: bool,
    /// Values are clamped to this range first (PSNR: no visible change
    /// above 45 dB).
    pub clamp: (f64, f64),
    /// How the worse image differs, for [`describe_gap`] ("noisier").
    pub worse: &'static str,
}

const NOISE_WEBER: f64 = 0.25;
/// PSNR above which differences are taken as invisible.
pub const PSNR_INVISIBLE_DB: f64 = 45.0;

/// The metric id and relative key of `key`: a full key
/// (`"visual_noise.chroma"`, `"deltae.2000.mean"`) or a bare metric id,
/// which stands for its headline (`"visual_noise"` is `visual_noise.index`).
fn split(key: &str) -> Option<(&'static str, String)> {
    let id = metrics::all().iter().map(|m| m.id()).filter(|id| key == *id || key.strip_prefix(id).is_some_and(|r| r.starts_with('.'))).max_by_key(|id| id.len())?;
    let sub = if key == id { metrics::headline_keys(id).first()?.to_string() } else { key[id.len() + 1..].to_string() };
    Some((id, sub))
}

/// The JND rule for metric value `key` (see [`split`] for the forms), or
/// `None` when there is no sensible one (counts, percentages, sharpness).
pub fn family(key: &str) -> Option<Family> {
    let (id, sub) = split(key)?;
    let any = (f64::NEG_INFINITY, f64::INFINITY);
    let linear = |step: f64, higher_is_better: bool, worse: &'static str| Family { scale: Scale::Linear { step }, higher_is_better, clamp: any, worse };
    let noisy = |offset: f64, worse: &'static str| Family { scale: Scale::Ratio { weber: NOISE_WEBER, offset }, higher_is_better: false, clamp: (0.0, f64::INFINITY), worse };
    let reference = "further from the reference";
    match (id, sub.as_str()) {
        ("deltae.2000" | "deltae.94", "mean" | "median" | "p95" | "max") => Some(linear(2.0, false, reference)),
        ("deltae.76", "mean" | "median" | "p95" | "max") => Some(linear(2.3, false, reference)),
        ("color_cast", "delta_e") => Some(linear(2.3, false, "more tinted")),
        ("psnr" | "psnr.rgb", s) if !s.starts_with("mse") && !s.starts_with("maxabs") => Some(Family { clamp: (f64::NEG_INFINITY, PSNR_INVISIBLE_DB), ..linear(1.5, true, reference) }),
        ("visual_noise", "index" | "a" | "b") => Some(noisy(0.5, "noisier")),
        ("visual_noise", "l") => Some(noisy(0.5, "noisier in brightness")),
        ("visual_noise", "chroma") => Some(noisy(0.5, "noisier in color")),
        ("noise", "y" | "u" | "v" | "cb" | "cr") => Some(noisy(1.0, "noisier")),
        _ => None,
    }
}

/// Approximate JNDs between A's value `a` and B's value `b` of metric value
/// `key`: positive when B is worse, negative when B is better, `None` for a
/// metric without a rule or a value that is not finite.
pub fn jnds(key: &str, a: f64, b: f64) -> Option<f64> {
    let f = family(key)?;
    if !a.is_finite() || !b.is_finite() {
        return None;
    }
    let (a, b) = (a.clamp(f.clamp.0, f.clamp.1), b.clamp(f.clamp.0, f.clamp.1));
    let d = match f.scale {
        Scale::Linear { step } => (b - a) / step,
        Scale::Ratio { weber, offset } => ((b + offset) / (a + offset)).ln() / (1.0 + weber).ln(),
    };
    Some(if f.higher_is_better { -d } else { d })
}

/// The difference in `key`'s units that is about one JND near `value`
/// (for "is this gap worth mentioning" tolerances).
pub fn step_at(key: &str, value: f64) -> Option<f64> {
    match family(key)?.scale {
        Scale::Linear { step } => Some(step),
        Scale::Ratio { weber, offset } => Some((value.max(0.0) + offset) * weber),
    }
}

/// Plain wording of the gap between A and B: "B is about 2 JND noisier
/// than A", or "A and B are within 1 JND (no visible difference)". `None`
/// when [`jnds`] is.
pub fn describe_gap(metric_id: &str, a: f64, b: f64) -> Option<String> {
    describe_gap_named(metric_id, a, b, "A", "B")
}

/// [`describe_gap`] with other names for the images ("C", "the Pixel 9").
pub fn describe_gap_named(metric_id: &str, a: f64, b: f64, name_a: &str, name_b: &str) -> Option<String> {
    let j = jnds(metric_id, a, b)?;
    let worse = family(metric_id)?.worse;
    let n = j.abs();
    if n < 0.5 {
        return Some(format!("{name_a} and {name_b} are within 1 JND (no visible difference)"));
    }
    let (w, bt) = if j > 0.0 { (name_b, name_a) } else { (name_a, name_b) };
    let amount = if n < 1.5 { "about 1 JND".to_string() } else { format!("about {n:.0} JND") };
    Some(format!("{w} is {amount} {worse} than {bt}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_steps() {
        assert!((jnds("deltae.2000.mean", 1.0, 3.0).unwrap() - 1.0).abs() < 1e-12);
        assert!((jnds("deltae.76.mean", 0.0, 4.6).unwrap() - 2.0).abs() < 1e-12);
        assert!((jnds("color_cast", 1.0, 3.3).unwrap() - 1.0).abs() < 1e-12);
        // Higher PSNR is better: B at 3 dB less is 2 JND worse.
        assert!((jnds("psnr.y", 36.0, 33.0).unwrap() - 2.0).abs() < 1e-12);
        // Both above the invisible limit: no difference.
        assert_eq!(jnds("psnr.avg", 48.0, 60.0), Some(0.0));
        // Noise is relative: the same 0.5 step matters more on a clean image.
        assert!(jnds("visual_noise.index", 0.5, 1.0).unwrap() > jnds("visual_noise.index", 3.0, 3.5).unwrap());
        assert!((step_at("visual_noise", 1.5).unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(step_at("deltae.2000.p95", 7.0), Some(2.0));
    }

    #[test]
    fn mapping_is_monotonic_and_antisymmetric() {
        for key in ["deltae.2000.mean", "deltae.76.max", "color_cast.delta_e", "psnr.y", "visual_noise.index", "visual_noise.chroma", "noise.y"] {
            let higher = family(key).unwrap().higher_is_better;
            let mut last = f64::NEG_INFINITY;
            for i in 0..40 {
                let b = 20.0 + i as f64 * 0.5;
                let j = jnds(key, 20.0, b).unwrap();
                // Worse B means more JND: increasing for lower-is-better values.
                let j = if higher { -j } else { j };
                assert!(j > last, "{key} not monotonic at {b}: {j} after {last}");
                last = j;
                let back = jnds(key, b, 20.0).unwrap();
                assert!((back + jnds(key, 20.0, b).unwrap()).abs() < 1e-9, "{key} not antisymmetric");
            }
        }
    }

    #[test]
    fn wording() {
        assert_eq!(describe_gap("visual_noise", 1.0, 2.0).unwrap(), "B is about 2 JND noisier than A");
        assert_eq!(describe_gap("visual_noise.chroma", 2.0, 1.0).unwrap(), "A is about 2 JND noisier in color than B");
        assert_eq!(describe_gap("deltae.2000.mean", 1.0, 1.4).unwrap(), "A and B are within 1 JND (no visible difference)");
        assert_eq!(describe_gap("psnr.y", 38.0, 36.5).unwrap(), "B is about 1 JND further from the reference than A");
        assert_eq!(describe_gap_named("color_cast.delta_e", 7.0, 1.0, "C", "D").unwrap(), "C is about 3 JND more tinted than D");
        // No rule for content-dependent or count values, nor for NaN.
        assert!(describe_gap("sharpness.laplacian", 100.0, 500.0).is_none());
        assert!(describe_gap("visual_noise.flat_pct", 10.0, 50.0).is_none());
        assert!(describe_gap("nope", 1.0, 2.0).is_none());
        assert!(describe_gap("noise", f64::NAN, 2.0).is_none());
    }
}
