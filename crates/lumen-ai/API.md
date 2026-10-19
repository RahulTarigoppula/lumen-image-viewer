# lumen-ai integration guide

`lumen-ai` is the remote AI critique client (plan: `docs/ANALYSIS-PLAN.md`
section 3) and the only network code in Lumen. It has no egui dependency:
the UI passes RGBA images and strings in, and gets `AiEvent`s back over a
channel plus a repaint callback. Everything blocking runs on worker threads
started by this crate; never call `Client` methods on the UI thread.

User flow: type the IP, press Test connection (a model is picked
automatically), pick a preset, press Run (send preview, then stream).

```rust
use lumen_ai::*;
```

## 1. Settings

`AiSettings` is serde with `#[serde(default)]`; store it under `ai` in
settings.json. Defaults follow plan 3.2 (`enabled: false`, port 8000, HTTP,
`lan_only`, `always_confirm`, `disable_thinking`, PNG, 1536 px overview,
1024 px tiles, 6 images, timeouts 5 / 120 / 600 s). The API key is never in
it; `api_key_ref` only says where the key lives.

The host field accepts anything a user pastes:

| Typed | Base URL |
|---|---|
| `192.168.1.20` | `http://192.168.1.20:8000/v1` (port field, HTTPS toggle) |
| `192.168.1.20:9000` | `http://192.168.1.20:9000/v1` |
| `http://192.168.1.20:8000/v1` (or `.../v1/chat/completions`) | `http://192.168.1.20:8000/v1` |
| `https://box.lan/vllm/v1` | `https://box.lan:443/vllm/v1` |
| `fd00::5`, `[fd00::5]:8000` | `http://[fd00::5]:8000/v1` |

- `settings.endpoint() -> Result<Endpoint, AiError>`: parse without changing
  anything. `Endpoint::{base_url, display, server_key, credential_target}`.
- `settings.apply_server_input(text)`: call when the host field loses focus
  or before testing; folds a pasted URL into `host`, `port`, `https`,
  `path_prefix` so the fields show what will be used. On error the fields
  are unchanged and the `AiError` message says what is wrong.
- `settings.warnings(has_key)`: show under the fields (key over plain HTTP).
- `settings.needs_confirmation(&req.server_key)`: whether to show the send
  preview. "Do not ask again for this server" pushes `server_key` into
  `trusted_servers` (it only matters when `always_confirm` is off).
- LAN policy: `lan_only` allows loopback, RFC 1918, link-local and IPv6
  ULA; `allow_cgnat` adds 100.64/10 (Tailscale). Every resolved address is
  checked at Test and again inside the connector right before each
  connection. Public addresses need `lan_only` off and HTTPS.

## 2. Secrets

```rust
let store = SecretStore::new(&settings::data_dir());   // the app's data dir
let target = settings.endpoint()?.credential_target();  // "Lumen/ai/host:port"
settings.api_key_ref = Some(store.save(&target, &ApiKey::new(&typed).unwrap())?);
let key: Option<ApiKey> = store.load(&target, settings.api_key_ref)?;
store.delete(&target)?;                                  // "Forget key"
```

`save` uses Credential Manager and falls back to a DPAPI file
(`ai-key.bin`). `ApiKey` zeroes itself on drop and prints as `<redacted>`.
Do not enable `log` TRACE for `ureq` (its wire log is not redacted).

## 3. Test connection and models

```rust
let client = Client::from_settings(&settings, key.clone())?;
let task = spawn_test_connection(client, settings.model.clone(), true, repaint.clone());
```

`repaint` is `Arc<dyn Fn() + Send + Sync>`, e.g.
`{ let c = ctx.clone(); Arc::new(move || c.request_repaint()) }`.

Each frame, `task.drain()`:
- `AiEvent::TestStep(StepResult { step, status, message, elapsed_ms })` for
  Connect, Health, Version, Models, Vision, in order. `status` is Ok,
  Warning, Failed or Skipped; `message` is ready to show ("Server not
  reachable at 192.168.1.20:8000: connection refused: is vllm serve running
  there, listening on 0.0.0.0...",
  "API key rejected (401)", "vLLM 0.32.0", "Qwen/Qwen3.5-9B accepts images").
- `AiEvent::TestDone(ConnectionReport)`: `ok()`, `failure()`, `version`,
  `models: Vec<ModelCard>`, `picked_model` (current model if still served,
  else the best vision candidate, else the first), `vision`.

