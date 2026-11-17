//! Frame timing checks from a video's frame index (presentation timestamps,
//! packet sizes and key flags): the real frame rate against the declared
//! one, variable frame rate, dropped frames, duplicated frames, timestamp
//! jitter and stutter, summed up in one plain-language verdict.
//!
//! Everything is measured against the local frame interval (the median of
//! the intervals around each frame), so a clip that changes its frame rate
//! on purpose (phones in low light) is not reported as dropping frames.

/// One frame of the index.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStamp {
    /// Presentation time in seconds.
    pub pts: f64,
    /// Coded size of the frame's packet in bytes.
    pub size: u32,
    /// The packet is a keyframe.
    pub key: bool,
    /// Further packets with this same timestamp that the indexer merged
    /// into this entry.
    pub repeats: u32,
}

/// What went wrong at a place in the video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueKind {
    /// A gap longer than 1.5 frame intervals: frames are missing.
    Dropped,
    /// Two or more frames with the same timestamp.
    RepeatedTimestamp,
    /// Non-key frames of only a few bytes, which normally repeat the
    /// previous picture.
    LikelyDuplicate,
    /// Frames shown noticeably early or late (by more than a quarter of a
    /// frame interval) without frames missing.
    Stutter,
}

impl IssueKind {
    pub fn label(self) -> &'static str {
        match self {
            IssueKind::Dropped => "Dropped",
            IssueKind::RepeatedTimestamp => "Repeated timestamp",
            IssueKind::LikelyDuplicate => "Likely duplicate",
            IssueKind::Stutter => "Stutter",
        }
    }
}

/// One problem place.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Issue {
    pub kind: IssueKind,
    /// Frame number (position in presentation order) where it shows: the
    /// first frame after a gap, the repeated or tiny frame, the first late
    /// or early frame.
    pub frame: usize,
    /// Where to look, in seconds: the last frame before a gap (step forward
    /// to see the jump), the repeated or tiny frame, the frame before a
    /// stutter.
    pub t: f64,
    /// Frames missing (drops), frames affected (duplicates, stutter).
    pub frames: u32,
    /// Missing time (drops) or the largest timing error (stutter), seconds.
    pub secs: f64,
}

/// The result of [`analyze`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimingReport {
    /// Frames in the index.
    pub frames: usize,
    /// From the first frame to the end of the last one, seconds.
    pub duration: f64,
    /// The frame rate the file declares (0 when unknown).
    pub declared_fps: f64,
    /// The real frame rate: 1 / the mean interval between frames, gaps
    /// left out.
    pub measured_fps: f64,
    /// Frames per second over the whole clip, gaps included.
    pub average_fps: f64,
    /// Median interval between frames, seconds.
    pub median_interval: f64,
    /// Slowest and fastest local frame rate.
    pub min_fps: f64,
    pub max_fps: f64,
    /// The frame rate changes during the clip.
    pub vfr: bool,
    /// Frames missing in all gaps, the number of gaps and their total length.
    pub dropped_frames: u32,
    pub dropped_events: usize,
    pub dropped_secs: f64,
    /// Frames whose timestamp repeats an earlier one.
    pub repeated_timestamps: u32,
    /// Tiny non-key frames that probably repeat the previous picture.
    pub likely_duplicates: u32,
    /// Places where frames come noticeably early or late.
    pub stutter_events: usize,
    /// Spread of the frame intervals around the local interval: standard
    /// deviation and largest deviation, milliseconds (gaps left out).
    pub jitter_std_ms: f64,
    pub jitter_max_ms: f64,
    /// Every problem place, in time order.
    pub issues: Vec<Issue>,
    /// No drops, duplicates or stutter and a constant frame rate.
    pub smooth: bool,
    /// One plain-language line ("Smooth: 29.97 fps, no dropped frames").
    pub verdict: String,
}

