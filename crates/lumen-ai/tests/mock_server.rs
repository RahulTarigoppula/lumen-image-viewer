//! End-to-end tests against a mock OpenAI-compatible server (a tiny
//! `std::net::TcpListener` responder replaying vLLM-shaped responses).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::{Rgba, RgbaImage};
use lumen_ai::*;
use serde_json::{json, Value};

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

enum Reply {
    Json(u16, Value),
    JsonWithHeaders(u16, Value, Vec<(&'static str, String)>),
    /// Event-stream chunks, each written after its delay.
    Sse(Vec<(Duration, String)>),
}

struct Mock {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    /// The client closed the connection while we were still streaming.
    disconnected: Arc<AtomicBool>,
}

type Handler = dyn Fn(&Recorded, usize) -> Reply + Send + Sync;

impl Mock {
    fn start(handler: impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let count = Arc::new(AtomicUsize::new(0));
        let (reqs, disc) = (requests.clone(), disconnected.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (handler, reqs, disc, count) = (handler.clone(), reqs.clone(), disc.clone(), count.clone());
                std::thread::spawn(move || serve(stream, &*handler, &reqs, &disc, &count));
            }
        });
        Mock { addr, requests, disconnected }
    }

    fn settings(&self) -> AiSettings {
        AiSettings { host: format!("127.0.0.1:{}", self.addr.port()), enabled: true, ..Default::default() }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn chats(&self) -> Vec<Recorded> {
        self.requests().into_iter().filter(|r| r.path == "/v1/chat/completions").collect()
    }
}

fn serve(stream: TcpStream, handler: &Handler, reqs: &Mutex<Vec<Recorded>>, disc: &AtomicBool, count: &AtomicUsize) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let path = it.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).unwrap();
    let rec = Recorded { method, path, headers, body };
    let n = count.fetch_add(1, Ordering::SeqCst);
    reqs.lock().unwrap().push(rec.clone());
    let mut out = stream;
    let reason = |s: u16| match s {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Status",
    };
    match handler(&rec, n) {
        Reply::Json(s, v) => write_json(&mut out, s, reason(s), &v, &[]),
        Reply::JsonWithHeaders(s, v, h) => write_json(&mut out, s, reason(s), &v, &h),
        Reply::Sse(chunks) => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
            if out.write_all(head.as_bytes()).is_err() {
                return;
            }
            let _ = out.flush();
            for (delay, data) in chunks {
                std::thread::sleep(delay);
                let ok = out.write_all(format!("data: {data}\n\n").as_bytes()).and_then(|_| out.flush());
                if ok.is_err() {
                    disc.store(true, Ordering::SeqCst);
                    return;
                }
            }
        }
    }
}

fn write_json(out: &mut TcpStream, status: u16, reason: &str, v: &Value, extra: &[(&str, String)]) {
    let body = if v.is_null() { String::new() } else { v.to_string() };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, val) in extra {
        head.push_str(&format!("{k}: {val}\r\n"));
    }
    head.push_str("\r\n");
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(body.as_bytes());
    let _ = out.flush();
}

fn models_json() -> Value {
    json!({"object": "list", "data": [
        {"id": "meta-llama/Llama-3.1-8B-Instruct", "object": "model", "created": 1760800000, "owned_by": "vllm",
         "root": "meta-llama/Llama-3.1-8B-Instruct", "parent": null, "max_model_len": 8192, "permission": []},
        {"id": "Qwen/Qwen3.5-9B", "object": "model", "created": 1760800000, "owned_by": "vllm",
         "root": "Qwen/Qwen3.5-9B", "parent": null, "max_model_len": 32768,
         "permission": [{"id": "modelperm-1", "object": "model_permission"}]}]})
}

fn completion(content: &str) -> Value {
    json!({"id": "chatcmpl-1", "object": "chat.completion", "model": "Qwen/Qwen3.5-9B",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 80, "completion_tokens": 1, "total_tokens": 81}})
}

fn err(msg: &str, code: u16) -> Value {
    json!({"error": {"message": msg, "type": "BadRequestError", "param": null, "code": code}})
}

