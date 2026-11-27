//! Pure parts of playing a video inside the viewer and the slideshow: the
//! transport bar's time text and seek bar arithmetic, frame steps without a
//! frame index, the playback clock and when a video slide has had its time.
//! No FFmpeg and no UI state, so it is built (and tested) without the
//! `video` feature too.

// Without the video player only the slideshow's parts are used.
#![cfg_attr(not(feature = "video"), allow(dead_code))]

use std::time::Instant;

/// Seconds Shift+Left / Shift+Right (and the transport's skip) move.
pub const SKIP_SECS: f64 = 5.0;

/// A position or length on the transport bar: "0:07", "12:34", "1:02:03".
/// Whole seconds, rounded down, so the time only reaches the length at the
/// very end. `total` picks the format, so position and length look alike.
pub fn clock_text(t: f64, total: f64) -> String {
    let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
    let s = t.floor() as u64;
    let long = total.is_finite() && total >= 3600.0;
    if long || s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// "0:01 / 0:04": the transport bar's readout. The length is rounded to the
/// nearest second (a 3.97 s clip reads 0:04).
pub fn position_text(t: f64, duration: f64) -> String {
    let d = if duration.is_finite() { duration.max(0.0) } else { 0.0 };
    format!("{} / {}", clock_text(t, d), clock_text(d.round(), d))
}

/// Video time under `x` on a seek bar that starts at `x0` and is `width` wide.
pub fn time_at(x: f32, x0: f32, width: f32, duration: f64) -> f64 {
    if !(width > 0.0) || !(duration > 0.0) || !duration.is_finite() || !x.is_finite() {
        return 0.0;
    }
    let f = ((x - x0) / width).clamp(0.0, 1.0) as f64;
    f * duration
}

/// How much of the seek bar is filled at `t`.
pub fn fraction(t: f64, duration: f64) -> f32 {
    if !(duration > 0.0) || !duration.is_finite() || !t.is_finite() {
        return 0.0;
    }
    (t / duration).clamp(0.0, 1.0) as f32
}

/// `t` kept inside the video: 0 to `last` (the last moment a seek can target).
pub fn clamp_time(t: f64, last: f64) -> f64 {
    let last = if last.is_finite() { last.max(0.0) } else { 0.0 };
    if t.is_finite() {
        t.clamp(0.0, last)
    } else {
        0.0
    }
}

/// A frame step of `delta` frames from the frame at `from` (frames `fd`
/// seconds long), without a frame index. Returns where to seek and the time
/// the wanted frame should have. The decoder shows the first frame at or
/// after the seek time, so the seek goes half a frame before the wanted one:
/// one frame on is `from + fd / 2` (never `from` itself), one frame back is
/// `from - 1.5 fd` (never the frame before that), and small timestamp jitter
/// does not matter.
pub fn step_target(from: f64, fd: f64, delta: i64, last: f64) -> (f64, f64) {
    let fd = if fd.is_finite() && fd > 0.0 { fd } else { 1.0 / 25.0 };
    let from = if from.is_finite() { from } else { 0.0 };
    let expect = clamp_time(from + delta as f64 * fd, last);
    let seek = clamp_time(from + (delta as f64 - 0.5) * fd, last);
    (seek, expect)
}

/// Where a skip of `delta` seconds from `t` lands.
pub fn skip_target(t: f64, delta: f64, last: f64) -> f64 {
    clamp_time(t + delta, last)
}

/// The icon of the mute button for this volume.
pub fn volume_icon(volume: f32, muted: bool) -> &'static str {
    use crate::ui::icons as I;
    if muted || volume <= 0.001 {
        I::SPEAKER_SLASH
    } else if volume < 0.5 {
        I::SPEAKER_LOW
    } else {
        I::SPEAKER_HIGH
    }
}

/// Slideshow: a video slide has had its time when the video ended, or when
/// it played for `max_secs` (0: no limit). `played` counts the seconds it
/// was on screen and not paused.
pub fn video_slide_due(ended: bool, played: f32, max_secs: f32) -> bool {
    ended || (max_secs > 0.0 && played >= max_secs)
}