/// Timestamps closer than this are the same.
const SAME_TS: f64 = 1e-6;
/// Intervals on each side of a frame for its local interval.
const LOCAL_HALF: usize = 15;
/// A gap is a drop above this many local intervals.
const DROP_RATIO: f64 = 1.5;
/// A frame this far from its local interval (fraction) is early or late.
const STUTTER_DEV: f64 = 0.25;
/// The frame rate is variable when the local interval varies by more than
/// this fraction of the median.
const VFR_SPREAD: f64 = 0.08;
/// Problem places listed in the verdict per kind.
const VERDICT_PLACES: usize = 3;

/// Check the timing of `frames` (any order). `declared_fps` is the rate
/// the container states (0 when unknown); it is only reported.
pub fn analyze(frames: &[FrameStamp], declared_fps: f64) -> TimingReport {
    let mut f: Vec<FrameStamp> = frames.iter().filter(|e| e.pts.is_finite()).copied().collect();
    f.sort_by(|a, b| a.pts.total_cmp(&b.pts));
    let n = f.len();
    let declared_fps = if declared_fps.is_finite() && declared_fps > 0.0 { declared_fps } else { 0.0 };
    let mut r = TimingReport { frames: n, declared_fps, ..Default::default() };

    // Repeated timestamps: merged by the indexer, or still in the list.
    let mut issues = Vec::new();
    for i in 0..n {
        let dup_here = i > 0 && f[i].pts - f[i - 1].pts < SAME_TS;
        let count = f[i].repeats + dup_here as u32;
        if count > 0 {
            r.repeated_timestamps += count;
            issues.push(Issue { kind: IssueKind::RepeatedTimestamp, frame: i, t: f[i].pts, frames: count, secs: 0.0 });
        }
    }
    // Frames with a timestamp of their own, and the intervals between them.
    let distinct: Vec<usize> = (0..n).filter(|&i| i == 0 || f[i].pts - f[i - 1].pts >= SAME_TS).collect();
    if distinct.len() < 3 {
        r.issues = issues;
        r.verdict = "Too few frames to check the timing".into();
        return r;
    }
    let d: Vec<f64> = distinct.windows(2).map(|w| f[w[1]].pts - f[w[0]].pts).collect();
    let med = median(&d);
    let local = local_intervals(&d, LOCAL_HALF);
    r.median_interval = med;
    r.duration = f[n - 1].pts - f[0].pts + med;
    r.average_fps = (distinct.len() - 1) as f64 / (f[n - 1].pts - f[0].pts);

    // Drops, stutter and jitter against the local interval.
    let mut regular = Vec::with_capacity(d.len());
    let mut devs = Vec::with_capacity(d.len());
    let mut stutter: Option<Issue> = None;
    fn flush_stutter(s: &mut Option<Issue>, issues: &mut Vec<Issue>, r: &mut TimingReport) {
        if let Some(st) = s.take() {
            r.stutter_events += 1;
            issues.push(st);
        }
    }
    for (k, (&dk, &lk)) in d.iter().zip(&local).enumerate() {
        let (before, after) = (distinct[k], distinct[k + 1]);
        if dk > DROP_RATIO * lk {
            flush_stutter(&mut stutter, &mut issues, &mut r);
            let missing = ((dk / lk).round() as i64 - 1).max(1) as u32;
            let secs = dk - lk;
            r.dropped_frames += missing;
            r.dropped_events += 1;
            r.dropped_secs += secs;
            issues.push(Issue { kind: IssueKind::Dropped, frame: after, t: f[before].pts, frames: missing, secs });
            continue;
        }
        regular.push(dk);
        let dev = dk - lk;
        devs.push(dev);
        if dev.abs() > STUTTER_DEV * lk {
            match &mut stutter {
                Some(st) => {
                    st.frames += 1;
                    st.secs = st.secs.max(dev.abs());
                }
                None => stutter = Some(Issue { kind: IssueKind::Stutter, frame: after, t: f[before].pts, frames: 1, secs: dev.abs() }),
            }
        } else {
            flush_stutter(&mut stutter, &mut issues, &mut r);
        }
    }
    flush_stutter(&mut stutter, &mut issues, &mut r);

    let mean_regular = if regular.is_empty() { med } else { regular.iter().sum::<f64>() / regular.len() as f64 };
    r.measured_fps = 1.0 / mean_regular;
    if !devs.is_empty() {
        let mean = devs.iter().sum::<f64>() / devs.len() as f64;
        let var = devs.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / devs.len() as f64;
        r.jitter_std_ms = var.sqrt() * 1000.0;
        r.jitter_max_ms = devs.iter().fold(0.0f64, |m, v| m.max(v.abs())) * 1000.0;
    }
    let (lo, hi) = local.iter().fold((f64::INFINITY, 0.0f64), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    r.min_fps = 1.0 / hi;
    r.max_fps = 1.0 / lo;
    // A rate that changes faster than the local window shows up as many
    // "drops": that is a variable rate too.
    r.vfr = (hi - lo) > VFR_SPREAD * med || r.dropped_events as f64 > 0.2 * d.len() as f64;

    // Likely duplicates: runs of tiny non-key frames.
    let mut sizes: Vec<f64> = f.iter().filter(|e| !e.key && e.size > 0).map(|e| e.size as f64).collect();
    if !sizes.is_empty() {
        let med_size = median_mut(&mut sizes);
        let tiny = (med_size * 0.03).min(512.0).max(8.0);
        let mut run: Option<Issue> = None;
        for (i, e) in f.iter().enumerate().skip(1) {
            if !e.key && (e.size as f64) <= tiny {
                r.likely_duplicates += 1;
                match &mut run {
                    Some(x) => x.frames += 1,
                    None => run = Some(Issue { kind: IssueKind::LikelyDuplicate, frame: i, t: e.pts, frames: 1, secs: 0.0 }),
                }
            } else if let Some(x) = run.take() {
                issues.push(x);
            }
        }
        issues.extend(run);
    }

    issues.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.frame.cmp(&b.frame)));
    r.issues = issues;
    r.smooth = !r.vfr && r.dropped_events == 0 && r.repeated_timestamps == 0 && r.likely_duplicates == 0 && r.stutter_events == 0;
    r.verdict = verdict(&r);
    r
}

