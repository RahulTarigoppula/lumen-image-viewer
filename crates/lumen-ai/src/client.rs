//! The HTTP client: the only code in Lumen that opens a socket.
//!
//! - `ureq` 3 on the calling (worker) thread, rustls with the Windows
//!   certificate store (or a user CA), no redirects, no proxy (environment
//!   proxies are ignored), and our own TCP connector that re-checks every
//!   resolved address against the LAN policy right before connecting, so a
//!   DNS answer cannot move a request off the local network.
//! - Each request gets a watchdog thread that, every 25 ms, flushes
//!   coalesced stream text to the UI and shuts the socket down on Cancel or
//!   when the first-token or total deadline passes. Shutting the socket
//!   down unblocks a read at once (no polling with socket timeouts) and
//!   makes vLLM abort the request.
//! - Test connection, model listing, chat completions (streaming and not)
//!   with a hand-written SSE parser, and mapping of every failure to
//!   `AiError`.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, RustlsConnector, Transport,
};
use ureq::Agent;

use crate::error::{AiError, TimeoutKind};
use crate::models::{ModelCard, VisionVerdict};
use crate::secret::ApiKey;
use crate::settings::{resolve_checked, AiSettings, Endpoint, NetPolicy};

/// Non-streaming probes (health, version, models).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the watchdog flushes text and checks deadlines.
const TICK: Duration = Duration::from_millis(25);

/// Set by the UI's Cancel button; checked by the worker and its watchdog.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> CancelToken {
        CancelToken::default()
    }
    /// Wraps a flag the caller already has.
    pub fn from_flag(flag: Arc<AtomicBool>) -> CancelToken {
        CancelToken(flag)
    }
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Token counts from the final stream chunk or a non-streamed response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptTokensDetails {
    pub cached_tokens: Option<u64>,
}

/// Receives stream progress. Called from the worker thread (`content`,
/// `reasoning`, `connected`, `usage`) and from the watchdog (`tick`).
pub trait ChatSink: Sync {
    fn connected(&self) {}
    fn content(&self, _text: &str) {}
    fn reasoning(&self, _text: &str) {}
    fn usage(&self, _usage: &Usage) {}
    fn tick(&self) {}
}

/// A sink that ignores everything.
pub struct NoSink;
impl ChatSink for NoSink {}

/// What a chat completion produced.
#[derive(Clone, Debug, Default)]
pub struct ChatOutput {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// The non-streamed response body (for logprobs).
    pub raw: Option<Value>,
}

/// Timeouts and policy for one client, taken from the settings.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub connect: Duration,
    pub first_token: Duration,
    pub total: Duration,
    pub policy: NetPolicy,
    pub custom_ca_pem: Option<String>,
}

impl ClientConfig {
    pub fn from_settings(s: &AiSettings) -> ClientConfig {
        let secs = |v: u32, lo: u64| Duration::from_secs((v as u64).max(lo));
        ClientConfig {
            connect: secs(s.timeout_connect_s, 1),
            first_token: secs(s.timeout_first_token_s, 1),
            total: secs(s.timeout_total_s, 1),
            policy: s.net_policy(),
            custom_ca_pem: s.custom_ca_pem.clone().filter(|p| !p.trim().is_empty()),
        }
    }
}

