//! Remote AI analysis state, shared by Settings > AI and the panel's AI tab
//! (it lives in `Core::ai`): the settings, the API key, Test connection, the
//! model list, the request being prepared, previewed or streamed, and the
//! per-file result history.
//!
//! All network and image work runs on `lumen_ai` worker threads or on a
//! preparation thread started here; the UI thread only drains channels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Receiver;
use image::RgbaImage;
use lumen_ai::{
    prepare, spawn_list_models, spawn_run, spawn_test_connection, spawn_vision_probe, AiError, AiEvent, AiSettings, ApiKey, Client,
    ConnectionReport, Endpoint, FrameRef, InputImage, ModelCard, OutputKind, PreparedRequest, Preset, ProbeCache, Repaint, RequestInput,
    RunResult, SecretStore, StepResult, Task, Usage, VisionVerdict,
};
use serde::{Deserialize, Serialize};

use super::{FrameInfo, Pixels, Subject, SubjectImage};
use crate::app::Core;

/// Results kept per file (oldest dropped first).
const HISTORY_PER_FILE: usize = 30;

/// What a run was about, carried from Run to its history record.
#[derive(Clone, Debug)]
pub struct RunMeta {
    pub file_key: String,
    pub label: String,
    pub frame: Option<FrameInfo>,
    /// Pixel size of A and B as sent (finding boxes are in these pixels).
    pub dims: [Option<(u32, u32)>; 2],
    pub preset: String,
}

/// A request that went through `lumen_ai::prepare`.
pub struct Prepared {
    pub req: PreparedRequest,
    pub dims: [Option<(u32, u32)>; 2],
}

pub enum Phase {
    Idle,
    /// Images are decoded, scaled and encoded off the UI thread.
    Preparing { rx: Receiver<Result<Prepared, String>>, meta: RunMeta },
    /// The send preview is showing; nothing has been sent.
    Preview { req: Box<PreparedRequest>, meta: RunMeta, thumbs: Vec<egui::TextureHandle> },
    /// Streaming from the server.
    Running { task: Task, meta: RunMeta, server: String },
}

/// The answer as it streams in.
#[derive(Default)]
pub struct Live {
    pub text: String,
    pub reasoning: String,
    pub usage: Option<Usage>,
    pub status: Option<String>,
    pub connected: bool,
    pub pass: Option<(u8, u8)>,
    pub started: Option<Instant>,
    /// Free-form answer (rendered as Markdown) rather than raw JSON.
    pub markdown: bool,
}

/// How a run ended.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Outcome {
    Done(Box<RunResult>),
    Failed { message: String, detail: Option<String>, partial: String },
    Cancelled { partial: String },
}

/// One entry of the per-file history (stored locally only).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    /// Local time, "2026-10-19 14:03:12".
    pub when: String,
    pub file_key: String,
    pub label: String,
    pub frame: Option<FrameInfo>,
    pub dims: [Option<(u32, u32)>; 2],
    pub preset: String,
    pub outcome: Outcome,
}

impl RunRecord {
    pub fn result(&self) -> Option<&RunResult> {
        match &self.outcome {
            Outcome::Done(r) => Some(r),
            _ => None,
        }
    }
}

/// What the AI tab asks for when Run is pressed.
pub struct RunRequest {
    pub subject: Subject,
    pub preset: Preset,
    /// Region in A's pixels (`subject.a.size`): x, y, w, h.
    pub roi: Option<(u32, u32, u32, u32)>,
    pub question: String,
    /// (name, value, unit) from the local metrics, if any.
    pub metrics: Vec<(String, f64, String)>,
    pub metric_scope: String,
}

pub struct AiState {
    pub settings: AiSettings,
    store: SecretStore,
    /// The key of `key_target`, read once from the store.
    key: Option<ApiKey>,
    key_target: Option<String>,
    /// Settings > AI edit buffers.
    pub host_edit: String,
    pub host_error: Option<String>,
    pub key_edit: String,
    pub model_edit: String,
    test: Option<Task>,
    pub steps: Vec<StepResult>,
    pub report: Option<ConnectionReport>,
    pub models: Vec<ModelCard>,
    models_task: Option<Task>,
    probe: Option<(String, Task)>,
    pub probes: ProbeCache,
    pub phase: Phase,
    pub live: Live,
    /// A problem found before anything was sent (no model, no image...).
    pub error: Option<String>,
    history: HashMap<String, Vec<RunRecord>>,
    /// The result shown in the panel and on the image: (file key, index).
    pub shown: Option<(String, usize)>,
    /// `settings` changed and must be copied into `Settings::ai`.
    pub dirty: bool,
    /// The copy in `Settings::ai` was written (a settings reset clears it).
    written: bool,
    repaint: Repaint,
    /// Messages for the app's toasts: (is error, text).
    pub notes: Vec<(bool, String)>,
}