fn verdict(r: &TimingReport) -> String {
    if r.smooth {
        return format!("Smooth: {} fps, no dropped frames", fmt_fps(r.measured_fps));
    }
    let places = |kind: IssueKind| -> String {
        let list: Vec<&Issue> = r.issues.iter().filter(|i| i.kind == kind).collect();
        let mut s: Vec<String> = list.iter().take(VERDICT_PLACES).map(|i| fmt_pos(i.t)).collect();
        if list.len() > VERDICT_PLACES {
            s.push("...".into());
        }
        s.join(", ")
    };
    let plural = |n: u64, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let mut parts = Vec::new();
    if r.vfr {
        parts.push(format!("Variable frame rate: {} to {} fps", fmt_fps(r.min_fps), fmt_fps(r.max_fps)));
    }
    if r.dropped_events > 0 {
        parts.push(format!("{} ({} s total) at {}", plural(r.dropped_frames as u64, "dropped frame", "dropped frames"), fmt_secs(r.dropped_secs), places(IssueKind::Dropped)));
    }
    if r.repeated_timestamps > 0 {
        parts.push(format!("{} at {}", plural(r.repeated_timestamps as u64, "repeated timestamp", "repeated timestamps"), places(IssueKind::RepeatedTimestamp)));
    }
    if r.likely_duplicates > 0 {
        parts.push(format!("{} at {}", plural(r.likely_duplicates as u64, "likely duplicated frame", "likely duplicated frames"), places(IssueKind::LikelyDuplicate)));
    }
    if r.stutter_events > 0 {
        parts.push(format!("{} at {}", plural(r.stutter_events as u64, "stutter", "stutters"), places(IssueKind::Stutter)));
    }
    if parts.len() == 1 && r.vfr {
        parts[0] += ", no dropped frames";
    }
    parts.join("; ")
}

