//! Full-window slideshow: GPU transitions, optional Ken Burns motion, captions,
//! a progress indicator and an auto-hiding control bar.
//!
//! Keys (shown in the on-screen help, `H`):
//!   Space / P / click  pause or resume (FastStone convention: Space pauses)
//!   → / PgDn / wheel ↓ next      ← / PgUp / wheel ↑ previous
//!   Home / End         first / last
//!   + / −              longer / shorter interval
//!   C caption, K Ken Burns, S shuffle, L loop, H help, Esc exit.
//!
//! Rendering never builds new textures during a transition: the next slide is
//! decoded in the background and its GPU tiles are uploaded ("pre-warmed")
//! while the current slide is on screen. Transitions are pure functions of the
//! eased progress (`transition_frame`) that describe up to two layers
//! (offset / scale / alpha / clip); the renderer turns those into tinted,
//! transformed and clipped `GpuImage::paint` calls.

use super::icons as I;
use super::widgets;
use crate::app::Core;
use crate::loader::LoadState;
use crate::render::GpuImage;
use crate::settings::FitMode;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Modifiers, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- transitions

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transition {
    None,
    Crossfade,
    FadeBlack,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    PushLeft,
    PushRight,
    PushUp,
    PushDown,
    ZoomIn,
    ZoomOut,
    WipeLeft,
    WipeRight,
    WipeUp,
    WipeDown,
    Iris,
    Blinds,
    BlindsVertical,
    Random,
}

impl Transition {
    pub const ALL: [Transition; 21] = [
        Transition::Crossfade,
        Transition::FadeBlack,
        Transition::SlideLeft,
        Transition::SlideRight,
        Transition::SlideUp,
        Transition::SlideDown,
        Transition::PushLeft,
        Transition::PushRight,
        Transition::PushUp,
        Transition::PushDown,
        Transition::ZoomIn,
        Transition::ZoomOut,
        Transition::WipeLeft,
        Transition::WipeRight,
        Transition::WipeUp,
        Transition::WipeDown,
        Transition::Iris,
        Transition::Blinds,
        Transition::BlindsVertical,
        Transition::Random,
        Transition::None,
    ];

    /// Transitions `Random` picks from.
    pub const CONCRETE: [Transition; 19] = [
        Transition::Crossfade,
        Transition::FadeBlack,
        Transition::SlideLeft,
        Transition::SlideRight,
        Transition::SlideUp,
        Transition::SlideDown,
        Transition::PushLeft,
        Transition::PushRight,
        Transition::PushUp,
        Transition::PushDown,
        Transition::ZoomIn,
        Transition::ZoomOut,
        Transition::WipeLeft,
        Transition::WipeRight,
        Transition::WipeUp,
        Transition::WipeDown,
        Transition::Iris,
        Transition::Blinds,
        Transition::BlindsVertical,
    ];

    /// Stable name stored in the settings file.
    pub fn name(self) -> &'static str {
        match self {
            Transition::None => "None",
            Transition::Crossfade => "Crossfade",
            Transition::FadeBlack => "FadeBlack",
            Transition::SlideLeft => "SlideLeft",
            Transition::SlideRight => "SlideRight",
            Transition::SlideUp => "SlideUp",
            Transition::SlideDown => "SlideDown",
            Transition::PushLeft => "PushLeft",
            Transition::PushRight => "PushRight",
            Transition::PushUp => "PushUp",
            Transition::PushDown => "PushDown",
            Transition::ZoomIn => "ZoomIn",
            Transition::ZoomOut => "ZoomOut",
            Transition::WipeLeft => "WipeLeft",
            Transition::WipeRight => "WipeRight",
            Transition::WipeUp => "WipeUp",
            Transition::WipeDown => "WipeDown",
            Transition::Iris => "Iris",
            Transition::Blinds => "Blinds",
            Transition::BlindsVertical => "BlindsVertical",
            Transition::Random => "Random",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Transition::None => "None (cut)",
            Transition::Crossfade => "Crossfade",
            Transition::FadeBlack => "Fade through black",
            Transition::SlideLeft => "Slide in from right",
            Transition::SlideRight => "Slide in from left",
            Transition::SlideUp => "Slide in from bottom",
            Transition::SlideDown => "Slide in from top",
            Transition::PushLeft => "Push left",
            Transition::PushRight => "Push right",
            Transition::PushUp => "Push up",
            Transition::PushDown => "Push down",
            Transition::ZoomIn => "Zoom in",
            Transition::ZoomOut => "Zoom out",
            Transition::WipeLeft => "Wipe right → left",
            Transition::WipeRight => "Wipe left → right",
            Transition::WipeUp => "Wipe bottom → top",
            Transition::WipeDown => "Wipe top → bottom",
            Transition::Iris => "Iris (circle reveal)",
            Transition::Blinds => "Blinds (horizontal)",
            Transition::BlindsVertical => "Blinds (vertical)",
            Transition::Random => "Random",
        }
    }

    /// Parse a settings value. Accepts loose spellings ("Slide", "fade", …).
    pub fn from_name(s: &str) -> Transition {
        let k: String = s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_ascii_lowercase();
        for t in Transition::ALL {
            if t.name().to_ascii_lowercase() == k {
                return t;
            }
        }
        match k.as_str() {
            "fade" | "fadethroughblack" | "fadetoblack" => Transition::FadeBlack,
            "slide" => Transition::SlideLeft,
            "push" => Transition::PushLeft,
            "zoom" => Transition::ZoomIn,
            "wipe" => Transition::WipeRight,
            "circle" | "irisreveal" => Transition::Iris,
            "cut" | "off" => Transition::None,
            _ => Transition::Crossfade,
        }
    }
}

/// Ease-in-out cubic on [0, 1].
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Linear progress of a timed effect, clamped to [0, 1]. A zero duration is complete.
pub fn progress(elapsed: f32, duration: f32) -> f32 {
    if duration <= 0.0 {
        1.0
    } else {
        (elapsed / duration).clamp(0.0, 1.0)
    }
}