/// A server plus key and timeouts. Cheap to clone; one per request is fine.
#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    key: Option<ApiKey>,
    cfg: ClientConfig,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.endpoint.base_url())
            .field("key", &self.key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Client {
    pub fn new(endpoint: Endpoint, key: Option<ApiKey>, cfg: ClientConfig) -> Client {
        Client { endpoint, key, cfg }
    }

    /// Client for the configured server.
    pub fn from_settings(s: &AiSettings, key: Option<ApiKey>) -> Result<Client, AiError> {
        Ok(Client::new(s.endpoint()?, key, ClientConfig::from_settings(s)))
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }
    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    fn agent(&self, conn: Arc<Conn>, recv_response: Duration, total: Duration) -> Result<Agent, AiError> {
        use ureq::tls::{PemItem, RootCerts, TlsConfig};
        let roots = match &self.cfg.custom_ca_pem {
            Some(pem) => {
                let certs: Vec<_> = ureq::tls::parse_pem(pem.as_bytes())
                    .filter_map(|i| match i {
                        Ok(PemItem::Certificate(c)) => Some(c),
                        _ => None,
                    })
                    .collect();
                if certs.is_empty() {
                    return Err(AiError::Tls("the custom CA contains no PEM certificate".into()));
                }
                RootCerts::new_with_certs(&certs)
            }
            None => RootCerts::PlatformVerifier,
        };
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .user_agent(format!("Lumen/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(self.cfg.connect))
            .timeout_recv_response(Some(recv_response))
            .timeout_recv_body(None)
            .timeout_global(Some(total))
            .tls_config(TlsConfig::builder().root_certs(roots).build())
            .build();
        let connector = ().chain(GuardedConnector(conn)).chain(RustlsConnector::default());
        Ok(Agent::with_parts(config, connector, DefaultResolver::default()))
    }

    fn conn(&self) -> Arc<Conn> {
        Arc::new(Conn {
            policy: self.cfg.policy,
            https: self.endpoint.https,
            connect: self.cfg.connect,
            sockets: Mutex::new(Vec::new()),
            aborted: AtomicBool::new(false),
        })
    }

    /// GET a route; returns the status and body.
    fn get(&self, url: &str, with_key: bool) -> Result<(u16, String), AiError> {
        let conn = self.conn();
        let agent = self.agent(conn, PROBE_TIMEOUT, PROBE_TIMEOUT)?;
        let mut req = agent.get(url).header("Accept", "application/json");
        if with_key {
            if let Some(k) = &self.key {
                req = req.header("Authorization", format!("Bearer {}", k.expose()));
            }
        }
        let mut resp = req.call().map_err(|e| self.map_ureq(e))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(16 << 20)
            .read_to_string()
            .map_err(|e| self.map_ureq(e))?;
        Ok((status, body))
    }

    /// `GET /v1/models`.
    pub fn list_models(&self) -> Result<Vec<ModelCard>, AiError> {
        let (status, body) = self.get(&self.endpoint.api("/models"), true)?;
        if status != 200 {
            return Err(match classify(status, &body, self.key.is_some(), None) {
                AiError::ModelNotFound(m) => AiError::Http {
                    status,
                    message: format!("no /v1/models route ({m}); is this an OpenAI-compatible server?"),
                },
                e => e,
            });
        }
        #[derive(Deserialize)]
        struct List {
            data: Vec<ModelCard>,
        }
        let list: List = serde_json::from_str(&body)
            .map_err(|e| AiError::Protocol(format!("/v1/models is not a model list ({e})")))?;
        if list.data.is_empty() {
            return Err(AiError::NoModels);
        }
        Ok(list.data)
    }

    /// TCP connect to the first allowed address.
    fn tcp_check(&self) -> Result<SocketAddr, AiError> {
        let mut addrs = resolve_checked(&self.endpoint, &self.cfg.policy)?;
        addrs.sort_by_key(|a| a.is_ipv6());
        let mut last = None;
        for a in &addrs {
            match TcpStream::connect_timeout(a, self.cfg.connect) {
                Ok(_) => return Ok(*a),
                Err(e) => last = Some(connect_error(&self.endpoint, *a, &e, self.cfg.connect)),
            }
        }
        Err(last.unwrap_or_else(|| AiError::Dns { host: self.endpoint.host.clone(), detail: "no addresses".into() }))
    }

    /// Runs the Test connection steps in order, reporting each as it ends.
    /// `current_model` is kept when the server still serves it; otherwise a
    /// vision model is picked. With `probe_vision`, the picked model gets the
    /// "Test vision" probe.
    pub fn test_connection(
        &self,
        current_model: Option<&str>,
        probe_vision: bool,
        on_step: &mut dyn FnMut(&StepResult),
    ) -> ConnectionReport {
        let mut report = ConnectionReport { server: self.endpoint.display(), ..Default::default() };
        let mut push = |r: &mut ConnectionReport, step: StepResult| {
            on_step(&step);
            r.steps.push(step);
        };
        let skip_rest = |r: &mut ConnectionReport, from: usize, push: &mut dyn FnMut(&mut ConnectionReport, StepResult)| {
            for step in &TestStep::ALL[from..] {
                push(r, StepResult::new(*step, StepStatus::Skipped, "Skipped".into(), Duration::ZERO));
            }
        };

        // 1. TCP
        let t = Instant::now();
        match self.tcp_check() {
            Ok(addr) => push(&mut report, StepResult::new(TestStep::Tcp, StepStatus::Ok, format!("Connected to {addr}"), t.elapsed())),
            Err(e) => {
                push(&mut report, StepResult::new(TestStep::Tcp, StepStatus::Failed, describe(&e), t.elapsed()));
                skip_rest(&mut report, 1, &mut push);
                return report;
            }
        }

        // 2. /health
        let t = Instant::now();
        match self.get(&self.endpoint.route("/health"), false) {
            Ok((200, _)) => push(&mut report, StepResult::new(TestStep::Health, StepStatus::Ok, "Server is healthy".into(), t.elapsed())),
            Ok((404, _)) => push(
                &mut report,
                StepResult::new(TestStep::Health, StepStatus::Warning, "No /health route (not vLLM?); continuing".into(), t.elapsed()),
            ),
            Ok((s, body)) => push(
                &mut report,
                StepResult::new(
                    TestStep::Health,
                    StepStatus::Warning,
                    format!("Server reports not healthy ({s}){}", tail(&error_message(&body))),
                    t.elapsed(),
                ),
            ),
            Err(e) => {
                // Transport failures here (TLS, protocol) will fail every later step too.
                push(&mut report, StepResult::new(TestStep::Health, StepStatus::Failed, describe(&e), t.elapsed()));
                skip_rest(&mut report, 2, &mut push);
                return report;
            }
        }

        // 3. /version
        let t = Instant::now();
        match self.get(&self.endpoint.route("/version"), false) {
            Ok((200, body)) => {
                let v = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|j| j.get("version").and_then(|v| v.as_str()).map(str::to_string));
                match v {
                    Some(v) => {
                        push(&mut report, StepResult::new(TestStep::Version, StepStatus::Ok, format!("vLLM {v}"), t.elapsed()));
                        report.version = Some(v);
                    }
                    None => push(
                        &mut report,
                        StepResult::new(TestStep::Version, StepStatus::Warning, "Version unknown".into(), t.elapsed()),
                    ),
                }
            }
            Ok((s, _)) => push(
                &mut report,
                StepResult::new(TestStep::Version, StepStatus::Warning, format!("Version unknown ({s}; not vLLM?)"), t.elapsed()),
            ),
            Err(e) => push(&mut report, StepResult::new(TestStep::Version, StepStatus::Warning, describe(&e), t.elapsed())),
        }

        // 4. /v1/models
        let t = Instant::now();
        match self.list_models() {
            Ok(models) => {
                let names: Vec<&str> = models.iter().take(4).map(|m| m.id.as_str()).collect();
                let more = if models.len() > 4 { format!(" and {} more", models.len() - 4) } else { String::new() };
                let msg = format!("{} model(s): {}{more}", models.len(), names.join(", "));
                push(&mut report, StepResult::new(TestStep::Models, StepStatus::Ok, msg, t.elapsed()));
                report.picked_model = crate::models::pick_model(&models, current_model);
                report.models = models;
            }
            Err(e) => {
                push(&mut report, StepResult::new(TestStep::Models, StepStatus::Failed, describe(&e), t.elapsed()));
                skip_rest(&mut report, 4, &mut push);
                return report;
            }
        }

        // 5. Vision probe
        let t = Instant::now();
        let picked = report.picked_model.clone().unwrap_or_default();
        let card = report.models.iter().find(|m| m.id == picked).cloned();
        if probe_vision && !picked.is_empty() {
            let verdict = self.probe_vision(&picked, &CancelToken::new());
            let (status, msg) = match &verdict {
                VisionVerdict::Yes => (StepStatus::Ok, format!("{picked} accepts images")),
                VisionVerdict::No(m) => (StepStatus::Warning, format!("{picked} looks text-only ({m}); pick another model or try anyway")),
                VisionVerdict::Unknown(m) => (StepStatus::Warning, format!("Could not confirm image support ({m}); will try anyway")),
            };
            push(&mut report, StepResult::new(TestStep::Vision, status, msg, t.elapsed()));
            report.vision = Some(verdict);
        } else {
            let hint = card.map(|c| c.vision_hint().label()).unwrap_or("vision unknown, will try");
            push(&mut report, StepResult::new(TestStep::Vision, StepStatus::Skipped, format!("Not probed: {hint}"), Duration::ZERO));
        }
        report
    }

    /// "Test vision": a 64x64 gray PNG with one output token. 200 means the
    /// model takes images; a 400 about images means text-only; anything
    /// else is inconclusive (never blocks a run).
    pub fn probe_vision(&self, model: &str, cancel: &CancelToken) -> VisionVerdict {
        let png = crate::images::probe_png();
        let mut ext = true;
        loop {
            let mut body = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": png.data_url()}},
                    {"type": "text", "text": "What color is this image? Answer in one word."}
                ]}],
                "max_completion_tokens": 1,
                "temperature": 0,
                "stream": false
            });
            if ext {
                body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
            }
            let bytes = serde_json::to_vec(&body).expect("json");
            let probe_total = self.cfg.first_token + PROBE_TIMEOUT;
            match self.chat_with(&bytes, false, cancel, &NoSink, self.cfg.first_token.min(probe_total), probe_total) {
                Ok(_) => return VisionVerdict::Yes,
                Err(AiError::NotMultimodal(m)) => return VisionVerdict::No(m),
                Err(AiError::ImageLimit { limit: Some(0), message }) => return VisionVerdict::No(message),
                Err(AiError::UnsupportedParameter(_)) if ext => ext = false,
                Err(e) => return VisionVerdict::Unknown(e.to_string()),
            }
        }
    }

    /// `POST /v1/chat/completions` with the client's first-token and total
    /// timeouts. `body` is the JSON request; `stream` must match its
    /// `stream` field.
    pub fn chat(&self, body: &[u8], stream: bool, cancel: &CancelToken, sink: &dyn ChatSink) -> Result<ChatOutput, AiError> {
        self.chat_with(body, stream, cancel, sink, self.cfg.first_token, self.cfg.total)
    }

    fn chat_with(
        &self,
        body: &[u8],
        stream: bool,
        cancel: &CancelToken,
        sink: &dyn ChatSink,
        first_token: Duration,
        total: Duration,
    ) -> Result<ChatOutput, AiError> {
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }
        let conn = self.conn();
        let agent = self.agent(conn.clone(), first_token, total + Duration::from_secs(1))?;
        let watch = Watch::default();
        let start = Instant::now();
        let result = std::thread::scope(|s| {
            s.spawn(|| {
                while !watch.done.load(Ordering::SeqCst) {
                    std::thread::sleep(TICK);
                    sink.tick();
                    if watch.fired.load(Ordering::SeqCst) != FIRED_NONE {
                        continue;
                    }
                    let fire = if cancel.is_cancelled() {
                        FIRED_CANCEL
                    } else if !watch.first.load(Ordering::SeqCst) && start.elapsed() > first_token {
                        FIRED_FIRST
                    } else if start.elapsed() > total {
                        FIRED_TOTAL
                    } else {
                        FIRED_NONE
                    };
                    if fire != FIRED_NONE {
                        watch.fired.store(fire, Ordering::SeqCst);
                        conn.shutdown();
                    }
                }
            });
            let r = self.chat_inner(&agent, body, stream, cancel, sink, &watch);
            watch.done.store(true, Ordering::SeqCst);
            r
        });
        let fired = watch.fired.load(Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }
        match (fired, result) {
            (FIRED_FIRST, Err(_)) => Err(AiError::Timeout(TimeoutKind::FirstToken, first_token)),
            (FIRED_TOTAL, Err(_)) => Err(AiError::Timeout(TimeoutKind::Total, total)),
            (_, r) => r,
        }
    }

    fn chat_inner(
        &self,
        agent: &Agent,
        body: &[u8],
        stream: bool,
        cancel: &CancelToken,
        sink: &dyn ChatSink,
        watch: &Watch,
    ) -> Result<ChatOutput, AiError> {
        let mut req = agent
            .post(self.endpoint.api("/chat/completions"))
            .header("Content-Type", "application/json")
            .header("Accept", if stream { "text/event-stream" } else { "application/json" });
        if let Some(k) = &self.key {
            req = req.header("Authorization", format!("Bearer {}", k.expose()));
        }
        let resp = req.send(body).map_err(|e| self.map_ureq(e))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let is_sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().starts_with("text/event-stream"))
            .unwrap_or(false);
        if (300..400).contains(&status) {
            let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("?").to_string();
            return Err(AiError::Http { status, message: format!("redirect to {loc} not followed") });
        }
        let mut reader = resp.into_body().into_reader();
        if !(200..300).contains(&status) {
            let mut buf = Vec::new();
            let _ = reader.by_ref().take(64 << 10).read_to_end(&mut buf);
            return Err(classify(status, &String::from_utf8_lossy(&buf), self.key.is_some(), retry_after));
        }
        sink.connected();
        if stream && is_sse {
            read_stream(BufReader::new(reader), cancel, sink, watch).map_err(|e| self.map_read(e, watch))
        } else {
            // Not streamed (requested, or the server ignored `stream`).
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).map_err(|e| self.map_read(StreamError::Io(e), watch))?;
            watch.first.store(true, Ordering::SeqCst);
            let out = parse_completion(&buf)?;
            if !out.reasoning.is_empty() {
                sink.reasoning(&out.reasoning);
            }
            if !out.content.is_empty() {
                sink.content(&out.content);
            }
            if let Some(u) = &out.usage {
                sink.usage(u);
            }
            Ok(out)
        }
    }

    fn map_read(&self, e: StreamError, watch: &Watch) -> AiError {
        match e {
            StreamError::Ai(e) => e,
            StreamError::Io(e) => {
                if let Some(ureq::Error::Timeout(_)) = e.get_ref().and_then(|i| i.downcast_ref::<ureq::Error>()) {
                    return AiError::Timeout(TimeoutKind::Total, self.cfg.total);
                }
                if watch.fired.load(Ordering::SeqCst) == FIRED_NONE && e.kind() == io::ErrorKind::TimedOut {
                    return AiError::Timeout(TimeoutKind::Total, self.cfg.total);
                }
                AiError::Unreachable { target: self.endpoint.display(), detail: format!("connection lost: {e}") }
            }
        }
    }

    fn map_ureq(&self, e: ureq::Error) -> AiError {
        use ureq::Error as E;
        let target = self.endpoint.display();
        match e {
            E::Other(b) => match b.downcast::<AiError>() {
                Ok(a) => *a,
                Err(b) => AiError::Protocol(b.to_string()),
            },
            E::HostNotFound => AiError::Dns { host: self.endpoint.host.clone(), detail: "host not found".into() },
            E::Timeout(t) => {
                use ureq::Timeout as T;
                match t {
                    T::Resolve | T::Connect => AiError::Timeout(TimeoutKind::Connect, self.cfg.connect),
                    T::RecvResponse => AiError::Timeout(TimeoutKind::FirstToken, self.cfg.first_token),
                    _ => AiError::Timeout(TimeoutKind::Total, self.cfg.total),
                }
            }
            E::Io(io) => match io.kind() {
                io::ErrorKind::TimedOut => AiError::Timeout(TimeoutKind::Connect, self.cfg.connect),
                io::ErrorKind::ConnectionRefused => AiError::Unreachable { target, detail: REFUSED_HINT.into() },
                _ => AiError::Unreachable { target, detail: io.to_string() },
            },
            E::ConnectionFailed => AiError::Unreachable { target, detail: "connection failed".into() },
            E::Tls(m) => AiError::Tls(tls_hint(m, self.endpoint.https)),
            E::Rustls(r) => AiError::Tls(tls_hint(&r.to_string(), self.endpoint.https)),
            E::Pem(p) => AiError::Tls(format!("bad custom CA: {p}")),
            E::Protocol(p) if !self.endpoint.https => {
                AiError::Protocol(format!("{p}; the server did not answer plain HTTP (does it need HTTPS?)"))
            }
            E::Protocol(p) => AiError::Protocol(p.to_string()),
            E::BadUri(u) => AiError::Address(u),
            other => AiError::Protocol(other.to_string()),
        }
    }
}

