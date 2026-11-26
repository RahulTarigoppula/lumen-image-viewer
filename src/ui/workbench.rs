//! The Workbench: a camera comparison project on screen.
//!
//! Open a folder of captures as a project (File, Open project, or Ctrl+Shift+O)
//! and Lumen finds the devices and scenes (see `lumen_core::workbench::detect`).
//! The overview is a grid with one row per scene and one column per device;
//! a scene opens in Compare with device names on the panes, a scene bar to
//! page through the scenes and the scorecard panel. The blind test shows
//! random pairs with hidden names and ranks the devices from the votes. The
//! round deck exports everything as one PowerPoint, PDF or web page.

use super::compare::SceneLink;
use super::icons as I;
use super::theme::{text, Palette};
use super::widgets;
use super::workbench_export::{self as export, DeckJob};
use crate::app::Core;
use crate::thumbs::{Thumb, ThumbKey};
use crossbeam_channel::Receiver;
use egui::{Align2, Color32, CornerRadius, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use lumen_core::report;
use lumen_core::workbench::{measure, rank, Choice, DeviceMeasure, Scale, Verdict, Vote, Workspace, ARTIFACT_TAGS};
use std::sync::atomic::AtomicBool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// What the workbench asks the app to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WbRequest {
    /// Open scene `n` in Compare.
    OpenScene(usize),
    /// Show the next blind trial in Compare.
    BlindTrial,
    /// Leave the workbench.
    Back,
}

/// A running blind test.
pub struct Blind {
    pub trial: Option<rank::Trial>,
    /// Votes given in this session.
    pub count: usize,
    rng: rank::Rng,
    pub rater: String,
}

/// A running measurement of the project's captures.
struct MeasureJob {
    rx: Receiver<(usize, String, std::collections::BTreeMap<String, f64>)>,
    done: Arc<AtomicUsize>,
    total: usize,
    cancel: Arc<AtomicBool>,
}

struct ExportForm {
    title: String,
    author: String,
    summary: String,
    format: report::Format,
    details: bool,
    reviewed_only: bool,
}

pub struct WorkbenchUi {
    pub ws: Workspace,
    /// The selected scene in the overview.
    pub selected: usize,
    dirty: Option<Instant>,
    disk_stamp: Option<SystemTime>,
    disk_check: Instant,
    /// File size and time per capture (thumbnail keys).
    meta: HashMap<PathBuf, (u64, Option<SystemTime>)>,
    filter: String,
    pub requests: Vec<WbRequest>,
    setup: bool,
    export: Option<ExportForm>,
    job: Option<(Receiver<Result<PathBuf, String>>, Arc<AtomicUsize>, usize)>,
    pub last_export: Option<PathBuf>,
    pub blind: Option<Blind>,
    /// Scroll the selected row into view next frame.
    scroll_to_selected: bool,
    measuring: Option<MeasureJob>,
    /// The previous round's project (regression tracking).
    pub prev: Option<Workspace>,
}

const SAVE_DELAY: Duration = Duration::from_millis(1200);
const NAME_W: f32 = 210.0;

/// Left edge of a device column's content (name, thumbnails, measure lines)
/// from the column's start; the header's color bar hangs left of it.
const COL_INSET: f32 = 14.0;