/// Where a layer is visible, in normalized view coordinates (0..1).
#[derive(Clone, Debug, PartialEq)]
pub enum Clip {
    Full,
    Rect(Rect),
    Strips(Vec<Rect>),
    /// Circle centred at `center`; `radius` 1.0 reaches the view corners.
    Circle { center: Pos2, radius: f32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// Translation as a fraction of the view size.
    pub offset: Vec2,
    /// Scale about the view centre.
    pub scale: f32,
    pub alpha: f32,
    pub clip: Clip,
    /// Draw fully opaque where the incoming layer covers it (avoids the
    /// mid-crossfade brightness dip); `alpha` applies elsewhere.
    pub hold_under_next: bool,
}

impl Layer {
    pub fn full() -> Layer {
        Layer { offset: Vec2::ZERO, scale: 1.0, alpha: 1.0, clip: Clip::Full, hold_under_next: false }
    }
    fn alpha(a: f32) -> Layer {
        Layer { alpha: a.clamp(0.0, 1.0), ..Layer::full() }
    }
}

/// The outgoing (`from`, drawn first) and incoming (`to`, drawn on top) layers.
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionFrame {
    pub from: Layer,
    pub to: Layer,
}

const BLINDS: usize = 10;

/// Describe transition `kind` at eased progress `t` (0 = only the old image, 1 = only the new one).
pub fn transition_frame(kind: Transition, t: f32) -> TransitionFrame {
    let t = t.clamp(0.0, 1.0);
    let s = 1.0 - t;
    let full = Layer::full;
    match kind {
        Transition::None | Transition::Random => TransitionFrame { from: Layer::alpha(0.0), to: full() },
        Transition::Crossfade => TransitionFrame { from: Layer { hold_under_next: true, ..Layer::alpha(s) }, to: Layer::alpha(t) },
        Transition::FadeBlack => TransitionFrame { from: Layer::alpha(1.0 - 2.0 * t), to: Layer::alpha(2.0 * t - 1.0) },
        Transition::SlideLeft => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(s, 0.0), ..full() } },
        Transition::SlideRight => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(-s, 0.0), ..full() } },
        Transition::SlideUp => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(0.0, s), ..full() } },
        Transition::SlideDown => TransitionFrame { from: full(), to: Layer { offset: Vec2::new(0.0, -s), ..full() } },
        Transition::PushLeft => TransitionFrame {
            from: Layer { offset: Vec2::new(-t, 0.0), ..full() },
            to: Layer { offset: Vec2::new(s, 0.0), ..full() },
        },
        Transition::PushRight => TransitionFrame {
            from: Layer { offset: Vec2::new(t, 0.0), ..full() },
            to: Layer { offset: Vec2::new(-s, 0.0), ..full() },
        },
        Transition::PushUp => TransitionFrame {
            from: Layer { offset: Vec2::new(0.0, -t), ..full() },
            to: Layer { offset: Vec2::new(0.0, s), ..full() },
        },
        Transition::PushDown => TransitionFrame {
            from: Layer { offset: Vec2::new(0.0, t), ..full() },
            to: Layer { offset: Vec2::new(0.0, -s), ..full() },
        },
        Transition::ZoomIn => TransitionFrame {
            from: Layer { scale: 1.0 + 0.5 * t, hold_under_next: true, ..Layer::alpha(s) },
            to: Layer { scale: 0.7 + 0.3 * t, ..Layer::alpha(t) },
        },
        Transition::ZoomOut => TransitionFrame {
            from: Layer { scale: 1.0 - 0.3 * t, ..Layer::alpha(s) },
            to: Layer { scale: 1.4 - 0.4 * t, ..Layer::alpha(t) },
        },
        Transition::WipeLeft => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(s, 0.0, 1.0, 1.0)), ..full() } },
        Transition::WipeRight => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(0.0, 0.0, t, 1.0)), ..full() } },
        Transition::WipeUp => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(0.0, s, 1.0, 1.0)), ..full() } },
        Transition::WipeDown => TransitionFrame { from: full(), to: Layer { clip: Clip::Rect(nrect(0.0, 0.0, 1.0, t)), ..full() } },
        Transition::Iris => TransitionFrame { from: full(), to: Layer { clip: Clip::Circle { center: Pos2::new(0.5, 0.5), radius: t }, ..full() } },
        Transition::Blinds | Transition::BlindsVertical => {
            let step = 1.0 / BLINDS as f32;
            let strips = (0..BLINDS)
                .map(|k| {
                    let a = k as f32 * step;
                    if kind == Transition::Blinds {
                        nrect(0.0, a, 1.0, a + step * t)
                    } else {
                        nrect(a, 0.0, a + step * t, 1.0)
                    }
                })
                .collect();
            TransitionFrame { from: full(), to: Layer { clip: Clip::Strips(strips), ..full() } }
        }
    }
}

fn nrect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1))
}

fn denorm(r: Rect, view: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(view.min.x + r.min.x * view.width(), view.min.y + r.min.y * view.height()),
        Pos2::new(view.min.x + r.max.x * view.width(), view.min.y + r.max.y * view.height()),
    )
}

/// Screen-space clip rectangles for `clip` inside `view`. Circles become a
/// stack of horizontal strips (at most ~160, so GPU tiles stay resident).
pub fn clip_rects(clip: &Clip, view: Rect) -> Vec<Rect> {
    match clip {
        Clip::Full => vec![view],
        Clip::Rect(r) => vec![denorm(*r, view)],
        Clip::Strips(v) => v.iter().map(|r| denorm(*r, view)).filter(|r| r.width() > 0.0 && r.height() > 0.0).collect(),
        Clip::Circle { center, radius } => {
            let c = Pos2::new(view.min.x + center.x * view.width(), view.min.y + center.y * view.height());
            let r = radius * 0.5 * view.size().length();
            if r <= 0.0 {
                return Vec::new();
            }
            if r >= 0.5 * view.size().length() - 0.5 {
                return vec![view];
            }
            let step = (view.height() / 160.0).max(2.0);
            let y0 = (c.y - r).max(view.min.y);
            let y1 = (c.y + r).min(view.max.y);
            let mut out = Vec::new();
            let mut y = y0;
            while y < y1 {
                let yb = (y + step).min(y1);
                // Widest chord within the strip (nearest row to the centre).
                let dy = if c.y >= y && c.y <= yb { 0.0 } else { (c.y - y).abs().min((c.y - yb).abs()) };
                let hw = (r * r - dy * dy).max(0.0).sqrt();
                if hw > 0.0 {
                    out.push(Rect::from_min_max(Pos2::new(c.x - hw, y), Pos2::new(c.x + hw, yb)).intersect(view));
                }
                y = yb;
            }
            out
        }
    }
}

/// Up to four rectangles covering `outer` minus `hole`.
pub fn rect_minus(outer: Rect, hole: Rect) -> Vec<Rect> {
    let h = hole.intersect(outer);
    if h.width() <= 0.0 || h.height() <= 0.0 {
        return vec![outer];
    }
    let mut v = Vec::new();
    let push = |v: &mut Vec<Rect>, r: Rect| {
        if r.width() > 0.0 && r.height() > 0.0 {
            v.push(r);
        }
    };
    push(&mut v, Rect::from_min_max(outer.min, Pos2::new(outer.max.x, h.min.y)));
    push(&mut v, Rect::from_min_max(Pos2::new(outer.min.x, h.max.y), outer.max));
    push(&mut v, Rect::from_min_max(Pos2::new(outer.min.x, h.min.y), Pos2::new(h.min.x, h.max.y)));
    push(&mut v, Rect::from_min_max(Pos2::new(h.max.x, h.min.y), Pos2::new(outer.max.x, h.max.y)));
    v
}

// ---------------------------------------------------------------- Ken Burns / randomness

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

pub fn seed_of(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

/// Slow pan/zoom of one slide, deterministic per seed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KenBurns {
    pub s0: f32,
    pub s1: f32,
    /// Pan positions in [-1, 1] (fraction of the available overflow).
    pub p0: Vec2,
    pub p1: Vec2,
}

impl KenBurns {
    pub fn from_seed(seed: u64) -> KenBurns {
        let r: Vec<f32> = (0..6).map(|i| unit(splitmix(seed ^ (i as u64).wrapping_mul(0xA24B_AED4_963E_E407)))).collect();
        let lo = 1.02 + 0.04 * r[0];
        let hi = 1.14 + 0.08 * r[1];
        let (s0, s1) = if r[2] < 0.5 { (lo, hi) } else { (hi, lo) };
        let p0 = Vec2::new(r[3] * 2.0 - 1.0, r[4] * 2.0 - 1.0);
        // Head roughly to the opposite side so there is visible motion.
        let p1 = Vec2::new(-p0.x * (0.4 + 0.6 * r[5]), -p0.y * (0.4 + 0.6 * (1.0 - r[5])));
        KenBurns { s0, s1, p0, p1 }
    }