impl AiState {
    pub fn new(ctx: &egui::Context, settings: &crate::settings::Settings) -> AiState {
        let ai: AiSettings = serde_json::from_value(settings.ai.clone()).unwrap_or_default();
        let repaint: Repaint = {
            let c = ctx.clone();
            Arc::new(move || c.request_repaint())
        };
        let mut st = AiState {
            host_edit: String::new(),
            model_edit: ai.model.clone().unwrap_or_default(),
            settings: ai,
            store: SecretStore::new(&crate::settings::data_dir()),
            key: None,
            key_target: None,
            host_error: None,
            key_edit: String::new(),
            test: None,
            steps: Vec::new(),
            report: None,
            models: Vec::new(),
            models_task: None,
            probe: None,
            probes: load_probes(),
            phase: Phase::Idle,
            live: Live::default(),
            error: None,
            history: HashMap::new(),
            shown: None,
            dirty: false,
            written: !settings.ai.is_null(),
            repaint,
            notes: Vec::new(),
        };
        st.host_edit = st.host_text();
        st
    }

    /// The host field as the user should see it: the host, with the port
    /// only when it differs from the port field's default.
    fn host_text(&self) -> String {
        self.settings.host.clone()
    }

    fn reload(&mut self, settings: &crate::settings::Settings) {
        self.settings = serde_json::from_value(settings.ai.clone()).unwrap_or_default();
        self.host_edit = self.host_text();
        self.model_edit = self.settings.model.clone().unwrap_or_default();
        self.key = None;
        self.key_target = None;
        self.written = false;
    }

    pub fn endpoint(&self) -> Option<Endpoint> {
        self.settings.endpoint().ok()
    }

    /// Folds the host field into the settings (on focus loss, Test, Run).
    pub fn apply_host(&mut self) -> bool {
        if self.host_edit.trim().is_empty() {
            self.host_error = None;
            if !self.settings.host.is_empty() {
                self.settings.host.clear();
                self.dirty = true;
            }
            return false;
        }
        let before = (self.settings.host.clone(), self.settings.port, self.settings.https, self.settings.path_prefix.clone());
        match self.settings.apply_server_input(&self.host_edit.clone()) {
            Ok(_) => {
                self.host_error = None;
                self.host_edit = self.host_text();
                if before != (self.settings.host.clone(), self.settings.port, self.settings.https, self.settings.path_prefix.clone()) {
                    // Another server: the last test says nothing about it.
                    self.dirty = true;
                    self.key_target = None;
                    self.report = None;
                    self.steps.clear();
                }
                true
            }
            Err(e) => {
                self.host_error = Some(e.to_string());
                false
            }
        }
    }

    /// The API key for the current server, read from the store once.
    pub fn api_key(&mut self) -> Result<Option<ApiKey>, AiError> {
        let Ok(ep) = self.settings.endpoint() else { return Ok(None) };
        let target = ep.credential_target();
        if self.key_target.as_deref() != Some(target.as_str()) {
            self.key = match self.settings.api_key_ref {
                Some(r) => self.store.load(&target, Some(r))?,
                None => None,
            };
            self.key_target = Some(target);
        }
        Ok(self.key.clone())
    }

    pub fn has_key(&mut self) -> bool {
        matches!(self.api_key(), Ok(Some(_)))
    }

    /// Saves the typed key in Credential Manager (or the DPAPI file).
    pub fn save_key(&mut self) {
        let Some(key) = ApiKey::new(&self.key_edit) else { return };
        wipe(&mut self.key_edit);
        let ep = match self.settings.endpoint() {
            Ok(ep) => ep,
            Err(e) => return self.notes.push((true, format!("Set the server first: {e}"))),
        };
        let target = ep.credential_target();
        match self.store.save(&target, &key) {
            Ok(store) => {
                self.settings.api_key_ref = Some(store);
                self.key = Some(key);
                self.key_target = Some(target);
                self.dirty = true;
                let where_ = match store {
                    lumen_ai::KeyStore::Credman => "Windows Credential Manager",
                    lumen_ai::KeyStore::Dpapi => "an encrypted file in the data folder",
                };
                self.notes.push((false, format!("API key saved in {where_}")));
            }
            Err(e) => self.notes.push((true, e.to_string())),
        }
    }