const REFUSED_HINT: &str = "connection refused: is vllm serve running there, listening on 0.0.0.0 (--host 0.0.0.0) and on this port?";

fn tls_hint(m: &str, https: bool) -> String {
    let low = m.to_ascii_lowercase();
    if low.contains("unknownissuer") || low.contains("unknown issuer") || low.contains("self signed") || low.contains("selfsigned") {
        format!("{m}; the certificate is not trusted: add the server's CA in Settings > AI")
    } else if https && (low.contains("corrupt") || low.contains("invalidcontenttype") || low.contains("invalid content type")) {
        format!("{m}; the server may not use HTTPS: turn HTTPS off")
    } else {
        m.to_string()
    }
}

fn connect_error(ep: &Endpoint, a: SocketAddr, e: &io::Error, timeout: Duration) -> AiError {
    // Name the address only when a host name resolved to it.
    let at = if ep.host.parse::<std::net::IpAddr>().is_ok() { String::new() } else { format!("{a}: ") };
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => AiError::Timeout(TimeoutKind::Connect, timeout),
        io::ErrorKind::ConnectionRefused => AiError::Unreachable { target: ep.display(), detail: format!("{at}{REFUSED_HINT}") },
        _ => AiError::Unreachable { target: ep.display(), detail: format!("{at}{e}") },
    }
}