const REPORT: &str = r#"{"overall": 7, "categories": {"sharpness_detail": {"score": 8, "rationale": "crisp"}, "noise": {"score": 6, "rationale": "chroma noise"}, "compression_artifacts": {"score": 7, "rationale": "light banding"}, "color": {"score": 8, "rationale": "neutral"}, "exposure_dynamic_range": {"score": 7, "rationale": "ok"}, "detail_vs_reference": null}, "findings": [{"category": "noise", "severity": "moderate", "image": "A", "tile": 0, "bbox": [0, 0, 500, 500], "description": "chroma noise top left"}], "summary": "Decent."}"#;

const COMPARE: &str = r#"{"preferred": "A", "confidence": 0.7, "overall_a": 8, "overall_b": 6, "scores_a": {}, "scores_b": {}, "findings": [{"category": "compression_artifacts", "severity": "severe", "image": "B", "tile": 0, "bbox": [600, 0, 900, 400], "description": "blocking"}], "summary": "B is blocky."}"#;

/// SSE chunks in vLLM's shape: role chunk, reasoning (new and legacy
/// field names), content split in pieces, finish, usage.
fn sse_for(content: &str, gap: Duration) -> Vec<(Duration, String)> {
    let base = |delta: Value, finish: Value| {
        json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m",
               "choices": [{"index": 0, "delta": delta, "logprobs": null, "finish_reason": finish}]})
        .to_string()
    };
    let mut v = vec![(gap, base(json!({"role": "assistant", "content": ""}), Value::Null))];
    v.push((gap, base(json!({"reasoning": "Looking at "}), Value::Null)));
    v.push((gap, base(json!({"reasoning_content": "the noise."}), Value::Null)));
    let chars: Vec<char> = content.chars().collect();
    for piece in chars.chunks(40) {
        v.push((gap, base(json!({"content": piece.iter().collect::<String>()}), Value::Null)));
    }
    v.push((gap, base(json!({"content": ""}), json!("stop"))));
    v.push((gap, json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "choices": [],
        "usage": {"prompt_tokens": 1400, "completion_tokens": 120, "total_tokens": 1520, "prompt_tokens_details": {"cached_tokens": 0}}}).to_string()));
    v.push((gap, "[DONE]".to_string()));
    v
}

fn image(w: u32, h: u32, seed: u8) -> Arc<RgbaImage> {
    Arc::new(RgbaImage::from_fn(w, h, |x, y| Rgba([(x as u8) ^ seed, y as u8, seed, 255])))
}

fn repaint() -> Repaint {
    Arc::new(|| {})
}

/// Collects events until a terminal one, or panics after `limit`.
fn collect(task: &Task, limit: Duration) -> Vec<AiEvent> {
    let end = Instant::now() + limit;
    let mut out = Vec::new();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match task.receiver().recv_timeout(left) {
            Ok(ev) => {
                let done = matches!(ev, AiEvent::Done(_) | AiEvent::TestDone(_) | AiEvent::Models(_) | AiEvent::Vision { .. });
                out.push(ev);
                if done {
                    return out;
                }
            }
            Err(_) => panic!("no terminal event within {limit:?}; got {out:?}"),
        }
    }
}

fn done(events: &[AiEvent]) -> &Result<RunResult, AiError> {
    match events.last() {
        Some(AiEvent::Done(r)) => r,
        other => panic!("last event is not Done: {other:?}"),
    }
}

fn text_of(events: &[AiEvent], reasoning: bool) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AiEvent::Delta(t) if !reasoning => Some(t.as_str()),
            AiEvent::Reasoning(t) if reasoning => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn image_parts(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|p| p["type"] == "image_url")
        .count()
}

fn vllm_mock(chat: impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static) -> Mock {
    Mock::start(move |r, n| match r.path.as_str() {
        "/health" => Reply::Json(200, Value::Null),
        "/version" => Reply::Json(200, json!({"version": "0.32.0"})),
        "/v1/models" => Reply::Json(200, models_json()),
        "/v1/chat/completions" => chat(r, n),
        _ => Reply::Json(404, json!({"detail": "Not Found"})),
    })
}