fn exif_of(path: &Path) -> (Option<String>, Option<i64>) {
    match crate::meta::read_exif(path) {
        Some(e) => (e.camera, e.taken_ts),
        None => (None, None),
    }
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

impl WorkbenchUi {
    /// Open the project in `root`, or detect devices and scenes and create it.
    /// The second value says whether a new project was made.
    pub fn open(root: &Path) -> anyhow::Result<(WorkbenchUi, bool)> {
        let (ws, created) = if root.join(lumen_core::workbench::PROJECT_FILE).is_file() {
            (Workspace::load(root)?, false)
        } else {
            let ws = Workspace::detect(root, &exif_of);
            anyhow::ensure!(!ws.project.scenes.is_empty(), "no images or videos were found in {}", root.display());
            ws.save()?;
            (ws, true)
        };
        let disk_stamp = ws.disk_stamp();
        let mut ui = WorkbenchUi {
            ws,
            selected: 0,
            dirty: None,
            disk_stamp,
            disk_check: Instant::now(),
            meta: HashMap::new(),
            filter: String::new(),
            requests: Vec::new(),
            setup: false,
            export: None,
            job: None,
            last_export: None,
            blind: None,
            scroll_to_selected: false,
            measuring: None,
            prev: None,
        };
        ui.load_previous();
        ui.refresh_meta();
        Ok((ui, created))
    }

    /// Load the previous round named in the project (if any).
    fn load_previous(&mut self) {
        self.prev = self.ws.project.previous.as_ref().and_then(|p| Workspace::load(&self.ws.abs(p)).ok());
    }

    fn refresh_meta(&mut self) {
        self.meta.clear();
        for s in &self.ws.project.scenes {
            for sh in &s.shots {
                let path = self.ws.abs(&sh.path);
                if let Ok(m) = std::fs::metadata(&path) {
                    self.meta.insert(path, (m.len(), m.modified().ok()));
                }
            }
        }
    }

    pub fn touch(&mut self) {
        self.dirty.get_or_insert_with(Instant::now);
    }

    pub fn save_now(&mut self, toasts: &mut widgets::Toasts) {
        if self.dirty.take().is_some() {
            match self.ws.save() {
                Ok(()) => self.disk_stamp = self.ws.disk_stamp(),
                Err(e) => toasts.error(format!("Could not save the project: {e:#}")),
            }
        }
    }

    /// Autosave, reload when someone else saved the project, export progress.
    pub fn tick(&mut self, core: &mut Core) {
        if let Some(t) = self.dirty {
            if t.elapsed() >= SAVE_DELAY {
                self.save_now(&mut core.toasts);
            } else {
                core.ctx.request_repaint_after(SAVE_DELAY);
            }
        }
        if self.dirty.is_none() && self.disk_check.elapsed() > Duration::from_secs(3) {
            self.disk_check = Instant::now();
            let now = self.ws.disk_stamp();
            if now.is_some() && now != self.disk_stamp {
                if let Ok(ws) = Workspace::load(&self.ws.root) {
                    self.ws = ws;
                    self.selected = self.selected.min(self.ws.project.scenes.len().saturating_sub(1));
                    self.refresh_meta();
                    core.toasts.info("The project was changed by someone else and has been reloaded");
                }
                self.disk_stamp = now;
            }
        }
        self.poll_measure(core);
        if let Some((rx, _, _)) = &self.job {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    core.toasts.success(format!("Report saved: {name}"));
                    crate::fileops::open_with_default(&path);
                    self.last_export = Some(path);
                    self.job = None;
                }
                Ok(Err(e)) => {
                    core.toasts.error(format!("Could not export the report: {e}"));
                    self.job = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    core.toasts.error("Could not export the report");
                    self.job = None;
                }
                Err(_) => core.ctx.request_repaint_after(Duration::from_millis(150)),
            }
        }
    }

    // ------------------------------------------------------------ measuring

    /// Measure every capture not measured yet (or all with `all`), off the UI thread.
    pub fn start_measure(&mut self, core: &mut Core, all: bool) {
        if measure::available().is_empty() {
            core.toasts.info("This build has no measurements");
            return;
        }
        let todo: Vec<(usize, String)> = if all {
            let mut v = Vec::new();
            for (i, s) in self.ws.project.scenes.iter().enumerate() {
                for sh in &s.shots {
                    if !v.iter().any(|(k, d): &(usize, String)| *k == i && *d == sh.device) {
                        v.push((i, sh.device.clone()));
                    }
                }
            }
            v
        } else {
            self.ws.project.unmeasured()
        };
        if todo.is_empty() {
            core.toasts.info("Every capture is measured already");
            return;
        }
        let jobs: Vec<(usize, String, PathBuf)> = todo.into_iter().filter_map(|(i, d)| Some((i, d.clone(), self.ws.abs(&self.ws.project.scenes.get(i)?.shot(&d)?.path)))).collect();
        let total = jobs.len();
        let done = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let (ctx, d2, c2) = (core.ctx.clone(), done.clone(), cancel.clone());
        std::thread::spawn(move || {
            crate::bg::background_priority();
            let pool = lumen_analysis::job::AnalysisPool::new(None);
            for (i, dev, path) in jobs {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let img = lumen_core::decode::decode_file(&path, lumen_core::decode::DecodeOptions { color_manage: true, animations: false }).ok()?;
                    Some(measure::measure(&img.image, &pool))
                }));
                if let Ok(Some(values)) = res {
                    let _ = tx.send((i, dev, values));
                } else {
                    log::warn!("workbench: could not measure {}", path.display());
                }
                d2.fetch_add(1, Ordering::Relaxed);
                ctx.request_repaint();
            }
        });
        self.measuring = Some(MeasureJob { rx, done, total, cancel });
    }

    fn poll_measure(&mut self, core: &mut Core) {
        let Some(job) = &self.measuring else { return };
        let mut got = Vec::new();
        while let Ok(m) = job.rx.try_recv() {
            got.push(m);
        }
        let total = job.total;
        let finished = job.done.load(Ordering::Relaxed) >= total || (job.cancel.load(Ordering::Relaxed) && job.rx.is_empty() && got.is_empty());
        if !got.is_empty() {
            for (i, dev, values) in got {
                if let Some(s) = self.ws.project.scenes.get_mut(i) {
                    s.measures.retain(|m| m.device != dev);
                    s.measures.push(DeviceMeasure { device: dev, values });
                }
            }
            self.touch();
        }
        if finished {
            let n = total;
            self.measuring = None;
            core.toasts.success(format!("Measured {n} capture{}", if n == 1 { "" } else { "s" }));
        } else {
            core.ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    // ------------------------------------------------------------ links

    /// The compare link for scene `k`: device labels and colors per capture.
    pub fn link_for(&self, k: usize) -> (Vec<PathBuf>, SceneLink) {
        let p = &self.ws.project;
        let files = self.ws.scene_files(k);
        let files: Vec<(String, PathBuf)> = files.into_iter().take(4).collect();
        let link = SceneLink {
            scene: k,
            count: p.scenes.len(),
            name: p.scenes.get(k).map(|s| s.name.clone()).unwrap_or_default(),
            labels: files.iter().map(|(d, _)| p.display(d).to_string()).collect(),
            colors: files.iter().map(|(d, _)| p.device_color(d)).collect(),
            blind: false,
            progress: String::new(),
            done: p.scenes.get(k).is_some_and(|s| s.done),
        };
        (files.into_iter().map(|(_, f)| f).collect(), link)
    }

    /// The scene `step` scenes away from `k` that has captures.
    pub fn step_scene(&self, k: usize, step: i32) -> Option<usize> {
        let n = self.ws.project.scenes.len() as i64;
        let mut i = k as i64;
        for _ in 0..n {
            i += step.signum() as i64;
            if i < 0 || i >= n {
                return None;
            }
            if self.ws.project.scenes[i as usize].shots.len() >= 2 {
                return Some(i as usize);
            }
        }
        None
    }

    pub fn set_done(&mut self, k: usize, done: bool) {
        if let Some(s) = self.ws.project.scenes.get_mut(k) {
            s.done = done;
            self.touch();
        }
    }

    // ------------------------------------------------------------ blind test

    pub fn start_blind(&mut self, rater: String) {
        let seed = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7);
        self.blind = Some(Blind { trial: None, count: 0, rng: rank::Rng::new(seed), rater });
        self.next_trial();
    }

    /// Pick the next blind pair; `None` when the project has no pairs.
    pub fn next_trial(&mut self) -> Option<rank::Trial> {
        let p = &self.ws.project;
        let available: Vec<Vec<String>> = p
            .scenes
            .iter()
            .map(|s| {
                let mut devs: Vec<String> = Vec::new();
                for sh in &s.shots {
                    if !devs.contains(&sh.device) {
                        devs.push(sh.device.clone());
                    }
                }
                devs
            })
            .collect();
        let names: Vec<String> = p.scenes.iter().map(|s| s.name.clone()).collect();
        let votes = p.votes.clone();
        let b = self.blind.as_mut()?;
        let t = rank::next_trial(&available, &names, &votes, &mut b.rng);
        b.trial = t.clone();
        t
    }

    /// The files and link of the current blind trial (left, right).
    pub fn blind_link(&self) -> Option<(Vec<PathBuf>, SceneLink)> {
        let b = self.blind.as_ref()?;
        let t = b.trial.as_ref()?;
        let s = self.ws.project.scenes.get(t.scene)?;
        let left = s.shot(&t.left)?;
        let right = s.shot(&t.right)?;
        let link = SceneLink {
            scene: t.scene,
            count: self.ws.project.scenes.len(),
            name: String::new(),
            labels: Vec::new(),
            colors: Vec::new(),
            blind: true,
            progress: format!("Trial {}  ·  {} in the project", b.count + 1, plural(self.ws.project.votes.len(), "vote")),
            done: false,
        };
        Some((vec![self.ws.abs(&left.path), self.ws.abs(&right.path)], link))
    }

    /// Record a vote on the current trial: 0 left, 1 right, 2 same.
    pub fn vote(&mut self, v: u8) {
        let Some(b) = self.blind.as_mut() else { return };
        let Some(t) = b.trial.clone() else { return };
        let choice = match v {
            0 => Choice::A,
            1 => Choice::B,
            _ => Choice::Tie,
        };
        let scene = self.ws.project.scenes.get(t.scene).map(|s| s.name.clone()).unwrap_or_default();
        self.ws.project.votes.push(Vote { scene, a: t.left, b: t.right, choice, rater: b.rater.clone() });
        b.count += 1;
        self.touch();
        self.next_trial();
    }

    // ------------------------------------------------------------ overview

    /// The thumbnail of `path` in the top-left of `rect` (so it lines up with
    /// the column's header and caption); returns where it was drawn.
    fn thumb(&self, core: &mut Core, ui: &Ui, path: &Path, rect: Rect) -> Rect {
        let p = core.pal;
        let Some((size, mtime)) = self.meta.get(path).copied() else {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.faint_bg);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(20.0), p.faint);
            return rect;
        };
        let key = ThumbKey::new(path, size, mtime);
        match core.thumbs.get(&key) {
            Thumb::Ready(t) => {
                let ts = t.tex.size_vec2();
                let s = (rect.width() / ts.x.max(1.0)).min(rect.height() / ts.y.max(1.0));
                let r = Rect::from_min_size(rect.min, ts * s);
                ui.painter().add(egui::epaint::RectShape::filled(r, CornerRadius::same(4), Color32::WHITE).with_texture(t.tex.id(), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))));
                r
            }
            Thumb::Pending => {
                ui.painter().rect_filled(rect, CornerRadius::same(6), p.faint_bg);
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, I::HOURGLASS, FontId::proportional(18.0), p.faint);
                rect
            }
            Thumb::Failed => {
                ui.painter().rect_filled(rect, CornerRadius::same(6), p.faint_bg);
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, I::IMAGE_BROKEN, FontId::proportional(20.0), p.faint);
                rect
            }
        }
    }

    /// Scenes shown with the current filter, as indices.
    fn visible(&self) -> Vec<usize> {
        let f = self.filter.trim().to_lowercase();
        self.ws.project.scenes.iter().enumerate().filter(|(_, s)| f.is_empty() || s.name.to_lowercase().contains(&f) || s.comment.to_lowercase().contains(&f)).map(|(i, _)| i).collect()
    }

    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        self.tick(core);
        egui::Panel::top("wb-toolbar")
            .resizable(false)
            .exact_size(super::theme::size::TOOLBAR)
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin::symmetric(10, 0)))
            .show(ui, |ui| self.toolbar(ui, core));
        egui::Panel::left("wb-devices")
            .resizable(true)
            .default_size(250.0)
            .size_range(200.0..=380.0)
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(egui::Margin { left: 12, right: 8, top: 10, bottom: 8 }))
            .show(ui, |ui| self.side(ui, core));
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(p.bg).inner_margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 4 })).show(ui, |ui| self.grid(ui, core));
        self.setup_window(ui.ctx(), core);
        self.export_dialog(ui.ctx(), core);
        self.keys(ui);
    }

    fn keys(&mut self, ui: &Ui) {
        if ui.ctx().egui_wants_keyboard_input() || self.export.is_some() || egui::Popup::is_any_open(ui.ctx()) {
            return;
        }
        let vis = self.visible();
        if vis.is_empty() {
            return;
        }
        let pos = vis.iter().position(|&i| i == self.selected).unwrap_or(0);
        let take = |k: Key| ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
        if take(Key::ArrowDown) {
            self.selected = vis[(pos + 1).min(vis.len() - 1)];
            self.scroll_to_selected = true;
        }
        if take(Key::ArrowUp) {
            self.selected = vis[pos.saturating_sub(1)];
            self.scroll_to_selected = true;
        }
        if take(Key::Home) {
            self.selected = vis[0];
            self.scroll_to_selected = true;
        }
        if take(Key::End) {
            self.selected = vis[vis.len() - 1];
            self.scroll_to_selected = true;
        }
        if take(Key::Enter) {
            self.requests.push(WbRequest::OpenScene(self.selected));
        }
        if take(Key::Escape) && !self.setup {
            self.requests.push(WbRequest::Back);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if widgets::icon_button(ui, I::ARROW_LEFT, "Back to the browser  (Esc)", false).clicked() {
                self.requests.push(WbRequest::Back);
            }
            ui.label(RichText::new(I::FLASK).size(16.0).color(p.accent));
            let name = if self.ws.project.name.trim().is_empty() { "Project".to_string() } else { self.ws.project.name.clone() };
            ui.label(RichText::new(name).font(text::title()).color(p.text)).on_hover_text(self.ws.file().display().to_string());
            let pr = &self.ws.project;
            ui.label(RichText::new(format!("{}  ·  {}", plural(pr.devices.len(), "device"), plural(pr.scenes.len(), "scene"))).size(text::SMALL).color(p.muted));
            ui.add_space(10.0);
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text(format!("{}  Filter scenes", I::FUNNEL)).desired_width(170.0)).on_hover_text("Show only scenes whose name or comment contains this text");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let exporting = self.job.is_some();
                let label = if exporting {
                    let (done, total) = self.job.as_ref().map(|(_, d, t)| (d.load(Ordering::Relaxed), *t)).unwrap_or((0, 0));
                    format!("{}  Exporting {done}/{total}", I::SPINNER)
                } else {
                    format!("{}  Round report", I::PRESENTATION_CHART)
                };
                let r = ui.add_enabled_ui(!exporting, |ui| widgets::primary_button(ui, &p, &label)).inner;
                if r.on_hover_text("One report for the whole project: summary with Win / Par / Lose and scores per device, flagged artifacts, blind test ranking, then every scene with its marks and details. PowerPoint, PDF or web page.").clicked() {
                    self.open_export();
                }
                let (mlabel, mtip) = match &self.measuring {
                    Some(j) => (format!("Measuring {}/{}", j.done.load(Ordering::Relaxed), j.total), "Measuring the captures; click to stop".to_string()),
                    None => ("Measure".to_string(), format!("Measure sharpness, noise, clipping and color of every capture, each scaled to {} px so devices are compared fairly. Shift+click measures everything again.", measure::MEASURE_SIDE)),
                };
                let r = widgets::labeled_icon_button(ui, if self.measuring.is_some() { I::SPINNER } else { I::GAUGE }, &mlabel, &mtip, self.measuring.is_some());
                if r.clicked() {
                    match &self.measuring {
                        Some(j) => j.cancel.store(true, Ordering::Relaxed),
                        None => {
                            let all = ui.input(|i| i.modifiers.shift);
                            self.start_measure(core, all);
                        }
                    }
                }
                if widgets::labeled_icon_button(ui, I::TABLE, "CSV", "Save scores, verdicts and notes per scene and device as a spreadsheet (CSV)", false).clicked() {
                    self.save_csv(core);
                }
                if widgets::labeled_icon_button(ui, I::EYE_SLASH, "Blind test", "Judge random pairs with names, files and camera data hidden; the devices are ranked from the votes", false).clicked() {
                    let rater = std::env::var("USERNAME").unwrap_or_default();
                    self.start_blind(rater);
                    if self.blind.as_ref().and_then(|b| b.trial.as_ref()).is_some() {
                        self.requests.push(WbRequest::BlindTrial);
                    } else {
                        self.blind = None;
                        core.toasts.info("A blind test needs scenes with captures from at least two devices");
                    }
                }
                if widgets::labeled_icon_button(ui, I::GEAR_SIX, "Setup", "Project name, device names, the reference device, the score scale and the scorecard attributes", self.setup).clicked() {
                    self.setup = !self.setup;
                }
                let can_open = self.ws.project.scenes.get(self.selected).is_some_and(|s| s.shots.len() >= 2);
                let r = ui.add_enabled_ui(can_open, |ui| widgets::labeled_icon_button(ui, I::COLUMNS, "Compare", "Open the selected scene in Compare: device names on the panes, Page Up / Page Down for the other scenes  (Enter)", false)).inner;
                if r.clicked() {
                    self.requests.push(WbRequest::OpenScene(self.selected));
                }
            });
        });
    }

    fn side(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(ui.available_width() - 6.0);
            widgets::section_title(ui, &p, "DEVICES");
            let tally = self.ws.project.tally();
            let reference = self.ws.project.reference.clone();
            let mut new_ref: Option<Option<String>> = None;
            for t in &tally {
                let color = rgb(self.ws.project.device_color(&t.device));
                let display = self.ws.project.display(&t.device).to_string();
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
                    ui.painter().rect_filled(Rect::from_center_size(r.center(), Vec2::new(8.0, 14.0)), CornerRadius::same(2), color);
                    ui.add(egui::Label::new(RichText::new(&display).font(text::body_strong()).color(p.text)).truncate()).on_hover_text(format!("{} captures (folder or name: {})", t.scenes, t.device));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let is_ref = reference.as_deref() == Some(t.device.as_str());
                        let tip = if is_ref { "The reference device: the others get Win / Par / Lose against it. Click to clear." } else { "Make this the reference device (golden sample) the others are judged against" };
                        if widgets::icon_button_sized(ui, I::STAR, tip, is_ref, 24.0, 13.0).clicked() {
                            new_ref = Some(if is_ref { None } else { Some(t.device.clone()) });
                        }
                    });
                });
                if t.win + t.par + t.lose > 0 || t.scored > 0 {
                    let mut line = String::new();
                    if t.win + t.par + t.lose > 0 {
                        line.push_str(&format!("{} win  ·  {} par  ·  {} lose", t.win, t.par, t.lose));
                    }
                    if let Some((_, v)) = t.mean.iter().find(|(a, _)| a == "Overall").or(t.mean.first()) {
                        if !line.is_empty() {
                            line.push_str("  ·  ");
                        }
                        line.push_str(&format!("mean {v:.1}"));
                    }
                    ui.label(RichText::new(line).size(text::SMALL).color(p.muted));
                }
                ui.add_space(2.0);
            }
            if let Some(r) = new_ref {
                self.ws.project.reference = r;
                self.touch();
            }
            if let Some(prev) = &self.prev {
                let d = lumen_core::workbench::compare_rounds(&self.ws.project, &prev.project, 1);
                ui.add_space(6.0);
                let pname = if prev.project.name.is_empty() { "previous round".to_string() } else { prev.project.name.clone() };
                widgets::section_title(ui, &p, &format!("SINCE {}", pname.to_uppercase()));
                ui.label(RichText::new(format!("{} scenes in both rounds", d.shared_scenes)).size(text::SMALL).color(p.faint));
                for dd in &d.devices {
                    let mut parts: Vec<String> = dd.mean.iter().filter(|(_, v)| v.abs() >= 0.05).map(|(a, v)| format!("{} {:+.1}", short(a), v)).collect();
                    if dd.wins_before != dd.wins_after || dd.losses_before != dd.losses_after {
                        parts.push(format!("wins {} to {}", dd.wins_before, dd.wins_after));
                    }
                    let line = if parts.is_empty() { "no change".to_string() } else { parts.join("  ·  ") };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(self.ws.project.display(&dd.device)).size(text::SMALL).color(p.text));
                        ui.label(RichText::new(line).size(text::SMALL).color(p.muted));
                    });
                }
                if d.regressions.is_empty() {
                    ui.label(RichText::new(format!("{}  No score went down", I::CHECK_CIRCLE)).size(text::SMALL).color(p.ok));
                } else {
                    ui.label(RichText::new(format!("{}  {} score{} went down", I::TREND_DOWN, d.regressions.len(), if d.regressions.len() == 1 { "" } else { "s" })).size(text::SMALL).color(p.danger));
                    for r in d.regressions.iter().take(8) {
                        let t = format!("{}: {} {} {} to {}", r.scene, self.ws.project.display(&r.device), r.attribute, r.before, r.after);
                        if ui.add(egui::Label::new(RichText::new(t).size(text::SMALL).color(p.muted)).sense(Sense::click())).on_hover_text("Open this scene").clicked() {
                            if let Some(k) = self.ws.project.scene_index(&r.scene) {
                                self.selected = k;
                                self.requests.push(WbRequest::OpenScene(k));
                            }
                        }
                    }
                }
            }
            ui.add_space(6.0);
            widgets::section_title(ui, &p, "COMPLETENESS");
            let missing = self.ws.project.missing();
            let dups = self.ws.project.duplicates();
            if missing.is_empty() && dups.is_empty() {
                ui.label(RichText::new(format!("{}  Every scene has one capture per device", I::CHECK_CIRCLE)).size(text::SMALL).color(p.ok));
            }
            if !missing.is_empty() {
                let list: Vec<String> = missing.iter().take(30).map(|(s, d)| format!("{}: {}", self.ws.project.scenes[*s].name, self.ws.project.display(d))).collect();
                ui.label(RichText::new(format!("{}  {} missing capture{}", I::WARNING, missing.len(), if missing.len() == 1 { "" } else { "s" })).size(text::SMALL).color(p.warn)).on_hover_text(list.join("\n"));
            }
            if !dups.is_empty() {
                let list: Vec<String> = dups.iter().take(30).map(|(s, d, n)| format!("{}: {} has {n}", self.ws.project.scenes[*s].name, self.ws.project.display(d))).collect();
                ui.label(RichText::new(format!("{}  {} scene{} with extra captures", I::WARNING, dups.len(), if dups.len() == 1 { "" } else { "s" })).size(text::SMALL).color(p.warn))
                    .on_hover_text(format!("Only the first capture of a device is compared and reported.\n{}", list.join("\n")));
            }
            let reviewed = self.ws.project.scenes.iter().filter(|s| s.done).count();
            ui.label(RichText::new(format!("{reviewed} of {} scenes reviewed", self.ws.project.scenes.len())).size(text::SMALL).color(p.muted));
            if !self.ws.project.detected_by.is_empty() {
                ui.label(RichText::new(format!("Found by: {}", self.ws.project.detected_by)).size(text::SMALL).color(p.faint)).on_hover_text("How Lumen paired the captures. Setup can find them again after files were added.");
            }
            if !self.ws.project.votes.is_empty() {
                ui.add_space(6.0);
                widgets::section_title(ui, &p, "BLIND TEST RANKING");
                let names: Vec<String> = self.ws.project.devices.iter().map(|d| d.name.clone()).collect();
                for r in rank::rank(&names, &self.ws.project.votes).into_iter().filter(|r| r.votes > 0) {
                    let color = rgb(self.ws.project.device_color(&r.device));
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(self.ws.project.display(&r.device)).color(p.text));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(format!("{:.0}", r.score)).font(text::body_strong()).color(color))
                                .on_hover_text(format!("Chance of being preferred over an average device: {:.0} (90% range {:.0} to {:.0}), from {}", r.score, r.low, r.high, plural(r.votes, "vote")));
                        });
                    });
                }
                ui.label(RichText::new(plural(self.ws.project.votes.len(), "vote")).size(text::SMALL).color(p.muted));
            }
            let ids = self.ws.project.measured_ids();
            if !ids.is_empty() {
                ui.add_space(6.0);
                widgets::section_title(ui, &p, "MEASURED (MEAN OVER SCENES)");
                let means = self.ws.project.measure_means();
                // A table: one column per device (its color on top), the unit
                // in the row name, the numbers right-aligned in equal columns,
                // so a value never wraps away from its device or unit.
                let devices: Vec<String> = means.iter().filter(|(_, m)| !m.is_empty()).map(|(d, _)| d.clone()).collect();
                let rows: Vec<(&String, Vec<Option<f64>>)> = ids
                    .iter()
                    .map(|id| (id, devices.iter().map(|d| means.iter().find(|(x, _)| x == d).and_then(|(_, m)| m.iter().find(|(k, _)| k == id).map(|(_, v)| *v))).collect::<Vec<_>>()))
                    .filter(|(_, vals)| vals.iter().any(|v| v.is_some()))
                    .collect();
                let font = FontId::proportional(text::SMALL);
                let val_w = rows
                    .iter()
                    .flat_map(|(_, vals)| vals.iter().flatten().map(|v| plain_number(*v)))
                    .map(|t| ui.painter().layout_no_wrap(t, font.clone(), p.text).size().x)
                    .fold(16.0f32, f32::max)
                    .ceil();
                let row_h = 18.0;
                // Short names when four or more devices leave little room.
                let compact = devices.len() > 3;
                egui::Grid::new("wb-measured").num_columns(devices.len() + 1).spacing(Vec2::new(8.0, 2.0)).min_row_height(row_h).show(ui, |ui| {
                    ui.label("");
                    for d in &devices {
                        let color = rgb(self.ws.project.device_color(d));
                        let display = self.ws.project.display(d).to_string();
                        right_cell(ui, val_w, row_h, |ui| {
                            let (r, resp) = ui.allocate_exact_size(Vec2::new(14.0, 12.0), Sense::hover());
                            ui.painter().rect_filled(Rect::from_center_size(r.center(), Vec2::new(6.0, 12.0)), CornerRadius::same(2), color);
                            resp.on_hover_text(display.as_str());
                        });
                    }
                    ui.end_row();
                    for (id, vals) in &rows {
                        let id = id.as_str();
                        let present: Vec<f64> = vals.iter().flatten().copied().collect();
                        let best = match measure::higher_is_better(id) {
                            Some(true) => present.iter().copied().fold(f64::MIN, f64::max),
                            Some(false) => present.iter().copied().fold(f64::MAX, f64::min),
                            None => f64::NAN,
                        };
                        let dir = match measure::higher_is_better(id) {
                            Some(true) => "higher is better",
                            Some(false) => "lower is better",
                            None => "",
                        };
                        let tip = format!("{} ({dir}). The best device is in green.", lumen_analysis::metrics::get(id).map(|m| m.info().name).unwrap_or(id));
                        let name = if compact { short(measure::name_of(id)) } else { measure::name_of(id).to_string() };
                        let name = match measure::unit_of(id) {
                            "" => name,
                            u => format!("{name} ({u})"),
                        };
                        ui.label(RichText::new(name).size(text::SMALL).color(p.text)).on_hover_text(tip.as_str());
                        for (d, v) in devices.iter().zip(vals) {
                            let display = self.ws.project.display(d).to_string();
                            right_cell(ui, val_w, row_h, |ui| {
                                let (t, best) = match v {
                                    Some(v) => (plain_number(*v), *v == best),
                                    None => ("-".to_string(), false),
                                };
                                let t = RichText::new(t).size(text::SMALL);
                                ui.label(if best { t.color(p.ok).strong() } else { t.color(p.muted) }).on_hover_text(display.as_str());
                            });
                        }
                        ui.end_row();
                    }
                });
                let left = self.ws.project.unmeasured().len();
                if left > 0 {
                    ui.label(RichText::new(format!("{left} capture{} not measured yet", if left == 1 { "" } else { "s" })).size(text::SMALL).color(p.faint));
                }
            }
            if let Some(path) = self.last_export.clone() {
                ui.add_space(8.0);
                widgets::section_title(ui, &p, "LAST REPORT");
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                ui.add(egui::Label::new(RichText::new(name).size(text::SMALL).color(p.muted)).truncate());
                ui.horizontal(|ui| {
                    if widgets::secondary_button(ui, "Open").on_hover_text(path.display().to_string()).clicked() {
                        crate::fileops::open_with_default(&path);
                    }
                    if widgets::secondary_button(ui, "Show in folder").clicked() {
                        crate::fileops::show_in_explorer(&path);
                    }
                });
            }
        });
    }

    fn grid(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        let devices: Vec<String> = self.ws.project.devices.iter().map(|d| d.name.clone()).collect();
        let n = devices.len().max(1);
        let avail = ui.available_width();
        let col_w = ((avail - NAME_W - 8.0) / n as f32).clamp(110.0, 280.0);
        let thumb_h = (col_w * 0.62).clamp(70.0, 170.0);
        let row_h = thumb_h + 30.0;
        // Header row.
        let (hr, _) = ui.allocate_exact_size(Vec2::new(avail, 26.0), Sense::hover());
        ui.painter().text(Pos2::new(hr.min.x + 8.0, hr.center().y), Align2::LEFT_CENTER, "Scene", text::body_strong(), p.muted);
        for (j, d) in devices.iter().enumerate() {
            // The device's color bar hangs left of the column; its name starts
            // at the column's one left edge (`COL_INSET`), with the thumbnails
            // and the measure lines below.
            let x = hr.min.x + NAME_W + j as f32 * col_w;
            let c = rgb(self.ws.project.device_color(d));
            let r = Rect::from_min_size(Pos2::new(x + COL_INSET - 10.0, hr.min.y + 4.0), Vec2::new(col_w - COL_INSET + 2.0, 18.0));
            ui.painter().rect_filled(Rect::from_min_size(r.min, Vec2::new(4.0, 18.0)), CornerRadius::same(2), c);
            let mut label = self.ws.project.display(d).to_string();
            if self.ws.project.reference.as_deref() == Some(d.as_str()) {
                label.push_str("  (reference)");
            }
            let g = ui.painter().layout_no_wrap(label, text::body_strong(), p.text);
            let clip = Rect::from_min_size(Pos2::new(r.min.x + 10.0, r.min.y), Vec2::new(col_w - 18.0, 18.0));
            ui.painter().with_clip_rect(clip).galley(Pos2::new(clip.min.x, r.center().y - g.size().y / 2.0), g, p.text);
        }
        ui.painter().line_segment([Pos2::new(hr.min.x, hr.max.y), Pos2::new(hr.max.x, hr.max.y)], Stroke::new(1.0, p.border));
        let vis = self.visible();
        if vis.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| ui.label(RichText::new("No scene matches the filter").color(p.muted)));
            return;
        }
        let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_to_selected) {
            if let Some(pos) = vis.iter().position(|&i| i == self.selected) {
                let y = pos as f32 * (row_h + ui.spacing().item_spacing.y);
                sa = sa.vertical_scroll_offset((y - 2.0 * row_h).max(0.0));
            }
        }
        let attrs = self.ws.project.attributes.clone();
        sa.show_rows(ui, row_h, vis.len(), |ui, range| {
            for &k in &vis[range] {
                let (row, resp) = ui.allocate_exact_size(Vec2::new(avail - 4.0, row_h), Sense::click());
                let sel = k == self.selected;
                let bg = if sel { p.selected_bg } else if resp.hovered() { p.hover } else { Color32::TRANSPARENT };
                ui.painter().rect_filled(row, CornerRadius::same(6), bg);
                let scene = self.ws.project.scenes[k].clone();
                // Name column.
                let nx = row.min.x + 8.0;
                let name_g = ui.painter().layout(scene.name.clone(), text::body_strong(), p.text, NAME_W - 20.0);
                let name_h = name_g.size().y;
                ui.painter().galley(Pos2::new(nx, row.min.y + 8.0), name_g, p.text);
                let mut y = row.min.y + 12.0 + name_h;
                if scene.done {
                    ui.painter().text(Pos2::new(nx, y), Align2::LEFT_TOP, format!("{}  Reviewed", I::CHECK_CIRCLE), FontId::proportional(text::SMALL), p.ok);
                    y += 16.0;
                }
                if !scene.comment.trim().is_empty() {
                    let g = ui.painter().layout(scene.comment.trim().to_string(), FontId::proportional(text::SMALL), p.muted, NAME_W - 20.0);
                    let clip = Rect::from_min_max(Pos2::new(nx, y), Pos2::new(row.min.x + NAME_W - 8.0, row.max.y - 6.0));
                    ui.painter().with_clip_rect(clip).galley(Pos2::new(nx, y), g, p.muted);
                }
                // Device columns.
                for (j, d) in devices.iter().enumerate() {
                    let cell = Rect::from_min_size(Pos2::new(row.min.x + NAME_W + j as f32 * col_w + COL_INSET, row.min.y + 6.0), Vec2::new(col_w - COL_INSET - 4.0, row_h - 12.0));
                    let tr = Rect::from_min_size(cell.min, Vec2::new(cell.width(), thumb_h));
                    // Where the picture is drawn (the verdict sits on its corner).
                    let mut shown = tr;
                    match scene.shot(d) {
                        Some(sh) => {
                            let path = self.ws.abs(&sh.path);
                            if ui.is_rect_visible(tr) {
                                shown = self.thumb(core, ui, &path, tr);
                            }
                        }
                        None => {
                            ui.painter().rect_stroke(tr.shrink(2.0), CornerRadius::same(6), Stroke::new(1.0, p.warn.gamma_multiply(0.7)), egui::StrokeKind::Inside);
                            ui.painter().text(tr.center(), Align2::CENTER_CENTER, "missing", FontId::proportional(text::SMALL), p.warn);
                        }
                    }
                    let sc = scene.score(d);
                    if let Some(v) = sc.and_then(|s| s.verdict) {
                        let label = v.label();
                        let w = ui.painter().layout_no_wrap(label.into(), FontId::proportional(10.5), Color32::WHITE).size().x + 12.0;
                        let br = Rect::from_min_size(Pos2::new(shown.max.x - w - 4.0, shown.min.y + 4.0), Vec2::new(w, 17.0));
                        ui.painter().rect_filled(br, CornerRadius::same(8), rgb(v.color()));
                        ui.painter().text(br.center(), Align2::CENTER_CENTER, label, FontId::proportional(10.5), Color32::WHITE);
                    }
                    let measured = scene.measure(d).filter(|m| !m.values.is_empty());
                    if sc.is_none_or(|s| s.attrs.is_empty()) && measured.is_some() {
                        let m = measured.expect("checked");
                        let line = measure::MEASURES.iter().filter_map(|(id, name)| m.values.get(*id).map(|v| format!("{} {}", short(name), measure::fmt_value(id, *v)))).take(3).collect::<Vec<_>>().join("  ");
                        let g = ui.painter().layout_no_wrap(line, FontId::proportional(text::CAPTION), p.faint);
                        let clip = Rect::from_min_max(Pos2::new(cell.min.x, tr.max.y + 2.0), Pos2::new(cell.max.x, cell.max.y));
                        ui.painter().with_clip_rect(clip).galley(Pos2::new(cell.min.x, tr.max.y + 4.0), g, p.faint);
                    }
                    if let Some(s) = sc.filter(|s| !s.attrs.is_empty()) {
                        let line = attrs.iter().filter_map(|a| s.attrs.get(a).map(|v| format!("{} {v}", short(a)))).collect::<Vec<_>>().join("  ");
                        let g = ui.painter().layout_no_wrap(line, FontId::proportional(text::CAPTION), p.muted);
                        let clip = Rect::from_min_max(Pos2::new(cell.min.x, tr.max.y + 2.0), Pos2::new(cell.max.x, cell.max.y));
                        ui.painter().with_clip_rect(clip).galley(Pos2::new(cell.min.x, tr.max.y + 4.0), g, p.muted);
                    }
                }
                let resp = resp.on_hover_text("Double-click (or Enter) to compare this scene");
                if resp.clicked() {
                    self.selected = k;
                }
                if resp.double_clicked() {
                    self.selected = k;
                    self.requests.push(WbRequest::OpenScene(k));
                }
            }
        });
    }

    // ------------------------------------------------------------ scorecard

    /// The scorecard panel shown beside Compare: verdict, attribute scores
    /// and a note per device of the scene, and the scene comment.
    pub fn scorecard(&mut self, ui: &mut Ui, p: &Palette, k: usize, devices: &[String]) -> bool {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(I::LIST_CHECKS).size(15.0).color(p.accent));
            ui.label(RichText::new("Scorecard").font(text::title()).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button_sized(ui, I::X, "Close the scorecard  (S)", false, 24.0, 14.0).clicked() {
                    close = true;
                }
            });
        });
        let Some(scene_name) = self.ws.project.scenes.get(k).map(|s| s.name.clone()) else { return close };
        ui.label(RichText::new(format!("{scene_name}  ·  saved in the project automatically")).size(text::SMALL).color(p.muted));
        ui.add_space(4.0);
        let scale = self.ws.project.scale;
        let (lo, hi) = scale.range();
        let attrs = self.ws.project.attributes.clone();
        let reference = self.ws.project.reference.clone();
        let mut changed = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(ui.available_width() - 8.0);
            // Once for all devices, not under each of them.
            if reference.is_none() {
                ui.add_space(4.0);
                ui.add(egui::Label::new(RichText::new("Pick a reference device (star in the project's device list) to give Win / Par / Lose").size(text::SMALL).color(p.faint)).wrap());
            }
            for d in devices {
                let color = rgb(self.ws.project.device_color(d));
                let display = self.ws.project.display(d).to_string();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(8.0, 18.0), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::same(2), color);
                    ui.label(RichText::new(&display).font(text::body_strong()).color(p.text));
                });
                let is_ref = reference.as_deref() == Some(d.as_str());
                let scene = &mut self.ws.project.scenes[k];
                let sc = scene.score_mut(d);
                if is_ref {
                    ui.label(RichText::new("Reference device").size(text::SMALL).color(p.muted));
                } else if reference.is_some() {
                    ui.horizontal(|ui| {
                        for v in Verdict::ALL {
                            let on = sc.verdict == Some(v);
                            let txt = RichText::new(v.label()).color(if on { Color32::WHITE } else { p.text });
                            let b = egui::Button::new(txt).fill(if on { rgb(v.color()) } else { p.faint_bg }).min_size(Vec2::new(54.0, 24.0));
                            if ui.add(b).on_hover_text(format!("{}. Click again to clear.", v.hint())).clicked() {
                                sc.verdict = if on { None } else { Some(v) };
                                changed = true;
                            }
                        }
                    });
                }
                for a in &attrs {
                    ui.horizontal(|ui| {
                        // Left-aligned in a fixed column (`add_sized` would centre it).
                        ui.allocate_ui_with_layout(Vec2::new(92.0, 22.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.set_min_width(92.0);
                            ui.set_max_width(92.0);
                            ui.add(egui::Label::new(RichText::new(a).size(text::SMALL).color(p.muted)).truncate());
                        });
                        for v in lo..=hi {
                            let on = sc.attrs.get(a) == Some(&v);
                            let label = if scale == Scale::Relative3 && v > 0 { format!("+{v}") } else { v.to_string() };
                            let b = egui::Button::new(RichText::new(label).size(text::SMALL).color(if on { p.on_accent } else { p.text })).fill(if on { p.accent_fill } else { p.faint_bg }).min_size(Vec2::new(24.0, 20.0));
                            if ui.add(b).on_hover_text(format!("{a}: {}. Click again to clear.", scale.describe(v))).clicked() {
                                if on {
                                    sc.attrs.remove(a);
                                } else {
                                    sc.attrs.insert(a.clone(), v);
                                }
                                changed = true;
                            }
                        }
                    });
                }
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut sc.note).hint_text(format!("Note on {display}")).desired_width(ui.available_width()))
                    .on_hover_text("A short note on this device for this scene; it goes into the round report")
                    .changed();
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Scene comment").font(text::body_strong()).color(p.text));
            let scene = &mut self.ws.project.scenes[k];
            changed |= ui
                .add(egui::TextEdit::multiline(&mut scene.comment).hint_text("What stands out in this scene").desired_rows(3).desired_width(ui.available_width()))
                .on_hover_text("Shown under the scene in the round report and in the project overview")
                .changed();
            ui.add_space(6.0);
            ui.label(RichText::new("Tip: tag marks with an artifact (Review panel, C) to count issues per device in the round report.").size(text::SMALL).color(p.faint));
        });
        if changed {
            self.touch();
        }
        close
    }

    // ------------------------------------------------------------ setup

    fn setup_window(&mut self, ctx: &egui::Context, core: &mut Core) {
        if !self.setup {
            return;
        }
        let mut open = true;
        let mut changed = false;
        let mut redetect = false;
        let (mut pick_prev, mut clear_prev) = (false, false);
        widgets::app_window("wb-setup", "Project setup").open(&mut open).default_size(520.0, 560.0).min_size(400.0, 320.0).show(ctx, |ui| {
            let p = core.pal;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(ui.available_width() - 8.0);
                widgets::section_title(ui, &p, "PROJECT");
                changed |= ui.add(egui::TextEdit::singleline(&mut self.ws.project.name).hint_text("Project name").desired_width(ui.available_width())).on_hover_text("Shown in the title and on the report's title page").changed();
                ui.label(RichText::new(self.ws.file().display().to_string()).size(text::SMALL).color(p.faint));
                widgets::section_title(ui, &p, "DEVICES");
                ui.label(RichText::new("Rename a device for the panes and reports; the folder or file names stay as they are.").size(text::SMALL).color(p.muted));
                for d in &mut self.ws.project.devices {
                    ui.horizontal(|ui| {
                        ui.add_sized(Vec2::new(140.0, 22.0), egui::Label::new(RichText::new(&d.name).color(p.muted)).truncate());
                        changed |= ui.add(egui::TextEdit::singleline(&mut d.alias).hint_text(d.name.clone()).desired_width(ui.available_width())).on_hover_text("Display name, e.g. \"Galaxy A56 (build 41)\"").changed();
                    });
                }
                widgets::section_title(ui, &p, "SCORES");
                let mut scale = self.ws.project.scale;
                if widgets::segmented_value(ui, &mut scale, &[(Scale::Absolute5, "1 to 5 per device"), (Scale::Relative3, "-3 to +3 vs reference")]) {
                    self.ws.project.scale = scale;
                    self.ws.project.normalize();
                    changed = true;
                }
                ui.add_space(6.0);
                ui.label(RichText::new("Scorecard attributes").font(text::body_strong()).color(p.text));
                let mut remove = None;
                for (i, a) in self.ws.project.attributes.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        changed |= ui.add(egui::TextEdit::singleline(a).desired_width(220.0)).lost_focus();
                        if widgets::icon_button_sized(ui, I::X, "Remove this attribute (its scores are dropped)", false, 22.0, 12.0).clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.ws.project.attributes.remove(i);
                    self.ws.project.normalize();
                    changed = true;
                }
                if widgets::secondary_button(ui, &format!("{}  Add attribute", I::PLUS)).clicked() {
                    self.ws.project.attributes.push(format!("Attribute {}", self.ws.project.attributes.len() + 1));
                    changed = true;
                }
                widgets::section_title(ui, &p, "CAPTURES");
                ui.label(RichText::new("Added or renamed files? Find devices and scenes again; scores and comments are kept for scenes with the same name.").size(text::SMALL).color(p.muted));
                if widgets::secondary_button(ui, &format!("{}  Find devices and scenes again", I::ARROW_CLOCKWISE)).clicked() {
                    redetect = true;
                }
                widgets::section_title(ui, &p, "PREVIOUS ROUND");
                ui.label(RichText::new("Pick the project of the previous round (another folder with a lumen-project.json) to see which scores went up or down, scene by scene.").size(text::SMALL).color(p.muted));
                ui.horizontal(|ui| {
                    let current = self.prev.as_ref().map(|w| w.root.display().to_string()).unwrap_or_else(|| "None".into());
                    ui.add(egui::Label::new(RichText::new(current).size(text::SMALL).color(p.text)).truncate());
                });
                ui.horizontal(|ui| {
                    if widgets::secondary_button(ui, &format!("{}  Choose...", I::FOLDER_OPEN)).clicked() {
                        pick_prev = true;
                    }
                    if self.prev.is_some() && widgets::secondary_button(ui, "Clear").clicked() {
                        clear_prev = true;
                    }
                });
                widgets::section_title(ui, &p, "ARTIFACT TAGS");
                let names: Vec<&str> = ARTIFACT_TAGS.iter().map(|(n, _)| *n).collect();
                ui.label(RichText::new(names.join(", ")).size(text::SMALL).color(p.muted));
            });
        });
        if !open {
            self.setup = false;
            // Attribute names edited in place: drop empty and duplicate ones.
            let mut seen = Vec::new();
            self.ws.project.attributes.retain(|a| {
                let a = a.trim().to_string();
                !a.is_empty() && !seen.contains(&a) && {
                    seen.push(a);
                    true
                }
            });
            self.ws.project.normalize();
            changed = true;
        }
        if redetect {
            self.redetect(core);
        }
        if clear_prev {
            self.ws.project.previous = None;
            self.prev = None;
            changed = true;
        }
        if pick_prev {
            let start = self.ws.root.parent().map(Path::to_path_buf).unwrap_or_else(|| self.ws.root.clone());
            if let Some(dir) = rfd::FileDialog::new().set_title("Folder of the previous round's project").set_directory(&start).pick_folder() {
                match Workspace::load(&dir) {
                    Ok(w) if w.root != self.ws.root => {
                        self.ws.project.previous = Some(relative_path(&self.ws.root, &dir));
                        self.prev = Some(w);
                        changed = true;
                    }
                    Ok(_) => core.toasts.info("That is this project"),
                    Err(_) => core.toasts.error("That folder has no Lumen project. Open it in the Workbench once first."),
                }
            }
        }
        if changed {
            self.touch();
        }
    }

    /// Find devices and scenes again, keeping scores, comments and votes of
    /// scenes and devices that still exist.
    fn redetect(&mut self, core: &mut Core) {
        let fresh = Workspace::detect(&self.ws.root, &exif_of);
        let old = std::mem::replace(&mut self.ws.project, fresh.project);
        let p = &mut self.ws.project;
        p.name = old.name;
        p.reference = old.reference.filter(|r| p.devices.iter().any(|d| &d.name == r));
        p.scale = old.scale;
        p.attributes = old.attributes;
        p.votes = old.votes;
        for d in &mut p.devices {
            if let Some(o) = old.devices.iter().find(|o| o.name == d.name) {
                d.alias = o.alias.clone();
            }
        }
        for s in &mut p.scenes {
            if let Some(o) = old.scenes.iter().find(|o| o.name == s.name) {
                s.scores = o.scores.clone();
                s.comment = o.comment.clone();
                s.done = o.done;
                if s.shots == o.shots {
                    s.measures = o.measures.clone();
                }
            }
        }
        p.normalize();
        let n = p.scenes.len();
        self.selected = self.selected.min(n.saturating_sub(1));
        self.refresh_meta();
        self.touch();
        core.toasts.info(format!("Found {} devices and {n} scenes", self.ws.project.devices.len()));
    }

    // ------------------------------------------------------------ export

    fn open_export(&mut self) {
        let prefs = super::review::Prefs::load();
        self.export = Some(ExportForm {
            title: self.ws.project.name.clone(),
            author: prefs.author.clone(),
            summary: String::new(),
            format: prefs.format(),
            details: true,
            reviewed_only: false,
        });
    }

    fn export_dialog(&mut self, ctx: &egui::Context, core: &mut Core) {
        let Some(form) = self.export.as_mut() else { return };
        let p = core.pal;
        let mut result = None;
        widgets::modal(ctx, egui::Id::unique("wb-export"), 480.0, |ui| {
            widgets::modal_title(ui, "Round report");
            widgets::modal_body(ui, "One report for the whole project: summary, scores, flagged artifacts, blind test ranking and every scene.");
            ui.add_space(8.0);
            ui.label(RichText::new("Title").size(text::SMALL).color(p.muted));
            ui.add(egui::TextEdit::singleline(&mut form.title).desired_width(ui.available_width()));
            ui.label(RichText::new("Author").size(text::SMALL).color(p.muted));
            ui.add(egui::TextEdit::singleline(&mut form.author).desired_width(ui.available_width()));
            ui.label(RichText::new("Summary").size(text::SMALL).color(p.muted));
            ui.add(egui::TextEdit::multiline(&mut form.summary).hint_text("The overall conclusion, shown first on the summary page").desired_rows(3).desired_width(ui.available_width()));
            ui.add_space(6.0);
            for f in report::Format::ALL {
                widgets::radio_value(ui, &mut form.format, f, f.label()).on_hover_text(f.hint());
            }
            ui.add_space(4.0);
            widgets::checkbox(ui, &mut form.details, "Include detail crops").on_hover_text("Each scene's detail regions (made with the Detail tool in Mark up) as extra pages");
            widgets::checkbox(ui, &mut form.reviewed_only, "Only scenes marked reviewed").on_hover_text("Leave out scenes nobody has marked as reviewed yet");
            ui.add_space(12.0);
            result = widgets::dialog_buttons(ui, &p, "Cancel", "Export...", widgets::ButtonKind::Primary, true);
        });
        match result {
            Some(true) => {
                let form = self.export.take().expect("form");
                self.start_export(form, core);
            }
            Some(false) => self.export = None,
            None => {}
        }
    }

    fn start_export(&mut self, form: ExportForm, core: &mut Core) {
        self.save_now(&mut core.toasts);
        let ext = form.format.ext();
        let stem = super::review::file_stem(if form.title.trim().is_empty() { "Round report" } else { &form.title });
        let Some(mut out) = rfd::FileDialog::new().add_filter(form.format.label(), &[ext]).set_directory(&self.ws.root).set_file_name(format!("{stem}.{ext}")).save_file() else { return };
        if out.extension().is_none_or(|e| !e.to_string_lossy().eq_ignore_ascii_case(ext)) {
            out.set_extension(ext);
        }
        let mut prefs = super::review::Prefs::load();
        prefs.author = form.author.trim().to_string();
        prefs.format = ext.into();
        prefs.save();
        let job = DeckJob { ws: self.ws.clone(), prev: self.prev.clone(), title: form.title, author: form.author, summary: form.summary, format: form.format, out, details: form.details, reviewed_only: form.reviewed_only };
        let total = job.ws.project.scenes.len();
        let done = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = core.ctx.clone();
        let counter = done.clone();
        std::thread::spawn(move || {
            crate::bg::background_priority();
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| export::run(&job, &counter).map_err(|e| format!("{e:#}"))));
            let _ = tx.send(res.unwrap_or_else(|_| Err("the export failed unexpectedly".into())));
            ctx.request_repaint();
        });
        self.job = Some((rx, done, total));
    }

    fn save_csv(&mut self, core: &mut Core) {
        let name = format!("{} scores.csv", super::review::file_stem(&self.ws.project.name));
        let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_directory(&self.ws.root).set_file_name(name).save_file() else { return };
        // A BOM so Excel reads the text as UTF-8.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(self.ws.project.scores_csv().into_bytes());
        match lumen_core::encode::write_atomic(&path, &bytes) {
            Ok(()) => core.toasts.success(format!("Saved {}", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())),
            Err(e) => core.toasts.error(format!("Could not save the CSV: {e:#}")),
        }
    }
}

