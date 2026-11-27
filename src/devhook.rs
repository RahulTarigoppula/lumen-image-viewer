//! Developer automation hook used for automated UI testing. Only active when
//! the `LUMEN_DEV_DIR` environment variable points at a folder; it then reads
//! `cmd.txt` from that folder (one command per line):
//!   shot <file.png>      save a screenshot of the window
//!   run <Command>        run a command by its debug name, e.g. `run NextImage`
//!   open <path>          open a file or folder
//!   state                append the app state to state.txt
//!   ai <args>            drive the Analysis panel's AI tab (see `analysis_ui::dev`)
//!   key <shortcut>       press a key as the window would get it, e.g. `key Ctrl+Q`
//!   click <x> <y> [right]  click at window point (x, y) (points, not pixels)
//!   drag <x0> <y0> <x1> <y1>  left-drag between two points (over several frames)
//!   type <text>          type text into the focused field
//!   hover <x> <y>        move the pointer to (x, y) without clicking (tooltips)
//!   scroll <dy>          mouse wheel at the pointer, in points (positive = up)
//!   ppp <scale>          act as if the window moved to a display with this
//!                        scale (1.0 = 100%, 1.5 = 150%, ...)
//!   size <w> <h>         resize the window (points)
//!   dialog <path>        the next file dialog that supports it (report
//!                        export, logo) returns <path> instead of opening
//!   crash panic|native  end the app to test crash.log
//!   bench <frames> [every <k> <line>]  time frames; optionally run <line>
//!                        (any command above) every k frames meanwhile
//! and writes `done.txt` when the batch has been processed.

use std::path::PathBuf;
use std::time::Instant;

/// Accumulated per-section timings while a `bench` command runs (dev only).
static PROF: std::sync::Mutex<Vec<(&'static str, f64)>> = std::sync::Mutex::new(Vec::new());
static BENCH_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record the time spent in a UI section since `start` (no-op unless benchmarking).
pub fn prof(name: &'static str, start: Instant) {
    if BENCH_ON.load(std::sync::atomic::Ordering::Relaxed) {
        if let Ok(mut v) = PROF.lock() {
            v.push((name, start.elapsed().as_secs_f64() * 1000.0));
        }
    }
}

/// Record a measured duration in milliseconds (no-op unless benchmarking).
pub fn prof_ms(name: &'static str, ms: f64) {
    if BENCH_ON.load(std::sync::atomic::Ordering::Relaxed) {
        if let Ok(mut v) = PROF.lock() {
            v.push((name, ms));
        }
    }
}

/// `dialog <path>`: the answer for the next file dialog (dev only).
static DIALOG_ANSWER: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// The path set with `dialog <path>`, once; `None` outside automated tests.
/// A file dialog asks this first, so scripted runs never block on a native dialog.
pub fn dialog_answer() -> Option<PathBuf> {
    DIALOG_ANSWER.lock().ok()?.take()
}

/// `dlg.pick_file()`, or the scripted answer.
pub fn pick_file(dlg: rfd::FileDialog) -> Option<PathBuf> {
    dialog_answer().or_else(|| dlg.pick_file())
}

static PROCESS_START:std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static MARKS: std::sync::Mutex<Vec<(&'static str, f64)>> = std::sync::Mutex::new(Vec::new());

/// Record a startup milestone (ms since process start). Cheap; always on.
pub fn mark(label: &'static str) {
    let t0 = *PROCESS_START.get_or_init(|| {
        // The first mark also records how long Windows took to get to it
        // (loading the image and its DLLs) as "launch".
        if let (Ok(mut m), Some(ms)) = (MARKS.lock(), since_process_created()) {
            m.push(("launch", -ms));
        }
        Instant::now()
    });
    if let Ok(mut m) = MARKS.lock() {
        m.push((label, t0.elapsed().as_secs_f64() * 1000.0));
    }
}

/// Milliseconds since the OS created this process.
fn since_process_created() -> Option<f64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let ticks = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let (mut created, mut exit, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: plain out-parameters on the current process pseudo-handle.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exit, &mut kernel, &mut user) }.ok()?;
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    Some(ticks(now).saturating_sub(ticks(created)) as f64 / 10_000.0)
}

pub struct DevHook {
    dir: Option<PathBuf>,
    last_check: Instant,
    queue: Vec<String>,
    wait_until: Option<Instant>,
    bench_left: u32,
    /// `bench ... every k line`: the line and its period.
    bench_every: Option<(u32, String)>,
}

