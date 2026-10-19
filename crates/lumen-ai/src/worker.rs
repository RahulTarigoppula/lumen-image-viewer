//! Background tasks for the UI: each runs on its own thread and reports
//! through a `crossbeam-channel` of `AiEvent`s, calling the repaint
//! callback after sending. Stream text is coalesced to at most about 30
//! events per second. Dropping a `Task` (or its receiver) makes late events
//! vanish; `Task::cancel` stops the request and closes its socket.
//!
//! A run applies the plan's recovery rules in order:
//! - 429, 502, 503 and 504 are retried twice (1 s, then 4 s, or the
//!   server's Retry-After), a refused or lost connection once after 0.5 s,
//!   only before the first token;
//! - "At most N image(s)" rebuilds the request for N images (a composite
//!   when N is 1) and reports the learned limit;
//! - a rejected `response_format` resends with the schema in the prompt;
//! - rejected vLLM extensions (`chat_template_kwargs`...) are dropped.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use serde::{Deserialize, Serialize};

use crate::client::{ChatOutput, ChatSink, CancelToken, Client, ConnectionReport, StepResult, Usage};
use crate::error::AiError;
use crate::images::{ImageRole, SentLayout};
use crate::models::{ModelCard, VisionVerdict};
use crate::prompts::OutputKind;
use crate::report::{parse_report, AiReport, QuickScore};
use crate::request::{BodyMode, PreparedRequest, SentRecord};

/// Called after events are sent; the app passes `ctx.request_repaint()`.
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Minimum spacing of coalesced text events.
const FLUSH_EVERY: Duration = Duration::from_millis(33);

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // a few events per run; boxing buys nothing
pub enum AiEvent {
    /// A Test connection step finished.
    TestStep(StepResult),
    /// Test connection finished (ends a test task).
    TestDone(ConnectionReport),
    /// Ends a list-models task.
    Models(Result<Vec<ModelCard>, AiError>),
    /// Ends a vision-probe task.
    Vision { model: String, verdict: VisionVerdict },
    /// Swap-check: pass `index` of `total` starts; clear the live text.
    Pass { index: u8, total: u8 },
    /// The server accepted the request (HTTP 200); output follows.
    Connected,
    /// A retry or automatic change, for the status line.
    Status(String),
    /// Persist with `AiSettings::remember_image_limit`.
    ImageLimitLearned { server_key: String, model: String, limit: u32 },
    /// Model reasoning text (for the collapsed "Model reasoning" section).
    Reasoning(String),
    /// Answer text.
    Delta(String),
    /// Stream chunks (about one token each) received so far, for a live
    /// count while the server does not report usage per chunk.
    Chunks(u64),
    Usage(Usage),
    /// Ends a run task (and any task that failed internally).
    Done(Result<RunResult, AiError>),
}

/// Second pass of a swap-check, already converted back to real labels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SwapCheck {
    pub text: String,
    pub report: Option<Result<AiReport, String>>,
    /// Both passes preferred the same image (`None` if either report failed).
    pub agree: Option<bool>,
    pub usage: Option<Usage>,
}

/// Everything a finished run produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    pub model: String,
    pub server: String,
    pub preset_name: String,
    /// Raw answer (JSON or Markdown).
    pub text: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    /// `finish_reason` was `length`.
    pub truncated: bool,
    pub usage: Option<Usage>,
    /// For JSON presets; findings carry source-pixel boxes.
    pub report: Option<Result<AiReport, String>>,
    pub quick_score: Option<Result<QuickScore, String>>,
    pub swap: Option<SwapCheck>,
    /// What was actually sent (after any automatic change).
    pub sent: SentRecord,
    pub layout: Vec<SentLayout>,
    pub elapsed_ms: u64,
    pub warnings: Vec<String>,
}

/// A running background task.
pub struct Task {
    id: u64,
    rx: Receiver<AiEvent>,
    cancel: CancelToken,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Task {
    /// Increasing id (generation), to ignore events of superseded tasks.
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }
    pub fn receiver(&self) -> &Receiver<AiEvent> {
        &self.rx
    }
    /// Everything received so far (call once per frame).
    pub fn drain(&self) -> Vec<AiEvent> {
        self.rx.try_iter().collect()
    }
}

#[derive(Default)]
struct Pending {
    delta: String,
    reasoning: String,
    /// Latest usage not sent yet (vLLM reports it with every chunk).
    usage: Option<Usage>,
    chunks: u64,
    chunks_sent: u64,
    last: Option<Instant>,
}

