//! Building a request: images, text parts and parameters, plus the
//! `PreparedRequest` the send preview shows before anything leaves the
//! machine. The request body is generated from it at send time, so what
//! the preview lists is exactly what is sent.

use std::sync::Arc;

use image::RgbaImage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::AiError;
use crate::images::{self, ImageLabel, ImageRole, Roi, SentImage, SentLayout};
use crate::models::{context_warning, estimate_prompt_tokens, ModelCard};
use crate::prompts::{self, Arity, FrameRef, OutputKind, Preset};
use crate::settings::{AiSettings, CoordConvention, ImageEncoding};

/// One input picture: decoded, sRGB, as Lumen displays it.
#[derive(Clone, Debug)]
pub struct InputImage {
    pub pixels: Arc<RgbaImage>,
    /// Sent only with "Include file name".
    pub file_name: Option<String>,
    /// Camera, lens, exposure (never GPS); sent only with "Include capture
    /// settings".
    pub capture: Option<String>,
    /// Set for video frames.
    pub frame: Option<FrameRef>,
}

impl InputImage {
    pub fn new(pixels: Arc<RgbaImage>) -> InputImage {
        InputImage { pixels, file_name: None, capture: None, frame: None }
    }
    pub fn with_file_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = Some(name.into());
        self
    }
    pub fn with_capture(mut self, summary: impl Into<String>) -> Self {
        self.capture = Some(summary.into());
        self
    }
    pub fn with_frame(mut self, frame: FrameRef) -> Self {
        self.frame = Some(frame);
        self
    }
}

/// How an A/B pair is sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbLayout {
    /// Separate, labeled, interleaved images (default).
    #[default]
    Separate,
    /// One side-by-side image (automatic for one-image servers).
    Composite,
}

/// What the user asked for.
#[derive(Clone, Debug)]
pub struct RequestInput {
    pub preset: Preset,
    pub a: InputImage,
    pub b: Option<InputImage>,
    /// Region of interest in A's pixels (mapped proportionally onto B).
    pub roi: Option<Roi>,
    /// Send the full view as well as the ROI tiles (default on).
    pub include_overview: bool,
    pub layout: AbLayout,
    /// (name, value, unit) from Lumen's local metrics.
    pub metrics: Vec<(String, f64, String)>,
    /// What the metrics describe, e.g. "B vs A, ROI".
    pub metric_scope: String,
    /// The Free-form question, or extra instructions for other presets.
    pub question: String,
}

impl RequestInput {
    pub fn new(preset: Preset, a: InputImage) -> RequestInput {
        RequestInput {
            preset,
            a,
            b: None,
            roi: None,
            include_overview: true,
            layout: AbLayout::Separate,
            metrics: Vec::new(),
            metric_scope: String::new(),
            question: String::new(),
        }
    }
    pub fn with_b(mut self, b: InputImage) -> Self {
        self.b = Some(b);
        self
    }
    pub fn with_roi(mut self, roi: Roi) -> Self {
        self.roi = Some(roi);
        self
    }
    pub fn with_metrics<N: Into<String>, U: Into<String>>(mut self, scope: &str, metrics: impl IntoIterator<Item = (N, f64, U)>) -> Self {
        self.metric_scope = scope.to_string();
        self.metrics = metrics.into_iter().map(|(n, v, u)| (n.into(), v, u.into())).collect();
        self
    }
    pub fn with_question(mut self, q: impl Into<String>) -> Self {
        self.question = q.into();
        self
    }
}

/// The settings that shape a request, captured when it is prepared.
#[derive(Clone, Debug)]
pub(crate) struct BuildConfig {
    destination: String,
    server: String,
    server_key: String,
    max_side: u32,
    tile_side: u32,
    max_images: u32,
    encoding: ImageEncoding,
    pub(crate) conv: CoordConvention,
    disable_thinking: bool,
    temperature: f32,
    max_completion_tokens: u32,
    include_filename: bool,
    include_exif: bool,
    blind: bool,
    swap_check: bool,
}

