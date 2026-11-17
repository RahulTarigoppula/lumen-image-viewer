//! Automatic start point alignment between clips of the same scene recorded
//! on different devices: how many seconds clip B runs behind or ahead of
//! clip A, found from the picture alone.
//!
//! Each decoded frame is reduced to a coarse grid of mean luma
//! ([`FrameSig`]). The signals compared are rates of change of the log
//! brightness, so a phone that exposes brighter or darker (a gain on the
//! signal) or renders different colors gives the same signal:
//!
//! - the whole picture's brightness change (lights, flashes, pans),
//! - motion energy: the mean absolute change over the grid cells (things
//!   moving, whatever the field of view),
//! - every cell's own change (where in the picture things happen).
//!
//! The signals are resampled onto one time grid (100 Hz by default, so clips
//! of different and variable frame rates compare directly), and the lag
//! with the highest normalized cross-correlation within +/- `max_lag` wins.
//! A parabola through the peak gives sub-frame precision; the confidence
//! compares the peak with the best competing peak elsewhere.

use rayon::prelude::*;

/// Grid cells per side the app reduces frames to.
pub const GRID: usize = 8;

/// One frame reduced to a grid of mean luma.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSig {
    /// Presentation time, seconds.
    pub t: f64,
    /// Mean luma per cell, 0..1, row major.
    pub cells: Vec<f32>,
}

/// Signals of one clip on a regular time grid (see [`features`]).
#[derive(Clone, Debug, Default)]
pub struct Features {
    /// Samples per second.
    pub rate: f64,
    /// Time of the first sample (the first frame), seconds.
    pub t0: f64,
    /// Channel 0: brightness change of the whole picture; 1: motion
    /// energy; 2 onwards: brightness change of each cell. Per second, in
    /// log units.
    pub channels: Vec<Vec<f32>>,
}

impl Features {
    /// Samples per channel.
    pub fn len(&self) -> usize {
        self.channels.first().map(|c| c.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Search settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Samples per second of the common time grid.
    pub rate: f64,
    /// Largest offset searched, seconds (either way).
    pub max_lag: f64,
    /// Least time both clips must share at a candidate offset, seconds.
    pub min_overlap: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params { rate: 100.0, max_lag: 10.0, min_overlap: 3.0 }
    }
}

/// How far apart two clips are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Alignment {
    /// Seconds clip B is behind A: what happens at time `t` in A happens at
    /// `t + offset` in B. B's start point is A's plus this.
    pub offset: f64,
    /// Correlation at the best offset (-1..1).
    pub peak: f64,
    /// The best competing correlation away from it (0..1).
    pub second: f64,
    /// 0..1: how clearly the best offset stands out (see [`Confidence`]).
    pub confidence: f64,
}

/// [`Alignment::confidence`] in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// Nothing stands out: the clips may show different scenes, or too
    /// little changes in them.
    None,
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::None => "no match",
            Confidence::Low => "low confidence",
            Confidence::Medium => "medium confidence",
            Confidence::High => "high confidence",
        }
    }
}

impl Alignment {
    pub fn level(&self) -> Confidence {
        match self.confidence {
            c if c >= 0.6 => Confidence::High,
            c if c >= 0.35 => Confidence::Medium,
            c if c >= 0.15 => Confidence::Low,
            _ => Confidence::None,
        }
    }
}

/// Added to luma before the logarithm (keeps black finite and its noise small).
const LOG_FLOOR: f32 = 0.02;
/// Weights of the whole-picture, motion and grid signals in the score.
const W_GLOBAL: f64 = 0.3;
const W_MOTION: f64 = 0.2;
const W_GRID: f64 = 0.5;
/// A channel whose variance over the overlap is below this is flat.
const FLAT: f64 = 1e-9;
/// The peak's lobe (left out when looking for a competing peak) spans at
/// least this and at most this many seconds on each side.
const LOBE_MIN_SECS: f64 = 0.1;
const LOBE_MAX_SECS: f64 = 1.0;
/// The rates of change are averaged over this long (centered): evens out
/// the steps that linear resampling of the frames leaves in them.
const SMOOTH_SECS: f64 = 0.03;

/// Centered moving average over `2 * half + 1` samples (fewer at the ends).
fn smooth(v: &[f32], half: usize) -> Vec<f32> {
    if half == 0 {
        return v.to_vec();
    }
    let mut pre = Vec::with_capacity(v.len() + 1);
    pre.push(0f64);
    for &x in v {
        let last = pre[pre.len() - 1];
        pre.push(last + x as f64);
    }
    (0..v.len())
        .map(|j| {
            let lo = j.saturating_sub(half);
            let hi = (j + half + 1).min(v.len());
            ((pre[hi] - pre[lo]) / (hi - lo) as f64) as f32
        })
        .collect()
}