pub enum DevAction {
    Run(String),
    /// `ai ...` lines for the Analysis panel.
    Analysis(String),
    Open(PathBuf),
    /// `key Ctrl+Q`: a key press injected into this frame's input.
    Key(String),
    /// `click x y [right]`: pointer press (`true`) or release at a point.
    Pointer { pos: egui::Pos2, secondary: bool, pressed: bool },
    State,
    /// Raw input events for this frame (`drag` steps, `type`).
    Events(Vec<egui::Event>),
    /// `hover x y`: pointer moved, no button.
    Hover(egui::Pos2),
    /// `scroll dy`: a mouse wheel movement.
    Scroll(f32),
    /// `ppp s`: pretend the display scale is `s`.
    Ppp(f32),
}

impl DevHook {
    pub fn new() -> Self {
        DevHook { dir: std::env::var_os("LUMEN_DEV_DIR").map(PathBuf::from), last_check: Instant::now(), queue: Vec::new(), wait_until: None, bench_left: 0, bench_every: None }
    }

    pub fn tick(&mut self, ctx: &egui::Context) -> Vec<DevAction> {
        let Some(dir) = self.dir.clone() else { return Vec::new() };
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        if self.bench_left > 0 {
            self.bench_left -= 1;
            ctx.request_repaint();
            if let Some((k, line)) = self.bench_every.clone() {
                if k > 0 && self.bench_left % k == 0 && self.bench_left > 0 {
                    let mut out = Vec::new();
                    self.exec(&line, ctx, &dir, &mut out);
                    return out;
                }
            }
            if self.bench_left == 0 {
                BENCH_ON.store(false, std::sync::atomic::Ordering::Relaxed);
                let v = PROF.lock().map(|v| v.clone()).unwrap_or_default();
                let mut names: Vec<&str> = v.iter().map(|(n, _)| *n).collect();
                names.sort();
                names.dedup();
                let mut out = String::from("bench:");
                for n in names {
                    let mut xs: Vec<f64> = v.iter().filter(|(m, _)| *m == n).map(|(_, t)| *t).collect();
                    xs.sort_by(f64::total_cmp);
                    let avg = xs.iter().sum::<f64>() / xs.len().max(1) as f64;
                    let max = xs.last().copied().unwrap_or(0.0);
                    let p95 = xs.get(xs.len() * 95 / 100).copied().unwrap_or(max);
                    let over = xs.iter().filter(|t| **t > 16.0).count();
                    let hitch = xs.iter().filter(|t| **t > 33.0).count();
                    out += &format!(" {n}=avg {avg:.2}ms p95 {p95:.2}ms max {max:.2}ms over16 {over} over33 {hitch}/{};", xs.len());
                }
                self.bench_every = None;
                self.write_state(&out);
            } else {
                return Vec::new();
            }
        }
        if let Some(w) = self.wait_until {
            if Instant::now() < w {
                return Vec::new();
            }
            self.wait_until = None;
        }
        if self.queue.is_empty() {
            if self.last_check.elapsed().as_millis() < 200 {
                return Vec::new();
            }
            self.last_check = Instant::now();
            let cmd = dir.join("cmd.txt");
            if let Ok(text) = std::fs::read_to_string(&cmd) {
                let _ = std::fs::remove_file(&cmd);
                self.queue = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                self.queue.push("__done".into());
            }
        }
        let mut out = Vec::new();
        while !self.queue.is_empty() {
            let line = self.queue.remove(0);
            if self.exec(&line, ctx, &dir, &mut out) {
                break;
            }
        }
        out
    }