/// One image in the send preview.
#[derive(Clone, Debug)]
pub struct PreviewImage {
    /// At most 256 px on the long side.
    pub thumbnail: RgbaImage,
    pub width: u32,
    pub height: u32,
    /// Encoded size (the base64 in the body is about 4/3 of this).
    pub bytes: usize,
    pub mime: &'static str,
    /// "A full view, 1536x864, downscaled 2.5x from 3840x2160".
    pub description: String,
}

#[derive(Clone, Debug)]
pub(crate) enum Part {
    Text(String),
    Image(String),
}

/// A request ready to send, and everything the send preview shows.
#[derive(Clone, Debug)]
pub struct PreparedRequest {
    /// Full URL of the chat completions route.
    pub destination: String,
    /// `192.168.1.20:8000`, for the network badge.
    pub server: String,
    /// For the trusted-server list and learned limits.
    pub server_key: String,
    pub model: String,
    pub preset_id: String,
    pub preset_name: String,
    pub output: OutputKind,
    /// Exact system prompt (with structured output on).
    pub system_prompt: String,
    /// Exact user text parts, with `[Image n: ...]` where images go.
    pub user_prompt: String,
    pub images: Vec<PreviewImage>,
    /// Sum of encoded image bytes.
    pub total_bytes: usize,
    pub estimated_prompt_tokens: u64,
    pub max_completion_tokens: u32,
    /// Things worth knowing before sending (rescaled B, flattened alpha...).
    pub notes: Vec<String>,
    /// The second pass of a swap-check (A and B exchanged), if enabled.
    pub swapped: Option<Box<PreparedRequest>>,
    /// Where each sent image came from, to map findings.
    pub layout: Vec<SentLayout>,
    /// This is the swapped pass (B shown as A).
    pub is_swapped: bool,
    pub(crate) parts: Vec<Part>,
    pub(crate) schema: Option<Value>,
    pub(crate) cfg: BuildConfig,
    pub(crate) input: Arc<RequestInput>,
}

/// How to write the body; the worker relaxes these after specific 400s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyMode {
    /// Send `response_format: json_schema` (else the schema goes into the
    /// system prompt).
    pub response_format: bool,
    /// Send vLLM extensions (`chat_template_kwargs`, `continue_final_message`,
    /// `stream_options.continuous_usage_stats`...).
    pub extensions: bool,
    pub stream: bool,
}

impl PreparedRequest {
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// True when this request asks for JSON.
    pub fn structured(&self) -> bool {
        self.schema.is_some()
    }

    pub fn coord_convention(&self) -> CoordConvention {
        self.cfg.conv
    }

    /// Body mode for the first attempt.
    pub fn default_mode(&self) -> BodyMode {
        BodyMode { response_format: self.schema.is_some(), extensions: true, stream: self.output != OutputKind::QuickScore }
    }

    /// Warning when the request may not fit the model's context.
    pub fn context_warning(&self, card: &ModelCard) -> Option<String> {
        context_warning(card, self.estimated_prompt_tokens, self.max_completion_tokens)
    }

    /// The system prompt for a body mode.
    pub fn system_for(&self, mode: BodyMode) -> String {
        let schema = if mode.response_format { None } else { self.schema.as_ref() };
        self.input.preset.system_prompt(self.cfg.conv, schema)
    }