/// `Display` plus the server's detail, for step messages.
pub fn describe(e: &AiError) -> String {
    match e.detail() {
        Some(d) => format!("{e}: {d}"),
        None => e.to_string(),
    }
}

fn tail(m: &str) -> String {
    if m.is_empty() {
        String::new()
    } else {
        format!(": {m}")
    }
}

// ---------------------------------------------------------------------------
// Test connection report

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestStep {
    Tcp,
    Health,
    Version,
    Models,
    Vision,
}

impl TestStep {
    pub const ALL: [TestStep; 5] = [TestStep::Tcp, TestStep::Health, TestStep::Version, TestStep::Models, TestStep::Vision];
    pub fn label(self) -> &'static str {
        match self {
            TestStep::Tcp => "Connect",
            TestStep::Health => "Health",
            TestStep::Version => "Version",
            TestStep::Models => "Models",
            TestStep::Vision => "Vision",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Ok,
    Warning,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    pub step: TestStep,
    pub status: StepStatus,
    pub message: String,
    pub elapsed_ms: u64,
}

impl StepResult {
    fn new(step: TestStep, status: StepStatus, message: String, elapsed: Duration) -> StepResult {
        StepResult { step, status, message, elapsed_ms: elapsed.as_millis() as u64 }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionReport {
    /// `192.168.1.20:8000`.
    pub server: String,
    pub steps: Vec<StepResult>,
    pub version: Option<String>,
    pub models: Vec<ModelCard>,
    /// The model to select: the current one if still served, else the best
    /// vision candidate, else the first.
    pub picked_model: Option<String>,
    pub vision: Option<VisionVerdict>,
}

impl ConnectionReport {
    /// The server can be used: connected and listed models.
    pub fn ok(&self) -> bool {
        self.steps.iter().any(|s| s.step == TestStep::Models && s.status == StepStatus::Ok)
    }
    /// The first failed step's message, for a one-line status.
    pub fn failure(&self) -> Option<&str> {
        self.steps.iter().find(|s| s.status == StepStatus::Failed).map(|s| s.message.as_str())
    }
}

// ---------------------------------------------------------------------------
// Error bodies

/// Pulls the message out of `{"error": {"message": ...}}`, the older
/// `{"object": "error", "message": ...}`, FastAPI's `{"detail": ...}`, or
/// returns the trimmed text.
pub fn error_message(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let pick = |v: &Value| -> Option<String> {
            match v {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("message").and_then(|m| m.as_str()).map(str::to_string),
                Value::Array(a) => {
                    let msgs: Vec<String> = a
                        .iter()
                        .filter_map(|e| e.get("msg").and_then(|m| m.as_str()).map(str::to_string))
                        .collect();
                    (!msgs.is_empty()).then(|| msgs.join("; "))
                }
                _ => None,
            }
        };
        if let Some(m) = v.get("error").and_then(pick).or_else(|| v.get("message").and_then(pick)).or_else(|| v.get("detail").and_then(pick)) {
            return m;
        }
    }
    let t = body.trim();
    if t.chars().count() > 300 {
        t.chars().take(300).collect::<String>() + "..."
    } else {
        t.to_string()
    }
}

/// Parses N from "At most N image(s)" style messages.
fn parse_image_limit(low: &str) -> Option<u32> {
    for pat in ["at most ", "only supports ", "maximum of ", "up to "] {
        if let Some(i) = low.find(pat) {
            let digits: String = low[i + pat.len()..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse() {
                return Some(n);
            }
        }
    }
    None
}

/// Maps an error status and body to `AiError` (the plan's error table).
pub fn classify(status: u16, body: &str, key_sent: bool, retry_after: Option<Duration>) -> AiError {
    let message = error_message(body);
    let low = message.to_ascii_lowercase();
    let has = |pats: &[&str]| pats.iter().any(|p| low.contains(p));
    match status {
        401 | 403 => AiError::Unauthorized { status, message, key_sent },
        404 => AiError::ModelNotFound(message),
        413 => AiError::PayloadTooLarge(message),
        429 | 502 | 503 | 504 => AiError::Busy { status, message, retry_after },
        400 | 422 => {
            let image_word = low.contains("image");
            if has(&["maximum context length", "context length", "max_model_len", "maximum model length", "prompt is too long", "too many tokens", "exceeds the model"]) {
                AiError::ContextLength(message)
            } else if image_word
                && (has(&["at most", "limit-mm-per-prompt", "limit_mm_per_prompt", "too many images", "maximum number of images", "only supports", "exceeds the limit", "exceeded the limit"]))
            {
                let limit = parse_image_limit(&low);
                if limit == Some(0) {
                    AiError::NotMultimodal(message)
                } else {
                    AiError::ImageLimit { limit, message }
                }
            } else if has(&["not a multimodal model", "is not multimodal", "does not support multimodal", "multimodal inputs are not supported", "does not support image", "image input is not supported", "images are not supported", "not support vision", "no multimodal"]) {
                AiError::NotMultimodal(message)
            } else if has(&["response_format", "json_schema", "structured output", "guided_decoding", "guided_json", "xgrammar", "outlines", "grammar"]) {
                AiError::ResponseFormatRejected(message)
            } else if has(&["chat_template_kwargs", "extra inputs are not permitted", "extra_forbidden", "unrecognized request argument", "unknown field", "top_k", "continue_final_message", "add_generation_prompt"]) {
                AiError::UnsupportedParameter(message)
            } else {
                AiError::Http { status, message }
            }
        }
        _ => AiError::Http { status, message },
    }
}

// ---------------------------------------------------------------------------
// SSE

/// One server-sent event.
#[derive(Clone, Debug, PartialEq)]
pub enum SseEvent {
    Data(String),
    Done,
}

/// Minimal SSE reader: `data:` lines are joined until a blank line,
/// comment lines (`:`) and other fields are ignored, `[DONE]` ends.
pub struct SseReader<R: BufRead> {
    r: R,
    line: Vec<u8>,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(r: R) -> Self {
        SseReader { r, line: Vec::new() }
    }

    /// Next event, `None` at end of stream. `between_lines` runs after
    /// every line (cancellation checks).
    pub fn next_event(&mut self, between_lines: &mut dyn FnMut() -> io::Result<()>) -> io::Result<Option<SseEvent>> {
        let mut data = String::new();
        let mut have = false;
        loop {
            self.line.clear();
            let n = self.r.read_until(b'\n', &mut self.line)?;
            between_lines()?;
            if n == 0 {
                return Ok(have.then(|| finish(data)));
            }
            let mut l = &self.line[..];
            while let [rest @ .., b'\n' | b'\r'] = l {
                l = rest;
            }
            if l.is_empty() {
                if have {
                    return Ok(Some(finish(data)));
                }
                continue;
            }
            if l[0] == b':' {
                continue;
            }
            let text = String::from_utf8_lossy(l);
            let (field, value) = match text.find(':') {
                Some(i) => {
                    let v = &text[i + 1..];
                    (&text[..i], v.strip_prefix(' ').unwrap_or(v))
                }
                None => (&text[..], ""),
            };
            if field == "data" {
                if have {
                    data.push('\n');
                }
                data.push_str(value);
                have = true;
            }
        }
        fn finish(d: String) -> SseEvent {
            if d.trim() == "[DONE]" {
                SseEvent::Done
            } else {
                SseEvent::Data(d)
            }
        }
    }
}

enum StreamError {
    Io(io::Error),
    Ai(AiError),
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Delta {
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChunkChoice {
    delta: Option<Delta>,
    message: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Chunk {
    choices: Vec<ChunkChoice>,
    usage: Option<Usage>,
    error: Option<Value>,
}

fn chunk_error(err: &Value) -> AiError {
    let status = err.get("code").and_then(|c| c.as_u64()).filter(|c| (400..600).contains(c)).unwrap_or(500) as u16;
    classify(status, &serde_json::json!({ "error": err }).to_string(), false, None)
}

fn read_stream<R: BufRead>(r: R, cancel: &CancelToken, sink: &dyn ChatSink, watch: &Watch) -> Result<ChatOutput, StreamError> {
    let mut sse = SseReader::new(r);
    let mut out = ChatOutput::default();
    let mut check = || {
        if cancel.is_cancelled() {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
        } else {
            Ok(())
        }
    };
    loop {
        let ev = match sse.next_event(&mut check) {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                if out.finish_reason.is_none() && out.content.is_empty() && out.reasoning.is_empty() {
                    return Err(StreamError::Ai(AiError::Protocol("the stream ended before any output".into())));
                }
                break;
            }
            Err(e) => return Err(StreamError::Io(e)),
        };
        let data = match ev {
            SseEvent::Done => break,
            SseEvent::Data(d) => d,
        };
        let chunk: Chunk = serde_json::from_str(&data)
            .map_err(|e| StreamError::Ai(AiError::Protocol(format!("bad stream chunk ({e})"))))?;
        if let Some(err) = &chunk.error {
            return Err(StreamError::Ai(chunk_error(err)));
        }
        if let Some(c) = chunk.choices.first() {
            if let Some(d) = c.delta.as_ref().or(c.message.as_ref()) {
                if let Some(r) = d.reasoning.as_deref().or(d.reasoning_content.as_deref()).filter(|s| !s.is_empty()) {
                    watch.first.store(true, Ordering::SeqCst);
                    out.reasoning.push_str(r);
                    sink.reasoning(r);
                }
                if let Some(t) = d.content.as_deref().filter(|s| !s.is_empty()) {
                    watch.first.store(true, Ordering::SeqCst);
                    out.content.push_str(t);
                    sink.content(t);
                }
            }
            if let Some(f) = &c.finish_reason {
                out.finish_reason = Some(f.clone());
            }
        }
        if let Some(u) = chunk.usage {
            sink.usage(&u);
            out.usage = Some(u);
        }
    }
    Ok(out)
}

/// Parses a non-streamed completion.
fn parse_completion(buf: &[u8]) -> Result<ChatOutput, AiError> {
    let v: Value = serde_json::from_slice(buf).map_err(|e| AiError::Protocol(format!("response is not JSON ({e})")))?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(chunk_error(err));
    }
    let chunk: Chunk = serde_json::from_value(v.clone()).map_err(|e| AiError::Protocol(format!("unexpected response ({e})")))?;
    let c = chunk.choices.first().ok_or_else(|| AiError::Protocol("response has no choices".into()))?;
    let m = c.message.as_ref().or(c.delta.as_ref());
    Ok(ChatOutput {
        content: m.and_then(|m| m.content.clone()).unwrap_or_default(),
        reasoning: m.and_then(|m| m.reasoning.clone().or_else(|| m.reasoning_content.clone())).unwrap_or_default(),
        finish_reason: c.finish_reason.clone(),
        usage: chunk.usage,
        raw: Some(v),
    })
}

// ---------------------------------------------------------------------------
// Watchdog state and the guarded connector

const FIRED_NONE: u8 = 0;
const FIRED_CANCEL: u8 = 1;
const FIRED_FIRST: u8 = 2;
const FIRED_TOTAL: u8 = 3;

#[derive(Default)]
struct Watch {
    /// A content or reasoning token arrived.
    first: AtomicBool,
    done: AtomicBool,
    fired: AtomicU8,
}

/// Sockets opened for one request, so the watchdog can stop them.
struct Conn {
    policy: NetPolicy,
    https: bool,
    connect: Duration,
    sockets: Mutex<Vec<TcpStream>>,
    /// Set by `shutdown`; reads waiting for data give up within a poll slice.
    aborted: AtomicBool,
}

impl Conn {
    /// Stops the request: on Linux `shutdown` wakes a blocked read at once;
    /// on Windows it does not, so reads wait in short `WSAPoll` slices and
    /// check `aborted` (see `wait_readable`).
    fn shutdown(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        for s in self.sockets.lock().unwrap().iter() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

struct GuardedConnector(Arc<Conn>);

impl fmt::Debug for GuardedConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GuardedConnector")
    }
}

impl Connector<()> for GuardedConnector {
    type Out = GuardedTransport;

    fn connect(&self, details: &ConnectionDetails, _chained: Option<()>) -> Result<Option<GuardedTransport>, ureq::Error> {
        let conn = &self.0;
        let mut addrs: Vec<SocketAddr> = details.addrs.to_vec();
        // Re-check what DNS returned this time, right before connecting.
        conn.policy.check_all(&addrs, conn.https).map_err(|e| ureq::Error::Other(Box::new(e)))?;
        // IPv4 first: `vllm serve --host 0.0.0.0` listens on IPv4 only, and a
        // refused IPv6 attempt costs about 2 s on Windows.
        addrs.sort_by_key(|a| a.is_ipv6());
        let mut last: Option<io::Error> = None;
        for a in &addrs {
            match TcpStream::connect_timeout(a, conn.connect) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    if let Ok(clone) = stream.try_clone() {
                        conn.sockets.lock().unwrap().push(clone);
                    }
                    let cfg = details.config;
                    let buffers = LazyBuffers::new(cfg.input_buffer_size(), cfg.output_buffer_size());
                    return Ok(Some(GuardedTransport { stream, buffers, conn: conn.clone(), timeout_read: None, timeout_write: None }));
                }
                Err(e) => last = Some(e),
            }
        }
        Err(match last {
            Some(e) if e.kind() == io::ErrorKind::TimedOut => ureq::Error::Timeout(ureq::Timeout::Connect),
            Some(e) => ureq::Error::Io(e),
            None => ureq::Error::HostNotFound,
        })
    }
}

/// A plain TCP transport (as ureq's own) over a socket the watchdog can
/// shut down.
struct GuardedTransport {
    stream: TcpStream,
    buffers: LazyBuffers,
    conn: Arc<Conn>,
    timeout_read: Option<Duration>,
    timeout_write: Option<Duration>,
}

impl fmt::Debug for GuardedTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardedTransport").field("peer", &self.stream.peer_addr().ok()).finish()
    }
}