Set `settings.model = report.picked_model` when the user has not chosen one.
The model dropdown lists `report.models`; label each with
`card.vision_hint().label()` ("probably vision" or "vision unknown, will
try") or a cached probe verdict. Also allow typing a model id by hand. A
failed or ambiguous probe never blocks Run.

Other tasks: `spawn_list_models(client, repaint)` ends with
`AiEvent::Models(Result<Vec<ModelCard>, AiError>)`;
`spawn_vision_probe(client, model, repaint)` ends with
`AiEvent::Vision { model, verdict }`. Keep verdicts in a `ProbeCache`
(serde; per server, model id and root).

## 4. Presets

`all_presets(&settings.custom_presets)` lists built-ins then custom ones;
`find_preset(&settings.preset, &settings.custom_presets)` resolves the
stored id. Built-in ids (`PresetId`): `quality`, `compare`, `encode`,
`photo`, `freeform`, `quick_score`, `video_frame`, `video_compare`.
`preset.arity` (One or Two) says whether an A/B pair is required;
`preset.output` is QualityJson, ComparisonJson, Markdown or QuickScore.

## 5. Build the request and show the preview

```rust
let a = InputImage::new(Arc::new(rgba_a))          // decoded sRGB, as displayed
    .with_file_name("IMG_0042.jpg")                // sent only with include_filename
    .with_capture("Canon R5, 50 mm, f/1.8, 1/200 s, ISO 3200") // only with include_exif; never GPS
    .with_frame(FrameRef { timestamp_s: Some(83.456), frame_index: Some(2001) }); // video
let input = RequestInput::new(preset, a)
    .with_b(b)                                     // pair presets
    .with_roi(Roi { x, y, w, h })                  // A's pixels; mapped onto B proportionally
    .with_metrics("B vs A, ROI", [("PSNR-Y", 38.2, "dB"), ("SSIM", 0.962, "")])
    .with_question("Is the sky banded?");           // Free-form question or extra instructions
let req: PreparedRequest = prepare(&settings, &model, input)?;   // CPU work: run off the UI thread
```

`input.layout = AbLayout::Composite` forces the side-by-side composite;
`input.include_overview = false` sends ROI tiles only. Metrics are left out
when `settings.blind` is on.

The send preview shows:
- `req.destination` (full URL), `req.server`, `req.model`, `req.preset_name`;
- `req.images`: `PreviewImage { thumbnail: RgbaImage (<= 256 px), width, height, bytes, mime, description }`,
  for example "A full view, 1536x864, downscaled 2.5x from 3840x2160" or
  "B ROI tile 2/4: same region";
- `req.system_prompt` and `req.user_prompt` (exact text, `[Image n: ...]` where images go);
- `req.total_bytes`, `req.estimated_prompt_tokens`, `req.notes`;
- `req.context_warning(&card)` when the request may exceed `max_model_len`;
- `req.swapped` (swap-check second pass) when `settings.swap_check` is on for Compare presets.

Buttons: Send (go to 6), Cancel (drop `req`), "Do not ask again for this server".

## 6. Run and events

```rust
let task = spawn_run(client, req, repaint);   // keep task.id() to ignore superseded runs
```

Drain every frame:

| Event | UI |
|---|---|
| `Connected` | network badge "Sending to 192.168.1.20:8000" stays; output is coming |
| `Pass { index, total }` | swap-check pass 2 of 2 starts: clear the live text |
| `Reasoning(String)` | append to the collapsed "Model reasoning" section |
| `Delta(String)` | append to the live output (Markdown or monospace raw JSON) |
| `Usage(Usage)` | token counts |
| `Status(String)` | status line: retries ("Server busy (503); retrying in 4 s") and automatic changes |
| `ImageLimitLearned { server_key, model, limit }` | `settings.remember_image_limit(...)` and save |
| `Done(Result<RunResult, AiError>)` | final state; the task is over |

Text events are coalesced to about 30 per second. Cancel: `task.cancel()`;
mark the job cancelled at once and drop the task (late events are then
discarded). The socket is closed within about 50 ms, which makes vLLM abort
the request; `Done(Err(AiError::Cancelled))` follows.

Automatic recovery (reported through `Status` and `RunResult.sent.changes`):
retries of connect failures, 429, 502, 503, 504 before the first token (1 s,
4 s, or Retry-After); "At most N image(s)" resends with N images (an A/B
pair becomes a composite at N = 1); a rejected `response_format` resends
with the schema in the system prompt; rejected vLLM-only fields
(`chat_template_kwargs`...) are dropped.

Errors: `AiError` `Display` is the user message (plan 3.5 table: "Server not
reachable", "API key rejected", "Model not loaded on server (refresh the
list)", "Request too large: reduce max image side or tiles", timeouts...).
`err.detail()` has the server's own text for a tooltip.

## 7. Results

`RunResult`: `text` (raw answer), `reasoning`, `finish_reason`, `truncated`
(show "Output truncated, raise max tokens"; partial text is kept), `usage`,
`report: Option<Result<AiReport, String>>` (JSON presets),
`quick_score: Option<Result<QuickScore, String>>` (`score` 0..1, `mos()`
1..5, `probs`), `swap: Option<SwapCheck>` (`agree`, second report already
mapped back to real labels), `sent: SentRecord` (what was sent, for the card
footer and history), `layout`, `elapsed_ms`, `warnings`. It is serde, so it
can be stored in the analysis history as is.

`AiReport`: `overall`, `categories: Vec<CategoryScore { key, label, score, rationale }>`
(six categories in schema order), `comparison: Option<Comparison { preferred, confidence, overall_a, overall_b, scores_a, scores_b }>`,
`findings: Vec<Finding { category, severity, image, tile, bbox, description, source }>`,
`summary`, `partial` (truncated JSON was repaired), `warnings`.

`finding.source: Option<SourceBox { label, x0, y0, x1, y1 }>` is the box in
source pixels of image `label` (A or B), already mapped through tiles,
downscaling, padding and composite halves: draw it directly on the viewer,
compare pane or video frame. Use `report.to_markdown(&run.preset_name)` for
"Copy as Markdown" and `serde_json::to_string_pretty(&run)` for "Export JSON".

## 8. Batch and blocking use

`run_blocking(&client, req, &CancelToken::new())` runs on the caller's thread
without events, for example Quick score over sampled video frames from an
analysis job thread.