fn quality_input() -> RequestInput {
    RequestInput::new(Preset::builtin(PresetId::Quality), InputImage::new(image(640, 480, 1)))
}

#[test]
fn test_connection_lists_models_and_picks_vision() {
    let mock = vllm_mock(|_, _| Reply::Json(200, completion("gray")));
    // The pasted-URL form works as well as host:port.
    let mut s = mock.settings();
    s.host = format!("http://127.0.0.1:{}/v1", mock.addr.port());
    let client = Client::from_settings(&s, ApiKey::new("sk-test")).unwrap();
    let task = spawn_test_connection(client, None, true, repaint());
    let events = collect(&task, Duration::from_secs(20));
    let steps: Vec<&StepResult> = events.iter().filter_map(|e| if let AiEvent::TestStep(s) = e { Some(s) } else { None }).collect();
    assert_eq!(steps.len(), 5);
    let AiEvent::TestDone(report) = events.last().unwrap() else { panic!() };
    assert!(report.ok(), "{report:?}");
    assert!(report.steps.iter().all(|s| s.status == StepStatus::Ok), "{:?}", report.steps);
    assert_eq!(report.version.as_deref(), Some("0.32.0"));
    assert_eq!(report.steps[2].message, "vLLM 0.32.0");
    assert_eq!(report.models.len(), 2);
    assert_eq!(report.models[1].max_model_len, Some(32768));
    assert_eq!(report.picked_model.as_deref(), Some("Qwen/Qwen3.5-9B"));
    assert_eq!(report.vision, Some(VisionVerdict::Yes));

    let reqs = mock.requests();
    let models = reqs.iter().find(|r| r.path == "/v1/models").unwrap();
    assert_eq!(models.header("authorization"), Some("Bearer sk-test"));
    let health = reqs.iter().find(|r| r.path == "/health").unwrap();
    assert_eq!(health.method, "GET");
    assert!(health.header("authorization").is_none(), "the key goes only to /v1 routes");
    // The vision probe: one tiny image, one token, thinking off.
    let probe = mock.chats()[0].json();
    assert_eq!(image_parts(&probe), 1);
    assert_eq!(probe["max_completion_tokens"], 1);
    assert_eq!(probe["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(probe["model"], "Qwen/Qwen3.5-9B");
}

#[test]
fn test_connection_reports_text_only_and_keeps_going() {
    let mock = vllm_mock(|_, _| Reply::Json(400, err("Qwen3ForCausalLM is not a multimodal model", 400)));
    let client = Client::from_settings(&mock.settings(), None).unwrap();
    let report = client.test_connection(Some("meta-llama/Llama-3.1-8B-Instruct"), true, &mut |_| {});
    assert!(report.ok());
    assert_eq!(report.picked_model.as_deref(), Some("meta-llama/Llama-3.1-8B-Instruct"), "current model kept");
    assert!(matches!(report.vision, Some(VisionVerdict::No(_))));
    assert_eq!(report.steps[4].status, StepStatus::Warning);
}

#[test]
fn test_connection_refused_is_precise() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let s = AiSettings { host: format!("127.0.0.1:{port}"), ..Default::default() };
    let report = Client::from_settings(&s, None).unwrap().test_connection(None, true, &mut |_| {});
    assert!(!report.ok());
    assert_eq!(report.steps[0].status, StepStatus::Failed);
    assert!(report.steps[0].message.contains("refused"), "{}", report.steps[0].message);
    assert!(report.steps[1..].iter().all(|s| s.status == StepStatus::Skipped));
}

#[test]
fn public_address_is_blocked_before_connecting() {
    let s = AiSettings { host: "8.8.8.8".into(), ..Default::default() };
    let report = Client::from_settings(&s, None).unwrap().test_connection(None, false, &mut |_| {});
    assert_eq!(report.steps[0].status, StepStatus::Failed);
    assert!(report.steps[0].message.contains("not a local network address"), "{}", report.steps[0].message);
    // The guarded connector refuses too, for any request path.
    let client = Client::from_settings(&s, None).unwrap();
    assert!(matches!(client.list_models(), Err(AiError::NotLocal { .. })));
}

#[test]
fn unauthorized_key() {
    let mock = Mock::start(|r, _| match r.path.as_str() {
        "/v1/models" => Reply::Json(401, err("Unauthorized", 401)),
        _ => Reply::Json(200, json!({"version": "0.32.0"})),
    });
    let with_key = Client::from_settings(&mock.settings(), ApiKey::new("wrong")).unwrap();
    let report = with_key.test_connection(None, true, &mut |_| {});
    assert_eq!(report.steps[3].status, StepStatus::Failed);
    assert!(report.steps[3].message.starts_with("API key rejected (401)"), "{}", report.steps[3].message);
    let no_key = Client::from_settings(&mock.settings(), None).unwrap();
    assert!(no_key.list_models().unwrap_err().to_string().contains("requires an API key"));
}

#[test]
fn stream_with_reasoning_usage_and_structured_report() {
    let mock = vllm_mock(|_, _| Reply::Sse(sse_for(REPORT, Duration::from_millis(5))));
    let s = mock.settings();
    let prepared = prepare(&s, "Qwen/Qwen3.5-9B", quality_input()).unwrap();
    assert_eq!(prepared.destination, format!("http://127.0.0.1:{}/v1/chat/completions", mock.addr.port()));
    let task = spawn_run(Client::from_settings(&s, None).unwrap(), prepared, repaint());
    let events = collect(&task, Duration::from_secs(20));
    assert!(matches!(events[0], AiEvent::Connected), "{:?}", events[0]);
    assert_eq!(text_of(&events, true), "Looking at the noise.");
    assert_eq!(text_of(&events, false), REPORT);
    assert!(events.iter().any(|e| matches!(e, AiEvent::Usage(u) if u.total_tokens == 1520)));
    let r = done(&events).as_ref().unwrap();
    assert_eq!(r.finish_reason.as_deref(), Some("stop"));
    assert!(!r.truncated);
    assert_eq!(r.usage.as_ref().unwrap().prompt_tokens, 1400);
    let report = r.report.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(report.overall, Some(7));
    // The finding maps back to source pixels (overview of 640x480, native).
    let src = report.findings[0].source.unwrap();
    assert_eq!(src.label, ImageLabel::A);
    assert!((src.x1 - 320.0).abs() < 1.0 && (src.y1 - 240.0).abs() < 1.0, "{src:?}");
    assert!(report.to_markdown("Quality critique", false).contains("chroma noise top left"));
    assert_eq!(r.sent.images.len(), 1);

    // The body vLLM received.
    let body = mock.chats()[0].json();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(image_parts(&body), 1);
    // Coalescing: fewer text events than chunks were sent.
    let deltas = events.iter().filter(|e| matches!(e, AiEvent::Delta(_))).count();
    assert!(deltas >= 1 && deltas < REPORT.len() / 40 + 1, "{deltas} delta events");
}

#[test]
fn non_stream_structured_and_quick_score() {
    let mock = vllm_mock(|r, _| {
        let b = r.json();
        if b["logprobs"] == true {
            Reply::Json(200, json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": " good"}, "finish_reason": "length",
                "logprobs": {"content": [{"token": " good", "logprob": -0.2, "top_logprobs": [
                    {"token": " good", "logprob": -0.2}, {"token": " excellent", "logprob": -1.9}, {"token": " fair", "logprob": -3.5}]}]}}],
                "usage": {"prompt_tokens": 300, "completion_tokens": 1, "total_tokens": 301}}))
        } else {
            Reply::Json(200, completion(&format!("```json\n{REPORT}\n```")))
        }
    });
    let s = mock.settings();
    let client = Client::from_settings(&s, None).unwrap();
    let q = RequestInput::new(Preset::builtin(PresetId::QuickScore), InputImage::new(image(512, 384, 2)));
    let r = run_blocking(&client, prepare(&s, "q-future/Q-ReAlign-Lite-4B", q).unwrap(), &CancelToken::new()).unwrap();
    let score = r.quick_score.unwrap().unwrap();
    assert!(score.score > 0.6 && score.score < 0.8, "{score:?}");
    assert!(r.warnings.is_empty(), "length is expected for one token: {:?}", r.warnings);
    let qb = mock.chats()[0].json();
    assert_eq!(qb["continue_final_message"], true);
    assert_eq!(qb["messages"][1]["role"], "assistant");

    // A server that ignores `stream` and answers with plain JSON.
    let r = run_blocking(&client, prepare(&s, "m", quality_input()).unwrap(), &CancelToken::new()).unwrap();
    assert_eq!(r.report.unwrap().unwrap().overall, Some(7));
}