/// Rotate a `w` x `h` grid clockwise by `rotation` degrees (0, 90, 180,
/// 270), like the picture is turned for display. Returns the cells and the
/// new width and height.
pub fn rotate_cells(cells: &[f32], w: usize, h: usize, rotation: i32) -> (Vec<f32>, usize, usize) {
    if cells.len() != w * h {
        return (cells.to_vec(), w, h);
    }
    match rotation.rem_euclid(360) {
        90 => {
            // New size h x w: (x', y') shows the source's (y', h - 1 - x').
            let out = (0..w).flat_map(|y2| (0..h).map(move |x2| (x2, y2))).map(|(x2, y2)| cells[(h - 1 - x2) * w + y2]).collect();
            (out, h, w)
        }
        180 => (cells.iter().rev().copied().collect(), w, h),
        270 => {
            // (x', y') shows the source's (w - 1 - y', x').
            let out = (0..w).flat_map(|y2| (0..h).map(move |x2| (x2, y2))).map(|(x2, y2)| cells[x2 * w + (w - 1 - y2)]).collect();
            (out, h, w)
        }
        _ => (cells.to_vec(), w, h),
    }
}

/// The signals of one clip on a grid of `rate` samples per second, from
/// its frames (any order; frames with another cell count than the first
/// are left out). `None` with fewer than two usable frames or under three
/// samples.
pub fn features(frames: &[FrameSig], rate: f64) -> Option<Features> {
    if !(rate.is_finite() && rate > 0.0) {
        return None;
    }
    let cells = frames.first()?.cells.len();
    if cells == 0 {
        return None;
    }
    let mut fr: Vec<&FrameSig> = frames.iter().filter(|f| f.t.is_finite() && f.cells.len() == cells).collect();
    fr.sort_by(|a, b| a.t.total_cmp(&b.t));
    fr.dedup_by(|a, b| (a.t - b.t).abs() < 1e-6);
    if fr.len() < 2 {
        return None;
    }
    // Log brightness per frame: the whole picture first, then each cell.
    let logs: Vec<Vec<f32>> = fr
        .iter()
        .map(|f| {
            let mean = f.cells.iter().sum::<f32>() / cells as f32;
            std::iter::once(mean).chain(f.cells.iter().copied()).map(|v| (v.max(0.0) + LOG_FLOOR).ln()).collect()
        })
        .collect();
    let t0 = fr[0].t;
    let span = fr[fr.len() - 1].t - t0;
    let n = (span * rate).floor() as usize + 1;
    if n < 3 {
        return None;
    }
    // Linear resampling onto the grid.
    let m = cells + 1;
    let mut res = vec![vec![0f32; n]; m];
    let mut seg = 0;
    for j in 0..n {
        let t = t0 + j as f64 / rate;
        while seg + 2 < fr.len() && fr[seg + 1].t <= t {
            seg += 1;
        }
        let (ta, tb) = (fr[seg].t, fr[seg + 1].t);
        let w = ((t - ta) / (tb - ta)).clamp(0.0, 1.0) as f32;
        for (c, r) in res.iter_mut().enumerate() {
            r[j] = logs[seg][c] * (1.0 - w) + logs[seg + 1][c] * w;
        }
    }
    // Rates of change (per second).
    let rate32 = rate as f32;
    let deriv = |v: &[f32]| -> Vec<f32> {
        let mut d: Vec<f32> = std::iter::once(0.0).chain(v.windows(2).map(|w| (w[1] - w[0]) * rate32)).collect();
        d[0] = d.get(1).copied().unwrap_or(0.0);
        d
    };
    let mut channels = Vec::with_capacity(cells + 2);
    channels.push(deriv(&res[0]));
    let cell_d: Vec<Vec<f32>> = res[1..].iter().map(|r| deriv(r)).collect();
    let motion: Vec<f32> = (0..n).map(|j| cell_d.iter().map(|c| c[j].abs()).sum::<f32>() / cells as f32).collect();
    channels.push(motion);
    channels.extend(cell_d);
    let half = ((SMOOTH_SECS * rate).round() as usize) / 2;
    let channels = channels.iter().map(|c| smooth(c, half)).collect();
    Some(Features { rate, t0, channels })
}

/// Prefix sums of a channel and of its squares.
struct Prefix {
    sum: Vec<f64>,
    sq: Vec<f64>,
}