    /// (scale, pan) at linear time `u` in [0, 1].
    pub fn at(&self, u: f32) -> (f32, Vec2) {
        let u = u.clamp(0.0, 1.0);
        // Gentle ease so motion starts/stops softly.
        let e = u * u * (3.0 - 2.0 * u) * 0.5 + u * 0.5;
        (self.s0 + (self.s1 - self.s0) * e, self.p0 + (self.p1 - self.p0) * e)
    }

    /// Apply to a fitted destination rect: grows it by `scale` and pans within
    /// the overflow so the original fitted area always stays covered.
    pub fn apply(fit: Rect, scale: f32, pan: Vec2) -> Rect {
        let size = fit.size() * scale.max(1.0);
        let room = (size - fit.size()) * 0.5;
        let c = fit.center() + Vec2::new(pan.x.clamp(-1.0, 1.0) * room.x, pan.y.clamp(-1.0, 1.0) * room.y);
        Rect::from_center_size(c, size)
    }
}

/// Zoom (physical px per image px) for showing `img` in a view of `view_px` physical pixels.
pub fn fit_zoom(mode: FitMode, img: (u32, u32), view_px: Vec2) -> f32 {
    let (w, h) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let fit = (view_px.x / w).min(view_px.y / h);
    match mode {
        FitMode::FitLarge => fit.min(1.0),
        FitMode::FitAlways => fit,
        FitMode::Fill => (view_px.x / w).max(view_px.y / h),
        FitMode::FitWidth => view_px.x / w,
        FitMode::FitHeight => view_px.y / h,
        FitMode::Actual => 1.0,
    }
}

// ---------------------------------------------------------------- order

/// Next position in a show of `n` slides (None at the end when not looping).
pub fn step_pos(pos: usize, n: usize, delta: isize, looping: bool) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let p = pos as isize + delta;
    if p >= 0 && (p as usize) < n {
        Some(p as usize)
    } else if looping {
        Some(p.rem_euclid(n as isize) as usize)
    } else {
        None
    }
}