fn aborted_error() -> ureq::Error {
    ureq::Error::Io(io::Error::new(io::ErrorKind::ConnectionAborted, "request stopped"))
}

/// Waits until the socket has data (or EOF, or an error) in 50 ms slices,
/// giving up when the request is aborted or ureq's timeout passes.
#[cfg(windows)]
fn wait_readable(stream: &TcpStream, aborted: &AtomicBool, timeout: NextTimeout) -> Result<(), ureq::Error> {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, SOCKET, WSAPOLLFD};
    let start = Instant::now();
    let limit = timeout.not_zero().map(|d| *d);
    loop {
        if aborted.load(Ordering::SeqCst) {
            return Err(aborted_error());
        }
        let slice = match limit {
            Some(l) if start.elapsed() >= l => return Err(ureq::Error::Timeout(timeout.reason)),
            Some(l) => (l - start.elapsed()).min(Duration::from_millis(50)),
            None => Duration::from_millis(50),
        };
        let mut fd = WSAPOLLFD { fd: SOCKET(stream.as_raw_socket() as usize), events: POLLRDNORM, revents: Default::default() };
        // SAFETY: one valid WSAPOLLFD for a socket owned by `stream`.
        let r = unsafe { WSAPoll(&mut fd, 1, slice.as_millis().max(1) as i32) };
        if r != 0 {
            // Readable, closed or failed: the read reports which.
            return Ok(());
        }
    }
}