impl Prefix {
    fn new(v: &[f32]) -> Prefix {
        let mut sum = Vec::with_capacity(v.len() + 1);
        let mut sq = Vec::with_capacity(v.len() + 1);
        let (mut s, mut q) = (0.0, 0.0);
        sum.push(0.0);
        sq.push(0.0);
        for &x in v {
            s += x as f64;
            q += x as f64 * x as f64;
            sum.push(s);
            sq.push(q);
        }
        Prefix { sum, sq }
    }

    /// (sum, sum of squares) of `lo..hi`.
    fn range(&self, lo: usize, hi: usize) -> (f64, f64) {
        (self.sum[hi] - self.sum[lo], self.sq[hi] - self.sq[lo])
    }
}

fn ncc(cov: f64, va: f64, vb: f64) -> Option<f64> {
    (va > FLAT && vb > FLAT).then(|| (cov / (va * vb).sqrt()).clamp(-1.0, 1.0))
}

/// The combined correlation when A's sample `j` meets B's sample `j + l`;
/// NaN when the clips share too little or nothing changes.
fn score(a: &Features, b: &Features, pa: &[Prefix], pb: &[Prefix], l: i64, min_ov: usize) -> f64 {
    let (na, nb) = (a.len() as i64, b.len() as i64);
    let j0 = (-l).max(0);
    let j1 = na.min(nb - l);
    if j1 - j0 < min_ov as i64 {
        return f64::NAN;
    }
    let (j0, j1) = (j0 as usize, j1 as usize);
    let k0 = (j0 as i64 + l) as usize;
    let len = j1 - j0;
    let n = len as f64;
    let stats = |c: usize| -> (f64, f64, f64) {
        let xa = &a.channels[c][j0..j1];
        let xb = &b.channels[c][k0..k0 + len];
        let sab: f64 = xa.iter().zip(xb).map(|(&x, &y)| x as f64 * y as f64).sum();
        let (sa, saa) = pa[c].range(j0, j1);
        let (sb, sbb) = pb[c].range(k0, k0 + len);
        (sab - sa * sb / n, saa - sa * sa / n, sbb - sb * sb / n)
    };
    let (mut num, mut wsum) = (0.0, 0.0);
    for (c, w) in [(0, W_GLOBAL), (1, W_MOTION)] {
        let (cov, va, vb) = stats(c);
        if let Some(v) = ncc(cov, va, vb) {
            num += w * v;
            wsum += w;
        }
    }
    if a.channels.len() > 2 {
        let (mut cov, mut va, mut vb) = (0.0, 0.0, 0.0);
        for c in 2..a.channels.len() {
            let s = stats(c);
            cov += s.0;
            va += s.1;
            vb += s.2;
        }
        if let Some(v) = ncc(cov, va, vb) {
            num += W_GRID * v;
            wsum += W_GRID;
        }
    }
    if wsum > 0.0 {
        num / wsum
    } else {
        f64::NAN
    }
}

