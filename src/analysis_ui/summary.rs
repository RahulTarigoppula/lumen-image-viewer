//! The plain-language summary the Metrics tab shows first: a headline and a
//! few compact lines ("Highlights clipped  2.4%"), each with a status dot
//! and a hover note that says what was measured and what it does not mean.
//!
//! Pure functions of [`Facts`] (values pulled out of a report), so the
//! wording and its thresholds are unit tested. The thresholds follow the
//! metric definitions in `crates/lumen-analysis` and the usual reading of
//! each scale; content-dependent measures (sharpness, repeated frames) never
//! turn red, and estimates that may not apply (gray-world color cast) say so.

use std::collections::BTreeMap;

use lumen_analysis::PSNR_CAP_DB;

/// Status dot of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Neutral information (gray dot).
    Info,
    Good,
    Fair,
    Poor,
}

/// One compact row: dot, short label, muted value, hover note.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub level: Level,
    pub label: String,
    pub value: String,
    pub tip: String,
    /// The report value the line is about (`"exposure.clip_pct"`; empty for
    /// counts that are not a table value). The "All numbers" table shows
    /// that value with the line's dot, so both always agree.
    pub key: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Head {
    pub level: Level,
    pub title: String,
    pub detail: String,
}

/// The one follow-up the summary offers.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// "Show where": turn on the heatmap of metric `id`; `why` names the
    /// difference it shows.
    ShowMap { id: String, why: String },
    /// "Show worst frame": seek video mode to A's frame `frame`.
    Seek { frame: u64 },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub head: Option<Head>,
    pub lines: Vec<Line>,
    pub action: Option<Action>,
    /// Small print under the lines (B rescaled, frames sampled).
    pub notes: Vec<String>,
    /// Video ranges: frames where a scene cut was found (clickable).
    pub cuts: Vec<u64>,
    /// Values measured but deliberately not graded, with why (no smooth
    /// area to judge banding on): the table shows them with a gray dot.
    pub ungraded: Vec<(String, String)>,
    /// Pairs: how close B is to A overall.
    pub closeness: Option<Closeness>,
}

impl Summary {
    /// The verdict on report value `key`: the dot of its line (or gray for
    /// an ungraded value) and what the summary says about it.
    pub fn verdict(&self, key: &str) -> Option<(Level, String)> {
        if let Some(l) = self.lines.iter().find(|l| l.key == key) {
            return Some((l.level, if l.value.is_empty() { l.label.clone() } else { format!("{}: {}", l.label, l.value) }));
        }
        self.ungraded.iter().find(|u| u.0 == key).map(|u| (Level::Info, u.1.clone()))
    }
}

/// What a report measured, in the terms the summary needs.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub pair: bool,
    pub video: bool,
    /// Values by full key (`"noise.y"`); video ranges: means over frames,
    /// temporal counts (`"temporal.freeze.duplicates"`), `"psnr.global"`.
    pub values: BTreeMap<String, f64>,
    /// Video ranges: the worst frame and its value per key.
    pub worst: BTreeMap<String, (u64, f64)>,
    /// Video ranges: frames measured and the stride.
    pub frames: usize,
    pub stride: usize,
    /// Video ranges: frames with PSNR-Y below [`PSNR_POOR`].
    pub low_frames: usize,
    /// A single video frame: its index.
    pub frame: Option<u64>,
    pub fps: f64,
    /// B was resampled to A's size.
    pub rescaled: bool,
    /// What the compared image is called (`"B"`; 4-image compare: `"C"`, `"D"`).
    pub other: String,
    /// Metrics with a heatmap in the report.
    pub maps: Vec<String>,
    /// Video ranges: frames with a scene cut.
    pub cuts: Vec<u64>,
}

impl Facts {
    fn get(&self, key: &str) -> Option<f64> {
        self.values.get(key).copied().filter(|v| v.is_finite())
    }

    fn first(&self, keys: &[&str]) -> Option<f64> {
        self.first_key(keys).map(|k| k.1)
    }

    /// The first of `keys` with a value, and the value.
    fn first_key<'k>(&self, keys: &[&'k str]) -> Option<(&'k str, f64)> {
        keys.iter().find_map(|k| self.get(k).map(|v| (*k, v)))
    }

    /// The compared image's letter.
    fn b(&self) -> &str {
        if self.other.is_empty() {
            "B"
        } else {
            &self.other
        }
    }
}

// ------------------------------------------------------------------ thresholds

