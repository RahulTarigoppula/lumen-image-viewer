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
use lumen_core::workbench::{measure, merge, rank, Choice, DeviceMeasure, Project, RoundDiff, Scale, Verdict, Vote, Workspace, ARTIFACT_TAGS};
use std::sync::atomic::AtomicBool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Time and size of the project file: how a save by someone else is noticed.
type Stamp = (SystemTime, u64);

fn stamp_of(file: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(file).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// File size and time per capture (thumbnail keys).
type ShotMeta = HashMap<PathBuf, (u64, Option<SystemTime>)>;

fn shot_paths(ws: &Workspace) -> Vec<PathBuf> {
    ws.project.scenes.iter().flat_map(|s| s.shots.iter().map(|sh| ws.abs(&sh.path))).collect()
}

fn stat_shots(paths: Vec<PathBuf>) -> ShotMeta {
    paths.into_iter().filter_map(|p| std::fs::metadata(&p).ok().map(|m| (p, (m.len(), m.modified().ok())))).collect()
}

/// What a background read or write of the project file found.
enum IoDone {
    /// The file's stamp now (the poll for saves by someone else).
    Stamp(Option<Stamp>),
    /// The file, read after it changed on disk.
    Read(Option<Stamp>, Result<Project, String>),
    Saved(Synced),
    /// "Save mine as a copy" wrote this file.
    Copied(Result<PathBuf, String>),
}

/// How a save ended.
enum Synced {
    /// Written, with someone else's changes merged in when `merged`: what
    /// is on disk now.
    Wrote { project: Arc<Project>, stamp: Option<Stamp>, merged: bool },
    /// Someone else saved changes to the same values (`theirs` is `None`
    /// when their file cannot be read; `why` says why).
    Conflict { theirs: Option<Project>, stamp: Option<Stamp>, why: String },
    Failed(String),
}

/// A background read or write of the project file.
struct Io {
    rx: Receiver<IoDone>,
    /// What a running save writes.
    sent: Option<Arc<Project>>,
}

/// Someone else saved the project with changes to some of the same values.
struct Conflict {
    theirs: Option<Project>,
    stamp: Option<Stamp>,
    why: String,
    /// "Save mine as a copy" is running.
    copying: bool,
}

fn write_project(file: &Path, p: &Project) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(p).map_err(|e| e.to_string())?;
    lumen_core::encode::write_atomic(file, &bytes).map_err(|e| format!("{e:#}"))
}

/// Save `mine` as the project in `root`. When the file changed since `known`
/// (someone else saved), their version is read and merged with `mine` first;
/// `base` is the version both started from.
fn sync(root: &Path, base: &Project, mine: Arc<Project>, known: Option<Stamp>) -> Synced {
    let file = root.join(lumen_core::workbench::PROJECT_FILE);
    let now = stamp_of(&file);
    let mut out = mine;
    let mut merged = false;
    if now.is_some() && now != known {
        let theirs = match Workspace::load(root) {
            Ok(w) => w.project,
            Err(e) => return Synced::Conflict { theirs: None, stamp: now, why: format!("{e:#}") },
        };
        match merge::merge(base, &out, &theirs) {
            Some(m) if m == theirs => return Synced::Wrote { project: Arc::new(theirs), stamp: now, merged: true },
            Some(m) => {
                out = Arc::new(m);
                merged = true;
            }
            None => return Synced::Conflict { theirs: Some(theirs), stamp: now, why: String::new() },
        }
    }
    match write_project(&file, &out) {
        Ok(()) => Synced::Wrote { project: out, stamp: stamp_of(&file), merged },
        Err(e) => Synced::Failed(e),
    }
}

/// Keep `p` in a file of its own: beside the project, or (`local`, or when
/// that fails) in Lumen's data folder.
fn keep_copy(root: &Path, p: &Project, local: bool) -> Result<PathBuf, String> {
    let time = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let beside = root.join(format!("lumen-project.yours-{time}.json"));
    if !local && write_project(&beside, p).is_ok() {
        return Ok(beside);
    }
    let name = super::review::file_stem(if p.name.trim().is_empty() { "project" } else { &p.name });
    let file = crate::settings::data_dir().join("unsaved-projects").join(format!("{name}-{time}.json"));
    let bytes = serde_json::to_vec_pretty(p).map_err(|e| e.to_string())?;
    lumen_core::persist::write_file(&file, &bytes).map_err(|e| format!("{e:#}"))?;
    Ok(file)
}