/// The offset of clip `b` relative to clip `a` (see [`Alignment`]). `None`
/// when the signals do not fit together (different rates or grids) or
/// nothing changes in the shared time.
pub fn align(a: &Features, b: &Features, p: &Params) -> Option<Alignment> {
    let rate = a.rate;
    if (a.rate - b.rate).abs() > 1e-9 || a.channels.len() != b.channels.len() || a.channels.len() < 2 || a.is_empty() || b.is_empty() {
        return None;
    }
    let (na, nb) = (a.len(), b.len());
    let min_ov = ((p.min_overlap.max(0.0) * rate).ceil() as usize).max(na.min(nb) / 4).max(8);
    // Offset d = l / rate + b.t0 - a.t0.
    let shift = (a.t0 - b.t0) * rate;
    let max_lag = p.max_lag.abs();
    let lo = (-max_lag * rate + shift).floor() as i64;
    let hi = (max_lag * rate + shift).ceil() as i64;
    let pa: Vec<Prefix> = a.channels.iter().map(|c| Prefix::new(c)).collect();
    let pb: Vec<Prefix> = b.channels.iter().map(|c| Prefix::new(c)).collect();
    let scores: Vec<f64> = (lo..=hi).into_par_iter().map(|l| score(a, b, &pa, &pb, l, min_ov)).collect();
    let (best, p1) = scores.iter().copied().enumerate().filter(|(_, s)| s.is_finite()).max_by(|x, y| x.1.total_cmp(&y.1))?;
    // Sub-sample peak from a parabola through its neighbours.
    let mut delta = 0.0;
    if best > 0 && best + 1 < scores.len() {
        let (y0, y2) = (scores[best - 1], scores[best + 1]);
        let denom = y0 - 2.0 * p1 + y2;
        if y0.is_finite() && y2.is_finite() && denom < 0.0 {
            delta = (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5);
        }
    }
    let l = lo as f64 + best as f64 + delta;
    let offset = l / rate + b.t0 - a.t0;
    // The best competing peak outside the main lobe.
    let rmin = ((LOBE_MIN_SECS * rate).round() as usize).max(2);
    let rmax = ((LOBE_MAX_SECS * rate).round() as usize).max(rmin);
    let half = p1 * 0.5;
    let (mut left, mut right) = (best, best);
    while left > 0 && best - left < rmax && scores[left - 1].is_finite() && scores[left - 1] > half {
        left -= 1;
    }
    while right + 1 < scores.len() && right - best < rmax && scores[right + 1].is_finite() && scores[right + 1] > half {
        right += 1;
    }
    let left = left.min(best.saturating_sub(rmin));
    let right = right.max(best + rmin);
    let is_peak = |k: usize| {
        let s = scores[k];
        let below = |o: Option<&f64>| o.is_none_or(|v| !v.is_finite() || *v <= s);
        s.is_finite() && below(k.checked_sub(1).and_then(|i| scores.get(i))) && below(scores.get(k + 1))
    };
    let second = (0..scores.len()).filter(|&k| k < left || k > right).filter(|&k| is_peak(k)).map(|k| scores[k]).fold(0.0f64, f64::max);
    let confidence = if p1 > 0.0 {
        let sharp = ((1.0 - second / p1) / 0.6).clamp(0.0, 1.0);
        let strength = (p1 / 0.25).clamp(0.0, 1.0);
        sharp * strength
    } else {
        0.0
    };
    Some(Alignment { offset, peak: p1, second, confidence })
}