struct Emitter {
    tx: Sender<AiEvent>,
    repaint: Repaint,
    pending: Mutex<Pending>,
}

impl Emitter {
    fn flush_locked(&self, p: &mut Pending) -> bool {
        let mut sent = false;
        if !p.reasoning.is_empty() {
            let _ = self.tx.send(AiEvent::Reasoning(std::mem::take(&mut p.reasoning)));
            sent = true;
        }
        if !p.delta.is_empty() {
            let _ = self.tx.send(AiEvent::Delta(std::mem::take(&mut p.delta)));
            sent = true;
        }
        if p.chunks != p.chunks_sent {
            let _ = self.tx.send(AiEvent::Chunks(p.chunks));
            p.chunks_sent = p.chunks;
            sent = true;
        }
        if let Some(u) = p.usage.take() {
            let _ = self.tx.send(AiEvent::Usage(u));
            sent = true;
        }
        p.last = Some(Instant::now());
        sent
    }

    /// Sends pending text, then `ev`, in order.
    fn send(&self, ev: AiEvent) {
        {
            let mut p = self.pending.lock().unwrap();
            self.flush_locked(&mut p);
            if matches!(ev, AiEvent::Pass { .. }) {
                // A new pass counts its own tokens.
                p.chunks = 0;
                p.chunks_sent = 0;
            }
            let _ = self.tx.send(ev);
        }
        (self.repaint)();
    }

    /// Sends pending text if the last flush was long enough ago.
    fn tick(&self) {
        let sent = {
            let mut p = self.pending.lock().unwrap();
            p.last.is_none_or(|t| t.elapsed() >= FLUSH_EVERY) && self.flush_locked(&mut p)
        };
        if sent {
            (self.repaint)();
        }
    }

    fn push(&self, reasoning: bool, text: &str) {
        let mut p = self.pending.lock().unwrap();
        p.chunks += 1;
        if reasoning {
            p.reasoning.push_str(text);
        } else {
            p.delta.push_str(text);
        }
    }

    fn set_usage(&self, usage: &Usage) {
        self.pending.lock().unwrap().usage = Some(usage.clone());
    }
}

struct Sink<'a> {
    em: &'a Emitter,
    got_token: AtomicBool,
}

impl ChatSink for Sink<'_> {
    fn connected(&self) {
        self.em.send(AiEvent::Connected);
    }
    fn content(&self, text: &str) {
        self.got_token.store(true, Ordering::SeqCst);
        self.em.push(false, text);
    }
    fn reasoning(&self, text: &str) {
        self.got_token.store(true, Ordering::SeqCst);
        self.em.push(true, text);
    }
    fn usage(&self, usage: &Usage) {
        self.em.set_usage(usage);
    }
    fn tick(&self) {
        self.em.tick();
    }
}

fn spawn(name: &str, repaint: Repaint, f: impl FnOnce(&Emitter, &CancelToken) + Send + 'static) -> Task {
    let (tx, rx) = unbounded();
    let cancel = CancelToken::new();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let em = Emitter { tx, repaint, pending: Mutex::new(Pending::default()) };
    let c = cancel.clone();
    let spawned = std::thread::Builder::new().name(format!("lumen-ai-{name}")).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&em, &c)));
        if let Err(p) = r {
            let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            log::error!("AI task panicked: {msg}");
            em.send(AiEvent::Done(Err(AiError::Protocol(format!("internal error: {msg}")))));
        }
    });
    if let Err(e) = spawned {
        log::error!("cannot start AI task: {e}");
    }
    Task { id, rx, cancel }
}

/// Test connection on a worker: `TestStep` per step, then `TestDone`.
/// `current_model` is kept if still served; `probe_vision` runs the
/// "Test vision" probe on the picked model.
pub fn spawn_test_connection(client: Client, current_model: Option<String>, probe_vision: bool, repaint: Repaint) -> Task {
    spawn("test", repaint, move |em, _| {
        let report = client.test_connection(current_model.as_deref(), probe_vision, &mut |s| em.send(AiEvent::TestStep(s.clone())));
        em.send(AiEvent::TestDone(report));
    })
}

/// Refreshes the model list: one `Models` event.
pub fn spawn_list_models(client: Client, repaint: Repaint) -> Task {
    spawn("models", repaint, move |em, _| em.send(AiEvent::Models(client.list_models())))
}