    /// The JSON body.
    pub fn body(&self, mode: BodyMode) -> Vec<u8> {
        let content: Vec<Value> = self
            .parts
            .iter()
            .map(|p| match p {
                Part::Text(t) => json!({"type": "text", "text": t}),
                Part::Image(url) => json!({"type": "image_url", "image_url": {"url": url}}),
            })
            .collect();
        let mut messages = Vec::new();
        let system = self.system_for(mode);
        if !system.is_empty() {
            messages.push(json!({"role": "system", "content": system}));
        }
        messages.push(json!({"role": "user", "content": content}));
        let mut body = if self.output == OutputKind::QuickScore {
            messages.push(json!({"role": "assistant", "content": prompts::QUICK_SCORE_PREFIX}));
            let mut b = json!({
                "model": self.model,
                "messages": messages,
                "max_completion_tokens": 1,
                "temperature": 0,
                "logprobs": true,
                "top_logprobs": 20,
                "stream": false,
            });
            if mode.extensions {
                b["continue_final_message"] = json!(true);
                b["add_generation_prompt"] = json!(false);
            }
            b
        } else {
            let mut b = json!({
                "model": self.model,
                "messages": messages,
                "max_completion_tokens": self.max_completion_tokens,
                // Rounded so the f32 setting serializes as 0.2, not 0.20000000298.
                "temperature": (self.cfg.temperature as f64 * 1000.0).round() / 1000.0,
                "top_p": 0.9,
                "seed": 42,
                "stream": mode.stream,
            });
            if mode.stream {
                // vLLM's continuous usage gives a live token count while streaming.
                b["stream_options"] = if mode.extensions { json!({"include_usage": true, "continuous_usage_stats": true}) } else { json!({"include_usage": true}) };
            }
            if let (true, Some(schema)) = (mode.response_format, &self.schema) {
                let name = if self.output == OutputKind::ComparisonJson { "lumen_comparison_report" } else { "lumen_quality_report" };
                b["response_format"] = prompts::response_format(name, schema);
            }
            b
        };
        if mode.extensions && self.cfg.disable_thinking {
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
        }
        serde_json::to_vec(&body).expect("json body")
    }

    /// The same request planned for at most `max_images` images (after an
    /// image-limit error). A limit of 1 turns an A/B pair into a composite.
    pub fn with_max_images(&self, max_images: u32) -> Result<PreparedRequest, AiError> {
        let mut r = build(&self.cfg, &self.model, &self.input, max_images, self.is_swapped)?;
        if let Some(s) = &self.swapped {
            r.swapped = Some(Box::new(s.with_max_images(max_images)?));
        }
        Ok(r)
    }

    /// What was sent, without pixels, for the result card.
    pub fn record(&self) -> SentRecord {
        SentRecord {
            destination: self.destination.clone(),
            model: self.model.clone(),
            preset: self.preset_name.clone(),
            images: self
                .images
                .iter()
                .map(|i| SentImageRecord { description: i.description.clone(), width: i.width, height: i.height, bytes: i.bytes, mime: i.mime.to_string() })
                .collect(),
            system_prompt: self.system_prompt.clone(),
            user_prompt: self.user_prompt.clone(),
            changes: Vec::new(),
        }
    }
}

/// What a request sent, stored with the result (locally only).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SentRecord {
    pub destination: String,
    pub model: String,
    pub preset: String,
    pub images: Vec<SentImageRecord>,
    pub system_prompt: String,
    pub user_prompt: String,
    /// Automatic changes made while sending (composite, no response_format...).
    pub changes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SentImageRecord {
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    pub mime: String,
}

/// Prepares a request for the send preview. Nothing is sent.
pub fn prepare(settings: &AiSettings, model: &str, input: RequestInput) -> Result<PreparedRequest, AiError> {
    let model = model.trim();
    if model.is_empty() {
        return Err(AiError::Invalid("Pick a model first (Test connection lists them)".into()));
    }
    let p = &input.preset;
    if p.arity == Arity::Two && input.b.is_none() {
        return Err(AiError::Invalid(format!("{} needs two images: open A and B in compare mode", p.name)));
    }
    if p.output == OutputKind::Markdown && p.builtin.is_some() && input.question.trim().is_empty() {
        return Err(AiError::Invalid("Type a question for the free-form preset".into()));
    }
    let (aw, ah) = input.a.pixels.dimensions();
    if aw == 0 || ah == 0 {
        return Err(AiError::Invalid("The image is empty".into()));
    }
    if let Some(r) = input.roi {
        if r.clamp_to(aw, ah).is_none() {
            return Err(AiError::Invalid("The ROI is outside the image".into()));
        }
    }
    let ep = settings.endpoint()?;
    let server_key = ep.server_key();
    let cfg = BuildConfig {
        destination: ep.api("/chat/completions"),
        server: ep.display(),
        max_side: settings.max_side_overview.clamp(256, 4096),
        tile_side: settings.tile_side.clamp(256, 2048),
        max_images: settings.effective_max_images(&server_key, model),
        server_key,
        encoding: settings.encoding,
        conv: settings.coord_convention,
        disable_thinking: settings.disable_thinking,
        temperature: settings.temperature.clamp(0.0, 2.0),
        max_completion_tokens: settings.max_completion_tokens.clamp(16, 32768),
        include_filename: settings.include_filename,
        include_exif: settings.include_exif,
        blind: settings.blind,
        swap_check: settings.swap_check,
    };
    let input = Arc::new(input);
    let mut req = build(&cfg, model, &input, cfg.max_images, false)?;
    if cfg.swap_check && input.preset.supports_swap_check() && input.b.is_some() {
        req.swapped = Some(Box::new(build(&cfg, model, &input, cfg.max_images, true)?));
        req.notes.push("Swap-check: the comparison runs twice, the second time with A and B exchanged.".into());
    }
    Ok(req)
}

