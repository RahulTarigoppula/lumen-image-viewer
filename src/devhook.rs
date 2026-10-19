//! Developer automation hook used for automated UI testing. Only active when
//! the `LUMEN_DEV_DIR` environment variable points at a folder; it then reads
//! `cmd.txt` from that folder (one command per line):
//!   shot <file.png>      save a screenshot of the window
//!   run <Command>        run a command by its debug name, e.g. `run NextImage`
//!   open <path>          open a file or folder
//!   state                append the app state to state.txt
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

static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
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
}

pub enum DevAction {
    Run(String),
    Open(PathBuf),
    State,
}

impl DevHook {
    pub fn new() -> Self {
        DevHook { dir: std::env::var_os("LUMEN_DEV_DIR").map(PathBuf::from), last_check: Instant::now(), queue: Vec::new(), wait_until: None, bench_left: 0 }
    }

    pub fn tick(&mut self, ctx: &egui::Context) -> Vec<DevAction> {
        let Some(dir) = self.dir.clone() else { return Vec::new() };
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        if self.bench_left > 0 {
            self.bench_left -= 1;
            ctx.request_repaint();
            if self.bench_left == 0 {
                BENCH_ON.store(false, std::sync::atomic::Ordering::Relaxed);
                let v = PROF.lock().map(|v| v.clone()).unwrap_or_default();
                let mut names: Vec<&str> = v.iter().map(|(n, _)| *n).collect();
                names.sort();
                names.dedup();
                let mut out = String::from("bench:");
                for n in names {
                    let xs: Vec<f64> = v.iter().filter(|(m, _)| *m == n).map(|(_, t)| *t).collect();
                    let avg = xs.iter().sum::<f64>() / xs.len().max(1) as f64;
                    let max = xs.iter().cloned().fold(0.0, f64::max);
                    out += &format!(" {n}=avg {avg:.2}ms max {max:.2}ms;");
                }
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
            let (verb, arg) = line.split_once(' ').unwrap_or((line.as_str(), ""));
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
                    break;
                }
                "wait" => {
                    let ms: u64 = arg.parse().unwrap_or(500);
                    self.wait_until = Some(Instant::now() + std::time::Duration::from_millis(ms));
                    break;
                }
                "run" => out.push(DevAction::Run(arg.to_string())),
                "open" => out.push(DevAction::Open(PathBuf::from(arg))),
                "state" => out.push(DevAction::State),
                "bench" => {
                    self.bench_left = arg.parse().unwrap_or(60);
                    if let Ok(mut v) = PROF.lock() {
                        v.clear();
                    }
                    BENCH_ON.store(true, std::sync::atomic::Ordering::Relaxed);
                    ctx.request_repaint();
                    break;
                }
                "__done" => {
                    let _ = std::fs::write(dir.join("done.txt"), "done");
                }
                _ => {}
            }
        }
        out
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