/// "Test vision" for one model: one `Vision` event.
pub fn spawn_vision_probe(client: Client, model: String, repaint: Repaint) -> Task {
    spawn("probe", repaint, move |em, cancel| {
        let verdict = client.probe_vision(&model, cancel);
        em.send(AiEvent::Vision { model, verdict });
    })
}

/// Sends a prepared request (after the send preview): `Connected`,
/// `Reasoning`/`Delta`, `Usage`, maybe `Status`/`Pass`/`ImageLimitLearned`,
/// then `Done`.
pub fn spawn_run(client: Client, prepared: PreparedRequest, repaint: Repaint) -> Task {
    spawn("run", repaint, move |em, cancel| {
        let r = run(&client, prepared, em, cancel);
        em.send(AiEvent::Done(r));
    })
}

/// Blocking version of a run, for callers with their own thread (batch
/// Quick score over video frames). Events are not reported.
pub fn run_blocking(client: &Client, prepared: PreparedRequest, cancel: &CancelToken) -> Result<RunResult, AiError> {
    let (tx, _rx) = unbounded();
    let em = Emitter { tx, repaint: Arc::new(|| {}), pending: Mutex::new(Pending::default()) };
    run(client, prepared, &em, cancel)
}

struct Fallback {
    mode: BodyMode,
    image_limit: Option<u32>,
    changes: Vec<String>,
}

fn sleep_cancellable(d: Duration, cancel: &CancelToken) -> Result<(), AiError> {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(20).min(end.saturating_duration_since(Instant::now())));
    }
    Ok(())
}

/// How long to wait before retry `attempt` (0-based) after `e`, or `None`
/// when it is not retried. A busy server gets two retries (1 s, then 4 s,
/// or its Retry-After); a refused or lost connection gets one quick retry
/// (a restarting server), so a server that is down fails within about a
/// second; a connect timeout already waited and is not retried.
fn retry_delay(e: &AiError, attempt: usize) -> Option<Duration> {
    const BUSY_BACKOFF: [u64; 2] = [1, 4];
    match e {
        AiError::Busy { retry_after, .. } => {
            let base = Duration::from_secs(*BUSY_BACKOFF.get(attempt)?);
            Some(retry_after.map(|d| d.clamp(Duration::from_secs(1), Duration::from_secs(30))).unwrap_or(base))
        }
        AiError::Unreachable { .. } if attempt == 0 => Some(Duration::from_millis(500)),
        _ => None,
    }
}