#[test]
fn context_length_error() {
    let mock = vllm_mock(|_, _| {
        Reply::Json(400, err("This model's maximum context length is 8192 tokens. However, you requested 9500 tokens (8000 in the messages, 1500 in the completion).", 400))
    });
    let s = mock.settings();
    let task = spawn_run(Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), repaint());
    let events = collect(&task, Duration::from_secs(20));
    let e = done(&events).as_ref().unwrap_err();
    assert!(matches!(e, AiError::ContextLength(_)), "{e:?}");
    assert_eq!(e.to_string(), "Request too large: reduce max image side or tiles");
    assert_eq!(mock.chats().len(), 1, "no retry for 400");
}

#[test]
fn model_not_found_error() {
    let mock = vllm_mock(|_, _| Reply::Json(404, err("The model `gone` does not exist.", 404)));
    let s = mock.settings();
    let r = run_blocking(&Client::from_settings(&s, None).unwrap(), prepare(&s, "gone", quality_input()).unwrap(), &CancelToken::new());
    assert_eq!(r.unwrap_err().to_string(), "Model not loaded on server (refresh the list)");
}

#[test]
fn image_limit_falls_back_to_composite() {
    let mock = vllm_mock(|r, _| {
        if image_parts(&r.json()) > 1 {
            Reply::Json(400, err("At most 1 image(s) may be provided in one prompt.", 400))
        } else {
            Reply::Sse(sse_for(COMPARE, Duration::from_millis(1)))
        }
    });
    let s = mock.settings();
    let input = RequestInput::new(Preset::builtin(PresetId::Compare), InputImage::new(image(800, 600, 1))).with_b(InputImage::new(image(800, 600, 7)));
    let prepared = prepare(&s, "Qwen/Qwen3.5-9B", input).unwrap();
    assert_eq!(prepared.image_count(), 2);
    let server_key = prepared.server_key.clone();
    let task = spawn_run(Client::from_settings(&s, None).unwrap(), prepared, repaint());
    let events = collect(&task, Duration::from_secs(20));
    assert!(events.iter().any(|e| matches!(e, AiEvent::ImageLimitLearned { limit: 1, server_key: k, .. } if *k == server_key)));
    assert!(events.iter().any(|e| matches!(e, AiEvent::Status(m) if m.contains("composite"))));
    let r = done(&events).as_ref().unwrap();
    assert_eq!(r.sent.images.len(), 1);
    assert_eq!(r.sent.changes.len(), 1);
    assert!(r.sent.user_prompt.contains("side by side"));
    let chats = mock.chats();
    assert_eq!(chats.len(), 2);
    assert_eq!(image_parts(&chats[1].json()), 1);
    // The B finding at x 600..900 of the composite lands in B's half.
    let report = r.report.as_ref().unwrap().as_ref().unwrap();
    let f = &report.findings[0];
    let src = f.source.unwrap();
    assert_eq!((f.image, src.label), (ImageLabel::B, ImageLabel::B));
    assert!(src.x0 >= 0.0 && src.x1 <= 800.0, "{src:?}");
    assert_eq!(report.comparison.as_ref().unwrap().preferred, Preference::A);

    // The learned limit is used up front next time.
    let mut s2 = s.clone();
    s2.remember_image_limit(&server_key, "Qwen/Qwen3.5-9B", 1);
    let input = RequestInput::new(Preset::builtin(PresetId::Compare), InputImage::new(image(800, 600, 1))).with_b(InputImage::new(image(800, 600, 7)));
    assert_eq!(prepare(&s2, "Qwen/Qwen3.5-9B", input).unwrap().image_count(), 1);
}