/// Slideshow: how far a video slide is, for the progress line. The video's
/// own position, or the time limit when that comes first.
pub fn video_slide_progress(pos: f64, duration: f64, played: f32, max_secs: f32) -> f32 {
    let own = fraction(pos, duration);
    if max_secs > 0.0 && (!(duration > 0.0) || (max_secs as f64) < duration) {
        (played / max_secs).clamp(0.0, 1.0)
    } else {
        own
    }
}

/// Slideshow: a video that shows no picture this long after it became the
/// current slide (a decoder that hangs) is skipped.
pub const VIDEO_SLIDE_STALL_SECS: f32 = 10.0;

/// The playback clock: video time that runs with the system clock while
/// playing and stands still while paused.
#[derive(Clone, Copy, Debug)]
pub struct PlayClock {
    base: f64,
    started: Instant,
    running: bool,
}

impl Default for PlayClock {
    fn default() -> Self {
        PlayClock { base: 0.0, started: Instant::now(), running: false }
    }
}

impl PlayClock {
    /// Video time now.
    pub fn now(&self) -> f64 {
        self.at(Instant::now())
    }

    /// Video time at `now`.
    pub fn at(&self, now: Instant) -> f64 {
        if self.running {
            self.base + now.saturating_duration_since(self.started).as_secs_f64()
        } else {
            self.base
        }
    }

    /// Run from `at` (now).
    pub fn start(&mut self, at: f64) {
        self.start_at(at, Instant::now());
    }

    fn start_at(&mut self, at: f64, now: Instant) {
        self.base = at;
        self.started = now;
        self.running = true;
    }

    /// Stand still at the current time.
    pub fn stop(&mut self) {
        self.stop_at(Instant::now());
    }

    fn stop_at(&mut self, now: Instant) {
        self.base = self.at(now);
        self.running = false;
    }

    /// Move to `at`, running or not as before.
    pub fn set(&mut self, at: f64) {
        if self.running {
            self.start(at);
        } else {
            self.base = at;
        }
    }