/// The same scenes with the same captures (measurements still fit).
fn same_scenes(a: &Project, b: &Project) -> bool {
    a.scenes.len() == b.scenes.len() && a.scenes.iter().zip(&b.scenes).all(|(x, y)| x.name == y.name && x.shots == y.shots)
}

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
    /// The name of the trial's scene: the vote names it, and the scene is
    /// found by it (the scene list can change while the trial is shown).
    scene: String,
    /// Votes given in this session.
    pub count: usize,
    rng: rank::Rng,
    pub rater: String,
}

/// One measured capture: scene name, device, the capture's stored path, values.
type Measured = (String, String, String, std::collections::BTreeMap<String, f64>);

/// A running measurement of the project's captures.
struct MeasureJob {
    rx: Receiver<Measured>,
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
    ctx: egui::Context,
    dirty: Option<Instant>,
    /// Save at the next tick (a blind vote, leaving a view), not after `SAVE_DELAY`.
    save_asap: bool,
    /// The last save failed: said once, retried every `SAVE_RETRY`.
    save_failed: bool,
    /// The project as last read from or written to the file: where a merge
    /// with someone else's save starts from.
    base: Arc<Project>,
    disk_stamp: Option<Stamp>,
    /// A change on disk that could not be read: not read again.
    bad_stamp: Option<Stamp>,
    disk_check: Instant,
    /// The background read or write of the project file (one at a time).
    io: Option<Io>,
    conflict: Option<Conflict>,
    /// Messages from background work for the next tick: (error, text).
    notes: Vec<(bool, String)>,
    /// Counts changes to the project: cached side panel values follow it.
    rev: u64,
    ranking: Option<(u64, Vec<rank::Ranked>)>,
    rounds: Option<(u64, RoundDiff)>,
    /// File size and time per capture (thumbnail keys).
    meta: ShotMeta,
    meta_rx: Option<Receiver<ShotMeta>>,
    /// "Find devices and scenes again" running.
    detecting: Option<Receiver<Project>>,
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
    /// The report branding dialog (opened from the round report dialog).
    branding: Option<super::branding::BrandingDialog>,
    /// Open the finished report in its app (not for a scripted test run).
    open_report: bool,
}