    /// "Forget key": removes it from both stores.
    pub fn forget_key(&mut self) {
        if let Ok(ep) = self.settings.endpoint() {
            if let Err(e) = self.store.delete(&ep.credential_target()) {
                self.notes.push((true, e.to_string()));
                return;
            }
        }
        self.settings.api_key_ref = None;
        self.key = None;
        self.key_target = None;
        self.dirty = true;
        self.notes.push((false, "API key removed".into()));
    }

    fn client(&mut self) -> Result<Client, AiError> {
        let key = self.api_key()?;
        Client::from_settings(&self.settings, key)
    }

    // ------------------------------------------------------------ connection

    pub fn testing(&self) -> bool {
        self.test.is_some()
    }

    pub fn test_connection(&mut self) {
        if !self.apply_host() {
            if self.host_error.is_none() {
                self.host_error = Some("Enter the server's IP address or host name".into());
            }
            return;
        }
        self.steps.clear();
        self.report = None;
        match self.client() {
            Ok(c) => self.test = Some(spawn_test_connection(c, self.settings.model.clone(), true, self.repaint.clone())),
            Err(e) => self.host_error = Some(e.to_string()),
        }
    }

    pub fn refresh_models(&mut self) {
        if self.models_task.is_some() || !self.apply_host() {
            return;
        }
        match self.client() {
            Ok(c) => self.models_task = Some(spawn_list_models(c, self.repaint.clone())),
            Err(e) => self.notes.push((true, e.to_string())),
        }
    }

    pub fn probing(&self) -> Option<&str> {
        self.probe.as_ref().map(|(m, _)| m.as_str())
    }

    pub fn probe_vision(&mut self, model: &str) {
        if self.probe.is_some() || model.trim().is_empty() {
            return;
        }
        match self.client() {
            Ok(c) => self.probe = Some((model.to_string(), spawn_vision_probe(c, model.to_string(), self.repaint.clone()))),
            Err(e) => self.notes.push((true, e.to_string())),
        }
    }

    /// Selects a model (dropdown or typed id).
    pub fn set_model(&mut self, id: &str) {
        let id = id.trim();
        let new = (!id.is_empty()).then(|| id.to_string());
        if self.settings.model != new {
            self.settings.model = new;
            self.dirty = true;
        }
        self.model_edit = id.to_string();
    }

    pub fn card(&self, id: &str) -> Option<&ModelCard> {
        self.models.iter().find(|m| m.id == id)
    }

    /// "vision", "text only", "probably vision" or "vision unknown, will try".
    pub fn vision_label(&self, id: &str) -> String {
        let server = self.endpoint().map(|e| e.server_key()).unwrap_or_default();
        match self.card(id) {
            Some(card) => match self.probes.get(&server, card) {
                Some(v) => v.label().to_string(),
                None => card.vision_hint().label().to_string(),
            },
            None => lumen_ai::models::vision_hint_for(id).label().to_string(),
        }
    }

    /// The server can be reached and lists models (after Test connection).
    pub fn connected(&self) -> Option<bool> {
        self.report.as_ref().map(|r| r.ok())
    }

    // ------------------------------------------------------------ runs