#[test]
fn response_format_rejected_falls_back_to_prompt_json() {
    let mock = vllm_mock(|r, _| {
        if r.json().get("response_format").is_some() {
            Reply::Json(400, err("The provided JSON schema contains features not supported by xgrammar.", 400))
        } else {
            Reply::Sse(sse_for(REPORT, Duration::from_millis(1)))
        }
    });
    let s = mock.settings();
    let r = run_blocking(&Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), &CancelToken::new()).unwrap();
    assert_eq!(r.report.unwrap().unwrap().overall, Some(7));
    assert_eq!(r.sent.changes.len(), 1);
    let second = mock.chats()[1].json();
    assert!(second.get("response_format").is_none());
    assert!(second["messages"][0]["content"].as_str().unwrap().contains("JSON Schema"));
    assert_eq!(second["chat_template_kwargs"]["enable_thinking"], false, "other fields unchanged");
}

#[test]
fn unsupported_extension_fields_are_dropped() {
    let mock = vllm_mock(|r, _| {
        if r.json().get("chat_template_kwargs").is_some() {
            Reply::Json(400, err("Unrecognized request argument supplied: chat_template_kwargs", 400))
        } else {
            Reply::Sse(sse_for(REPORT, Duration::from_millis(1)))
        }
    });
    let s = mock.settings();
    let r = run_blocking(&Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), &CancelToken::new()).unwrap();
    assert!(r.report.unwrap().is_ok());
    assert!(mock.chats()[1].json().get("response_format").is_some(), "structured output kept");
}