/// A play order over `n` items starting with `first`; shuffled when `random`.
pub fn make_order(n: usize, first: usize, random: bool, seed: u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    if random && n > 1 {
        let mut s = seed;
        for i in (1..n).rev() {
            s = splitmix(s);
            let j = (s % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
        if let Some(k) = v.iter().position(|&x| x == first) {
            v.swap(0, k);
        }
    }
    v
}

const INTERVALS: [f32; 15] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 15.0, 20.0, 30.0, 45.0, 60.0, 120.0];
const DURATIONS: [f32; 7] = [0.3, 0.5, 0.8, 1.0, 1.5, 2.0, 3.0];

pub fn next_interval(cur: f32, up: bool) -> f32 {
    if up {
        INTERVALS.iter().copied().find(|&v| v > cur + 0.01).unwrap_or(INTERVALS[INTERVALS.len() - 1])
    } else {
        INTERVALS.iter().rev().copied().find(|&v| v < cur - 0.01).unwrap_or(INTERVALS[0])
    }
}

fn fmt_secs(v: f32) -> String {
    if (v - v.round()).abs() < 0.01 {
        format!("{:.0} s", v)
    } else {
        format!("{:.1} s", v)
    }
}

// ---------------------------------------------------------------- slideshow

struct Slide {
    path: PathBuf,
    gpu: GpuImage,
    /// Seconds since this slide started appearing (drives Ken Burns).
    clock: f32,
    kb: KenBurns,
    /// All tiles for its display zoom are on the GPU.
    warm: bool,
}

struct Active {
    kind: Transition,
    elapsed: f32,
    duration: f32,
}

pub struct Slideshow {
    pub list: Vec<PathBuf>,
    pub index: usize,
    base_list: Vec<PathBuf>,
    order: Vec<usize>,
    pos: usize,
    current: Option<Slide>,
    outgoing: Option<Slide>,
    active: Option<Active>,
    /// Prepared (decoded + GPU) slides waiting to be shown.
    ready: Vec<Slide>,
    /// Target position we are waiting on, and the direction to skip failures.
    pending: Option<(usize, isize)>,
    failed: HashSet<PathBuf>,
    all_failed: bool,
    display: f32,
    paused: bool,
    ended: bool,
    last_tick: Instant,
    last_activity: Instant,
    wheel_acc: f32,
    help: bool,
    flash: Option<(&'static str, Instant)>,
    captions: HashMap<PathBuf, Option<String>>,
    random_count: u64,
    popup_was_open: bool,
    pending_since: Instant,
    exit_requested: bool,
}

impl Slideshow {
    pub fn new(list: Vec<PathBuf>, start: usize, core: &Core) -> Self {
        let start = start.min(list.len().saturating_sub(1));
        let base_list = list.clone();
        let start_path = list.get(start).cloned();
        let mut s = Slideshow {
            list,
            index: start,
            base_list,
            order: Vec::new(),
            pos: 0,
            current: None,
            outgoing: None,
            active: None,
            ready: Vec::new(),
            pending: None,
            failed: HashSet::new(),
            all_failed: false,
            display: 0.0,
            paused: false,
            ended: false,
            last_tick: Instant::now(),
            last_activity: Instant::now(),
            wheel_acc: 0.0,
            help: false,
            flash: None,
            captions: HashMap::new(),
            random_count: 0,
            popup_was_open: false,
            pending_since: Instant::now(),
            exit_requested: false,
        };
        if core.settings.slideshow.include_subfolders {
            s.rebuild_list(true, start_path.as_deref());
        }
        s.reorder(core.settings.slideshow.random_order);
        s.pending = if s.order.is_empty() { None } else { Some((s.pos, 1)) };
        s
    }

    fn rebuild_list(&mut self, include_sub: bool, keep: Option<&Path>) {
        let mut list = self.base_list.clone();
        if include_sub {
            if let Some(dir) = keep.or(self.base_list.first().map(|p| p.as_path())).and_then(|p| p.parent()) {
                let mut all: Vec<PathBuf> = crate::catalog::list_recursive(dir, 50_000).into_iter().map(|e| e.path).collect();
                let direct = all.iter().filter(|p| p.parent() == Some(dir)).count();
                // Only expand when the whole folder was being shown (not a hand-picked selection).
                if self.base_list.len() >= direct && all.len() > self.base_list.len() {
                    all.sort_by(|a, b| {
                        let da = a.parent().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
                        let db = b.parent().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default();
                        (a.parent() != Some(dir))
                            .cmp(&(b.parent() != Some(dir)))
                            .then_with(|| crate::catalog::natural_cmp(&da, &db))
                            .then_with(|| crate::catalog::natural_cmp(&a.file_name().unwrap_or_default().to_string_lossy(), &b.file_name().unwrap_or_default().to_string_lossy()))
                    });
                    // Keep the browser's order for the top folder.
                    let mut merged = self.base_list.clone();
                    merged.extend(all.into_iter().filter(|p| p.parent() != Some(dir)));
                    list = merged;
                }
            }
        }
        self.index = keep.and_then(|k| list.iter().position(|p| p == k)).unwrap_or(0);
        self.list = list;
    }

    fn reorder(&mut self, random: bool) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let seed = splitmix(nanos ^ ((std::process::id() as u64) << 32));
        self.order = make_order(self.list.len(), self.index, random, seed);
        self.pos = self.order.iter().position(|&i| i == self.index).unwrap_or(0);
    }

    fn path_at(&self, pos: usize) -> Option<&PathBuf> {
        self.order.get(pos).and_then(|&i| self.list.get(i))
    }

    fn step(&self, pos: usize, delta: isize, looping: bool) -> Option<usize> {
        step_pos(pos, self.order.len(), delta, looping)
    }

    fn go(&mut self, pos: usize, dir: isize) {
        self.pending = Some((pos, dir));
        self.pending_since = Instant::now();
        self.ended = false;
    }

    fn go_rel(&mut self, delta: isize, looping: bool) {
        let from = self.pending.map(|(p, _)| p).unwrap_or(self.pos);
        if let Some(p) = self.step(from, delta, looping) {
            self.go(p, delta.signum());
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused && self.ended {
            self.ended = false;
            self.display = 0.0;
            if self.step(self.pos, 1, true).is_some() {
                self.go(self.step(self.pos, 1, true).unwrap_or(0), 1);
            }
        }
        self.flash = Some((if self.paused { I::PAUSE } else { I::PLAY }, Instant::now()));
    }

    /// Positions worth having decoded, most urgent first.
    fn wanted(&self, looping: bool) -> Vec<usize> {
        let mut v = Vec::new();
        let base = self.pending.map(|(p, _)| p).unwrap_or(self.pos);
        v.push(base);
        for d in [1isize, 2, -1] {
            if let Some(p) = self.step(base, d, looping) {
                if !v.contains(&p) {
                    v.push(p);
                }
            }
        }
        v
    }

    fn holds(&self, path: &Path) -> bool {
        self.current.as_ref().is_some_and(|s| s.path == path) || self.outgoing.as_ref().is_some_and(|s| s.path == path) || self.ready.iter().any(|s| s.path == path)
    }

    fn take_ready(&mut self, path: &Path) -> Option<Slide> {
        if self.outgoing.as_ref().is_some_and(|s| s.path == path) {
            // Going back to the image we are transitioning away from.
            self.active = None;
            return self.outgoing.take();
        }
        if let Some(i) = self.ready.iter().position(|s| s.path == path) {
            return Some(self.ready.remove(i));
        }
        None
    }

    fn caption_for(&mut self, path: &Path) {
        if !self.captions.contains_key(path) {
            let date = crate::meta::read_exif(path).and_then(|e| e.taken);
            self.captions.insert(path.to_path_buf(), date);
        }
    }

    /// Load/prepare slides and resolve the pending navigation target.
    fn update_loading(&mut self, core: &mut Core) {
        let looping = core.settings.slideshow.loop_show;
        let wanted = self.wanted(looping);
        let mut keep_q = Vec::new();
        for (prio, &pos) in wanted.iter().enumerate() {
            let Some(path) = self.path_at(pos).cloned() else { continue };
            if self.failed.contains(&path) {
                continue;
            }
            if !self.holds(&path) {
                match core.loader.state(&path) {
                    Some(LoadState::Ready(_)) => {
                        if let Some(img) = core.loader.get_ready(&path) {
                            let gpu = GpuImage::new(&core.ctx, &format!("slide:{}", path.display()), img.pyramid.clone());
                            self.caption_for(&path);
                            self.ready.push(Slide { kb: KenBurns::from_seed(seed_of(&path)), path: path.clone(), gpu, clock: 0.0, warm: false });
                        }
                    }
                    Some(LoadState::Failed(_)) => {
                        self.failed.insert(path.clone());
                    }
                    _ => {
                        core.loader.request(&path, prio as u32, None);
                        keep_q.push(path);
                    }
                }
            }
        }
        core.loader.retain_queued(&keep_q);
        // Drop prepared slides no longer near the play position.
        let near: Vec<PathBuf> = wanted.iter().filter_map(|&p| self.path_at(p).cloned()).collect();
        self.ready.retain(|s| near.contains(&s.path));

        // Resolve pending target (skipping failures).
        if let Some((mut pos, dir)) = self.pending {
            let n = self.order.len();
            let mut tries = 0;
            loop {
                let Some(path) = self.path_at(pos).cloned() else {
                    self.pending = None;
                    break;
                };
                if self.failed.contains(&path) {
                    tries += 1;
                    if tries >= n {
                        self.all_failed = true;
                        self.pending = None;
                        break;
                    }
                    match self.step(pos, if dir == 0 { 1 } else { dir }, true) {
                        Some(p) if looping || (dir >= 0 && p > pos) || (dir < 0 && p < pos) => {
                            pos = p;
                            self.pending = Some((p, dir));
                            continue;
                        }
                        _ => {
                            self.pending = None;
                            if self.current.is_none() {
                                self.all_failed = true;
                            }
                            break;
                        }
                    }
                }
                if self.current.as_ref().is_some_and(|c| c.path == path) {
                    self.pending = None;
                    self.pos = pos;
                    break;
                }
                if let Some(slide) = self.take_ready(&path) {
                    self.pending = None;
                    self.pos = pos;
                    self.index = self.order[pos];
                    self.start_transition(slide, core);
                }
                break;
            }
        }
    }

    fn start_transition(&mut self, mut slide: Slide, core: &Core) {
        // A transition still running is finished instantly.
        if self.active.is_some() {
            self.outgoing = None;
        }
        slide.clock = 0.0;
        let old = self.current.take();
        let cfg = &core.settings.slideshow;
        let mut kind = Transition::from_name(&cfg.transition);
        if kind == Transition::Random {
            self.random_count += 1;
            let h = splitmix(seed_of(&slide.path) ^ self.random_count);
            kind = Transition::CONCRETE[(h % Transition::CONCRETE.len() as u64) as usize];
        }
        let mut duration = cfg.transition_secs.clamp(0.0, 5.0);
        if old.is_none() {
            // First slide fades in from black.
            kind = Transition::Crossfade;
            duration = duration.clamp(0.25, 0.6);
        }
        if kind == Transition::None {
            duration = 0.0;
        }
        self.outgoing = old;
        self.current = Some(slide);
        self.display = 0.0;
        self.active = if duration > 0.0 { Some(Active { kind, elapsed: 0.0, duration }) } else { None };
        if self.active.is_none() {
            if let Some(o) = self.outgoing.take() {
                self.ready.push(o);
            }
        }
    }

    // ------------------------------------------------------------ input

    fn handle_input(&mut self, ui: &mut Ui, core: &mut Core, stage: &egui::Response) -> bool {
        let looping = core.settings.slideshow.loop_show;
        let popup_open = egui::Popup::is_any_open(ui.ctx());
        let take = |k: Key| ui.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if take(Key::Escape) {
            if self.help {
                self.help = false;
            } else if !popup_open {
                return false;
            }
        }
        if take(Key::Space) || take(Key::P) {
            self.toggle_pause();
        }
        if take(Key::ArrowRight) || take(Key::PageDown) || take(Key::N) {
            self.go_rel(1, true);
        }
        if take(Key::ArrowLeft) || take(Key::PageUp) || take(Key::Backspace) {
            self.go_rel(-1, true);
        }
        if take(Key::Home) {
            self.go(0, 1);
        }
        if take(Key::End) {
            let n = self.order.len();
            if n > 0 {
                self.go(n - 1, -1);
            }
        }
        let cfg = &mut core.settings.slideshow;
        if take(Key::Plus) || take(Key::Equals) {
            cfg.interval_secs = next_interval(cfg.interval_secs, true);
            self.flash_text();
        }
        if take(Key::Minus) {
            cfg.interval_secs = next_interval(cfg.interval_secs, false);
            self.flash_text();
        }
        if take(Key::C) {
            cfg.show_caption = !cfg.show_caption;
        }
        if take(Key::K) {
            cfg.ken_burns = !cfg.ken_burns;
        }
        if take(Key::L) {
            cfg.loop_show = !cfg.loop_show;
        }
        let mut reshuffle = false;
        if take(Key::S) {
            cfg.random_order = !cfg.random_order;
            reshuffle = true;
        }
        if take(Key::H) || take(Key::F1) {
            self.help = !self.help;
        }
        if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == "?"))) {
            self.help = !self.help;
        }
        if reshuffle {
            let r = core.settings.slideshow.random_order;
            self.reorder(r);
        }
        // Wheel navigates.
        let wd = ui.input(super::wheel_delta);
        if stage.hovered() && wd != 0.0 {
            self.wheel_acc += wd;
            if self.wheel_acc.abs() >= 40.0 {
                let d = if self.wheel_acc > 0.0 { -1 } else { 1 };
                self.wheel_acc = 0.0;
                self.go_rel(d, looping);
            }
        }
        if stage.clicked() && !self.popup_was_open && !popup_open {
            if self.help {
                self.help = false;
            } else {
                self.toggle_pause();
            }
        }
        if stage.secondary_clicked() {
            self.go_rel(-1, true);
        }
        self.popup_was_open = popup_open;
        true
    }

    fn flash_text(&mut self) {
        self.flash = Some((I::TIMER, Instant::now()));
        self.last_activity = Instant::now();
    }

    // ------------------------------------------------------------ drawing

    fn base_dest(slide: &Slide, view: Rect, ppp: f32, fit: FitMode, kb: bool, kb_total: f32) -> (Rect, f32) {
        let dims = (slide.gpu.width, slide.gpu.height);
        let zoom = fit_zoom(fit, dims, view.size() * ppp);
        let size = Vec2::new(dims.0 as f32, dims.1 as f32) * zoom / ppp;
        let mut dest = Rect::from_center_size(view.center(), size);
        let mut z = zoom;
        if kb {
            let (s, pan) = slide.kb.at(progress(slide.clock, kb_total));
            dest = KenBurns::apply(dest, s, pan);
            z *= s.max(1.0);
        } else {
            // Snap to physical pixels for a crisp still image.
            let min = Pos2::new((dest.min.x * ppp).round() / ppp, (dest.min.y * ppp).round() / ppp);
            dest = Rect::from_min_size(min, dest.size());
        }
        (dest, z)
    }

    fn layer_dest(view: Rect, base: (Rect, f32), layer: &Layer) -> (Rect, f32) {
        let (dest, zoom) = base;
        let c = view.center() + (dest.center() - view.center()) * layer.scale + layer.offset * view.size();
        (Rect::from_center_size(c, dest.size() * layer.scale), zoom * layer.scale)
    }

    fn paint_layer(ctx: &egui::Context, painter: &egui::Painter, view: Rect, slide: &mut Slide, d: (Rect, f32), layer: &Layer, hold: Option<Rect>) {
        if layer.alpha <= 0.002 {
            // Still draw the held part (under the incoming image) if any.
            if hold.is_none() {
                return;
            }
        }
        let (dest, zoom) = d;
        let tint = Color32::WHITE.gamma_multiply(layer.alpha.clamp(0.0, 1.0));
        let clips = clip_rects(&layer.clip, view);
        for clip in clips {
            let clip = clip.intersect(view);
            if clip.width() <= 0.0 || clip.height() <= 0.0 {
                continue;
            }
            match hold {
                Some(h) => {
                    let inside = clip.intersect(h);
                    if inside.width() > 0.0 && inside.height() > 0.0 {
                        slide.gpu.paint(ctx, &painter.with_clip_rect(inside), dest, inside, zoom, false, Color32::WHITE);
                    }
                    if layer.alpha > 0.002 {
                        for r in rect_minus(clip, h) {
                            slide.gpu.paint(ctx, &painter.with_clip_rect(r), dest, r, zoom, false, tint);
                        }
                    }
                }
                None => {
                    slide.gpu.paint(ctx, &painter.with_clip_rect(clip), dest, clip, zoom, false, tint);
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        let p = core.pal;
        let view = ui.max_rect();
        let ppp = ui.ctx().pixels_per_point();
        ui.painter().rect_filled(view, 0.0, Color32::BLACK);
        let stage = ui.interact(view, ui.scope_id().with("slideshow-stage"), Sense::click());

        if ui.input(|i| i.pointer.delta() != Vec2::ZERO || i.pointer.any_pressed()) {
            self.last_activity = now;
        }

        if !self.handle_input(ui, core, &stage) {
            return false;
        }
        self.update_loading(core);

        let cfg = core.settings.slideshow.clone();
        let interval = cfg.interval_secs.clamp(0.5, 3600.0);
        let kb_total = interval + 2.0 * cfg.transition_secs.max(0.0);

        // ---- Clocks
        if let Some(a) = &mut self.active {
            a.elapsed += dt.min(0.1);
            if a.elapsed >= a.duration {
                self.active = None;
                // Keep the previous slide around for instant "back".
                if let Some(o) = self.outgoing.take() {
                    self.ready.push(o);
                }
            }
        }
        if !self.paused {
            if let Some(c) = &mut self.current {
                c.clock += dt.min(0.5);
            }
            if let Some(o) = &mut self.outgoing {
                o.clock += dt.min(0.5);
            }
            if self.active.is_none() && self.current.is_some() {
                self.display += dt;
            }
        }

        // ---- Auto advance
        if !self.paused && self.active.is_none() && self.pending.is_none() && self.current.is_some() && self.display >= interval {
            match self.step(self.pos, 1, cfg.loop_show) {
                Some(p) if self.order.len() > 1 => self.go(p, 1),
                _ => {
                    if self.order.len() > 1 {
                        self.ended = true;
                        self.paused = true;
                    }
                }
            }
            if self.pending.is_some() {
                self.update_loading(core);
            }
        }

        // ---- Draw slides
        let painter = ui.painter().with_clip_rect(view);
        let ctx = ui.ctx().clone();
        let t = self.active.as_ref().map(|a| ease_in_out_cubic(progress(a.elapsed, a.duration)));
        match (t, self.active.as_ref().map(|a| a.kind)) {
            (Some(t), Some(kind)) => {
                let frame = transition_frame(kind, t);
                let to_d = self.current.as_ref().map(|c| Self::layer_dest(view, Self::base_dest(c, view, ppp, cfg.fit, cfg.ken_burns, kb_total), &frame.to));
                if let Some(o) = &mut self.outgoing {
                    let base = Self::base_dest(o, view, ppp, cfg.fit, cfg.ken_burns, kb_total);
                    let d = Self::layer_dest(view, base, &frame.from);
                    let hold = if frame.from.hold_under_next { to_d.map(|(r, _)| r.intersect(view)) } else { None };
                    Self::paint_layer(&ctx, &painter, view, o, d, &frame.from, hold);
                }
                if let (Some(c), Some(d)) = (&mut self.current, to_d) {
                    Self::paint_layer(&ctx, &painter, view, c, d, &frame.to, None);
                }
            }
            _ => {
                if let Some(c) = &mut self.current {
                    let d = Self::base_dest(c, view, ppp, cfg.fit, cfg.ken_burns, kb_total);
                    c.gpu.paint(&ctx, &painter, d.0, view, d.1, false, Color32::WHITE);
                    c.warm = true;
                }
            }
        }
        // Pre-warm the next slide's tiles while nothing is moving.
        if self.active.is_none() {
            let hidden = ui.painter().with_clip_rect(Rect::NOTHING);
            if let Some(s) = self.ready.iter_mut().find(|s| !s.warm) {
                let d = Self::base_dest(s, view, ppp, cfg.fit, false, kb_total);
                let pending = s.gpu.paint(&ctx, &hidden, d.0, view, d.1, false, Color32::TRANSPARENT);
                s.warm = !pending;
            }
        }

        // ---- Status / placeholders
        if self.current.is_none() {
            if self.all_failed || self.order.is_empty() {
                painter.text(view.center() - Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(44.0), p.faint);
                painter.text(view.center() + Vec2::new(0.0, 24.0), Align2::CENTER_CENTER, "None of these images could be shown", FontId::proportional(15.0), Color32::from_gray(200));
                painter.text(view.center() + Vec2::new(0.0, 48.0), Align2::CENTER_CENTER, "Press Esc to leave the slideshow", FontId::proportional(12.5), Color32::from_gray(140));
            } else if self.pending_since.elapsed() > Duration::from_millis(250) {
                let r = Rect::from_center_size(view.center(), Vec2::splat(30.0));
                egui::Spinner::new().size(30.0).color(Color32::from_gray(160)).paint_at(ui, r);
            }
        } else if self.pending.is_some() && self.pending_since.elapsed() > Duration::from_millis(400) {
            let r = Rect::from_center_size(view.right_top() + Vec2::new(-28.0, 28.0), Vec2::splat(18.0));
            egui::Spinner::new().size(18.0).color(Color32::from_gray(180)).paint_at(ui, r);
        }

        // ---- Overlays
        let idle = self.last_activity.elapsed() > Duration::from_secs(2);
        let bar_rect = Rect::from_center_size(Pos2::new(view.center().x, view.max.y - 40.0), Vec2::new(820.0_f32.min(view.width() - 24.0), 56.0));
        let hover_bar = ui.input(|i| i.pointer.hover_pos()).is_some_and(|pos| bar_rect.expand(12.0).contains(pos));
        let show_bar = !idle || hover_bar || self.popup_was_open || self.help;
        if cfg.show_caption {
            let y = if show_bar { bar_rect.min.y - 12.0 } else { view.max.y - 18.0 };
            self.paint_caption(ui, view, y);
        }
        if self.ended {
            let text = "End of slideshow  ·  Space to restart  ·  Esc to exit";
            let g = ui.painter().layout_no_wrap(text.into(), FontId::proportional(14.0), Color32::from_white_alpha(235));
            let r = Rect::from_center_size(view.center_top() + Vec2::new(0.0, 40.0), g.size() + Vec2::new(28.0, 14.0));
            ui.painter().rect_filled(r, CornerRadius::same(10), Color32::from_black_alpha(170));
            ui.painter().galley(r.min + Vec2::new(14.0, 7.0), g, Color32::WHITE);
        }
        if show_bar {
            self.control_bar(ui, core, bar_rect, interval);
        } else {
            ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        }
        let mut flashing = false;
        if let Some((icon, at)) = self.flash {
            let a = 1.0 - progress(at.elapsed().as_secs_f32(), 0.7);
            if a <= 0.0 {
                self.flash = None;
            } else {
                flashing = true;
                let r = Rect::from_center_size(view.center(), Vec2::splat(84.0));
                ui.painter().rect_filled(r, CornerRadius::same(42), Color32::from_black_alpha((150.0 * a) as u8));
                if icon == I::TIMER {
                    let text = fmt_secs(core.settings.slideshow.interval_secs);
                    ui.painter().text(r.center() - Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, icon, FontId::proportional(24.0), Color32::from_white_alpha((255.0 * a) as u8));
                    ui.painter().text(r.center() + Vec2::new(0.0, 16.0), Align2::CENTER_CENTER, text, FontId::proportional(14.0), Color32::from_white_alpha((255.0 * a) as u8));
                } else {
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, icon, super::theme::icon_fill(36.0), Color32::from_white_alpha((255.0 * a) as u8));
                }
            }
        }
        if self.help {
            self.help_overlay(ui, view);
        }

        // ---- Repaint scheduling: smooth while moving, near-idle otherwise.
        let warming = self.active.is_none() && self.ready.iter().any(|s| !s.warm);
        let moving = self.active.is_some() || (cfg.ken_burns && self.current.is_some() && !self.paused) || flashing || warming;
        if moving || show_bar {
            ui.ctx().request_repaint();
        } else if !self.paused && self.current.is_some() && self.pending.is_none() {
            let left = (interval - self.display).max(0.0);
            ui.ctx().request_repaint_after(Duration::from_secs_f32(left.min(10.0) + 0.001));
        } else if self.pending.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        !self.exit_requested
    }

    fn paint_caption(&self, ui: &Ui, view: Rect, bottom: f32) {
        let Some(c) = &self.current else { return };
        let name = c.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut parts = vec![format!("{} / {}", self.pos + 1, self.order.len()), name];
        parts.push(format!("{} × {}", c.gpu.width, c.gpu.height));
        if let Some(Some(d)) = self.captions.get(&c.path) {
            parts.push(d.clone());
        }
        let text = parts.join("   ·   ");
        let g = ui.painter().layout(text, FontId::proportional(13.0), Color32::from_white_alpha(235), view.width() - 60.0);
        let r = Rect::from_min_size(Pos2::new(view.min.x + 18.0, bottom - g.size().y - 12.0), g.size() + Vec2::new(24.0, 12.0));
        ui.painter().rect_filled(r, CornerRadius::same(9), Color32::from_black_alpha(140));
        ui.painter().galley(r.min + Vec2::new(12.0, 6.0), g, Color32::WHITE);
    }

    fn control_bar(&mut self, ui: &mut Ui, core: &mut Core, rect: Rect, interval: f32) {
        let p = core.pal;
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(10.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
        // Swallow clicks on the bar background so they don't toggle pause.
        let _ = child.interact(rect, child.scope_id().with("bar-blocker"), Sense::click());
        let bg = Color32::from_rgba_unmultiplied(18, 19, 22, 225);
        ui.painter().rect_filled(rect, CornerRadius::same(14), bg);
        ui.painter().rect_stroke(rect, CornerRadius::same(14), Stroke::new(1.0, Color32::from_white_alpha(22)), egui::StrokeKind::Inside);
        // Progress of the current slide.
        let frac = if self.current.is_some() { progress(self.display, interval) } else { 0.0 };
        let track = Rect::from_min_size(rect.min + Vec2::new(14.0, 5.0), Vec2::new(rect.width() - 28.0, 3.0));
        ui.painter().rect_filled(track, CornerRadius::same(2), Color32::from_white_alpha(28));
        let mut fill = track;
        fill.max.x = track.min.x + track.width() * frac;
        ui.painter().rect_filled(fill, CornerRadius::same(2), if self.paused { p.muted } else { p.accent });

        // Dark styling for widgets regardless of the app theme.
        let vis = child.visuals_mut();
        *vis = egui::Visuals::dark();
        vis.widgets.inactive.weak_bg_fill = Color32::from_white_alpha(14);
        vis.widgets.inactive.bg_fill = Color32::from_white_alpha(14);
        vis.widgets.hovered.weak_bg_fill = Color32::from_white_alpha(30);
        vis.widgets.inactive.bg_stroke = Stroke::NONE;
        vis.selection.bg_fill = p.accent.gamma_multiply(0.8);
        child.spacing_mut().item_spacing.x = 2.0;
        child.add_space(2.0);

        let looping = core.settings.slideshow.loop_show;
        if widgets::icon_button(&mut child, I::SKIP_BACK, "Previous  (← / PgUp)", false).clicked() {
            self.go_rel(-1, true);
        }
        let (icon, tip) = if self.paused { (I::PLAY, "Play  (Space)") } else { (I::PAUSE, "Pause  (Space)") };
        if widgets::icon_button_sized(&mut child, icon, tip, false, 34.0, 20.0).clicked() {
            self.toggle_pause();
        }
        if widgets::icon_button(&mut child, I::SKIP_FORWARD, "Next  (→ / PgDn)", false).clicked() {
            self.go_rel(1, true);
        }
        let counter = format!("{} / {}", (self.pending.map(|(p, _)| p).unwrap_or(self.pos) + 1).min(self.order.len()), self.order.len());
        child.add_sized([64.0, 26.0], egui::Label::new(RichText::new(counter).size(12.0).color(Color32::from_gray(200))));
        widgets::separator_v(&mut child);

        let cfg = &mut core.settings.slideshow;
        child.label(RichText::new(I::TIMER).size(15.0).color(Color32::from_gray(190))).on_hover_text("Time per slide  (+ / −)");
        egui::ComboBox::from_id_salt("ss-interval").width(62.0).selected_text(fmt_secs(cfg.interval_secs)).show_ui(&mut child, |ui| {
            for v in INTERVALS {
                ui.selectable_value(&mut cfg.interval_secs, v, fmt_secs(v));
            }
        });
        child.add_space(4.0);
        child.label(RichText::new(I::SPARKLE).size(15.0).color(Color32::from_gray(190))).on_hover_text("Transition");
        let cur = Transition::from_name(&cfg.transition);
        egui::ComboBox::from_id_salt("ss-transition").width(150.0).selected_text(cur.label()).height(420.0).show_ui(&mut child, |ui| {
            for t in Transition::ALL {
                if ui.selectable_label(cur == t, t.label()).clicked() {
                    cfg.transition = t.name().to_string();
                }
            }
        });
        egui::ComboBox::from_id_salt("ss-duration").width(56.0).selected_text(fmt_secs(cfg.transition_secs)).show_ui(&mut child, |ui| {
            for v in DURATIONS {
                ui.selectable_value(&mut cfg.transition_secs, v, fmt_secs(v));
            }
        }).response.on_hover_text("Transition duration");
        widgets::separator_v(&mut child);

        let mut reshuffle = None;
        if widgets::icon_button(&mut child, I::SHUFFLE, "Shuffle  (S)", cfg.random_order).clicked() {
            cfg.random_order = !cfg.random_order;
            reshuffle = Some(cfg.random_order);
        }
        if widgets::icon_button(&mut child, I::REPEAT, "Loop  (L)", looping).clicked() {
            cfg.loop_show = !cfg.loop_show;
        }
        if widgets::icon_button(&mut child, I::FRAME_CORNERS, "Ken Burns pan & zoom  (K)", cfg.ken_burns).clicked() {
            cfg.ken_burns = !cfg.ken_burns;
        }
        if widgets::icon_button(&mut child, I::CLOSED_CAPTIONING, "Caption  (C)", cfg.show_caption).clicked() {
            cfg.show_caption = !cfg.show_caption;
        }
        let mut subfolders = None;
        if widgets::icon_button(&mut child, I::FOLDERS, "Include subfolders", cfg.include_subfolders).clicked() {
            cfg.include_subfolders = !cfg.include_subfolders;
            subfolders = Some(cfg.include_subfolders);
        }
        widgets::separator_v(&mut child);
        if widgets::icon_button(&mut child, I::QUESTION, "Keyboard shortcuts  (H)", self.help).clicked() {
            self.help = !self.help;
        }
        if widgets::icon_button(&mut child, I::X, "Exit slideshow  (Esc)", false).clicked() {
            self.exit_requested = true;
        }
        if let Some(r) = reshuffle {
            self.reorder(r);
        }
        if let Some(sub) = subfolders {
            let keep = self.current.as_ref().map(|c| c.path.clone()).or_else(|| self.list.get(self.index).cloned());
            self.rebuild_list(sub, keep.as_deref());
            let r = core.settings.slideshow.random_order;
            self.reorder(r);
            core.toasts.info(format!("{} images in the slideshow", self.list.len()));
        }
    }

    fn help_overlay(&self, ui: &Ui, view: Rect) {
        let rows: [(&str, &str); 13] = [
            ("Space / P / click", "Pause or resume"),
            ("→  PgDn  wheel ↓", "Next image"),
            ("←  PgUp  wheel ↑", "Previous image"),
            ("Right-click", "Previous image"),
            ("Home / End", "First / last image"),
            ("+ / −", "Longer / shorter interval"),
            ("C", "Show / hide caption"),
            ("K", "Ken Burns pan & zoom"),
            ("S", "Shuffle"),
            ("L", "Loop"),
            ("H  ?", "This help"),
            ("F11", "Toggle full screen"),
            ("Esc", "Exit slideshow"),
        ];
        let w = 380.0;
        let h = 56.0 + rows.len() as f32 * 24.0;
        let r = Rect::from_center_size(view.center(), Vec2::new(w, h));
        let painter = ui.painter();
        painter.rect_filled(r, CornerRadius::same(14), Color32::from_rgba_unmultiplied(18, 19, 22, 238));
        painter.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, Color32::from_white_alpha(26)), egui::StrokeKind::Inside);
        painter.text(r.min + Vec2::new(22.0, 22.0), Align2::LEFT_CENTER, "Slideshow shortcuts", super::theme::semibold(15.0), Color32::WHITE);
        for (i, (k, v)) in rows.iter().enumerate() {
            let y = r.min.y + 52.0 + i as f32 * 24.0;
            painter.text(Pos2::new(r.min.x + 22.0, y), Align2::LEFT_CENTER, *k, FontId::proportional(13.0), Color32::from_gray(150));
            painter.text(Pos2::new(r.min.x + 170.0, y), Align2::LEFT_CENTER, *v, FontId::proportional(13.0), Color32::from_gray(235));
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_endpoints_and_symmetry() {
        assert_eq!(ease_in_out_cubic(0.0), 0.0);
        assert_eq!(ease_in_out_cubic(1.0), 1.0);
        assert!((ease_in_out_cubic(0.5) - 0.5).abs() < 1e-6);
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            assert!((ease_in_out_cubic(t) + ease_in_out_cubic(1.0 - t) - 1.0).abs() < 1e-5);
        }
        // Monotonic.
        let mut last = -1.0;
        for i in 0..=100 {
            let v = ease_in_out_cubic(i as f32 / 100.0);
            assert!(v >= last);
            last = v;
        }
        assert_eq!(ease_in_out_cubic(-1.0), 0.0);
        assert_eq!(ease_in_out_cubic(2.0), 1.0);
    }

    #[test]
    fn progress_clamps() {
        assert_eq!(progress(0.0, 1.0), 0.0);
        assert_eq!(progress(0.5, 1.0), 0.5);
        assert_eq!(progress(3.0, 1.0), 1.0);
        assert_eq!(progress(0.0, 0.0), 1.0);
    }

    #[test]
    fn transition_names_round_trip() {
        for t in Transition::ALL {
            assert_eq!(Transition::from_name(t.name()), t);
        }
        assert_eq!(Transition::from_name("Slide"), Transition::SlideLeft);
        assert_eq!(Transition::from_name("fade through black"), Transition::FadeBlack);
        assert_eq!(Transition::from_name("garbage"), Transition::Crossfade);
    }

    #[test]
    fn transition_frames_start_and_end_states() {
        for kind in Transition::CONCRETE {
            let end = transition_frame(kind, 1.0);
            assert!((end.to.alpha - 1.0).abs() < 1e-6, "{kind:?}");
            assert_eq!(end.to.offset, Vec2::ZERO, "{kind:?}");
            assert!((end.to.scale - 1.0).abs() < 1e-6, "{kind:?}");
            let view = Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 900.0));
            let area: f32 = clip_rects(&end.to.clip, view).iter().map(|r| r.area()).sum();
            assert!(area >= view.area() * 0.999, "{kind:?} end not fully revealed ({area})");

            let start = transition_frame(kind, 0.0);
            let visible_area: f32 = clip_rects(&start.to.clip, view).iter().map(|r| r.area()).sum();
            let hidden = start.to.alpha == 0.0 || visible_area == 0.0 || start.to.offset.length() >= 1.0 || start.to.scale != 1.0;
            assert!(hidden || kind == Transition::None, "{kind:?} should start hidden");
        }
    }

    #[test]
    fn wipe_and_push_midpoints() {
        let f = transition_frame(Transition::WipeRight, 0.5);
        assert_eq!(f.to.clip, Clip::Rect(nrect(0.0, 0.0, 0.5, 1.0)));
        let f = transition_frame(Transition::PushLeft, 0.25);
        assert_eq!(f.from.offset, Vec2::new(-0.25, 0.0));
        assert_eq!(f.to.offset, Vec2::new(0.75, 0.0));
        let f = transition_frame(Transition::FadeBlack, 0.5);
        assert!(f.from.alpha.abs() < 1e-6 && f.to.alpha.abs() < 1e-6);
        let f = transition_frame(Transition::Blinds, 0.5);
        match f.to.clip {
            Clip::Strips(v) => {
                assert_eq!(v.len(), BLINDS);
                let covered: f32 = v.iter().map(|r| r.height()).sum();
                assert!((covered - 0.5).abs() < 1e-5);
            }
            _ => panic!("blinds use strips"),
        }
    }

    #[test]
    fn iris_strips_approximate_circle() {
        let view = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0));
        let half_diag = 0.5 * view.size().length();
        let radius = 0.5; // normalized
        let rects = clip_rects(&Clip::Circle { center: Pos2::new(0.5, 0.5), radius }, view);
        assert!(!rects.is_empty() && rects.len() <= 200);
        let area: f32 = rects.iter().map(|r| r.area()).sum();
        let r = radius * half_diag;
        let expect = std::f32::consts::PI * r * r;
        assert!((area - expect).abs() / expect < 0.03, "area {area} vs {expect}");
        assert!(clip_rects(&Clip::Circle { center: Pos2::new(0.5, 0.5), radius: 0.0 }, view).is_empty());
    }

    #[test]
    fn rect_minus_covers_complement() {
        let outer = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0));
        let hole = Rect::from_min_max(Pos2::new(20.0, 10.0), Pos2::new(60.0, 40.0));
        let parts = rect_minus(outer, hole);
        let area: f32 = parts.iter().map(|r| r.area()).sum();
        assert!((area - (outer.area() - hole.area())).abs() < 1e-3);
        for p in &parts {
            assert!(p.intersect(hole).area() <= 0.0 || p.intersect(hole).width() <= 0.0 || p.intersect(hole).height() <= 0.0);
        }
        assert_eq!(rect_minus(outer, Rect::from_min_size(Pos2::new(200.0, 200.0), Vec2::splat(5.0))), vec![outer]);
    }

    #[test]
    fn ken_burns_is_deterministic_and_keeps_fit_covered() {
        let a = KenBurns::from_seed(42);
        let b = KenBurns::from_seed(42);
        assert_eq!(a, b);
        assert_ne!(KenBurns::from_seed(1), KenBurns::from_seed(2));
        let fit = Rect::from_min_size(Pos2::new(100.0, 0.0), Vec2::new(800.0, 600.0));
        for i in 0..=10 {
            let (s, pan) = a.at(i as f32 / 10.0);
            assert!(s >= 1.0 && s < 1.3);
            let d = KenBurns::apply(fit, s, pan);
            assert!(d.contains_rect(fit.shrink(0.01)), "{d:?} must cover {fit:?}");
        }
    }

    #[test]
    fn order_and_stepping() {
        assert_eq!(step_pos(0, 5, 1, false), Some(1));
        assert_eq!(step_pos(4, 5, 1, false), None);
        assert_eq!(step_pos(4, 5, 1, true), Some(0));
        assert_eq!(step_pos(0, 5, -1, true), Some(4));
        assert_eq!(step_pos(0, 0, 1, true), None);
        let o = make_order(10, 3, false, 7);
        assert_eq!(o, (0..10).collect::<Vec<_>>());
        let o = make_order(10, 3, true, 7);
        assert_eq!(o[0], 3);
        let mut s = o.clone();
        s.sort();
        assert_eq!(s, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn interval_steps() {
        assert_eq!(next_interval(4.0, true), 5.0);
        assert_eq!(next_interval(4.0, false), 3.0);
        assert_eq!(next_interval(4.5, true), 5.0);
        assert_eq!(next_interval(1.0, false), 1.0);
        assert_eq!(next_interval(120.0, true), 120.0);
    }

    #[test]
    fn fit_zoom_modes() {
        let v = Vec2::new(1920.0, 1080.0);
        assert!((fit_zoom(FitMode::FitAlways, (3840, 2160), v) - 0.5).abs() < 1e-6);
        assert!((fit_zoom(FitMode::FitLarge, (960, 540), v) - 1.0).abs() < 1e-6);
        assert!((fit_zoom(FitMode::FitAlways, (960, 540), v) - 2.0).abs() < 1e-6);
        assert!((fit_zoom(FitMode::Fill, (1000, 1000), v) - 1.92).abs() < 1e-6);
    }
}