    pub fn busy(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// Server the current request goes to, while one is in flight.
    pub fn sending_to(&self) -> Option<String> {
        match &self.phase {
            Phase::Running { server, .. } => Some(server.clone()),
            _ if self.test.is_some() || self.models_task.is_some() || self.probe.is_some() => self.endpoint().map(|e| e.display()),
            _ => None,
        }
    }

    pub fn preview_open(&self) -> bool {
        matches!(self.phase, Phase::Preview { .. })
    }

    /// Starts preparing a request (the send preview or the run follows).
    pub fn start_run(&mut self, r: RunRequest) {
        if self.busy() {
            return;
        }
        self.error = None;
        if !self.settings.enabled {
            self.error = Some("Remote AI analysis is off. Turn it on in Settings > AI.".into());
            return;
        }
        if self.settings.host.trim().is_empty() {
            self.error = Some("Set your server in Settings > AI, then press Test connection.".into());
            return;
        }
        let Some(model) = self.settings.model.clone().filter(|m| !m.trim().is_empty()) else {
            self.error = Some("Pick a model first (Settings > AI, Test connection lists them).".into());
            return;
        };
        let (Some(file_key), Some(a)) = (r.subject.file_key(), r.subject.a.as_ref()) else {
            self.error = Some("Nothing to analyze: open an image first.".into());
            return;
        };
        if matches!(a.pixels, Pixels::Pending) || r.subject.b.as_ref().is_some_and(|b| matches!(b.pixels, Pixels::Pending)) {
            self.error = Some("The image is still loading.".into());
            return;
        }
        let meta = RunMeta { file_key, label: r.subject.label(), frame: r.subject.frame(), dims: [None, None], preset: r.preset.name.clone() };
        let settings = self.settings.clone();
        let repaint = self.repaint.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new().name("lumen-ai-prepare".into()).spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_request(&settings, &model, r)))
                .unwrap_or_else(|_| Err("Internal error while preparing the images".into()));
            let _ = tx.send(res);
            repaint();
        });
        match spawned {
            Ok(_) => self.phase = Phase::Preparing { rx, meta },
            Err(e) => self.error = Some(format!("Could not start: {e}")),
        }
    }

    /// Send preview: Send.
    pub fn send_previewed(&mut self, trust: bool) {
        if let Phase::Preview { req, meta, .. } = std::mem::replace(&mut self.phase, Phase::Idle) {
            if trust {
                // "Do not ask again for this server": the plan's rule is that
                // the first request to a server is confirmed, and every one
                // while Always confirm is on.
                if !self.settings.is_trusted(&req.server_key) {
                    self.settings.trusted_servers.push(req.server_key.clone());
                }
                self.settings.always_confirm = false;
                self.dirty = true;
                self.notes.push((false, format!("No send preview for {} from now on (Settings > AI to turn it back on)", req.server)));
            }
            self.send(*req, meta);
        }
    }

    /// Cancel: in the preview, while preparing, or mid-stream.
    pub fn cancel(&mut self) {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Running { task, meta, .. } => {
                // Closes the socket at once (vLLM aborts the request); late
                // events are discarded with the task.
                task.cancel();
                let partial = std::mem::take(&mut self.live.text);
                self.live.status = Some("Cancelled".into());
                self.push_record(meta, Outcome::Cancelled { partial });
            }
            Phase::Preview { .. } | Phase::Preparing { .. } | Phase::Idle => {}
        }
    }

    fn send(&mut self, req: PreparedRequest, meta: RunMeta) {
        let client = match self.client() {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        self.live = Live { started: Some(Instant::now()), markdown: req.output == OutputKind::Markdown, ..Default::default() };
        let server = req.server.clone();
        let task = spawn_run(client, req, self.repaint.clone());
        self.phase = Phase::Running { task, meta, server };
    }

    fn poll(&mut self) {
        // Test connection.
        let mut done = None;
        if let Some(t) = &self.test {
            for ev in t.drain() {
                match ev {
                    AiEvent::TestStep(s) => self.steps.push(s),
                    AiEvent::TestDone(r) => done = Some(r),
                    AiEvent::Done(Err(e)) => self.notes.push((true, e.to_string())),
                    _ => {}
                }
            }
        }
        if let Some(r) = done {
            self.test = None;
            self.finish_test(r);
        }
        // Model list refresh.
        let mut models = None;
        if let Some(t) = &self.models_task {
            for ev in t.drain() {
                if let AiEvent::Models(m) = ev {
                    models = Some(m);
                }
            }
        }
        if let Some(m) = models {
            self.models_task = None;
            match m {
                Ok(list) => {
                    self.models = list;
                    if self.settings.model.as_ref().is_none_or(|m| self.card(m).is_none()) {
                        if let Some(m) = lumen_ai::pick_model(&self.models, None) {
                            self.set_model(&m);
                        }
                    }
                }
                Err(e) => self.notes.push((true, e.to_string())),
            }
        }
        // Vision probe.
        let mut verdict = None;
        if let Some((_, t)) = &self.probe {
            for ev in t.drain() {
                if let AiEvent::Vision { model, verdict: v } = ev {
                    verdict = Some((model, v));
                }
            }
        }
        if let Some((model, v)) = verdict {
            self.probe = None;
            self.remember_verdict(&model, v);
        }
        // Run.
        self.poll_run();
    }

    fn finish_test(&mut self, r: ConnectionReport) {
        self.models = r.models.clone();
        if let (Some(v), Some(m)) = (r.vision.clone(), r.picked_model.clone()) {
            self.remember_verdict(&m, v);
        }
        // Keep the user's model while the server still serves it; otherwise
        // take the one Test picked (the best vision candidate).
        let served = self.settings.model.as_ref().is_some_and(|m| self.card(m).is_some());
        if !served {
            if let Some(m) = &r.picked_model {
                self.set_model(&m.clone());
            }
        }
        self.report = Some(r);
    }

    fn remember_verdict(&mut self, model: &str, v: VisionVerdict) {
        let server = self.endpoint().map(|e| e.server_key()).unwrap_or_default();
        let card = self.card(model).cloned().unwrap_or(ModelCard { id: model.to_string(), ..Default::default() });
        self.probes.insert(&server, &card, v);
        save_probes(&self.probes);
    }

    fn poll_run(&mut self) {
        match &mut self.phase {
            Phase::Preparing { rx, meta } => match rx.try_recv() {
                Ok(Ok(p)) => {
                    let mut meta = meta.clone();
                    meta.dims = p.dims;
                    if self.settings.needs_confirmation(&p.req.server_key) {
                        self.phase = Phase::Preview { req: Box::new(p.req), meta, thumbs: Vec::new() };
                    } else {
                        self.phase = Phase::Idle;
                        self.send(p.req, meta);
                    }
                }
                Ok(Err(e)) => {
                    self.phase = Phase::Idle;
                    self.error = Some(e);
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.phase = Phase::Idle,
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            },
            Phase::Running { task, .. } => {
                let mut finished = None;
                for ev in task.drain() {
                    match ev {
                        AiEvent::Connected => self.live.connected = true,
                        AiEvent::Pass { index, total } => {
                            self.live.text.clear();
                            self.live.reasoning.clear();
                            self.live.pass = Some((index, total));
                        }
                        AiEvent::Reasoning(s) => self.live.reasoning.push_str(&s),
                        AiEvent::Delta(s) => self.live.text.push_str(&s),
                        AiEvent::Usage(u) => self.live.usage = Some(u),
                        AiEvent::Status(s) => self.live.status = Some(s),
                        AiEvent::ImageLimitLearned { server_key, model, limit } => {
                            self.settings.remember_image_limit(&server_key, &model, limit);
                            self.dirty = true;
                        }
                        AiEvent::Done(r) => finished = Some(r),
                        _ => {}
                    }
                }
                if let Some(r) = finished {
                    let Phase::Running { meta, .. } = std::mem::replace(&mut self.phase, Phase::Idle) else { return };
                    let outcome = match r {
                        Ok(res) => {
                            self.live.status = None;
                            Outcome::Done(Box::new(res))
                        }
                        Err(AiError::Cancelled) => Outcome::Cancelled { partial: std::mem::take(&mut self.live.text) },
                        Err(e) => {
                            self.live.status = Some(e.to_string());
                            Outcome::Failed { message: e.to_string(), detail: e.detail().map(str::to_string), partial: self.live.text.clone() }
                        }
                    };
                    self.push_record(meta, outcome);
                }
            }
            Phase::Preview { .. } | Phase::Idle => {}
        }
    }

    // ------------------------------------------------------------ history

    /// Results for `file_key`, oldest first.
    pub fn records(&mut self, file_key: &str) -> &[RunRecord] {
        self.ensure_loaded(file_key);
        self.history.get(file_key).map(|v| v.as_slice()).unwrap_or_default()
    }

    /// The result to show for `file_key`: the one picked in the history, else
    /// the newest.
    pub fn shown_index(&mut self, file_key: &str) -> Option<usize> {
        let n = self.records(file_key).len();
        match &self.shown {
            Some((k, i)) if k == file_key && *i < n => Some(*i),
            _ => n.checked_sub(1),
        }
    }

    pub fn shown_record(&mut self, file_key: &str) -> Option<&RunRecord> {
        let i = self.shown_index(file_key)?;
        self.history.get(file_key).and_then(|v| v.get(i))
    }

    pub fn show_record(&mut self, file_key: &str, i: usize) {
        self.shown = Some((file_key.to_string(), i));
    }

    pub fn clear_history(&mut self, file_key: &str) {
        self.history.insert(file_key.to_string(), Vec::new());
        self.shown = None;
        let _ = std::fs::remove_file(history_path(file_key));
    }

    fn ensure_loaded(&mut self, file_key: &str) {
        if !self.history.contains_key(file_key) {
            let list: Vec<RunRecord> = std::fs::read(history_path(file_key))
                .ok()
                .and_then(|b| serde_json::from_slice::<Vec<RunRecord>>(&b).ok())
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.file_key == file_key)
                .collect();
            self.history.insert(file_key.to_string(), list);
        }
    }

    fn push_record(&mut self, meta: RunMeta, outcome: Outcome) {
        let key = meta.file_key.clone();
        self.ensure_loaded(&key);
        let list = self.history.entry(key.clone()).or_default();
        list.push(RunRecord {
            when: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            file_key: key.clone(),
            label: meta.label,
            frame: meta.frame,
            dims: meta.dims,
            preset: meta.preset,
            outcome,
        });
        if list.len() > HISTORY_PER_FILE {
            let extra = list.len() - HISTORY_PER_FILE;
            list.drain(..extra);
        }
        let n = list.len();
        save_history(&key, list);
        self.shown = Some((key, n - 1));
    }
}