/// Frame rate for display: up to two decimals, no trailing zeros
/// ("29.97", "30", "23.98").
pub fn fmt_fps(fps: f64) -> String {
    if !fps.is_finite() || fps <= 0.0 {
        return "?".into();
    }
    let s = format!("{fps:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Seconds with one decimal, or two below a second ("0.13", "2.4").
fn fmt_secs(s: f64) -> String {
    if s < 0.995 {
        format!("{s:.2}")
    } else {
        format!("{s:.1}")
    }
}

/// A position in the clip: "00:12.4", or "1:02:03.4" past an hour.
pub fn fmt_pos(t: f64) -> String {
    let t = (t.max(0.0) * 10.0).round() / 10.0;
    let h = (t / 3600.0) as u64;
    let m = ((t % 3600.0) / 60.0) as u64;
    let s = t % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{s:04.1}")
    } else {
        format!("{m:02}:{s:04.1}")
    }
}

/// CSV of reports, one per video: a summary row per video, then one row
/// per problem place. `names` label the videos (letters or file names).
pub fn to_csv(reports: &[(String, &TimingReport)]) -> String {
    let q = |s: &str| if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
    let mut s = String::from("# Lumen frame timing. Intervals are compared with the local frame interval; a gap above 1.5 intervals is a drop.\n");
    s += "video,frames,duration_s,declared_fps,measured_fps,average_fps,min_fps,max_fps,vfr,dropped_frames,dropped_gaps,dropped_s,repeated_timestamps,likely_duplicates,stutters,jitter_std_ms,jitter_max_ms,verdict\n";
    for (name, r) in reports {
        s += &format!(
            "{},{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{},{},{},{:.4},{},{},{},{:.3},{:.3},{}\n",
            q(name.as_str()),
            r.frames,
            r.duration,
            r.declared_fps,
            r.measured_fps,
            r.average_fps,
            r.min_fps,
            r.max_fps,
            r.vfr,
            r.dropped_frames,
            r.dropped_events,
            r.dropped_secs,
            r.repeated_timestamps,
            r.likely_duplicates,
            r.stutter_events,
            r.jitter_std_ms,
            r.jitter_max_ms,
            q(r.verdict.as_str())
        );
    }
    s += "\nvideo,issue,frame,time_s,frames,seconds\n";
    for (name, r) in reports {
        for i in &r.issues {
            s += &format!("{},{},{},{:.4},{},{:.4}\n", q(name.as_str()), i.kind.label(), i.frame, i.t, i.frames, i.secs);
        }
    }
    s
}

fn median(v: &[f64]) -> f64 {
    let mut w = v.to_vec();
    median_mut(&mut w)
}

/// Median (upper middle for even counts) of a non-empty list; reorders it.
fn median_mut(v: &mut [f64]) -> f64 {
    let mid = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    *m
}

/// The local interval at every position: the mean of the intervals within
/// `half` positions (fewer at the ends) that are within 40% of their
/// median. The median alone flips between the values of a jittery pattern;
/// the trimmed mean is steady and still ignores gaps and bunched frames.
fn local_intervals(d: &[f64], half: usize) -> Vec<f64> {
    let mut win = Vec::with_capacity(2 * half + 1);
    (0..d.len())
        .map(|k| {
            let lo = k.saturating_sub(half);
            let hi = (k + half + 1).min(d.len());
            win.clear();
            win.extend_from_slice(&d[lo..hi]);
            let m = median_mut(&mut win);
            let (sum, n) = d[lo..hi].iter().filter(|&&x| x >= 0.6 * m && x <= 1.4 * m).fold((0.0, 0usize), |(s, n), &x| (s + x, n + 1));
            if n > 0 {
                sum / n as f64
            } else {
                m
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamps(pts: &[f64]) -> Vec<FrameStamp> {
        pts.iter().enumerate().map(|(i, &p)| FrameStamp { pts: p, size: if i % 30 == 0 { 50_000 } else { 8_000 }, key: i % 30 == 0, repeats: 0 }).collect()
    }

    fn cfr(n: usize, fps: f64) -> Vec<f64> {
        (0..n).map(|i| i as f64 / fps).collect()
    }

    #[test]
    fn constant_rate_is_smooth() {
        let r = analyze(&stamps(&cfr(300, 30000.0 / 1001.0)), 30000.0 / 1001.0);
        assert!(r.smooth, "{r:?}");
        assert!((r.measured_fps - 29.97).abs() < 0.01);
        assert!(!r.vfr);
        assert_eq!(r.dropped_frames, 0);
        assert!(r.jitter_std_ms < 1e-6);
        assert_eq!(r.verdict, "Smooth: 29.97 fps, no dropped frames");
        assert!((r.duration - 300.0 * 1001.0 / 30000.0).abs() < 1e-9);
    }

    #[test]
    fn millisecond_timestamps_are_still_smooth() {
        // 29.97 fps in a 1 ms time base: intervals of 33 and 34 ms.
        let pts: Vec<f64> = (0..600).map(|i| ((i as f64 * 1001.0 / 30.0).round()) / 1000.0).collect();
        let r = analyze(&stamps(&pts), 0.0);
        assert!(r.smooth, "{r:?}");
        assert!((r.measured_fps - 29.97).abs() < 0.01, "{}", r.measured_fps);
        assert!(r.jitter_max_ms <= 1.0 + 1e-9 && r.jitter_std_ms > 0.0);
    }

    #[test]
    fn drops_are_counted_and_placed() {
        let mut pts = cfr(300, 30.0);
        // Remove 1 frame after 4 s and 3 frames after 6 s.
        pts.retain(|&p| {
            let f = (p * 30.0).round() as i64;
            f != 121 && !(181..=183).contains(&f)
        });
        let r = analyze(&stamps(&pts), 30.0);
        assert!(!r.smooth);
        assert_eq!(r.dropped_events, 2);
        assert_eq!(r.dropped_frames, 4);
        assert!((r.dropped_secs - 4.0 / 30.0).abs() < 1e-9);
        assert!((r.measured_fps - 30.0).abs() < 1e-6);
        let drops: Vec<&Issue> = r.issues.iter().filter(|i| i.kind == IssueKind::Dropped).collect();
        assert!((drops[0].t - 120.0 / 30.0).abs() < 1e-9 && drops[0].frames == 1 && drops[0].frame == 121);
        assert!((drops[1].t - 180.0 / 30.0).abs() < 1e-9 && drops[1].frames == 3);
        assert_eq!(r.verdict, "4 dropped frames (0.13 s total) at 00:04.0, 00:06.0");
        assert!(!r.vfr);
    }

    #[test]
    fn many_drops_list_three_places() {
        let mut pts = cfr(900, 30.0);
        pts.retain(|&p| {
            let f = (p * 30.0).round() as i64;
            !matches!(f, 100 | 300 | 500 | 700)
        });
        let r = analyze(&stamps(&pts), 30.0);
        assert_eq!(r.dropped_frames, 4);
        assert!(r.verdict.ends_with("at 00:03.3, 00:10.0, 00:16.6, ..."), "{}", r.verdict);
    }

    #[test]
    fn variable_rate_is_not_dropping() {
        // 10 s at 30 fps, then 10 s at 20 fps (low light), then 30 again.
        let mut pts = Vec::new();
        let mut t = 0.0;
        for (secs, fps) in [(10.0, 30.0), (10.0, 20.0), (10.0, 30.0)] {
            let n = (secs * fps) as usize;
            for _ in 0..n {
                pts.push(t);
                t += 1.0 / fps;
            }
        }
        let r = analyze(&stamps(&pts), 30.0);
        assert!(r.vfr, "{r:?}");
        assert_eq!(r.dropped_events, 0, "{:?}", r.issues);
        assert!((r.min_fps - 20.0).abs() < 0.01 && (r.max_fps - 30.0).abs() < 0.01);
        assert!(r.verdict.starts_with("Variable frame rate: 20 to 30 fps"), "{}", r.verdict);
        assert!(r.verdict.contains("no dropped frames"), "{}", r.verdict);
    }

    #[test]
    fn repeated_timestamps_and_tiny_frames_are_duplicates() {
        let mut s = stamps(&cfr(120, 30.0));
        // The indexer merged one repeat into frame 10; frame 50 repeats 49's time.
        s[10].repeats = 1;
        s[50].pts = s[49].pts;
        // Frames 70 and 71 are near-empty P-frames.
        s[70].size = 12;
        s[71].size = 9;
        let r = analyze(&s, 30.0);
        assert_eq!(r.repeated_timestamps, 2);
        assert_eq!(r.likely_duplicates, 2);
        let dups: Vec<&Issue> = r.issues.iter().filter(|i| i.kind == IssueKind::LikelyDuplicate).collect();
        assert_eq!(dups.len(), 1);
        assert_eq!((dups[0].frame, dups[0].frames), (70, 2));
        // The repeated timestamp leaves a gap of 2 intervals behind it.
        assert_eq!(r.dropped_frames, 1);
        assert!(r.verdict.contains("2 repeated timestamps at 00:00.3, 00:01.6"), "{}", r.verdict);
        assert!(r.verdict.contains("2 likely duplicated frames at 00:02.3"), "{}", r.verdict);
    }

    #[test]
    fn jitter_and_stutter() {
        // 30 fps with small random-looking jitter (+-2 ms) and one frame 12 ms late.
        let jit = [0.0, 0.002, -0.0015, 0.001, -0.002, 0.0005];
        let mut pts: Vec<f64> = (0..300).map(|i| i as f64 / 30.0 + jit[i % jit.len()]).collect();
        pts[150] += 0.012;
        let r = analyze(&stamps(&pts), 30.0);
        assert_eq!(r.dropped_events, 0);
        assert_eq!(r.stutter_events, 1, "{:?}", r.issues);
        let st = r.issues.iter().find(|i| i.kind == IssueKind::Stutter).unwrap();
        assert_eq!(st.frame, 150);
        assert_eq!(st.frames, 2, "late then early");
        assert!(r.jitter_max_ms > 10.0 && r.jitter_max_ms < 13.0, "{}", r.jitter_max_ms);
        assert!(r.jitter_std_ms > 2.0 && r.jitter_std_ms < 3.5, "{}", r.jitter_std_ms);
        assert!(!r.vfr);
        assert!(r.verdict.contains("1 stutter at 00:05.0"), "{}", r.verdict);
    }

    #[test]
    fn small_and_unsorted_inputs() {
        assert_eq!(analyze(&[], 30.0).verdict, "Too few frames to check the timing");
        assert_eq!(analyze(&stamps(&[0.0, 0.1]), 0.0).frames, 2);
        let mut pts = cfr(60, 25.0);
        pts.reverse();
        let r = analyze(&stamps(&pts), f64::NAN);
        assert!(r.smooth && r.declared_fps == 0.0);
        assert!((r.measured_fps - 25.0).abs() < 1e-6);
    }

    #[test]
    fn formatting_and_csv() {
        assert_eq!(fmt_fps(30.0), "30");
        assert_eq!(fmt_fps(29.97002997), "29.97");
        assert_eq!(fmt_fps(23.976), "23.98");
        assert_eq!(fmt_fps(12.5), "12.5");
        assert_eq!(fmt_pos(12.44), "00:12.4");
        assert_eq!(fmt_pos(59.97), "01:00.0");
        assert_eq!(fmt_pos(3723.4), "1:02:03.4");
        let mut pts = cfr(100, 30.0);
        pts.remove(50);
        let r = analyze(&stamps(&pts), 30.0);
        let csv = to_csv(&[("A, phone".to_string(), &r)]);
        assert!(csv.contains("\"A, phone\",99,"), "{csv}");
        assert!(csv.contains("\"A, phone\",Dropped,50,1.6333,1,0.0333"), "{csv}");
    }
}