#[cfg(not(windows))]
fn wait_readable(_: &TcpStream, aborted: &AtomicBool, _: NextTimeout) -> Result<(), ureq::Error> {
    if aborted.load(Ordering::SeqCst) {
        return Err(aborted_error());
    }
    Ok(())
}

fn io_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

impl Transport for GuardedTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let t = timeout.not_zero().map(|d| *d);
        if t != self.timeout_write {
            self.stream.set_write_timeout(t)?;
            self.timeout_write = t;
        }
        let output = &self.buffers.output()[..amount];
        match self.stream.write_all(output) {
            Err(e) if io_timeout(&e) => Err(ureq::Error::Timeout(timeout.reason)),
            r => Ok(r?),
        }
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let t = timeout.not_zero().map(|d| *d);
        if t != self.timeout_read {
            self.stream.set_read_timeout(t)?;
            self.timeout_read = t;
        }
        wait_readable(&self.stream, &self.conn.aborted, timeout)?;
        let input = self.buffers.input_append_buf();
        let n = match self.stream.read(input) {
            Err(e) if io_timeout(&e) => return Err(ureq::Error::Timeout(timeout.reason)),
            r => r?,
        };
        self.buffers.input_appended(n);
        Ok(n > 0)
    }

    fn is_open(&mut self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut b = [0u8];
        let open = matches!(self.stream.peek(&mut b), Err(e) if e.kind() == io::ErrorKind::WouldBlock);
        let _ = self.stream.set_nonblocking(false);
        open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(s: &str) -> Vec<SseEvent> {
        let mut r = SseReader::new(io::Cursor::new(s.as_bytes().to_vec()));
        let mut out = Vec::new();
        while let Some(e) = r.next_event(&mut || Ok(())).unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    fn sse_parsing() {
        let e = events(": keepalive\r\ndata: {\"a\":1}\r\n\r\nevent: x\ndata: line1\ndata: line2\n\n\ndata:[DONE]\n\n");
        assert_eq!(
            e,
            vec![SseEvent::Data("{\"a\":1}".into()), SseEvent::Data("line1\nline2".into()), SseEvent::Done]
        );
        // A last event without the blank line still counts.
        assert_eq!(events("data: tail"), vec![SseEvent::Data("tail".into())]);
        assert!(events(": only comments\n\n").is_empty());
    }

    #[test]
    fn error_shapes() {
        assert_eq!(error_message(r#"{"error":{"message":"bad","type":"BadRequestError","param":null,"code":400}}"#), "bad");
        assert_eq!(error_message(r#"{"object":"error","message":"old shape","type":"x","code":400}"#), "old shape");
        assert_eq!(error_message(r#"{"detail":[{"msg":"field required"}]}"#), "field required");
        assert_eq!(error_message("  plain text "), "plain text");
    }

    #[test]
    fn classification() {
        let b = |m: &str| serde_json::json!({"error": {"message": m}}).to_string();
        assert!(matches!(
            classify(400, &b("This model's maximum context length is 32768 tokens. However, you requested 40000 tokens"), true, None),
            AiError::ContextLength(_)
        ));
        assert!(matches!(
            classify(400, &b("At most 1 image(s) may be provided in one prompt."), true, None),
            AiError::ImageLimit { limit: Some(1), .. }
        ));
        assert!(matches!(classify(400, &b("At most 0 image(s) may be provided in one request."), true, None), AiError::NotMultimodal(_)));
        assert!(matches!(classify(400, &b("Qwen3ForCausalLM is not a multimodal model"), true, None), AiError::NotMultimodal(_)));
        assert!(matches!(
            classify(400, &b("The provided JSON schema contains features not supported by xgrammar."), true, None),
            AiError::ResponseFormatRejected(_)
        ));
        assert!(matches!(classify(400, &b("Unrecognized request argument supplied: chat_template_kwargs"), true, None), AiError::UnsupportedParameter(_)));
        assert!(matches!(classify(401, &b("Unauthorized"), true, None), AiError::Unauthorized { key_sent: true, .. }));
        assert_eq!(classify(401, "", false, None).to_string(), "Server requires an API key (401)");
        assert!(matches!(classify(404, &b("The model `x` does not exist."), true, None), AiError::ModelNotFound(_)));
        assert!(matches!(classify(413, "", true, None), AiError::PayloadTooLarge(_)));
        let busy = classify(503, "", true, Some(Duration::from_secs(3)));
        assert!(busy.is_retryable());
        assert!(!classify(400, "nope", true, None).is_retryable());
    }

    #[test]
    fn completion_parsing() {
        let body = br#"{"choices":[{"message":{"role":"assistant","content":"hi","reasoning_content":"think"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}"#;
        let o = parse_completion(body).unwrap();
        assert_eq!((o.content.as_str(), o.reasoning.as_str(), o.finish_reason.as_deref()), ("hi", "think", Some("stop")));
        assert_eq!(o.usage.unwrap().total_tokens, 6);
    }
}