/// Per-frame upkeep (see `analysis_ui::tick`).
pub fn tick(core: &mut Core) {
    // "Reset all settings" in Settings cleared the stored AI settings.
    if core.settings.ai.is_null() && core.ai.written {
        core.ai.reload(&core.settings);
    }
    core.ai.poll();
    for (err, msg) in std::mem::take(&mut core.ai.notes) {
        if err {
            core.toasts.error(msg);
        } else {
            core.toasts.success(msg);
        }
    }
    if core.ai.dirty {
        core.ai.dirty = false;
        if let Ok(v) = serde_json::to_value(&core.ai.settings) {
            core.settings.ai = v;
            core.ai.written = true;
        }
    }
}

/// Clears a secret typed into a text field before dropping it.
fn wipe(s: &mut String) {
    // SAFETY: zero bytes are valid UTF-8, and the string is cleared right after.
    unsafe { s.as_bytes_mut().fill(0) };
    s.clear();
}

// ---------------------------------------------------------------- preparation

/// Runs on the preparation thread: decodes video frames, builds the input and
/// calls `lumen_ai::prepare` (scaling, tiling, encoding).
fn build_request(settings: &AiSettings, model: &str, r: RunRequest) -> Result<Prepared, String> {
    let a = r.subject.a.as_ref().ok_or("Nothing to analyze")?;
    let pa = pixels(a)?;
    let mut input = RequestInput::new(r.preset, input_image(settings, a, pa.clone()));
    let mut dims = [Some(pa.dimensions()), None];
    if let Some(b) = &r.subject.b {
        let pb = pixels(b)?;
        dims[1] = Some(pb.dimensions());
        input = input.with_b(input_image(settings, b, pb));
    }
    if let Some(roi) = r.roi {
        let shown = a.size.unwrap_or(pa.dimensions());
        input = input.with_roi(scale_roi(roi, shown, pa.dimensions()));
    }
    if !r.metrics.is_empty() {
        input = input.with_metrics(&r.metric_scope, r.metrics);
    }
    if !r.question.trim().is_empty() {
        input = input.with_question(r.question.trim());
    }
    let req = prepare(settings, model, input).map_err(|e| e.to_string())?;
    Ok(Prepared { req, dims })
}