/// [`features`] of both clips at `p.rate`, then [`align`].
pub fn estimate(a: &[FrameSig], b: &[FrameSig], p: &Params) -> Option<Alignment> {
    align(&features(a, p.rate)?, &features(b, p.rate)?, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            // xorshift64*
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// A scene seen by a fixed camera: each cell has a base brightness;
    /// things passing through cells and lights add short bumps.
    struct Scene {
        base: Vec<f64>,
        /// (time, cell, amplitude, width)
        bumps: Vec<(f64, usize, f64, f64)>,
        /// (time, amplitude, width): the whole picture.
        flashes: Vec<(f64, f64, f64)>,
    }

    const CELLS: usize = 16;

    fn scene(seed: u64, secs: f64) -> Scene {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let base = (0..CELLS).map(|_| 0.25 + 0.4 * r.next()).collect();
        let (t0, t1) = (-15.0, secs + 15.0);
        let n = ((t1 - t0) * 4.0) as usize;
        let bumps = (0..n)
            .map(|_| {
                let t = t0 + (t1 - t0) * r.next();
                let cell = ((r.next() * CELLS as f64) as usize).min(CELLS - 1);
                let amp = (0.1 + 0.2 * r.next()) * if r.next() < 0.5 { -1.0 } else { 1.0 };
                (t, cell, amp, 0.1 + 0.3 * r.next())
            })
            .collect();
        let flashes = (0..((t1 - t0) * 0.3) as usize).map(|_| (t0 + (t1 - t0) * r.next(), 0.05 + 0.1 * r.next(), 0.15 + 0.3 * r.next())).collect();
        Scene { base, bumps, flashes }
    }

    impl Scene {
        fn at(&self, t: f64) -> Vec<f64> {
            let g = |tc: f64, w: f64| (-((t - tc) / w).powi(2)).exp();
            let flash: f64 = self.flashes.iter().map(|&(tc, a, w)| a * g(tc, w)).sum();
            let mut v: Vec<f64> = self.base.iter().map(|b| b + flash).collect();
            for &(tc, c, a, w) in &self.bumps {
                if (t - tc).abs() < 4.0 * w {
                    v[c] += a * g(tc, w);
                }
            }
            v
        }
    }

    /// A camera recording `scene` at `fps` for `secs`, `delay` seconds
    /// after it (what happens at scene time T shows at T + delay), with an
    /// exposure `gain`, a black level `lift` and noise.
    fn record(s: &Scene, fps: f64, secs: f64, delay: f64, gain: f64, lift: f64, noise: f64, seed: u64) -> Vec<FrameSig> {
        let mut r = Rng(seed | 1);
        (0..(secs * fps) as usize)
            .map(|k| {
                let t = k as f64 / fps;
                let cells = s.at(t - delay).iter().map(|v| ((gain * v + lift + noise * (r.next() - 0.5)).clamp(0.0, 1.0)) as f32).collect();
                FrameSig { t, cells }
            })
            .collect()
    }

    #[test]
    fn finds_a_known_offset_despite_exposure_and_frame_rate() {
        let s = scene(1, 25.0);
        let a = record(&s, 30000.0 / 1001.0, 25.0, 0.0, 1.0, 0.0, 0.01, 11);
        let b = record(&s, 60.0, 25.0, 1.237, 0.6, 0.02, 0.01, 12);
        let al = estimate(&a, &b, &Params::default()).unwrap();
        assert!((al.offset - 1.237).abs() < 0.01, "{al:?}");
        assert_eq!(al.level(), Confidence::High, "{al:?}");
    }

    #[test]
    fn finds_a_negative_offset() {
        let s = scene(2, 25.0);
        let a = record(&s, 30.0, 25.0, 0.0, 1.0, 0.0, 0.01, 21);
        let b = record(&s, 30.0, 25.0, -2.5, 1.4, 0.0, 0.01, 22);
        let al = estimate(&a, &b, &Params::default()).unwrap();
        assert!((al.offset + 2.5).abs() < 0.01, "{al:?}");
        assert!(al.level() >= Confidence::Medium, "{al:?}");
    }

    #[test]
    fn sub_frame_precision() {
        let s = scene(3, 20.0);
        let a = record(&s, 30.0, 20.0, 0.0, 1.0, 0.0, 0.005, 31);
        let b = record(&s, 30.0, 20.0, 0.0123, 1.0, 0.0, 0.005, 32);
        let al = estimate(&a, &b, &Params { max_lag: 5.0, ..Params::default() }).unwrap();
        // A frame lasts 33 ms; the estimate is within a fraction of that.
        assert!((al.offset - 0.0123).abs() < 0.006, "{al:?}");
    }

    #[test]
    fn different_scenes_have_low_confidence() {
        let a = record(&scene(4, 25.0), 30.0, 25.0, 0.0, 1.0, 0.0, 0.01, 41);
        let b = record(&scene(5, 25.0), 30.0, 25.0, 0.0, 1.0, 0.0, 0.01, 42);
        let al = estimate(&a, &b, &Params::default()).unwrap();
        assert!(al.level() <= Confidence::Low, "{al:?}");
    }

    #[test]
    fn still_pictures_cannot_be_aligned() {
        let still: Vec<FrameSig> = (0..300).map(|k| FrameSig { t: k as f64 / 30.0, cells: vec![0.5; CELLS] }).collect();
        assert!(estimate(&still, &still, &Params::default()).is_none());
        assert!(features(&still[..1], 100.0).is_none());
        assert!(features(&[], 100.0).is_none());
    }

    #[test]
    fn features_resample_onto_the_grid() {
        // Brightness doubling every second: log change ln 2 per second everywhere.
        let frames: Vec<FrameSig> = (0..31).map(|k| FrameSig { t: 1.0 + k as f64 / 30.0, cells: vec![(0.1 * 2f64.powf(k as f64 / 30.0)) as f32 - LOG_FLOOR; 4] }).collect();
        let f = features(&frames, 100.0).unwrap();
        assert_eq!(f.t0, 1.0);
        assert_eq!(f.len(), 101);
        assert_eq!(f.channels.len(), 6);
        for c in [0, 1, 2, 5] {
            assert!(f.channels[c][10..90].iter().all(|v| (v - std::f32::consts::LN_2).abs() < 0.02), "channel {c}: {:?}", &f.channels[c][..20]);
        }
    }

    #[test]
    fn grid_rotation() {
        // 3 x 2: 0 1 2 / 3 4 5
        let g = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
        // Clockwise: 3 0 / 4 1 / 5 2
        assert_eq!(rotate_cells(&g, 3, 2, 90), (vec![3.0, 0.0, 4.0, 1.0, 5.0, 2.0], 2, 3));
        assert_eq!(rotate_cells(&g, 3, 2, 180), (vec![5.0, 4.0, 3.0, 2.0, 1.0, 0.0], 3, 2));
        // Counter-clockwise: 2 5 / 1 4 / 0 3
        assert_eq!(rotate_cells(&g, 3, 2, 270), (vec![2.0, 5.0, 1.0, 4.0, 0.0, 3.0], 2, 3));
        assert_eq!(rotate_cells(&g, 3, 2, -90).0, rotate_cells(&g, 3, 2, 270).0);
        assert_eq!(rotate_cells(&g, 3, 2, 0).0, g.to_vec());
    }
}