/// Laplacian variance (8-bit levels squared) of the sharpest tiles: at or
/// above this, edges are crisp; below [`SHARP_SOFT`] even the sharpest parts
/// carry little fine detail (the common blur-detection cut-off is 100).
pub const SHARP_GOOD: f64 = 500.0;
pub const SHARP_SOFT: f64 = 100.0;
/// Immerkaer noise sigma in 8-bit levels: below 2 grain is hard to see,
/// above 5 it is plain at 100%.
pub const NOISE_GOOD: f64 = 2.0;
pub const NOISE_POOR: f64 = 5.0;
/// Clipped pixels in percent: a few light sources are normal.
pub const CLIP_GOOD: f64 = 0.5;
pub const CLIP_POOR: f64 = 2.0;
/// Color cast strength (Delta-E 76 of the mean neutral color from gray).
pub const CAST_GOOD: f64 = 2.0;
pub const CAST_POOR: f64 = 5.0;
/// Near-neutral pixels (percent) below which the cast is a gray-world guess.
pub const CAST_MIN_NEUTRAL: f64 = 2.0;
/// Blockiness score: 0 is no grid, 0.3 is block edges 30% stronger.
pub const BLOCK_GOOD: f64 = 0.1;
pub const BLOCK_POOR: f64 = 0.3;
/// Banding score (percent of smooth area on one-level contours), and the
/// smooth area (percent) below which there is nothing to judge.
pub const BAND_GOOD: f64 = 1.0;
pub const BAND_POOR: f64 = 5.0;
pub const BAND_MIN_SMOOTH: f64 = 1.0;
/// PSNR in dB: above 40 usually transparent, 30 to 40 good to fair, below
/// 30 visibly degraded.
pub const PSNR_GOOD: f64 = 40.0;
pub const PSNR_CLOSE: f64 = 35.0;
pub const PSNR_POOR: f64 = 30.0;
/// SSIM (1 is identical).
pub const SSIM_GOOD: f64 = 0.98;
pub const SSIM_CLOSE: f64 = 0.95;
pub const SSIM_POOR: f64 = 0.90;
/// CIEDE2000 mean: below 1 not perceptible, 1 to 2 on close inspection,
/// above 2 at a glance.
pub const DE_GOOD: f64 = 1.0;
pub const DE_CLOSE: f64 = 2.0;
pub const DE_POOR: f64 = 3.0;

fn lo_level(v: f64, good: f64, poor: f64) -> Level {
    if v < good {
        Level::Good
    } else if v > poor {
        Level::Poor
    } else {
        Level::Fair
    }
}

/// How close B is to A, from best to worst.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Closeness {
    Identical,
    VeryClose,
    Close,
    Noticeable,
    Different,
}

impl Closeness {
    /// A few words for lists ("Very close").
    pub fn short(self) -> &'static str {
        match self {
            Closeness::Identical => "Identical",
            Closeness::VeryClose => "Very close",
            Closeness::Close => "Close",
            Closeness::Noticeable => "Differs",
            Closeness::Different => "Clearly different",
        }
    }

    pub fn level(self) -> Level {
        match self {
            Closeness::Identical | Closeness::VeryClose => Level::Good,
            Closeness::Close | Closeness::Noticeable => Level::Fair,
            Closeness::Different => Level::Poor,
        }
    }
}

pub fn psnr_closeness(db: f64) -> Closeness {
    match db {
        d if d >= PSNR_CAP_DB => Closeness::Identical,
        d if d >= PSNR_GOOD => Closeness::VeryClose,
        d if d >= PSNR_CLOSE => Closeness::Close,
        d if d >= PSNR_POOR => Closeness::Noticeable,
        _ => Closeness::Different,
    }
}

pub fn ssim_closeness(v: f64) -> Closeness {
    match v {
        v if v >= 1.0 => Closeness::Identical,
        v if v >= SSIM_GOOD => Closeness::VeryClose,
        v if v >= SSIM_CLOSE => Closeness::Close,
        v if v >= SSIM_POOR => Closeness::Noticeable,
        _ => Closeness::Different,
    }
}

pub fn deltae_closeness(v: f64) -> Closeness {
    match v {
        v if v <= 0.0 => Closeness::Identical,
        v if v < DE_GOOD => Closeness::VeryClose,
        v if v < DE_CLOSE => Closeness::Close,
        v if v <= DE_POOR => Closeness::Noticeable,
        _ => Closeness::Different,
    }
}

// ------------------------------------------------------------------ formatting