fn pixels(img: &SubjectImage) -> Result<Arc<RgbaImage>, String> {
    match &img.pixels {
        Pixels::Ready(p) => Ok(p.clone()),
        Pixels::Pending => Err("The image is still loading".into()),
        #[cfg(feature = "video")]
        Pixels::VideoFrame { path, pts } => {
            crate::video::engine::frame_at(path, *pts).map(Arc::new).map_err(|e| format!("Could not decode the video frame: {e}"))
        }
        #[cfg(not(feature = "video"))]
        Pixels::VideoFrame { .. } => Err("This build has no video support".into()),
    }
}

fn input_image(settings: &AiSettings, img: &SubjectImage, px: Arc<RgbaImage>) -> InputImage {
    let mut i = InputImage::new(px);
    if settings.include_filename {
        i = i.with_file_name(img.file_name());
    }
    if settings.include_exif && img.frame.is_none() {
        if let Some(line) = crate::meta::read_exif(&img.path).map(|e| capture_line(&e)).filter(|l| !l.is_empty()) {
            i = i.with_capture(line);
        }
    }
    if let Some(f) = img.frame {
        i = i.with_frame(FrameRef { timestamp_s: Some(f.timestamp_s), frame_index: Some(f.index) });
    }
    i
}

/// Camera, lens and exposure; never GPS or personal fields.
fn capture_line(e: &crate::meta::ExifSummary) -> String {
    let iso = e.iso.as_ref().map(|v| if v.to_ascii_uppercase().starts_with("ISO") { v.clone() } else { format!("ISO {v}") });
    [e.camera.clone(), e.lens.clone(), e.focal.clone(), e.aperture.clone(), e.exposure.clone(), iso].into_iter().flatten().collect::<Vec<_>>().join(", ")
}

