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

pub struct DevHook {
    dir: Option<PathBuf>,
    last_check: Instant,
    queue: Vec<String>,
    wait_until: Option<Instant>,
}

pub enum DevAction {
    Run(String),
    Open(PathBuf),
    State,
}

impl DevHook {
    pub fn new() -> Self {
        DevHook { dir: std::env::var_os("LUMEN_DEV_DIR").map(PathBuf::from), last_check: Instant::now(), queue: Vec::new(), wait_until: None }
    }

    pub fn dir(&self) -> Option<&PathBuf> {
        self.dir.as_ref()
    }

    pub fn tick(&mut self, ctx: &egui::Context) -> Vec<DevAction> {
        let Some(dir) = self.dir.clone() else { return Vec::new() };
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
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
                "__done" => {
                    let _ = std::fs::write(dir.join("done.txt"), "done");
                }
                _ => {}
            }
        }
        out
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