    /// The time the clock was last started or stopped at.
    pub fn base(&self) -> f64 {
        self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn time_text() {
        assert_eq!(clock_text(0.0, 4.0), "0:00");
        assert_eq!(clock_text(3.99, 4.0), "0:03");
        assert_eq!(clock_text(65.2, 600.0), "1:05");
        assert_eq!(clock_text(754.0, 800.0), "12:34");
        assert_eq!(clock_text(3723.0, 4000.0), "1:02:03");
        // A long video shows hours from the start, so both sides look alike.
        assert_eq!(clock_text(5.0, 4000.0), "0:00:05");
        assert_eq!(clock_text(-3.0, 4.0), "0:00");
        assert_eq!(clock_text(f64::NAN, 4.0), "0:00");
        assert_eq!(position_text(1.5, 4.0), "0:01 / 0:04");
        assert_eq!(position_text(1.5, 3.97), "0:01 / 0:04");
        assert_eq!(position_text(0.0, f64::INFINITY), "0:00 / 0:00");
    }

    #[test]
    fn seek_bar_math() {
        assert_eq!(time_at(100.0, 100.0, 200.0, 4.0), 0.0);
        assert_eq!(time_at(200.0, 100.0, 200.0, 4.0), 2.0);
        assert_eq!(time_at(300.0, 100.0, 200.0, 4.0), 4.0);
        // Past either end of the bar: the start or the end.
        assert_eq!(time_at(50.0, 100.0, 200.0, 4.0), 0.0);
        assert_eq!(time_at(900.0, 100.0, 200.0, 4.0), 4.0);
        // No width or no length: never NaN.
        assert_eq!(time_at(150.0, 100.0, 0.0, 4.0), 0.0);
        assert_eq!(time_at(150.0, 100.0, 200.0, 0.0), 0.0);
        assert_eq!(time_at(f32::NAN, 100.0, 200.0, 4.0), 0.0);
        assert_eq!(fraction(1.0, 4.0), 0.25);
        assert_eq!(fraction(9.0, 4.0), 1.0);
        assert_eq!(fraction(1.0, 0.0), 0.0);
        assert_eq!(fraction(f64::NAN, 4.0), 0.0);
        assert_eq!(clamp_time(5.0, 3.999), 3.999);
        assert_eq!(clamp_time(-1.0, 3.999), 0.0);
        assert_eq!(clamp_time(f64::NAN, 3.999), 0.0);
        assert_eq!(skip_target(1.0, SKIP_SECS, 3.999), 3.999);
        assert_eq!(skip_target(1.0, -SKIP_SECS, 3.999), 0.0);
        assert_eq!(skip_target(10.0, -SKIP_SECS, 30.0), 5.0);
    }

    #[test]
    fn frame_steps_land_between_frames() {
        let fd = 1.0 / 30.0;
        let last = 4.0 - 0.001;
        let from = 30.0 * fd; // frame 30
        // The decoder shows the first frame at or after the seek time.
        let first_at_or_after = |t: f64| (0..120).map(|i| i as f64 * fd).find(|p| *p >= t - 1e-4).unwrap();
        for (delta, frame) in [(1i64, 31usize), (-1, 29), (2, 32), (-3, 27)] {
            let (seek, expect) = step_target(from, fd, delta, last);
            assert!((first_at_or_after(seek) - frame as f64 * fd).abs() < 1e-9, "{delta}");
            assert!((expect - frame as f64 * fd).abs() < 1e-9, "{delta}");
        }
        // Timestamps a little off the grid still land on the neighbours.
        let (seek, _) = step_target(from + 0.004, fd, 1, last);
        assert!((first_at_or_after(seek) - 31.0 * fd).abs() < 1e-9);
        // At the start a step back stays on the first frame.
        let (seek, expect) = step_target(0.0, fd, -1, last);
        assert_eq!((seek, expect), (0.0, 0.0));
        // At the end a step on stays inside the video.
        let (seek, expect) = step_target(last, fd, 1, last);
        assert_eq!((seek, expect), (last, last));
        // A file without a frame rate steps by 1/25 s.
        let (seek, _) = step_target(1.0, 0.0, 1, last);
        assert!((seek - 1.02).abs() < 1e-9);
    }

    #[test]
    fn slideshow_timing() {
        // Plays to its end without a limit.
        assert!(!video_slide_due(false, 30.0, 0.0));
        assert!(video_slide_due(true, 3.0, 0.0));
        // A limit ends long videos early.
        assert!(!video_slide_due(false, 9.9, 10.0));
        assert!(video_slide_due(false, 10.0, 10.0));
        // Progress follows the video, or the limit when it comes first.
        assert_eq!(video_slide_progress(1.0, 4.0, 1.0, 0.0), 0.25);
        assert_eq!(video_slide_progress(1.0, 4.0, 1.0, 10.0), 0.25);
        assert_eq!(video_slide_progress(30.0, 120.0, 5.0, 10.0), 0.5);
        assert_eq!(video_slide_progress(0.0, 0.0, 20.0, 10.0), 1.0);
    }

    #[test]
    fn clock_runs_only_while_playing() {
        let t0 = Instant::now();
        let mut c = PlayClock::default();
        assert_eq!(c.at(t0 + Duration::from_secs(5)), 0.0);
        c.start_at(2.0, t0);
        assert!(c.running);
        assert!((c.at(t0 + Duration::from_millis(1500)) - 3.5).abs() < 1e-9);
        // A time before the start does not run backwards.
        if let Some(before) = t0.checked_sub(Duration::from_secs(1)) {
            assert_eq!(c.at(before), 2.0);
        }
        c.stop_at(t0 + Duration::from_secs(1));
        assert!(!c.running);
        assert!((c.base() - 3.0).abs() < 1e-9);
        assert!((c.at(t0 + Duration::from_secs(9)) - 3.0).abs() < 1e-9);
        c.set(1.0);
        assert_eq!(c.now(), 1.0);
        assert!(!c.running);
    }

    #[test]
    fn mute_icon_follows_volume() {
        use crate::ui::icons as I;
        assert_eq!(volume_icon(1.0, false), I::SPEAKER_HIGH);
        assert_eq!(volume_icon(0.3, false), I::SPEAKER_LOW);
        assert_eq!(volume_icon(1.0, true), I::SPEAKER_SLASH);
        assert_eq!(volume_icon(0.0, false), I::SPEAKER_SLASH);
    }
}