/// A rectangle in an image shown at `from` pixels, in an image of `to` pixels.
pub fn scale_roi((x, y, w, h): (u32, u32, u32, u32), from: (u32, u32), to: (u32, u32)) -> lumen_ai::Roi {
    if from == to || from.0 == 0 || from.1 == 0 {
        return lumen_ai::Roi { x, y, w, h };
    }
    let (kx, ky) = (to.0 as f64 / from.0 as f64, to.1 as f64 / from.1 as f64);
    let x0 = (x as f64 * kx).round() as u32;
    let y0 = (y as f64 * ky).round() as u32;
    let x1 = ((x + w) as f64 * kx).round() as u32;
    let y1 = ((y + h) as f64 * ky).round() as u32;
    lumen_ai::Roi { x: x0, y: y0, w: x1.saturating_sub(x0).max(1), h: y1.saturating_sub(y0).max(1) }
}

// ---------------------------------------------------------------- storage

fn ai_cache_dir() -> PathBuf {
    crate::settings::cache_dir().join("analysis").join("ai")
}

/// FNV-1a: a stable file name for a history key.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

fn history_path(file_key: &str) -> PathBuf {
    ai_cache_dir().join(format!("{:016x}.json", fnv1a(file_key)))
}

fn save_history(file_key: &str, list: &[RunRecord]) {
    let path = history_path(file_key);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(text) = serde_json::to_vec(list) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn load_probes() -> ProbeCache {
    std::fs::read(ai_cache_dir().join("vision-probes.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_probes(p: &ProbeCache) {
    let dir = ai_cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_vec(p) {
        let _ = std::fs::write(dir.join("vision-probes.json"), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roi_scales_to_the_sent_image() {
        let r = scale_roi((100, 50, 200, 100), (1000, 500), (2000, 1000));
        assert_eq!((r.x, r.y, r.w, r.h), (200, 100, 400, 200));
        let same = scale_roi((1, 2, 3, 4), (10, 10), (10, 10));
        assert_eq!((same.x, same.y, same.w, same.h), (1, 2, 3, 4));
    }

    #[test]
    fn history_names_are_stable() {
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(history_path("C:\\a.jpg"), history_path("C:\\b.jpg"));
    }

    #[test]
    fn capture_line_never_has_gps() {
        let e = crate::meta::ExifSummary {
            camera: Some("Canon R5".into()),
            iso: Some("3200".into()),
            gps: Some((1.0, 2.0)),
            artist: Some("Someone".into()),
            ..Default::default()
        };
        assert_eq!(capture_line(&e), "Canon R5, ISO 3200");
    }

    #[test]
    fn records_round_trip() {
        let r = RunRecord {
            when: "2026-10-19 10:00:00".into(),
            file_key: "k".into(),
            label: "x.jpg".into(),
            frame: Some(FrameInfo { timestamp_s: 1.5, index: 36 }),
            dims: [Some((10, 20)), None],
            preset: "Quality critique".into(),
            outcome: Outcome::Failed { message: "Server not reachable".into(), detail: None, partial: String::new() },
        };
        let j = serde_json::to_string(&r).unwrap();
        let back: RunRecord = serde_json::from_str(&j).unwrap();
        assert_eq!(back.frame, r.frame);
        assert!(back.result().is_none());
    }
}