/// "01:23", or "1:02:03" past an hour.
pub fn fmt_clock(secs: f64) -> String {
    let s = secs.max(0.0).floor() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// "2nd", "3rd", "11th".
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// "Every frame", "Every 4th frame".
pub fn every(stride: usize) -> String {
    if stride <= 1 {
        "Every frame".into()
    } else {
        format!("Every {} frame", ordinal(stride))
    }
}

// Values are written with the decimals the "All numbers" table uses
// (`metrics_view::fmt_value`), so a number reads the same in both places.

/// Percent: one decimal, `<0.1%` for traces.
pub fn pct(v: f64) -> String {
    if v > 0.0 && v < 0.1 {
        "<0.1%".into()
    } else {
        format!("{v:.1}%")
    }
}

fn db(v: f64) -> String {
    format!("{v:.2} dB")
}

// ------------------------------------------------------------------ summaries

/// The summary of a report described by `f`.
pub fn summarize(f: &Facts) -> Summary {
    let has_fr = f.first(&["psnr.global", "psnr.avg", "psnr.y", "ssim.gauss.y", "ssim.fast.y", "ms_ssim.y", "deltae.2000.mean"]).is_some();
    if f.pair && has_fr {
        if f.video && f.frames > 1 {
            video_pair(f)
        } else {
            pair(f)
        }
    } else {
        single(f)
    }
}

/// One image or frame (or a range of one video): sharpness, noise,
/// exposure, color cast, blocking and banding, then temporal counts.
fn single(f: &Facts) -> Summary {
    let range = f.video && f.frames > 1;
    let avg = if range { " (mean over the measured frames)" } else { "" };
    let mut lines = Vec::new();
    let mut ungraded = Vec::new();
    // The sharpest tiles decide (the table's headline); reports from before
    // that value existed have the whole-frame variance only.
    if let Some((key, x)) = f.first_key(&["sharpness.laplacian.p90", "sharpness.laplacian.var"]) {
        let overall = f.get("sharpness.laplacian.var").filter(|_| key.ends_with("p90"));
        let (level, label) = if x >= SHARP_GOOD {
            (Level::Good, "Sharp detail")
        } else if x >= SHARP_SOFT {
            (Level::Fair, "Moderate detail")
        } else {
            (Level::Fair, "Soft, little fine detail")
        };
        let basis = match overall {
            Some(v) => format!("{x:.0} in the sharpest areas, {v:.0} over the whole picture{avg}"),
            None => format!("{x:.0}{avg}"),
        };
        lines.push(Line {
            level,
            label: label.into(),
            value: format!("{x:.0}"),
            tip: format!("Edge contrast (Laplacian variance): {basis}. Above {SHARP_GOOD:.0} edges are crisp, below {SHARP_SOFT:.0} the picture looks soft. Smooth subjects such as sky or walls also score low, so this does not prove the focus is off."),
            key: key.into(),
        });
    }
    if let Some(v) = f.get("noise.y") {
        let (level, label) = match lo_level(v, NOISE_GOOD, NOISE_POOR) {
            Level::Good => (Level::Good, "Low noise"),
            Level::Fair => (Level::Fair, "Slightly noisy"),
            _ => (Level::Poor, "Noisy"),
        };
        lines.push(Line {
            level,
            label: label.into(),
            value: format!("{v:.2}"),
            tip: format!("Estimated luma noise: {v:.2} levels{avg} (Immerkaer, edges masked). Below {NOISE_GOOD:.0} grain is hard to see, above {NOISE_POOR:.0} it shows at 100%. Fine texture such as foliage can add to it."),
            key: "noise.y".into(),
        });
    }
    if let (Some(hi), Some(lo)) = (f.get("exposure.clip_hi_pct"), f.get("exposure.clip_lo_pct")) {
        let level = lo_level(hi.max(lo), CLIP_GOOD, CLIP_POOR);
        let (label, value) = match (hi >= CLIP_GOOD, lo >= CLIP_GOOD) {
            (false, false) => ("No clipping", pct(hi.max(lo))),
            (true, false) => ("Highlights clipped", pct(hi)),
            (false, true) => ("Shadows clipped", pct(lo)),
            (true, true) => ("Highlights and shadows clipped", format!("{} / {}", pct(hi), pct(lo))),
        };
        lines.push(Line {
            level,
            label: label.into(),
            value,
            tip: format!("{} of pixels have a channel at full white (blown highlights or saturated color), {} are pure black{avg}. A few light sources or a black background are normal.", pct(hi), pct(lo)),
            key: "exposure.clip_pct".into(),
        });
    }
    if let Some(de) = f.get("color_cast.delta_e") {
        let neutral = f.get("color_cast.neutral_pct").unwrap_or(100.0);
        let hue = f.get("color_cast.hue").map(lumen_analysis::metrics::noref::cast::describe).unwrap_or("color");
        if neutral < CAST_MIN_NEUTRAL {
            lines.push(Line {
                level: Level::Info,
                label: "Color cast not measurable".into(),
                value: "few gray areas".into(),
                tip: format!("Only {} of the picture is near gray, so a cast cannot be told apart from the colors of the subject. The average color leans {hue} (Delta-E {de:.2}).", pct(neutral)),
                key: "color_cast.delta_e".into(),
            });
        } else {
            let level = lo_level(de, CAST_GOOD, CAST_POOR);
            let label = match level {
                Level::Good => "No color cast".to_string(),
                Level::Fair => format!("Slight {hue} cast"),
                _ => format!("Strong {hue} cast"),
            };
            lines.push(Line {
                level,
                label,
                value: format!("dE {de:.2}"),
                tip: format!("Average color of the near-gray areas ({} of the picture) is {de:.2} Delta-E from neutral{avg}. Below {CAST_GOOD:.0} it is hard to see. Warm evening light or a graded look can be intentional.", pct(neutral)),
                key: "color_cast.delta_e".into(),
            });
        }
    }
    if let Some(v) = f.get("blockiness.score") {
        let level = lo_level(v, BLOCK_GOOD, BLOCK_POOR);
        let label = match level {
            Level::Good => "No compression blocking",
            Level::Fair => "Slight compression blocking",
            _ => "Visible compression blocking",
        };
        lines.push(Line {
            level,
            label: label.into(),
            value: format!("{v:.2}"),
            tip: format!("Blockiness {v:.2}{avg}: how much stronger edges are on the 8x8 grid that JPEG and most video codecs leave (0 is none, 0.3 is 30% stronger). Ringing and blur from compression are not measured here."),
            key: "blockiness.score".into(),
        });
    }
    if let Some(v) = f.get("banding.score") {
        let smooth = f.get("banding.smooth_pct").unwrap_or(100.0);
        if smooth >= BAND_MIN_SMOOTH {
            let level = lo_level(v, BAND_GOOD, BAND_POOR);
            let label = match level {
                Level::Good => "No banding",
                Level::Fair => "Slight banding in gradients",
                _ => "Visible banding in gradients",
            };
            lines.push(Line {
                level,
                label: label.into(),
                value: pct(v),
                tip: format!("{} of the smooth areas ({} of the picture) are one-level steps between flat bands{avg}, the stair-step look of a posterized gradient.", pct(v), pct(smooth)),
                key: "banding.score".into(),
            });
        } else {
            ungraded.push(("banding.score".to_string(), format!("Not graded: only {} of the picture is smooth enough to show banding", pct(smooth))));
        }
    }
    if let Some(n) = f.get("temporal.freeze.duplicates") {
        let share = 100.0 * n / f.frames.max(1) as f64;
        let longest = f.get("temporal.freeze.longest_freeze").map(|l| format!(", the longest run {l:.0} frames")).unwrap_or_default();
        let sampled = if f.stride > 1 { " Measured on the sampled frames, so it compares frames that are not neighbours." } else { "" };
        lines.push(Line {
            level: Level::Info,
            label: if n < 0.5 { "No repeated frames".into() } else { format!("{n:.0} repeated frames") },
            value: pct(share),
            tip: format!("Frames identical to the one before{longest}. Normal for still content such as screen recordings; in camera footage it means dropped or frozen frames.{sampled}"),
            key: "temporal.freeze.duplicates".into(),
        });
    }
    if let Some(n) = f.get("temporal.scene.cuts") {
        lines.push(Line {
            level: Level::Info,
            label: match n as u64 {
                0 => "No scene cuts".into(),
                1 => "1 scene cut".into(),
                n => format!("{n} scene cuts"),
            },
            value: String::new(),
            tip: "Large picture changes between frames, such as cuts between shots. Click a time below to see the cut.".into(),
            key: "temporal.scene.cuts".into(),
        });
    }
    let head = if range {
        Some(Head { level: Level::Info, title: format!("Average of {} frames", f.frames), detail: format!("{}. Hover a line for details.", every(f.stride)) })
    } else {
        let issues = lines.iter().filter(|l| matches!(l.level, Level::Fair | Level::Poor)).count();
        let what = if f.video { "frame" } else { "image" };
        let at = f.frame.map(|i| format!("Frame {i} at {}. ", fmt_clock(i as f64 / f.fps.max(1.0)))).unwrap_or_default();
        match issues {
            _ if lines.is_empty() => None,
            0 => Some(Head { level: Level::Good, title: "No issues found".into(), detail: format!("{at}These checks found nothing to fix in this {what}.") }),
            n => Some(Head {
                level: lines.iter().map(|l| l.level).max().unwrap_or(Level::Fair),
                title: if n == 1 { "1 thing to check".into() } else { format!("{n} things to check") },
                detail: format!("{at}Hover a line for what it means."),
            }),
        }
    };
    let cuts = if range { f.cuts.clone() } else { Vec::new() };
    Summary { head, lines, cuts, ungraded, ..Default::default() }
}

/// The aspects of a pair: (closeness, line). `psnr` is the key to read.
fn pair_lines(f: &Facts, psnr_key: Option<&str>, avg: &str) -> (Vec<(Closeness, &'static str)>, Vec<Line>) {
    let mut close = Vec::new();
    let mut lines = Vec::new();
    let b = f.b();
    if let Some((key, v)) = f.first_key(&["ssim.gauss.y", "ssim.fast.y", "ms_ssim.y"]) {
        let c = ssim_closeness(v);
        close.push((c, "ssim"));
        let label = match c {
            Closeness::Identical | Closeness::VeryClose => "Structure matches",
            Closeness::Close => "Slight structural differences",
            Closeness::Noticeable => "Structural differences",
            Closeness::Different => "Large structural differences",
        };
        lines.push(Line {
            level: c.level(),
            label: label.into(),
            value: format!("SSIM {v:.4}"),
            tip: format!("SSIM{avg} compares local brightness, contrast and structure (1 is identical). Blur, noise, blocking and sharpening all lower it. Above {SSIM_GOOD} differences are usually hard to see, below {SSIM_POOR} they are clear."),
            key: key.into(),
        });
    }
    if let Some(v) = f.get("deltae.2000.mean") {
        let c = deltae_closeness(v);
        close.push((c, "deltae"));
        let label = match c {
            Closeness::Identical | Closeness::VeryClose => "Colors match",
            Closeness::Close => "Slight color differences",
            Closeness::Noticeable => "Color differences",
            Closeness::Different => "Large color differences",
        };
        let visible = f.get("deltae.2000.above_jnd_pct").map(|p| format!(" {} of pixels differ visibly (above 2.3).", pct(p))).unwrap_or_default();
        lines.push(Line {
            level: c.level(),
            label: label.into(),
            value: format!("dE {v:.2}"),
            tip: format!("Average CIEDE2000 color difference{avg}. Below {DE_GOOD:.0} it is not perceptible, up to {DE_CLOSE:.0} only on close inspection, above that at a glance.{visible}"),
            key: "deltae.2000.mean".into(),
        });
    }
    if let Some((key, v)) = psnr_key.and_then(|k| f.get(k).map(|v| (k, v))) {
        let c = psnr_closeness(v);
        close.push((c, "psnr"));
        let label = match c {
            Closeness::Identical => "Pixels identical",
            Closeness::VeryClose => "Very low pixel error",
            Closeness::Close | Closeness::Noticeable => "Moderate pixel error",
            Closeness::Different => "High pixel error",
        };
        lines.push(Line {
            level: c.level(),
            label: label.into(),
            value: if c == Closeness::Identical { "PSNR max".into() } else { format!("PSNR {}", db(v)) },
            tip: format!("Peak signal-to-noise ratio of the pixel differences{avg}. Above {PSNR_GOOD:.0} dB {b} usually looks the same, {PSNR_POOR:.0} to {PSNR_GOOD:.0} dB good to fair, below {PSNR_POOR:.0} dB visibly degraded."),
            key: key.into(),
        });
    }
    if let Some(v) = f.get("ssimulacra2.score") {
        let level = if v >= 70.0 { Level::Good } else if v < 50.0 { Level::Poor } else { Level::Fair };
        let label = match level {
            Level::Good => "High perceived quality",
            Level::Fair => "Medium perceived quality",
            _ => "Low perceived quality",
        };
        let tip = format!("Perceptual quality of {b} against A (100 is identical, 70 and up is high quality, below 50 low).");
        lines.push(Line { level, label: label.into(), value: format!("SSIMULACRA 2 {v:.2}"), tip, key: "ssimulacra2.score".into() });
    }
    if let Some(v) = f.get("butteraugli.score") {
        let level = lo_level(v, 1.0, 2.0);
        let label = match level {
            Level::Good => "Differences below visibility",
            Level::Fair => "Differences near visibility",
            _ => "Clearly visible differences",
        };
        let tip = "Largest perceptual difference (Butteraugli): below 1 usually invisible, above 2 clearly visible.".to_string();
        lines.push(Line { level, label: label.into(), value: format!("Butteraugli {v:.2}"), tip, key: "butteraugli.score".into() });
    }
    (close, lines)
}

/// "SSIM 0.990, PSNR 42.1 dB" for the headline.
fn numbers(f: &Facts, psnr_key: Option<&str>) -> String {
    let mut v = Vec::new();
    if let Some(s) = f.first(&["ssim.gauss.y", "ssim.fast.y", "ms_ssim.y"]) {
        v.push(format!("SSIM {s:.4}"));
    }
    if let Some(p) = psnr_key.and_then(|k| f.get(k)).filter(|p| *p < PSNR_CAP_DB) {
        v.push(format!("PSNR {}", db(p)));
    }
    if v.is_empty() {
        if let Some(d) = f.get("deltae.2000.mean") {
            v.push(format!("Delta-E {d:.2}"));
        }
    }
    v.join(", ")
}

/// Headline and detail for a pair, `b` naming the compared image.
fn verdict(c: Closeness, b: &str) -> (String, &'static str) {
    let (title, what) = match c {
        Closeness::Identical => ("is identical to A", "Every measured pixel matches"),
        Closeness::VeryClose => ("is very close to A", "Differences barely visible"),
        Closeness::Close => ("is close to A", "Small differences, visible on close inspection"),
        Closeness::Noticeable => ("differs from A", "Differences are noticeable"),
        Closeness::Different => ("is clearly different from A", "Differences are easy to see"),
    };
    (format!("{b} {title}"), what)
}

/// How far a value is past its "very close" limit, in units of the span to
/// its "poor" limit: the aspect with the largest one differs most.
fn badness(aspect: &str, f: &Facts) -> Option<f64> {
    match aspect {
        "ssim" => f.first(&["ssim.gauss.y", "ssim.fast.y", "ms_ssim.y"]).map(|v| (SSIM_GOOD - v) / (SSIM_GOOD - SSIM_POOR)),
        "deltae" => f.get("deltae.2000.mean").map(|v| (v - DE_GOOD) / (DE_POOR - DE_GOOD)),
        _ => None,
    }
}

/// Two images, or two videos at one frame.
fn pair(f: &Facts) -> Summary {
    let psnr_key = ["psnr.avg", "psnr.y", "psnr.rgb.avg"].into_iter().find(|k| f.get(k).is_some());
    let (close, lines) = pair_lines(f, psnr_key, "");
    let overall = close.iter().map(|c| c.0).max().unwrap_or(Closeness::Close);
    // Identical only when every measure says so.
    let overall = if overall == Closeness::Identical && close.iter().all(|c| c.0 == Closeness::Identical) { overall } else { overall.max(Closeness::VeryClose) };
    let (title, what) = verdict(overall, f.b());
    let nums = numbers(f, psnr_key);
    let at = f.frame.map(|i| format!("Frame {i}: ")).unwrap_or_default();
    let detail = if nums.is_empty() || overall == Closeness::Identical { format!("{what}.") } else { format!("{what} ({nums}).") };
    let mut s = Summary { head: Some(Head { level: overall.level(), title: format!("{at}{title}"), detail }), lines, closeness: Some(overall), ..Default::default() };
    if overall != Closeness::Identical {
        s.action = show_where(f);
    }
    if f.rescaled {
        s.notes.push(format!("{} was resized to A's size before comparing.", f.b()));
    }
    s
}

/// The heatmap that shows the largest kind of difference.
fn show_where(f: &Facts) -> Option<Action> {
    let has = |id: &str| f.maps.iter().any(|m| m == id);
    let mut aspects: Vec<(&str, f64)> = ["ssim", "deltae"].into_iter().filter_map(|a| badness(a, f).map(|b| (a, b))).collect();
    aspects.sort_by(|a, b| b.1.total_cmp(&a.1));
    let map_of = |a: &str| match a {
        "ssim" => ["ssim.gauss"].into_iter().find(|id| has(id)).map(|id| (id, "detail and structure")),
        _ => ["deltae.2000"].into_iter().find(|id| has(id)).map(|id| (id, "color")),
    };
    aspects
        .iter()
        .find_map(|(a, _)| map_of(a))
        .or_else(|| ["diff.abs", "flip", "butteraugli"].into_iter().find(|id| has(id)).map(|id| (id, "pixel values")))
        .map(|(id, why)| Action::ShowMap { id: id.to_string(), why: why.to_string() })
}

/// Two videos over a range of frames.
fn video_pair(f: &Facts) -> Summary {
    let avg = " (mean over the measured frames)";
    let psnr_key = ["psnr.global", "psnr.y", "psnr.avg"].into_iter().find(|k| f.get(k).is_some());
    let (close, mut lines) = pair_lines(f, psnr_key, avg);
    let overall = close.iter().map(|c| c.0).max().unwrap_or(Closeness::Close);
    let worst = ["psnr.y", "psnr.avg", "ssim.fast.y", "ssim.gauss.y"].iter().find_map(|k| f.worst.get(*k).map(|w| (*k, *w)));
    let at = |frame: u64| format!("{} (frame {frame})", fmt_clock(frame as f64 / f.fps.max(1.0)));
    let head = match psnr_key.and_then(|k| f.get(k)) {
        Some(p) if p >= PSNR_CAP_DB => Head { level: Level::Good, title: "B is identical to A".into(), detail: format!("Every measured frame matches ({} of them).", f.frames) },
        Some(p) => {
            let (_, what) = verdict(overall, f.b());
            let w = match worst {
                Some((k, (i, v))) if k.starts_with("psnr") => format!("; worst at {}, {}", at(i), db(v)),
                Some((_, (i, _))) => format!("; worst at {}", at(i)),
                None => String::new(),
            };
            Head { level: overall.level(), title: format!("Average quality {}{w}", db(p)), detail: format!("{what} on average.") }
        }
        None => {
            let (title, what) = verdict(overall, f.b());
            Head { level: overall.level(), title, detail: format!("{what} on average.") }
        }
    };
    if f.values.contains_key("psnr.y") || f.values.contains_key("psnr.global") {
        let share = 100.0 * f.low_frames as f64 / f.frames.max(1) as f64;
        let level = match f.low_frames {
            0 => Level::Good,
            _ if share <= 5.0 => Level::Fair,
            _ => Level::Poor,
        };
        lines.push(Line {
            level,
            label: match f.low_frames {
                0 => format!("No frames below {PSNR_POOR:.0} dB"),
                1 => format!("1 frame below {PSNR_POOR:.0} dB"),
                n => format!("{n} frames below {PSNR_POOR:.0} dB"),
            },
            value: pct(share),
            tip: format!("Frames whose PSNR-Y is under {PSNR_POOR:.0} dB, where encoding damage is usually visible. The graph under the timeline shows where they are."),
            key: String::new(),
        });
    }
    let mut s = Summary { head: Some(head), lines, closeness: Some(overall), ..Default::default() };
    if let Some((_, (i, _))) = worst.filter(|_| overall != Closeness::Identical) {
        s.action = Some(Action::Seek { frame: i });
    }
    s.notes.push(format!("{} measured: {}.", every(f.stride), f.frames));
    s
}

// ------------------------------------------------------------------ stride

/// Steps offered for the automatic stride.
const STRIDES: [usize; 24] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 15, 20, 25, 30, 40, 50, 60, 75, 100, 120, 150, 200, 250, 300, 500];

/// Seconds a range of `frames` frames at `stride` takes, at `ms_per_mpix`
/// milliseconds per measured frame and megapixel, plus start-up.
pub fn estimate_secs(frames: usize, stride: usize, mpix: f64, ms_per_mpix: f64) -> f64 {
    0.5 + frames.div_ceil(stride.max(1)) as f64 * mpix * ms_per_mpix / 1000.0
}

/// The smallest stride that measures `frames` frames within `budget_s`,
/// keeping at least 50 frames (or all of them in short clips).
pub fn auto_stride(frames: usize, mpix: f64, ms_per_mpix: f64, budget_s: f64) -> usize {
    let max = (frames / 50).max(1);
    STRIDES.iter().copied().take_while(|&s| s <= max).find(|&s| estimate_secs(frames, s, mpix, ms_per_mpix) <= budget_s).unwrap_or_else(|| STRIDES.iter().copied().take_while(|&s| s <= max).last().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(pairs: &[(&str, f64)]) -> Facts {
        Facts { values: pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect(), fps: 25.0, ..Default::default() }
    }

    fn line<'a>(s: &'a Summary, label: &str) -> &'a Line {
        s.lines.iter().find(|l| l.label.contains(label)).unwrap_or_else(|| panic!("no line {label:?} in {:?}", s.lines))
    }

    #[test]
    fn photo_lines_follow_the_thresholds() {
        let s = summarize(&facts(&[
            ("sharpness.laplacian.var", 650.0),
            ("noise.y", 3.1),
            ("exposure.clip_hi_pct", 2.4),
            ("exposure.clip_lo_pct", 0.1),
            ("color_cast.delta_e", 0.8),
            ("color_cast.neutral_pct", 30.0),
            ("blockiness.score", 0.02),
            ("banding.score", 0.4),
            ("banding.smooth_pct", 40.0),
        ]));
        assert_eq!(line(&s, "Sharp").level, Level::Good);
        assert_eq!((line(&s, "noisy").label.as_str(), line(&s, "noisy").level), ("Slightly noisy", Level::Fair));
        let clip = line(&s, "clipped");
        assert_eq!((clip.label.as_str(), clip.value.as_str(), clip.level), ("Highlights clipped", "2.4%", Level::Poor));
        assert_eq!(line(&s, "cast").label, "No color cast");
        assert_eq!(line(&s, "blocking").label, "No compression blocking");
        assert_eq!(line(&s, "banding").label, "No banding");
        let head = s.head.unwrap();
        assert_eq!((head.title.as_str(), head.level), ("2 things to check", Level::Poor));
    }

    #[test]
    fn clean_image_and_edge_values() {
        let s = summarize(&facts(&[("noise.y", 1.99), ("exposure.clip_hi_pct", 0.49), ("exposure.clip_lo_pct", 0.0), ("blockiness.score", 0.3)]));
        assert_eq!(line(&s, "noise").level, Level::Good);
        assert_eq!(line(&s, "clipping").label, "No clipping");
        // 0.3 is the poor limit itself: still amber.
        assert_eq!(line(&s, "blocking").label, "Slight compression blocking");
        let s = summarize(&facts(&[("noise.y", 1.0), ("exposure.clip_hi_pct", 0.0), ("exposure.clip_lo_pct", 0.0)]));
        assert_eq!(s.head.unwrap().title, "No issues found");
        let s = summarize(&facts(&[("exposure.clip_hi_pct", 0.6), ("exposure.clip_lo_pct", 3.0)]));
        let clip = line(&s, "clipped");
        assert_eq!((clip.label.as_str(), clip.level), ("Highlights and shadows clipped", Level::Poor));
    }

    #[test]
    fn content_dependent_values_do_not_overclaim() {
        // Low sharpness is amber at worst: a foggy sky is not a focus error.
        let s = summarize(&facts(&[("sharpness.laplacian.var", 12.0)]));
        assert_eq!((line(&s, "Soft").level), Level::Fair);
        // The sharpest tiles decide, not the mean over a smooth background.
        let s = summarize(&facts(&[("sharpness.laplacian.p90", 900.0), ("sharpness.laplacian.var", 80.0)]));
        let sharp = line(&s, "Sharp");
        assert_eq!((sharp.level, sharp.value.as_str(), sharp.key.as_str()), (Level::Good, "900", "sharpness.laplacian.p90"));
        // A gray-world estimate on a colorful picture is not called a cast.
        let s = summarize(&facts(&[("color_cast.delta_e", 50.0), ("color_cast.neutral_pct", 0.5), ("color_cast.hue", 10.0)]));
        let c = line(&s, "Color cast");
        assert_eq!((c.level, c.label.as_str()), (Level::Info, "Color cast not measurable"));
        let s = summarize(&facts(&[("color_cast.delta_e", 6.0), ("color_cast.neutral_pct", 20.0), ("color_cast.hue", 270.0)]));
        assert_eq!(line(&s, "cast").label, "Strong blue (cool) cast");
        // No smooth area: nothing to say about banding, and the table says so too.
        let s = summarize(&facts(&[("banding.score", 9.0), ("banding.smooth_pct", 0.2)]));
        assert!(s.lines.is_empty());
        assert_eq!(s.verdict("banding.score").map(|v| v.0), Some(Level::Info));
    }

    #[test]
    fn the_table_reads_the_summary_verdicts() {
        let s = summarize(&facts(&[("sharpness.laplacian.p90", 764.0), ("sharpness.laplacian.var", 229.5), ("color_cast.delta_e", 48.6), ("color_cast.neutral_pct", 0.4), ("exposure.clip_hi_pct", 3.02), ("exposure.clip_lo_pct", 0.0)]));
        // The headline value the table shows first carries the line's dot.
        assert_eq!(s.verdict("sharpness.laplacian.p90"), Some((Level::Good, "Sharp detail: 764".into())));
        assert_eq!(s.verdict("color_cast.delta_e").map(|v| v.0), Some(Level::Info));
        assert_eq!(s.verdict("exposure.clip_pct"), Some((Level::Poor, "Highlights clipped: 3.0%".into())));
        assert_eq!(s.verdict("sharpness.laplacian.var"), None);
        // In a 4-image compare, C and D are named.
        let mut f = facts(&[("psnr.avg", 25.0), ("ssim.gauss.y", 0.9)]);
        f.pair = true;
        f.other = "C".into();
        assert_eq!(summarize(&f).head.unwrap().title, "C is clearly different from A");
    }

    #[test]
    fn pair_headline_and_show_where() {
        let mut f = facts(&[("psnr.avg", 42.3), ("ssim.gauss.y", 0.991), ("deltae.2000.mean", 0.6)]);
        f.pair = true;
        f.maps = vec!["ssim.gauss".into(), "deltae.2000".into()];
        let s = summarize(&f);
        let h = s.head.clone().unwrap();
        assert_eq!(h.title, "B is very close to A");
        assert_eq!(h.detail, "Differences barely visible (SSIM 0.9910, PSNR 42.30 dB).");
        assert_eq!(h.level, Level::Good);
        // JPEG q30 of a photo: the worst measure decides.
        let mut f = facts(&[("psnr.avg", 33.3), ("ssim.gauss.y", 0.976), ("deltae.2000.mean", 1.23)]);
        f.pair = true;
        f.maps = vec!["ssim.gauss".into(), "deltae.2000".into()];
        let s = summarize(&f);
        assert_eq!(s.head.as_ref().unwrap().title, "B differs from A");
        assert_eq!(s.action, Some(Action::ShowMap { id: "deltae.2000".into(), why: "color".into() }));
        // Structure loss dominates: the SSIM map.
        f.values.insert("ssim.gauss.y".into(), 0.91);
        assert_eq!(summarize(&f).action, Some(Action::ShowMap { id: "ssim.gauss".into(), why: "detail and structure".into() }));
        // Without maps there is nothing to show.
        f.maps.clear();
        assert_eq!(summarize(&f).action, None);
    }

    #[test]
    fn identical_pairs() {
        let mut f = facts(&[("psnr.avg", 100.0), ("ssim.gauss.y", 1.0), ("deltae.2000.mean", 0.0)]);
        f.pair = true;
        f.maps = vec!["ssim.gauss".into()];
        let s = summarize(&f);
        assert_eq!(s.head.as_ref().unwrap().title, "B is identical to A");
        assert_eq!(s.action, None);
        assert_eq!(line(&s, "Pixels").value, "PSNR max");
        // PSNR capped but a color difference left: not identical.
        f.values.insert("deltae.2000.mean".into(), 0.4);
        assert_eq!(summarize(&f).head.unwrap().title, "B is very close to A");
    }

    #[test]
    fn video_pair_range() {
        let mut f = facts(&[("psnr.global", 41.2), ("psnr.y", 43.0), ("ssim.fast.y", 0.985)]);
        f.pair = true;
        f.video = true;
        f.frames = 1000;
        f.stride = 4;
        f.fps = 30.0;
        f.low_frames = 12;
        f.worst.insert("psnr.y".into(), (2490, 31.4));
        let s = summarize(&f);
        let h = s.head.as_ref().unwrap();
        assert_eq!(h.title, "Average quality 41.20 dB; worst at 01:23 (frame 2490), 31.40 dB");
        assert_eq!(s.action, Some(Action::Seek { frame: 2490 }));
        let low = line(&s, "below");
        assert_eq!((low.label.as_str(), low.level), ("12 frames below 30 dB", Level::Fair));
        assert_eq!(s.notes, ["Every 4th frame measured: 1000."]);
    }

    #[test]
    fn single_video_range_and_frame() {
        let mut f = facts(&[("noise.y", 0.03), ("temporal.freeze.duplicates", 289.0), ("temporal.scene.cuts", 0.0)]);
        f.video = true;
        f.frames = 308;
        f.stride = 1;
        f.cuts = vec![40, 120];
        let s = summarize(&f);
        assert_eq!(s.cuts, [40, 120]);
        assert_eq!(s.head.as_ref().unwrap().title, "Average of 308 frames");
        let d = line(&s, "repeated");
        assert_eq!((d.label.as_str(), d.level), ("289 repeated frames", Level::Info));
        assert_eq!(line(&s, "scene").label, "No scene cuts");
        let mut f = facts(&[("noise.y", 0.03)]);
        f.video = true;
        f.frame = Some(75);
        f.frames = 1;
        assert!(summarize(&f).head.unwrap().detail.starts_with("Frame 75 at 00:03."));
    }

    #[test]
    fn wording_helpers() {
        assert_eq!(fmt_clock(83.9), "01:23");
        assert_eq!(fmt_clock(3723.0), "1:02:03");
        assert_eq!([1, 2, 3, 4, 11, 12, 13, 21, 22].map(ordinal), ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd"]);
        assert_eq!(every(1), "Every frame");
        assert_eq!(every(4), "Every 4th frame");
        assert_eq!(pct(0.05), "<0.1%");
        assert_eq!(pct(0.0), "0.0%");
        // No typographic punctuation in any text.
        let mut f = facts(&[("psnr.avg", 33.3), ("ssim.gauss.y", 0.95), ("noise.y", 3.0)]);
        f.pair = true;
        let s = summarize(&f);
        for t in s.lines.iter().flat_map(|l| [&l.label, &l.value, &l.tip]) {
            assert!(t.is_ascii(), "{t}");
        }
    }

    #[test]
    fn stride_fits_the_budget() {
        // A short clip is measured whole.
        assert_eq!(auto_stride(308, 1.46, 16.0, 30.0), 1);
        // Ten minutes of 1080p60: about 36000 frames.
        let s = auto_stride(36_000, 2.07, 10.0, 30.0);
        assert!(estimate_secs(36_000, s, 2.07, 10.0) <= 30.0, "{s}");
        let prev = STRIDES[STRIDES.iter().position(|&x| x == s).unwrap() - 1];
        assert!(estimate_secs(36_000, prev, 2.07, 10.0) > 30.0, "{prev}");
        // Never fewer than 50 frames, even when that is over budget.
        assert_eq!(auto_stride(500, 33.0, 100.0, 30.0), 10);
    }
}