#[test]
fn busy_server_is_retried_before_the_first_token() {
    let mock = vllm_mock(|_, n| {
        if n == 0 {
            Reply::JsonWithHeaders(503, err("Service Unavailable", 503), vec![("Retry-After", "1".into())])
        } else {
            Reply::Sse(sse_for(REPORT, Duration::from_millis(1)))
        }
    });
    let s = mock.settings();
    let t = Instant::now();
    let task = spawn_run(Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), repaint());
    let events = collect(&task, Duration::from_secs(20));
    assert!(done(&events).is_ok());
    assert!(events.iter().any(|e| matches!(e, AiEvent::Status(m) if m.contains("retrying in 1 s"))), "{events:?}");
    assert!(t.elapsed() >= Duration::from_secs(1));
    assert_eq!(mock.chats().len(), 2);
}

#[test]
fn slow_first_token_times_out() {
    let mock = vllm_mock(|_, _| Reply::Sse(vec![(Duration::from_secs(6), "{}".into())]));
    let mut s = mock.settings();
    s.timeout_first_token_s = 1;
    let t = Instant::now();
    let r = run_blocking(&Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), &CancelToken::new());
    let e = r.unwrap_err();
    assert!(matches!(e, AiError::Timeout(TimeoutKind::FirstToken, _)), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    assert_eq!(mock.chats().len(), 1, "timeouts are not retried");
}