    /// Run one command line; true when the rest of the batch waits for a later frame.
    fn exec(&mut self, line: &str, ctx: &egui::Context, dir: &std::path::Path, out: &mut Vec<DevAction>) -> bool {
        let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
        let nums: Vec<f32> = arg.split_whitespace().filter_map(|t| t.parse::<f32>().ok()).collect();
        let num = |i: usize| nums.get(i).copied().unwrap_or(0.0);
        match verb {
            "shot" => {
                let path = dir.join(arg);
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                    let [w, h] = img.size;
                    let mut buf = Vec::with_capacity(w * h * 4);
                    for p in &img.pixels {
                        buf.extend_from_slice(&p.to_srgba_unmultiplied());
                    }
                    if let Some(rgba) = image::RgbaImage::from_raw(w as u32, h as u32, buf) {
                        let _ = rgba.save(&path);
                    }
                })));
                // Give the screenshot a few frames to complete.
                self.wait_until = Some(Instant::now() + std::time::Duration::from_millis(500));
                ctx.request_repaint();
                true
            }
            "wait" => {
                let ms: u64 = arg.parse().unwrap_or(500);
                self.wait_until = Some(Instant::now() + std::time::Duration::from_millis(ms));
                true
            }
            "run" => {
                out.push(DevAction::Run(arg.to_string()));
                false
            }
            "open" => {
                out.push(DevAction::Open(PathBuf::from(arg)));
                false
            }
            "state" => {
                out.push(DevAction::State);
                false
            }
            "key" => {
                out.push(DevAction::Key(arg.to_string()));
                // One key per frame, like a person typing.
                true
            }
            "click" | "__release" => {
                let v: Vec<&str> = arg.split_whitespace().collect();
                let pos = egui::Pos2::new(num(0), num(1));
                let secondary = v.get(2) == Some(&"right");
                let pressed = verb == "click";
                out.push(DevAction::Pointer { pos, secondary, pressed });
                if pressed {
                    // Release on the next frame, as a real click does.
                    self.queue.insert(0, format!("__release {arg}"));
                }
                true
            }
            "drag" => {
                let v: Vec<f32> = arg.split_whitespace().filter_map(|t| t.parse().ok()).collect();
                if v.len() == 4 {
                    let (a, b) = (egui::Pos2::new(v[0], v[1]), egui::Pos2::new(v[2], v[3]));
                    let steps = 12;
                    let mut lines = vec![format!("__ev press {} {}", a.x, a.y)];
                    for k in 1..=steps {
                        let p = a + (b - a) * (k as f32 / steps as f32);
                        lines.push(format!("__ev move {} {}", p.x, p.y));
                    }
                    lines.push(format!("__ev release {} {}", b.x, b.y));
                    for (k, l) in lines.into_iter().enumerate() {
                        self.queue.insert(k, l);
                    }
                }
                false
            }
            "__ev" => {
                let v: Vec<&str> = arg.split_whitespace().collect();
                let n = |i: usize| v.get(i).and_then(|t| t.parse::<f32>().ok()).unwrap_or(0.0);
                let pos = egui::Pos2::new(n(1), n(2));
                let button = |pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
                let ev = match v.first() {
                    Some(&"press") => vec![egui::Event::PointerMoved(pos), button(true)],
                    Some(&"release") => vec![egui::Event::PointerMoved(pos), button(false)],
                    _ => vec![egui::Event::PointerMoved(pos)],
                };
                out.push(DevAction::Events(ev));
                true
            }
            "type" => {
                out.push(DevAction::Events(vec![egui::Event::Text(arg.to_string())]));
                true
            }
            "hover" => {
                out.push(DevAction::Hover(egui::Pos2::new(num(0), num(1))));
                true
            }
            "scroll" => {
                out.push(DevAction::Scroll(num(0)));
                true
            }
            "ppp" => {
                out.push(DevAction::Ppp(if num(0) > 0.0 { num(0) } else { 1.0 }));
                true
            }
            "size" => {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::Vec2::new(num(0).max(1.0), num(1).max(1.0))));
                ctx.request_repaint();
                true
            }
            "ai" => {
                out.push(DevAction::Analysis(arg.to_string()));
                false
            }
            "dialog" => {
                if let Ok(mut a) = DIALOG_ANSWER.lock() {
                    *a = Some(PathBuf::from(arg.trim()));
                }
                false
            }
            "bench" => {
                let (frames, every) = match arg.split_once(" every ") {
                    Some((f, rest)) => {
                        let (k, l) = rest.split_once(' ').unwrap_or((rest, ""));
                        (f, k.parse::<u32>().ok().map(|k| (k.max(1), l.to_string())))
                    }
                    None => (arg, None),
                };
                self.bench_left = frames.trim().parse().unwrap_or(60);
                self.bench_every = every;
                if let Ok(mut v) = PROF.lock() {
                    v.clear();
                }
                BENCH_ON.store(true, std::sync::atomic::Ordering::Relaxed);
                ctx.request_repaint();
                true
            }
            // Checks crash.log: `crash panic` (UI thread panic) or `crash native` (access violation).
            "crash" => {
                if arg == "native" {
                    // SAFETY: deliberately not safe; ends the process to test the native handler.
                    unsafe { std::ptr::with_exposed_provenance_mut::<u8>(16).write_volatile(1) };
                }
                panic!("dev hook: requested crash");
            }
            "__done" => {
                let _ = std::fs::write(dir.join("done.txt"), "done");
                false
            }
            _ => false,
        }
    }

    pub fn startup_report() -> String {
        MARKS.lock().map(|m| m.iter().map(|(l, t)| format!("{l}={t:.0}ms")).collect::<Vec<_>>().join(" ")).unwrap_or_default()
    }

    pub fn write_state(&self, text: &str) {
        if let Some(dir) = &self.dir {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("state.txt")) {
                let _ = writeln!(f, "{text}");
            }
        }
    }
}