fn send_with_retries(client: &Client, body: &[u8], stream: bool, em: &Emitter, cancel: &CancelToken) -> Result<ChatOutput, AiError> {
    let mut attempt = 0;
    loop {
        let sink = Sink { em, got_token: AtomicBool::new(false) };
        match client.chat(body, stream, cancel, &sink) {
            Ok(o) => return Ok(o),
            Err(e) if e.is_retryable() && !sink.got_token.load(Ordering::SeqCst) => {
                let Some(wait) = retry_delay(&e, attempt) else { return Err(e) };
                let after = if wait < Duration::from_secs(1) { format!("{} ms", wait.as_millis()) } else { format!("{} s", wait.as_secs()) };
                em.send(AiEvent::Status(format!("{e}; retrying in {after}")));
                sleep_cancellable(wait, cancel)?;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn run_pass(client: &Client, mut req: PreparedRequest, st: &mut Fallback, em: &Emitter, cancel: &CancelToken) -> Result<(PreparedRequest, ChatOutput), AiError> {
    req.swapped = None;
    if let Some(l) = st.image_limit {
        if req.image_count() as u32 > l {
            req = req.with_max_images(l)?;
        }
    }
    let (mut tried_limit, mut tried_rf, mut tried_ext) = (false, false, false);
    loop {
        let body = req.body(st.mode);
        let err = match send_with_retries(client, &body, st.mode.stream, em, cancel) {
            Ok(out) => return Ok((req, out)),
            Err(e) => e,
        };
        let msg = match err {
            AiError::ImageLimit { limit, .. } if !tried_limit && req.image_count() > 1 => {
                tried_limit = true;
                let l = limit.unwrap_or(1).clamp(1, req.image_count() as u32 - 1);
                st.image_limit = Some(l);
                em.send(AiEvent::ImageLimitLearned { server_key: req.server_key.clone(), model: req.model.clone(), limit: l });
                req = req.with_max_images(l)?;
                req.swapped = None;
                if req.layout.iter().any(|e| e.role == ImageRole::Composite) {
                    format!("The server accepts {l} image(s) per request: resent with A and B in one composite")
                } else {
                    format!("The server accepts {l} image(s) per request: resent with {} image(s)", req.image_count())
                }
            }
            AiError::ResponseFormatRejected(_) if st.mode.response_format && !tried_rf => {
                tried_rf = true;
                st.mode.response_format = false;
                "The server rejected structured output: resent with the JSON schema in the prompt".to_string()
            }
            AiError::UnsupportedParameter(_) if st.mode.extensions && !tried_ext => {
                tried_ext = true;
                st.mode.extensions = false;
                "The server rejected vLLM-specific fields: resent without them".to_string()
            }
            e => return Err(e),
        };
        st.changes.push(msg.clone());
        em.send(AiEvent::Status(msg));
    }
}

type Parsed = (Option<Result<AiReport, String>>, Option<Result<QuickScore, String>>);

fn parse_output(req: &PreparedRequest, out: &ChatOutput, warnings: &mut Vec<String>) -> Parsed {
    let conv = req.coord_convention();
    if out.finish_reason.as_deref() == Some("length") && req.output != OutputKind::QuickScore {
        warnings.push("Output truncated, raise max tokens".into());
    }
    if req.structured() && out.content.trim().is_empty() && !out.reasoning.is_empty() {
        warnings.push("The model used its output budget on reasoning: raise max tokens or turn thinking off".into());
    }
    let report = req.structured().then(|| {
        parse_report(&out.content, req.output, conv).map(|mut r| {
            r.map_findings(&req.layout, conv);
            r
        })
    });
    let quick = (req.output == OutputKind::QuickScore).then(|| match &out.raw {
        Some(raw) => QuickScore::from_response(raw),
        None => Err("the server streamed the answer; logprobs need a non-streamed response".into()),
    });
    (report, quick)
}

fn run(client: &Client, prepared: PreparedRequest, em: &Emitter, cancel: &CancelToken) -> Result<RunResult, AiError> {
    let start = Instant::now();
    let swapped = prepared.swapped.clone();
    let total = if swapped.is_some() { 2 } else { 1 };
    let mut st = Fallback { mode: prepared.default_mode(), image_limit: None, changes: Vec::new() };
    let mut warnings = Vec::new();
    if total == 2 {
        em.send(AiEvent::Pass { index: 1, total });
    }
    let (req, out) = run_pass(client, prepared, &mut st, em, cancel)?;
    let (report, quick_score) = parse_output(&req, &out, &mut warnings);

    let swap = match swapped {
        Some(s) => {
            em.send(AiEvent::Pass { index: 2, total });
            let (sreq, sout) = run_pass(client, *s, &mut st, em, cancel)?;
            let mut w2 = Vec::new();
            let (mut srep, _) = parse_output(&sreq, &sout, &mut w2);
            warnings.extend(w2.into_iter().map(|w| format!("Swap pass: {w}")));
            if let Some(Ok(r)) = &mut srep {
                r.unswap();
            }
            let pref = |r: &Option<Result<AiReport, String>>| r.as_ref().and_then(|r| r.as_ref().ok()).and_then(|r| r.comparison.as_ref()).map(|c| c.preferred);
            let agree = match (pref(&report), pref(&srep)) {
                (Some(a), Some(b)) => Some(a == b),
                _ => None,
            };
            if agree == Some(false) {
                warnings.push("Swap-check disagrees: the preference flips when A and B are exchanged, so it is not reliable".into());
            }
            Some(SwapCheck { text: sout.content, report: srep, agree, usage: sout.usage })
        }
        None => None,
    };

    let mut sent = req.record();
    sent.changes = st.changes;
    Ok(RunResult {
        model: req.model.clone(),
        server: req.server.clone(),
        preset_name: req.preset_name.clone(),
        truncated: out.finish_reason.as_deref() == Some("length"),
        text: out.content,
        reasoning: out.reasoning,
        finish_reason: out.finish_reason,
        usage: out.usage,
        report,
        quick_score,
        swap,
        sent,
        layout: req.layout.clone(),
        elapsed_ms: start.elapsed().as_millis() as u64,
        warnings,
    })
}