#[test]
fn mid_stream_cancel_closes_the_connection() {
    let mock = vllm_mock(|_, _| {
        let mut v = sse_for(REPORT, Duration::from_millis(100));
        v.truncate(4);
        v.extend((0..100).map(|_| (Duration::from_millis(100), json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": null}]}).to_string())));
        Reply::Sse(v)
    });
    let s = mock.settings();
    let task = spawn_run(Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), repaint());
    let mut deltas = 0;
    let end = Instant::now() + Duration::from_secs(20);
    while deltas < 2 {
        match task.receiver().recv_timeout(end.saturating_duration_since(Instant::now())).unwrap() {
            AiEvent::Delta(_) => deltas += 1,
            AiEvent::Done(r) => panic!("finished early: {r:?}"),
            _ => {}
        }
    }
    let t = Instant::now();
    task.cancel();
    let events = collect(&task, Duration::from_secs(5));
    assert!(matches!(done(&events), Err(AiError::Cancelled)));
    assert!(t.elapsed() < Duration::from_millis(1500), "cancel took {:?}", t.elapsed());
    // The server notices the closed socket on its next write.
    let end = Instant::now() + Duration::from_secs(3);
    while !mock.disconnected.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(mock.disconnected.load(Ordering::SeqCst), "client did not close the connection");
}

#[test]
fn truncated_output_keeps_partial_report() {
    let cut = &REPORT[..REPORT.find("\"findings\"").unwrap()];
    let mock = vllm_mock(move |_, _| {
        let mut v = sse_for(cut, Duration::from_millis(1));
        // Replace the stop chunk with a length finish.
        for (_, d) in &mut v {
            *d = d.replace("\"stop\"", "\"length\"");
        }
        Reply::Sse(v)
    });
    let s = mock.settings();
    let r = run_blocking(&Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), &CancelToken::new()).unwrap();
    assert!(r.truncated);
    assert!(r.warnings.iter().any(|w| w == "Output truncated, raise max tokens"));
    let report = r.report.unwrap().unwrap();
    assert!(report.partial);
    assert_eq!(report.overall, Some(7));
}

#[test]
fn swap_check_runs_two_passes() {
    // A model with position bias: always prefers the first image.
    let mock = vllm_mock(|_, _| Reply::Sse(sse_for(COMPARE, Duration::from_millis(1))));
    let mut s = mock.settings();
    s.swap_check = true;
    let input = RequestInput::new(Preset::builtin(PresetId::Compare), InputImage::new(image(320, 240, 1))).with_b(InputImage::new(image(320, 240, 9)));
    let task = spawn_run(Client::from_settings(&s, None).unwrap(), prepare(&s, "m", input).unwrap(), repaint());
    let events = collect(&task, Duration::from_secs(20));
    assert!(events.iter().any(|e| matches!(e, AiEvent::Pass { index: 2, total: 2 })));
    let r = done(&events).as_ref().unwrap();
    let swap = r.swap.as_ref().unwrap();
    assert_eq!(swap.agree, Some(false), "a first-position bias flips the preference");
    let second = swap.report.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(second.comparison.as_ref().unwrap().preferred, Preference::B);
    assert_eq!(second.findings[0].image, ImageLabel::A, "labels mapped back");
    assert!(r.warnings.iter().any(|w| w.contains("Swap-check disagrees")));
    assert_eq!(mock.chats().len(), 2);
}

#[test]
fn host_name_resolves_and_prefers_ipv4() {
    let mock = vllm_mock(|_, _| Reply::Json(200, completion("x")));
    let s = AiSettings { host: format!("localhost:{}", mock.addr.port()), ..Default::default() };
    let t = Instant::now();
    let models = Client::from_settings(&s, None).unwrap().list_models().unwrap();
    assert_eq!(models.len(), 2);
    // Well under the 5 s connect timeout an IPv6-first attempt would hit (loose for loaded CI).
    assert!(t.elapsed() < Duration::from_millis(4000), "{:?}", t.elapsed());
}

#[test]
fn cancel_while_the_server_is_silent() {
    // Headers arrive, then nothing (long prefill): Cancel must not wait for data.
    let mock = vllm_mock(|_, _| Reply::Sse(vec![(Duration::from_secs(8), "{}".into())]));
    let s = mock.settings();
    let task = spawn_run(Client::from_settings(&s, None).unwrap(), prepare(&s, "m", quality_input()).unwrap(), repaint());
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        match task.receiver().recv_timeout(end.saturating_duration_since(Instant::now())).unwrap() {
            AiEvent::Connected => break,
            AiEvent::Done(r) => panic!("finished early: {r:?}"),
            _ => {}
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    task.cancel();
    let events = collect(&task, Duration::from_secs(5));
    assert!(matches!(done(&events), Err(AiError::Cancelled)));
    assert!(t.elapsed() < Duration::from_millis(1500), "cancel took {:?}", t.elapsed());
}