/// `target` relative to `base` ("../Round2"), or absolute when on another drive.
fn relative_path(base: &Path, target: &Path) -> String {
    let b: Vec<_> = base.components().collect();
    let t: Vec<_> = target.components().collect();
    let common = b.iter().zip(&t).take_while(|(x, y)| x.as_os_str().eq_ignore_ascii_case(y.as_os_str())).count();
    if common == 0 {
        return target.to_string_lossy().to_string();
    }
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), b.len() - common).collect();
    parts.extend(t[common..].iter().map(|c| c.as_os_str().to_string_lossy().to_string()));
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// "1 vote", "3 votes".
fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// A cell `w` points wide with its content right-aligned (numbers in a table).
fn right_cell(ui: &mut Ui, w: f32, h: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(Vec2::new(w, h), egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.set_min_width(w);
        add(ui);
    });
}

/// A measured value rounded like `measure::fmt_value`, without its unit
/// (the table has the unit in the row name).
fn plain_number(v: f64) -> String {
    let a = v.abs();
    if a >= 100.0 {
        format!("{v:.0}")
    } else if a >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

fn short(a: &str) -> String {
    match a {
        "Exposure" => "Exp".into(),
        "Dynamic range" => "DR".into(),
        "Artifacts" => "Art".into(),
        "Sharpness" => "Sharp".into(),
        "Visual noise" => "Vis. noise".into(),
        "White balance" => "WB".into(),
        "Clipping" => "Clip".into(),
        "Banding" => "Band".into(),
        _ => a.chars().take(5).collect(),
    }
}