fn roi_for(roi: Option<Roi>, from: (u32, u32), img: &RgbaImage) -> Option<Roi> {
    let r = roi?.clamp_to(from.0, from.1)?;
    if img.dimensions() == from {
        return Some(r);
    }
    let f = r.to_f().rescaled(from, img.dimensions());
    Roi { x: f.x.round() as u32, y: f.y.round() as u32, w: (f.w.round() as u32).max(1), h: (f.h.round() as u32).max(1) }
        .clamp_to(img.width(), img.height())
}

fn part_text(img: &SentImage, roles: (Option<&str>, Option<&str>)) -> String {
    match img.label {
        Some(l) if img.role == ImageRole::Overview => {
            let role = match l {
                ImageLabel::A => roles.0,
                ImageLabel::B => roles.1,
            };
            let rest = img.description.strip_prefix(&format!("{} ", l.as_str())).unwrap_or(&img.description);
            match role {
                Some(r) => format!("Image {} ({r}): {rest}.", l.as_str()),
                None => format!("Image {}: {rest}.", l.as_str()),
            }
        }
        Some(_) => format!("Image {}.", img.description),
        None => format!("{}.", img.description),
    }
}

pub(crate) fn build(cfg: &BuildConfig, model: &str, input: &Arc<RequestInput>, max_images: u32, swapped: bool) -> Result<PreparedRequest, AiError> {
    let preset = &input.preset;
    let quick = preset.output == OutputKind::QuickScore;
    let has_pair = input.b.is_some() && (preset.arity == Arity::Two || preset.output == OutputKind::Markdown);
    let pair = has_pair && !quick;
    let max_images = max_images.max(1);
    let a_size = input.a.pixels.dimensions();
    // Presented order: the swapped pass shows B as A.
    let (first, second) = match (&input.b, swapped && pair) {
        (Some(b), true) => (b, Some(&input.a)),
        (b, _) => (&input.a, if pair { b.as_ref() } else { None }),
    };
    let mut notes = Vec::new();
    let roi_first = roi_for(input.roi, a_size, &first.pixels);
    let composite = pair && (input.layout == AbLayout::Composite || max_images < 2);

    let sent: Vec<SentImage> = if quick {
        match roi_first {
            Some(r) => images::roi_tiles(&first.pixels, ImageLabel::A, r, cfg.tile_side, 1),
            None => vec![images::overview(&first.pixels, ImageLabel::A, cfg.max_side)],
        }
    } else if composite {
        let second = second.expect("pair");
        let full = |i: &InputImage| Roi::full(i.pixels.width(), i.pixels.height());
        let ra = roi_first.unwrap_or_else(|| full(first)).to_f();
        let rb = roi_for(input.roi, a_size, &second.pixels).unwrap_or_else(|| full(second)).to_f();
        // Labels are as presented; a swapped report is unswapped later.
        let c = images::composite(&first.pixels, ra, &second.pixels, rb, cfg.max_side);
        if input.layout != AbLayout::Composite {
            notes.push(format!("The server accepts {max_images} image(s) per request, so A and B are sent as one side-by-side composite (half resolution each)."));
        }
        vec![c]
    } else {
        let sides = if pair { 2 } else { 1 };
        let mut with_overview = input.include_overview || roi_first.is_none();
        let mut per_side = match roi_first {
            Some(_) => max_images.saturating_sub(if with_overview { sides } else { 0 }) / sides,
            None => 0,
        };
        if roi_first.is_some() && per_side == 0 {
            // Not enough images for both: the ROI matters more.
            with_overview = false;
            per_side = (max_images / sides).max(1);
            notes.push("The full view was left out to fit the server's image limit.".into());
        }
        let mut a_list = Vec::new();
        if with_overview {
            a_list.push(images::overview(&first.pixels, ImageLabel::A, cfg.max_side));
        }
        if let Some(r) = roi_first {
            a_list.extend(images::roi_tiles(&first.pixels, ImageLabel::A, r, cfg.tile_side, per_side));
        }
        match second {
            Some(b) => {
                if b.pixels.dimensions() != first.pixels.dimensions() {
                    notes.push(format!(
                        "A is {}x{} and B is {}x{}: B is resampled to A's sent sizes over the same relative region.",
                        first.pixels.width(),
                        first.pixels.height(),
                        b.pixels.width(),
                        b.pixels.height()
                    ));
                }
                let b_list = images::matching(&b.pixels, &a_list, first.pixels.dimensions());
                // Interleave: A1, B1, A2, B2... so each pair is adjacent.
                a_list.into_iter().zip(b_list).flat_map(|(x, y)| [x, y]).collect()
            }
            None => a_list,
        }
    };
    if sent.is_empty() {
        return Err(AiError::Invalid("Nothing to send: the ROI is empty".into()));
    }
    if sent.iter().any(|s| matches!(s.role, ImageRole::RoiTile { .. }) && s.regions[0].downscale() > 1.0 + 1e-9) {
        notes.push("The ROI is larger than the tile budget allows, so its tiles are downscaled (see each tile's text).".into());
    }

    // Encode in parallel; PNG of a 1536 px view takes tens of milliseconds.
    let encoded: Vec<Result<(images::Encoded, bool), AiError>> = std::thread::scope(|s| {
        let handles: Vec<_> = sent.iter().map(|img| s.spawn(|| images::encode(&img.pixels, cfg.encoding))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(AiError::Image("encoder panicked".into())))).collect()
    });
    let mut previews = Vec::new();
    let mut parts = Vec::new();
    let mut user_prompt = String::new();
    let mut flattened = false;
    let roles = if swapped || (cfg.swap_check && preset.supports_swap_check()) { (None, None) } else { preset.roles };
    let push_text = |parts: &mut Vec<Part>, user_prompt: &mut String, t: String| {
        if !user_prompt.is_empty() {
            user_prompt.push('\n');
        }
        user_prompt.push_str(&t);
        parts.push(Part::Text(t));
    };

    // Text before the first image: file names and capture settings.
    let mut intro = Vec::new();
    for (img, label) in [(Some(first), "A"), (second, "B")] {
        let Some(img) = img else { continue };
        if cfg.include_filename {
            if let Some(n) = &img.file_name {
                intro.push(format!("Image {label} file name: {n}."));
            }
        }
        if cfg.include_exif {
            if let Some(c) = &img.capture {
                intro.push(format!("Image {label} capture settings: {c}."));
            }
        }
    }
    let mut first_text = intro.join(" ");

    for (i, (img, enc)) in sent.iter().zip(encoded).enumerate() {
        let (enc, alpha) = enc?;
        flattened |= alpha;
        let mut t = if quick { String::new() } else { part_text(img, roles) };
        if i == 0 && !first_text.is_empty() {
            t = format!("{} {t}", std::mem::take(&mut first_text)).trim().to_string();
        }
        if !t.is_empty() {
            push_text(&mut parts, &mut user_prompt, t);
        }
        user_prompt.push_str(&format!("\n[Image {}: {}, {} KB]", i + 1, img.description, enc.bytes.len().div_ceil(1024)));
        previews.push(PreviewImage {
            thumbnail: images::thumbnail(&img.pixels),
            width: img.pixels.width(),
            height: img.pixels.height(),
            bytes: enc.bytes.len(),
            mime: enc.mime,
            description: img.description.clone(),
        });
        parts.push(Part::Image(enc.data_url()));
    }
    if flattened {
        notes.push("Transparent areas are shown over mid gray.".into());
    }

    // The closing text: context, metrics, task.
    let mut tail = Vec::new();
    if quick {
        tail.push(prompts::QUICK_SCORE_QUESTION.to_string());
    } else {
        let (fa, fb) = (first.frame.as_ref(), second.and_then(|s| s.frame.as_ref()));
        if let Some(f) = prompts::frame_context(fa, fb, pair) {
            tail.push(f);
        } else if preset.video {
            tail.push(if pair {
                "A and B are the same still frame from two versions of a video.".to_string()
            } else {
                "Image A is a still frame from a video.".to_string()
            });
        }
        if composite {
            tail.push("A and B are shown side by side in one composite image: A on the left, B on the right.".into());
        }
        if roles == (None, None) && pair && preset.supports_swap_check() {
            tail.push("Neither image is a reference here: judge A and B as two candidates.".into());
        }
        let metrics = if cfg.blind {
            None
        } else {
            prompts::metric_context(&input.metric_scope, &input.metrics)
        };
        let task = preset.task.trim();
        let question = input.question.trim();
        if task.contains("{metrics}") {
            tail.push(task.replace("{metrics}", metrics.as_deref().unwrap_or("")).trim().to_string());
        } else {
            if let Some(m) = metrics {
                tail.push(m);
            }
            match preset.output {
                OutputKind::Markdown => {
                    if !question.is_empty() {
                        tail.push(format!("Question: {question}"));
                    }
                    if preset.builtin.is_none() && !task.is_empty() {
                        tail.push(task.to_string());
                    } else if question.is_empty() {
                        tail.push(format!("Task: {task}"));
                    }
                }
                _ => {
                    if !question.is_empty() {
                        tail.push(format!("Additional instructions: {question}"));
                    }
                    if !task.is_empty() {
                        tail.push(format!("Task: {task}"));
                    }
                }
            }
        }
    }
    let tail = tail.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
    if !tail.is_empty() {
        push_text(&mut parts, &mut user_prompt, tail);
    }

    let max_sent_side = sent.iter().map(|s| s.pixels.width().max(s.pixels.height())).max().unwrap_or(1000);
    let max_coord = match cfg.conv {
        CoordConvention::Relative1000 => 1000,
        CoordConvention::AbsolutePixels => max_sent_side,
    };
    let schema = match preset.output {
        OutputKind::QualityJson => Some(prompts::quality_schema(if pair { &["A", "B"] } else { &["A"] }, max_coord)),
        OutputKind::ComparisonJson => Some(prompts::comparison_schema(max_coord)),
        _ => None,
    };
    if !cfg.disable_thinking && !quick {
        notes.push("Thinking is on: reasoning counts toward max tokens and takes longer.".into());
    }
    let system_prompt = preset.system_prompt(cfg.conv, None);
    let dims: Vec<(u32, u32)> = sent.iter().map(|s| s.pixels.dimensions()).collect();
    let text_chars = system_prompt.len() + parts.iter().map(|p| if let Part::Text(t) = p { t.len() } else { 0 }).sum::<usize>();
    let total_bytes = previews.iter().map(|p| p.bytes).sum();
    Ok(PreparedRequest {
        destination: cfg.destination.clone(),
        server: cfg.server.clone(),
        server_key: cfg.server_key.clone(),
        model: model.to_string(),
        preset_id: preset.id.clone(),
        preset_name: preset.name.clone(),
        output: preset.output,
        system_prompt,
        user_prompt,
        images: previews,
        total_bytes,
        estimated_prompt_tokens: estimate_prompt_tokens(&dims, text_chars),
        max_completion_tokens: if quick { 1 } else { cfg.max_completion_tokens },
        notes,
        swapped: None,
        layout: sent.iter().map(|s| s.layout()).collect(),
        is_swapped: swapped && pair,
        parts,
        schema,
        cfg: cfg.clone(),
        input: input.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts::PresetId;
    use image::Rgba;

    fn img(w: u32, h: u32, seed: u8) -> Arc<RgbaImage> {
        Arc::new(RgbaImage::from_fn(w, h, |x, y| Rgba([(x as u8).wrapping_add(seed), y as u8, seed, 255])))
    }

    fn settings() -> AiSettings {
        AiSettings { host: "192.168.1.20".into(), ..Default::default() }
    }

    fn body(r: &PreparedRequest, mode: BodyMode) -> Value {
        serde_json::from_slice(&r.body(mode)).unwrap()
    }

    #[test]
    fn single_image_request_shape() {
        let input = RequestInput::new(Preset::builtin(PresetId::Quality), InputImage::new(img(3840, 2160, 0)).with_file_name("secret.jpg"))
            .with_metrics("A", [("Sharpness (Laplacian var)", 512.3, ""), ("Noise sigma Y", 1.84, "")]);
        let r = prepare(&settings(), "Qwen/Qwen3.5-9B", input).unwrap();
        assert_eq!(r.destination, "http://192.168.1.20:8000/v1/chat/completions");
        assert_eq!(r.images.len(), 1);
        assert_eq!((r.images[0].width, r.images[0].height), (1536, 864));
        assert!(r.images[0].thumbnail.width() <= 256);
        assert!(!r.user_prompt.contains("secret.jpg"), "file names are off by default");
        assert!(r.user_prompt.contains("Image A: full view, 1536x864, downscaled 2.5x from 3840x2160."));
        assert!(r.user_prompt.contains("Objective metrics (Lumen, A): Sharpness (Laplacian var) 512.3, Noise sigma Y 1.84."));
        let b = body(&r, r.default_mode());
        assert_eq!(b["model"], "Qwen/Qwen3.5-9B");
        assert_eq!(b["stream"], true);
        assert_eq!(b["stream_options"]["include_usage"], true);
        assert_eq!(b["max_completion_tokens"], 1500);
        assert!(b.get("max_tokens").is_none());
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(b["response_format"]["type"], "json_schema");
        assert_eq!(b["response_format"]["json_schema"]["name"], "lumen_quality_report");
        assert_eq!(b["messages"][0]["role"], "system");
        let content = b["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "image_url");
        assert!(content[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert!(content.last().unwrap()["text"].as_str().unwrap().ends_with("Task: Assess the technical quality of image A and return the quality report."));
        // Fallbacks: schema in the prompt, no vLLM extensions.
        let f = body(&r, BodyMode { response_format: false, extensions: false, stream: true });
        assert!(f.get("response_format").is_none() && f.get("chat_template_kwargs").is_none());
        assert!(f["messages"][0]["content"].as_str().unwrap().contains("JSON Schema"));
    }

    #[test]
    fn privacy_toggles_and_blind() {
        let mut s = settings();
        s.include_filename = true;
        s.include_exif = true;
        s.blind = true;
        let input = RequestInput::new(
            Preset::builtin(PresetId::Photo),
            InputImage::new(img(800, 600, 0)).with_file_name("IMG_1.jpg").with_capture("Canon R5, 50 mm, f/1.8, 1/200 s, ISO 3200"),
        )
        .with_metrics("A", [("x", 1.0, "")]);
        let r = prepare(&s, "m", input).unwrap();
        assert!(r.user_prompt.starts_with("Image A file name: IMG_1.jpg. Image A capture settings: Canon R5"));
        assert!(!r.user_prompt.contains("Objective metrics"), "blind drops metrics");
        assert!(r.user_prompt.contains("native resolution"));
    }

    #[test]
    fn pair_with_roi_interleaves_and_budgets() {
        let input = RequestInput::new(Preset::builtin(PresetId::Encode), InputImage::new(img(3840, 2160, 0)))
            .with_b(InputImage::new(img(3840, 2160, 9)))
            .with_roi(Roi { x: 1200, y: 400, w: 2000, h: 1000 });
        let r = prepare(&settings(), "m", input).unwrap();
        // 6 images: 2 overviews + 2 tiles per side.
        let d: Vec<&str> = r.images.iter().map(|i| i.description.as_str()).collect();
        assert_eq!(r.images.len(), 6, "{d:?}");
        assert!(d[0].starts_with("A full view") && d[1].starts_with("B full view"));
        assert!(d[2].starts_with("A ROI tile 1/2") && d[3].starts_with("B ROI tile 1/2") && d[4].starts_with("A ROI tile 2/2"));
        assert!(r.user_prompt.contains("Image A (reference): full view"));
        assert!(r.user_prompt.contains("Image B (encode): full view, same scaling."));
        assert_eq!(r.layout.len(), 6);
        assert_eq!(r.layout[3].label, Some(ImageLabel::B));
        let b = body(&r, r.default_mode());
        assert_eq!(b["response_format"]["json_schema"]["name"], "lumen_comparison_report");
        // An image limit of 1: one composite.
        let one = r.with_max_images(1).unwrap();
        assert_eq!(one.images.len(), 1);
        assert_eq!(one.layout[0].label, None);
        assert!(one.user_prompt.contains("side by side"));
        // A limit of 2 with a ROI: tiles only.
        let two = r.with_max_images(2).unwrap();
        assert_eq!(two.images.len(), 2);
        assert!(two.images[0].description.starts_with("A ROI tile 1/1"));
    }

    #[test]
    fn swap_check_builds_second_pass() {
        let mut s = settings();
        s.swap_check = true;
        let input = RequestInput::new(Preset::builtin(PresetId::Compare), InputImage::new(img(640, 480, 0)))
            .with_b(InputImage::new(img(640, 480, 50)));
        let r = prepare(&s, "m", input).unwrap();
        let sw = r.swapped.as_ref().unwrap();
        assert!(sw.is_swapped && !r.is_swapped);
        assert!(r.user_prompt.contains("Neither image is a reference"));
        // The swapped pass shows original B first, labeled A.
        assert_eq!(sw.images[0].thumbnail, r.images[1].thumbnail);
        assert!(sw.images[0].description.starts_with("A full view"));
    }

    #[test]
    fn video_frames_and_freeform() {
        let f = FrameRef { timestamp_s: Some(83.456), frame_index: Some(2001) };
        let input = RequestInput::new(Preset::builtin(PresetId::VideoCompare), InputImage::new(img(1920, 1080, 0)).with_frame(f.clone()))
            .with_b(InputImage::new(img(1920, 1080, 3)).with_frame(f));
        let r = prepare(&settings(), "m", input).unwrap();
        assert!(r.user_prompt.contains("A and B are the same still frame of a video, taken at 00:01:23.456 (frame 2001)"));
        assert!(r.system_prompt.contains("paused from moving video"));

        let ff = RequestInput::new(Preset::builtin(PresetId::FreeForm), InputImage::new(img(64, 64, 0)));
        assert!(matches!(prepare(&settings(), "m", ff.clone()), Err(AiError::Invalid(_))));
        let r = prepare(&settings(), "m", ff.with_question("Is the sky banded?")).unwrap();
        assert!(r.user_prompt.ends_with("Question: Is the sky banded?"));
        assert!(r.schema.is_none());
        assert!(body(&r, r.default_mode()).get("response_format").is_none());
    }

    #[test]
    fn quick_score_body() {
        let input = RequestInput::new(Preset::builtin(PresetId::QuickScore), InputImage::new(img(800, 600, 0)));
        let r = prepare(&settings(), "q-future/Q-ReAlign-Lite-4B", input).unwrap();
        let b = body(&r, r.default_mode());
        assert_eq!(b["stream"], false);
        assert_eq!(b["logprobs"], true);
        assert_eq!(b["top_logprobs"], 20);
        assert_eq!(b["continue_final_message"], true);
        assert_eq!(b["add_generation_prompt"], false);
        assert_eq!(b["max_completion_tokens"], 1);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m.len(), 2, "no system prompt");
        assert_eq!(m[1]["content"], "The quality of the image is");
        assert_eq!(m[0]["content"][1]["text"], "How would you rate the quality of this image?");
    }

    #[test]
    fn rejects_bad_input() {
        let compare = RequestInput::new(Preset::builtin(PresetId::Compare), InputImage::new(img(64, 64, 0)));
        assert!(prepare(&settings(), "m", compare).is_err(), "needs B");
        let q = RequestInput::new(Preset::builtin(PresetId::Quality), InputImage::new(img(64, 64, 0)));
        assert!(prepare(&settings(), " ", q.clone()).is_err(), "needs a model");
        assert!(prepare(&settings(), "m", q.with_roi(Roi { x: 100, y: 100, w: 5, h: 5 })).is_err());
    }
}