const SAVE_DELAY: Duration = Duration::from_millis(1200);
/// A failed save (an offline share) is tried again after this.
const SAVE_RETRY: Duration = Duration::from_secs(10);
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
    /// The second value says whether a new project was made. Reads files
    /// (and EXIF when detecting): run it off the UI thread.
    pub fn open(root: &Path, ctx: &egui::Context) -> anyhow::Result<(WorkbenchUi, bool)> {
        let (ws, created) = if root.join(lumen_core::workbench::PROJECT_FILE).is_file() {
            (Workspace::load(root)?, false)
        } else {
            let ws = Workspace::detect(root, &exif_of);
            anyhow::ensure!(!ws.project.scenes.is_empty(), "no images or videos were found in {}", root.display());
            ws.save()?;
            (ws, true)
        };
        let disk_stamp = stamp_of(&ws.file());
        let base = Arc::new(ws.project.clone());
        let meta = stat_shots(shot_paths(&ws));
        let mut ui = WorkbenchUi {
            ws,
            selected: 0,
            ctx: ctx.clone(),
            dirty: None,
            save_asap: false,
            save_failed: false,
            base,
            disk_stamp,
            bad_stamp: None,
            disk_check: Instant::now(),
            io: None,
            conflict: None,
            notes: Vec::new(),
            rev: 0,
            ranking: None,
            rounds: None,
            meta,
            meta_rx: None,
            detecting: None,
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
            branding: None,
            open_report: true,
        };
        ui.load_previous();
        Ok((ui, created))
    }

    /// Load the previous round named in the project (if any).
    fn load_previous(&mut self) {
        self.prev = self.ws.project.previous.as_ref().and_then(|p| Workspace::load(&self.ws.abs(p)).ok());
    }

    /// Read the captures' sizes and times again, in the background.
    fn refresh_meta(&mut self) {
        let paths = shot_paths(&self.ws);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new().name("lumen-wb-stat".into()).spawn(move || {
            let _ = tx.send(stat_shots(paths));
            ctx.request_repaint();
        });
        if spawned.is_ok() {
            self.meta_rx = Some(rx);
        }
    }

    pub fn touch(&mut self) {
        self.dirty.get_or_insert_with(Instant::now);
        self.rev += 1;
    }

    /// Save at the next tick instead of after the usual delay.
    pub fn save_soon(&mut self) {
        if self.dirty.is_some() {
            self.save_asap = true;
        }
    }

    /// Run `job` (a read or write of the project file) on a worker thread;
    /// `tick` takes its result. The job says whether the UI must wake up.
    fn spawn_io(&mut self, sent: Option<Arc<Project>>, job: impl FnOnce() -> (IoDone, bool) + Send + 'static) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = self.ctx.clone();
        let spawned = std::thread::Builder::new().name("lumen-wb-io".into()).spawn(move || {
            let (done, wake) = job();
            if tx.send(done).is_ok() && wake {
                ctx.request_repaint();
            }
        });
        match spawned {
            Ok(_) => self.io = Some(Io { rx, sent }),
            Err(e) => {
                log::warn!("workbench: no thread for the project file: {e}");
                if sent.is_some() {
                    self.dirty = Some(Instant::now() + SAVE_RETRY);
                }
            }
        }
    }

    fn start_save(&mut self) {
        self.dirty = None;
        self.save_asap = false;
        let mine = Arc::new(self.ws.project.clone());
        let (root, base, known) = (self.ws.root.clone(), self.base.clone(), self.disk_stamp);
        self.spawn_io(Some(mine.clone()), move || (IoDone::Saved(sync(&root, &base, mine, known)), true));
    }

    /// Has someone else saved the project? A stat off the UI thread: the
    /// project can be on a share that went offline.
    fn start_poll(&mut self) {
        let file = self.ws.file();
        let (known, bad) = (self.disk_stamp, self.bad_stamp);
        self.spawn_io(None, move || {
            let now = stamp_of(&file);
            let changed = now.is_some() && now != known && now != bad;
            (IoDone::Stamp(now), changed)
        });
    }

    fn start_read(&mut self) {
        let (root, file) = (self.ws.root.clone(), self.ws.file());
        self.spawn_io(None, move || {
            let stamp = stamp_of(&file);
            (IoDone::Read(stamp, Workspace::load(&root).map(|w| w.project).map_err(|e| format!("{e:#}"))), true)
        });
    }

    /// Show `next` (a merge, a reload, scenes found again).
    fn apply(&mut self, next: Project) {
        if next == self.ws.project {
            return;
        }
        let scenes_changed = !same_scenes(&self.ws.project, &next);
        self.ws.project = next;
        self.rev += 1;
        self.selected = self.selected.min(self.ws.project.scenes.len().saturating_sub(1));
        if scenes_changed {
            // Its results belong to scenes that may be gone or renumbered.
            if let Some(j) = self.measuring.take() {
                j.cancel.store(true, Ordering::Relaxed);
                self.notes.push((false, "Measuring stopped: the scenes changed".into()));
            }
            self.refresh_meta();
        }
    }

    /// Someone else saved `theirs`: add their changes to the open project.
    fn take_theirs(&mut self, theirs: Project, stamp: Option<Stamp>) {
        match merge::merge(&self.base, &self.ws.project, &theirs) {
            Some(next) => {
                let msg = if self.dirty.is_some() { "The project was changed by someone else: their changes were added to yours" } else { "The project was changed by someone else and has been reloaded" };
                self.apply(next);
                self.base = Arc::new(theirs);
                self.disk_stamp = stamp;
                // Not during a blind test: other raters' votes come in all the time.
                if self.blind.is_none() {
                    self.notes.push((false, msg.into()));
                }
            }
            None => self.conflict = Some(Conflict { theirs: Some(theirs), stamp, why: String::new(), copying: false }),
        }
    }

    fn on_io(&mut self, done: IoDone, sent: Option<Arc<Project>>) {
        match done {
            IoDone::Stamp(now) => {
                if now.is_some() && now != self.disk_stamp && now != self.bad_stamp {
                    self.start_read();
                }
            }
            IoDone::Read(stamp, Ok(theirs)) => self.take_theirs(theirs, stamp),
            IoDone::Read(stamp, Err(e)) => {
                log::warn!("workbench: the project file changed but cannot be read: {e}");
                self.bad_stamp = stamp;
            }
            IoDone::Saved(Synced::Wrote { project, stamp, merged }) => {
                if self.save_failed {
                    self.notes.push((false, "The project is saved again".into()));
                }
                self.save_failed = false;
                self.bad_stamp = None;
                if merged {
                    // Their changes, plus what changed here while the save ran.
                    let sent = sent.unwrap_or_else(|| project.clone());
                    match merge::merge(&sent, &self.ws.project, &project) {
                        Some(next) => self.apply(next),
                        None => self.conflict = Some(Conflict { theirs: Some((*project).clone()), stamp, why: String::new(), copying: false }),
                    }
                    if self.blind.is_none() {
                        self.notes.push((false, "The project was also changed by someone else: both sets of changes are kept".into()));
                    }
                }
                self.base = project;
                self.disk_stamp = stamp;
            }
            IoDone::Saved(Synced::Conflict { theirs, stamp, why }) => self.conflict = Some(Conflict { theirs, stamp, why, copying: false }),
            IoDone::Saved(Synced::Failed(e)) => {
                if !self.save_failed {
                    self.notes.push((true, format!("Could not save the project (trying again): {e}")));
                }
                self.save_failed = true;
                self.dirty = Some(Instant::now() + SAVE_RETRY);
            }
            IoDone::Copied(Ok(path)) => {
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                self.notes.push((false, format!("Your version was saved as {name}")));
                if let Some(c) = self.conflict.take() {
                    match c.theirs {
                        Some(t) => {
                            self.apply(t.clone());
                            self.base = Arc::new(t);
                            self.disk_stamp = c.stamp;
                        }
                        None => self.bad_stamp = c.stamp,
                    }
                    self.dirty = None;
                }
            }
            IoDone::Copied(Err(e)) => {
                self.notes.push((true, format!("Could not save your version: {e}")));
                if let Some(c) = &mut self.conflict {
                    c.copying = false;
                }
            }
        }
    }

    fn poll_io(&mut self) {
        let Some(io) = &self.io else { return };
        let done = match io.rx.try_recv() {
            Ok(d) => d,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                // The worker ended without an answer: save again later.
                if self.io.take().is_some_and(|io| io.sent.is_some()) {
                    self.dirty = Some(Instant::now() + SAVE_RETRY);
                }
                return;
            }
        };
        let sent = self.io.take().and_then(|io| io.sent);
        self.on_io(done, sent);
    }

    /// Save now on this thread (the window closes, another project opens):
    /// waits for a running save, and keeps a copy when it cannot ask.
    pub fn save_blocking(&mut self) {
        let mut stuck = false;
        if let Some(io) = self.io.take() {
            match io.rx.recv_timeout(Duration::from_secs(10)) {
                Ok(done @ (IoDone::Saved(_) | IoDone::Copied(_))) => self.on_io(done, io.sent),
                // A poll or read: the save below looks at the file itself.
                Ok(_) => {}
                Err(e) => {
                    // A share that does not answer: keep a local copy instead.
                    stuck = e.is_timeout();
                    if io.sent.is_some() {
                        self.dirty.get_or_insert_with(Instant::now);
                    }
                }
            }
        }
        let kept = |res: Result<PathBuf, String>| match res {
            Ok(p) => log::warn!("workbench: your version of the project was saved as {}", p.display()),
            Err(e) => log::error!("workbench: could not keep your version of the project: {e}"),
        };
        if self.conflict.take().is_some() {
            // Nobody to ask: their version stays, mine is kept beside it.
            kept(keep_copy(&self.ws.root, &self.ws.project, stuck));
            return;
        }
        if self.dirty.take().is_none() {
            return;
        }
        if stuck {
            kept(keep_copy(&self.ws.root, &self.ws.project, true));
            return;
        }
        match sync(&self.ws.root, &self.base, Arc::new(self.ws.project.clone()), self.disk_stamp) {
            Synced::Wrote { project, stamp, .. } => {
                self.base = project;
                self.disk_stamp = stamp;
            }
            Synced::Conflict { .. } => kept(keep_copy(&self.ws.root, &self.ws.project, false)),
            Synced::Failed(e) => {
                log::error!("workbench: could not save the project: {e}");
                kept(keep_copy(&self.ws.root, &self.ws.project, true));
            }
        }
    }

    /// Autosave, reload when someone else saved the project, background
    /// results, export progress. Runs every frame in every mode.
    pub fn tick(&mut self, core: &mut Core) {
        self.poll_io();
        if self.io.is_some() && self.dirty.is_some() {
            // A poll that found nothing does not wake the UI: look again soon.
            core.ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.io.is_none() && self.conflict.is_none() {
            if let Some(t) = self.dirty {
                let wait = if self.save_asap { Duration::ZERO } else { SAVE_DELAY.saturating_sub(t.elapsed()) };
                if wait.is_zero() {
                    self.start_save();
                } else {
                    core.ctx.request_repaint_after(wait);
                }
            } else if self.disk_check.elapsed() > Duration::from_secs(3) {
                self.disk_check = Instant::now();
                self.start_poll();
            }
        }
        for (error, msg) in std::mem::take(&mut self.notes) {
            if error {
                core.toasts.error(msg);
            } else {
                core.toasts.info(msg);
            }
        }
        if let Some(rx) = &self.meta_rx {
            match rx.try_recv() {
                Ok(m) => {
                    self.meta = m;
                    self.meta_rx = None;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.meta_rx = None,
                Err(_) => {}
            }
        }
        if let Some(rx) = &self.detecting {
            match rx.try_recv() {
                Ok(fresh) => {
                    self.detecting = None;
                    self.finish_redetect(fresh, core);
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.detecting = None;
                    core.toasts.error("Could not find the devices and scenes");
                }
                Err(_) => {}
            }
        }
        self.poll_measure(core);
        if let Some((rx, _, _)) = &self.job {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    core.toasts.success(format!("Report saved: {name}"));
                    if self.open_report {
                        crate::fileops::open_with_default(&path);
                    }
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
        // Results name their scene and capture (not an index): the scenes can
        // be found again or reloaded while this runs.
        let jobs: Vec<(String, String, String, PathBuf)> = todo
            .into_iter()
            .filter_map(|(i, d)| {
                let s = self.ws.project.scenes.get(i)?;
                let stored = s.shot(&d)?.path.clone();
                let path = self.ws.abs(&stored);
                Some((s.name.clone(), d, stored, path))
            })
            .collect();
        let total = jobs.len();
        let done = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();
        let (ctx, d2, c2) = (core.ctx.clone(), done.clone(), cancel.clone());
        std::thread::spawn(move || {
            crate::bg::background_priority();
            let pool = lumen_analysis::job::AnalysisPool::new(None);
            for (scene, dev, stored, path) in jobs {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let img = lumen_core::decode::decode_file(&path, lumen_core::decode::DecodeOptions { color_manage: true, animations: false }).ok()?;
                    Some(measure::measure(&img.image, &pool))
                }));
                if let Ok(Some(values)) = res {
                    let _ = tx.send((scene, dev, stored, values));
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
            for (scene, dev, stored, values) in got {
                // Only into the scene and capture that was measured.
                if let Some(s) = self.ws.project.scenes.iter_mut().find(|s| s.name == scene && s.shot(&dev).is_some_and(|sh| sh.path == stored)) {
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
            branding: p.branding.as_ref().map(|b| b.resolved(Some(&self.ws.root))),
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
        self.blind = Some(Blind { trial: None, scene: String::new(), count: 0, rng: rank::Rng::new(seed), rater });
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
        b.scene = t.as_ref().and_then(|t| names.get(t.scene)).cloned().unwrap_or_default();
        b.trial = t.clone();
        t
    }

    /// The files and link of the current blind trial (left, right).
    pub fn blind_link(&self) -> Option<(Vec<PathBuf>, SceneLink)> {
        let b = self.blind.as_ref()?;
        let t = b.trial.as_ref()?;
        let k = self.ws.project.scene_index(&b.scene)?;
        let s = &self.ws.project.scenes[k];
        let left = s.shot(&t.left)?;
        let right = s.shot(&t.right)?;
        let link = SceneLink {
            scene: k,
            count: self.ws.project.scenes.len(),
            name: String::new(),
            labels: Vec::new(),
            colors: Vec::new(),
            blind: true,
            progress: format!("Trial {}  ·  {} in the project", b.count + 1, plural(self.ws.project.votes.len(), "vote")),
            done: false,
            branding: None,
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
        // The scene shown (by name: the scene list may have changed since).
        let scene = b.scene.clone();
        if self.ws.project.scene_index(&scene).is_none() {
            return;
        }
        self.ws.project.votes.push(Vote { scene, a: t.left, b: t.right, choice, rater: b.rater.clone() });
        b.count += 1;
        // Every vote is written right away: closing the window loses none.
        self.touch();
        self.save_asap = true;
        self.next_trial();
    }

    // ------------------------------------------------------------ overview

    /// The thumbnail of `path` in the top-left of `rect` (so it lines up with
    /// the column's header and caption); returns where it was drawn.
    fn thumb(&self, core: &mut Core, ui: &Ui, path: &Path, rect: Rect) -> Rect {
        let p = core.pal;
        let Some((size, mtime)) = self.meta.get(path).copied() else {
            // Still being looked up (scenes found again), or missing.
            let icon = if self.meta_rx.is_some() { I::HOURGLASS } else { I::IMAGE_BROKEN };
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.faint_bg);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(20.0), p.faint);
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

    /// The blind test ranking (a fit and 200 bootstrap fits): computed
    /// again only after a change, not on every repaint.
    fn ranking(&mut self) -> Vec<rank::Ranked> {
        if self.ranking.as_ref().is_none_or(|(rev, _)| *rev != self.rev) {
            let names: Vec<String> = self.ws.project.devices.iter().map(|d| d.name.clone()).collect();
            self.ranking = Some((self.rev, rank::rank(&names, &self.ws.project.votes)));
        }
        self.ranking.as_ref().map(|(_, r)| r.clone()).unwrap_or_default()
    }

    /// What changed since the previous round, computed again only after a change.
    fn rounds(&mut self) -> Option<RoundDiff> {
        let prev = self.prev.as_ref()?;
        if self.rounds.as_ref().is_none_or(|(rev, _)| *rev != self.rev) {
            self.rounds = Some((self.rev, lumen_core::workbench::compare_rounds(&self.ws.project, &prev.project, 1)));
        }
        self.rounds.as_ref().map(|(_, d)| d.clone())
    }

    /// Scenes shown with the current filter, as indices.
    fn visible(&self) -> Vec<usize> {
        let f = self.filter.trim().to_lowercase();
        self.ws.project.scenes.iter().enumerate().filter(|(_, s)| f.is_empty() || s.name.to_lowercase().contains(&f) || s.comment.to_lowercase().contains(&f)).map(|(i, _)| i).collect()
    }

    /// The overview (the app calls `tick` every frame, in every mode).
    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
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
            let rounds = self.rounds();
            if let (Some(prev), Some(d)) = (&self.prev, rounds) {
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
                for r in self.ranking().into_iter().filter(|r| r.votes > 0) {
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
                let finding = self.detecting.is_some();
                let label = if finding { format!("{}  Finding devices and scenes...", I::SPINNER) } else { format!("{}  Find devices and scenes again", I::ARROW_CLOCKWISE) };
                if ui.add_enabled_ui(!finding, |ui| widgets::secondary_button(ui, &label)).inner.clicked() {
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
            self.redetect();
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

    /// Find devices and scenes again (a folder scan and EXIF reads, so on a
    /// worker thread); `finish_redetect` takes the result.
    fn redetect(&mut self) {
        if self.detecting.is_some() {
            return;
        }
        let (root, ctx) = (self.ws.root.clone(), self.ctx.clone());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new().name("lumen-wb-detect".into()).spawn(move || {
            crate::bg::background_priority();
            let _ = tx.send(Workspace::detect(&root, &exif_of).project);
            ctx.request_repaint();
        });
        if spawned.is_ok() {
            self.detecting = Some(rx);
        }
    }

    /// The scenes found again: keep scores, comments and votes of scenes and
    /// devices that still exist (also those made while the scan ran).
    fn finish_redetect(&mut self, fresh: Project, core: &mut Core) {
        let old = &self.ws.project;
        let mut p = fresh;
        p.name = old.name.clone();
        p.reference = old.reference.clone().filter(|r| p.devices.iter().any(|d| &d.name == r));
        p.scale = old.scale;
        p.attributes = old.attributes.clone();
        p.votes = old.votes.clone();
        p.previous = old.previous.clone();
        p.branding = old.branding.clone();
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
        // Stops a measurement when the scenes changed (its results would
        // land in the wrong scene) and reads the captures' times again.
        self.apply(p);
        self.touch();
        core.toasts.info(format!("Found {} devices and {n} scenes", self.ws.project.devices.len()));
    }

    // ------------------------------------------------------------ export

    /// The round report dialog (title, author, format, branding).
    pub fn open_export(&mut self) {
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
        let mut edit_branding = false;
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
            ui.add_space(8.0);
            let (b, whose) = match &self.ws.project.branding {
                Some(b) => (b, "this project"),
                None => (&core.settings.report_branding, "default"),
            };
            if super::branding::summary_row(ui, &p, b, whose, true) {
                edit_branding = true;
            }
            ui.add_space(4.0);
            result = widgets::dialog_buttons(ui, &p, "Cancel", "Export...", widgets::ButtonKind::Primary, true);
        });
        if edit_branding {
            self.branding = Some(Default::default());
        }
        if let Some(d) = &mut self.branding {
            let target = super::branding::Target::Project { own: &mut self.ws.project.branding, defaults: &mut core.settings.report_branding, root: &self.ws.root };
            let out = d.show(ctx, &p, target);
            if out.project_changed {
                self.dirty.get_or_insert_with(Instant::now);
            }
            if let Some(e) = out.error {
                core.toasts.error(e);
            }
            if !out.open {
                self.branding = None;
            }
            // The branding dialog is on top: the export dialog waits.
            return;
        }
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
        self.save_soon();
        let ext = form.format.ext();
        let stem = super::review::file_stem(if form.title.trim().is_empty() { "Round report" } else { &form.title });
        let dlg = rfd::FileDialog::new().add_filter(form.format.label(), &[ext]).set_directory(&self.ws.root).set_file_name(format!("{stem}.{ext}"));
        // A scripted test run answers the dialog itself and opens nothing.
        let scripted = crate::devhook::dialog_answer();
        self.open_report = scripted.is_none();
        let Some(mut out) = scripted.or_else(|| dlg.save_file()) else { return };
        if out.extension().is_none_or(|e| !e.to_string_lossy().eq_ignore_ascii_case(ext)) {
            out.set_extension(ext);
        }
        let mut prefs = super::review::Prefs::load();
        prefs.author = form.author.trim().to_string();
        prefs.format = ext.into();
        prefs.save();
        let branding = super::branding::effective(self.ws.project.branding.as_ref(), &core.settings.report_branding, Some(&self.ws.root));
        let job = DeckJob { ws: self.ws.clone(), prev: self.prev.clone(), title: form.title, author: form.author, summary: form.summary, format: form.format, out, details: form.details, reviewed_only: form.reviewed_only, branding };
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

    // ------------------------------------------------------------ conflicts

    /// Someone else saved changes to the same values: ask which to keep.
    /// Shown over every mode; saving waits for the answer.
    pub fn conflict_dialog(&mut self, ctx: &egui::Context, core: &mut Core) {
        let Some(c) = &self.conflict else { return };
        let p = core.pal;
        let (readable, copying) = (c.theirs.is_some(), c.copying);
        let body = if readable {
            "Someone else saved this project while you were working on it, and some of the same scores, notes or settings were changed by both of you.".to_string()
        } else {
            format!("Someone else changed the project file and it cannot be read now: {}", c.why)
        };
        let mut choice = None;
        widgets::modal(ctx, egui::Id::new("wb-conflict"), 460.0, |ui| {
            widgets::modal_title(ui, "The project was changed by someone else");
            widgets::modal_body(ui, &body);
            ui.add_space(super::theme::space::BUTTONS_GAP);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(!copying, |ui| {
                    if widgets::primary_button(ui, &p, "Keep mine").on_hover_text("Save your version over theirs").clicked() {
                        choice = Some(0);
                    }
                    if readable && widgets::secondary_button(ui, "Load theirs").on_hover_text("Drop your changes and show their version").clicked() {
                        choice = Some(1);
                    }
                    let label = if copying { "Saving..." } else { "Save mine as a copy" };
                    if widgets::secondary_button(ui, label).on_hover_text("Keep your version in a file of its own beside the project, then show theirs").clicked() {
                        choice = Some(2);
                    }
                });
            });
        });
        match choice {
            Some(0) => {
                let c = self.conflict.take().expect("conflict");
                if let Some(t) = c.theirs {
                    self.base = Arc::new(t);
                }
                self.disk_stamp = c.stamp;
                self.touch();
                self.save_asap = true;
            }
            Some(1) => {
                let c = self.conflict.take().expect("conflict");
                if let Some(t) = c.theirs {
                    self.apply(t.clone());
                    self.base = Arc::new(t);
                    self.disk_stamp = c.stamp;
                    self.dirty = None;
                }
            }
            Some(2) if self.io.is_none() => {
                if let Some(c) = &mut self.conflict {
                    c.copying = true;
                }
                let (root, mine) = (self.ws.root.clone(), self.ws.project.clone());
                self.spawn_io(None, move || (IoDone::Copied(keep_copy(&root, &mine, false)), true));
            }
            _ => {}
        }
    }
}

impl Drop for WorkbenchUi {
    /// Nothing is lost when the window closes or another project opens.
    fn drop(&mut self) {
        if let Some(j) = self.measuring.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        self.save_blocking();
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

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::workbench::detect::{self, FileInfo};

    #[test]
    fn a_save_merges_with_someone_elses_and_never_overwrites_a_conflict() {
        let root = std::env::temp_dir().join(format!("lumen-wb-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join(lumen_core::workbench::PROJECT_FILE);
        let base = Project::from_detection("Round", &detect::detect(&[FileInfo::new("A56/day.jpg"), FileInfo::new("X/day.jpg")]));
        let Synced::Wrote { stamp, merged: false, .. } = sync(&root, &base, Arc::new(base.clone()), None) else { panic!("first save") };
        assert_eq!(stamp, stamp_of(&file));
        // Someone else votes and saves while I write a comment.
        let mut theirs = base.clone();
        theirs.votes.push(Vote { scene: base.scenes[0].name.clone(), a: "A56".into(), b: "X".into(), choice: Choice::A, rater: "b".into() });
        write_project(&file, &theirs).unwrap();
        let mut mine = base.clone();
        mine.scenes[0].comment = "flare".into();
        let Synced::Wrote { project, merged: true, stamp } = sync(&root, &base, Arc::new(mine.clone()), stamp) else { panic!("not merged") };
        let on_disk = Workspace::load(&root).unwrap().project;
        assert_eq!(on_disk, *project);
        assert_eq!((on_disk.votes.len(), on_disk.scenes[0].comment.as_str()), (1, "flare"));
        assert_eq!(stamp, stamp_of(&file));
        // Both change the same comment: nothing is written, theirs stays.
        let mut theirs = on_disk.clone();
        theirs.scenes[0].comment = "halo".into();
        write_project(&file, &theirs).unwrap();
        let mut mine = on_disk.clone();
        mine.scenes[0].comment = "soft".into();
        assert!(matches!(sync(&root, &on_disk, Arc::new(mine.clone()), stamp), Synced::Conflict { theirs: Some(_), .. }));
        assert_eq!(Workspace::load(&root).unwrap().project.scenes[0].comment, "halo");
        // "Save mine as a copy" keeps it beside the project.
        let copy = keep_copy(&root, &mine, false).unwrap();
        assert_eq!(copy.parent(), Some(root.as_path()));
        let kept: Project = serde_json::from_slice(&std::fs::read(&copy).unwrap()).unwrap();
        assert_eq!(kept.scenes[0].comment, "soft");
        let _ = std::fs::remove_dir_all(&root);
    }
}
