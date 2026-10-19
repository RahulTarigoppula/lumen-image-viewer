//! Presets, system prompts, JSON schemas and the text parts of the user
//! message (metric context, frame context, location rules).
//!
//! Prompts follow the same pattern: a role, what to judge and what not to,
//! a rubric with anchors, location rules for findings, honesty rules, and
//! how to use the objective metrics. Scores are 1..10 so the anchors fit
//! one line each; low temperature keeps them repeatable.

use serde_json::{json, Value};

use crate::settings::{CoordConvention, CustomPreset};

/// Built-in presets. Stable ids are stored in settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresetId {
    Quality,
    Compare,
    Encode,
    Photo,
    FreeForm,
    QuickScore,
    VideoFrame,
    VideoCompare,
}

impl PresetId {
    pub const ALL: [PresetId; 8] = [
        PresetId::Quality,
        PresetId::Compare,
        PresetId::Encode,
        PresetId::Photo,
        PresetId::FreeForm,
        PresetId::QuickScore,
        PresetId::VideoFrame,
        PresetId::VideoCompare,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PresetId::Quality => "quality",
            PresetId::Compare => "compare",
            PresetId::Encode => "encode",
            PresetId::Photo => "photo",
            PresetId::FreeForm => "freeform",
            PresetId::QuickScore => "quick_score",
            PresetId::VideoFrame => "video_frame",
            PresetId::VideoCompare => "video_compare",
        }
    }

    pub fn parse(s: &str) -> Option<PresetId> {
        PresetId::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

/// How many images a preset takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arity {
    One,
    Two,
}

/// What the model is asked to return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// The strict quality report (`quality_schema`).
    QualityJson,
    /// The comparison report (`comparison_schema`).
    ComparisonJson,
    /// Free text, rendered as Markdown.
    Markdown,
    /// One token with logprobs (Q-ReAlign).
    QuickScore,
}

impl OutputKind {
    pub fn is_json(self) -> bool {
        matches!(self, OutputKind::QualityJson | OutputKind::ComparisonJson)
    }
}

/// A resolved preset: everything needed to build the prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    /// Built-in id or `custom:<name>`.
    pub id: String,
    pub builtin: Option<PresetId>,
    pub name: String,
    /// One line for the preset menu.
    pub description: String,
    pub arity: Arity,
    pub output: OutputKind,
    /// Frames come from video (changes prompt wording).
    pub video: bool,
    /// Role-specific system prompt, before the shared rules.
    pub system: String,
    /// The task sentence at the end of the user message.
    pub task: String,
    /// Names for A and B in the text parts ("reference", "encode").
    pub roles: (Option<&'static str>, Option<&'static str>),
}

const ROLE_ANALYST: &str = "You are an expert image quality analyst with experience in broadcast quality control, video encoding and professional photo editing.";

const ROLE_VIDEO: &str = "You are a video quality engineer reviewing still frames extracted from video, with experience in broadcast quality control and codec tuning.";

const RUBRIC: &str = "Score each category from 1 (unacceptable) to 10 (flawless, indistinguishable from a pristine source) using these anchors:
- 9-10: no visible defects at 100% zoom.
- 7-8: minor defects visible only on close inspection.
- 5-6: clearly visible defects that do not ruin the image.
- 3-4: distracting defects.
- 1-2: severe defects; the image is unusable for its purpose.
The overall score is your holistic judgement, not an average; one severe defect can dominate it.";

const CATEGORIES_TEXT: &str = "Categories:
- sharpness_detail: focus accuracy, resolution, fine texture (hair, foliage, fabric, text), oversharpening halos.
- noise: luma and chroma grain, fixed-pattern noise, and smearing or plastic skin from noise reduction.
- compression_artifacts: blocking, ringing, mosquito noise around edges, banding and posterization in gradients, chroma bleeding.
- color: accuracy, color casts, white balance, saturation, consistency of the grade.
- exposure_dynamic_range: clipped highlights, crushed shadows, flat or excessive contrast, tone-mapping halos.
- detail_vs_reference: only when a reference image is given (detail kept or lost relative to it); otherwise null.";

const VIDEO_NOTES: &str = "These are single frames paused from moving video. Motion blur on moving objects and gentle softness from camera motion are normal at playback speed: do not score them as focus defects unless they look wrong (for example a static background blurred, or ghosting from bad deinterlacing). Look for codec artifacts that are typical of video: macroblocking in dark and fast-moving areas, banding in skies and fades, mosquito noise around titles and edges, texture smeared by deblocking or denoising, interlacing combs, scaling aliasing, and limited/full range mistakes (gray blacks or crushed shadows).";

const HONESTY: &str = "Rules:
- Judge only what is visible in the provided pixels. Do not guess about content you cannot see.
- Do not invent defects. If you are unsure, say so in the finding and use severity minor.
- Intentional choices (shallow depth of field, added film grain, low-key lighting, stylized grading) are not defects unless they look like mistakes.
- Downscaled views hide noise and fine detail: judge sharpness, noise and compression mainly from the native-resolution tiles when they are provided, and say when only a downscaled view was available.
- Keep rationales short and concrete (what, where, how bad).";

const METRICS_RULE: &str = "Objective metrics computed by software may be provided as context. Use them to check your judgement, not to replace it, and mention when what you see disagrees with them.";

fn location_rules(conv: CoordConvention) -> String {
    let coords = match conv {
        CoordConvention::Relative1000 => "integer coordinates from 0 to 1000 relative to the width and height of that image as you see it ([0, 0] is the top-left corner, [1000, 1000] the bottom-right)",
        CoordConvention::AbsolutePixels => "integer pixel coordinates of that image as you see it ([0, 0] is the top-left corner)",
    };
    format!(
        "Locations: every finding names the image (\"A\" or \"B\"), the tile number (0 for a full view or a composite, otherwise the ROI tile number given in the text before that image), and a bounding box [x0, y0, x1, y1] in {coords}. Draw the box tightly around the defect. When A and B are shown in one composite image, use tile 0 and give the box relative to the whole composite."
    )
}

impl Preset {
    pub fn builtin(id: PresetId) -> Preset {
        let p = |name: &str, description: &str, arity, output, video, system: String, task: &str, roles| Preset {
            id: id.as_str().to_string(),
            builtin: Some(id),
            name: name.to_string(),
            description: description.to_string(),
            arity,
            output,
            video,
            system,
            task: task.to_string(),
            roles,
        };
        match id {
            PresetId::Quality => p(
                "Quality critique",
                "Technical quality of one image, scored per category",
                Arity::One,
                OutputKind::QualityJson,
                false,
                format!(
                    "{ROLE_ANALYST}\nAssess the technical quality of the image: how well it was captured, processed and encoded, not whether the subject or composition is pleasing."
                ),
                "Assess the technical quality of image A and return the quality report.",
                (None, None),
            ),
            PresetId::Compare => p(
                "A vs B comparison",
                "Which of two versions looks better, and where they differ",
                Arity::Two,
                OutputKind::ComparisonJson,
                false,
                format!(
                    "{ROLE_ANALYST}\nYou compare two versions of the same image. A is the reference unless stated otherwise. Describe differences, not absolute quality: point to regions where B lost detail or gained artifacts, or where B is better than A. Prefer the version a careful viewer would choose at 100% zoom; answer tie when the differences are invisible at normal viewing. Confidence is your probability (0 to 1) that the preferred version is really better; use about 0.5 for a tie or when unsure."
                ),
                "Compare B with A and return the comparison report.",
                (Some("reference"), Some("test")),
            ),
            PresetId::Encode => p(
                "Encode review",
                "B is an encode of reference A: artifacts and detail loss",
                Arity::Two,
                OutputKind::ComparisonJson,
                false,
                format!(
                    "{ROLE_ANALYST}\nB is a lossy encode (or transcode, resize or re-export) of the reference A. Review the encode: where B lost detail (smeared texture, softened edges, missing grain), and which artifacts it gained (blocking, ringing, mosquito noise, banding, posterization, chroma bleeding or subsampling blur on saturated edges and text, color or gamma shifts, limited/full range errors). A difference that exists in A too is not an encode defect. Judge whether the encode is visually transparent: preferred is A when B shows visible losses, tie when they are indistinguishable."
                ),
                "Review B as an encode of the reference A and return the comparison report.",
                (Some("reference"), Some("encode")),
            ),
            PresetId::Photo => p(
                "Photo technical review",
                "Exposure, focus, noise and color of a camera image",
                Arity::One,
                OutputKind::QualityJson,
                false,
                format!(
                    "{ROLE_ANALYST}\nReview this camera photograph technically, as a photo editor deciding whether it is usable. Focus: is the intended subject sharp, and is softness missed focus, subject motion or camera shake? Noise: luminance and chroma noise for the apparent ISO, and over-aggressive noise reduction. Exposure: clipped highlights (skies, skin, specular areas), blocked shadows, overall brightness and contrast. Color: white balance, casts, skin tones, saturation. Optics and processing: chromatic aberration on high-contrast edges, vignetting, distortion, sharpening halos, HDR halos. If capture settings are given, use them to set expectations (for example noise at high ISO, blur at slow shutter speeds)."
                ),
                "Review photo A technically and return the quality report.",
                (None, None),
            ),
            PresetId::FreeForm => p(
                "Free-form",
                "Your own question, answered in Markdown",
                Arity::One,
                OutputKind::Markdown,
                false,
                format!(
                    "{ROLE_ANALYST}\nAnswer the user's question about the provided image or images in concise Markdown. Be concrete: refer to locations (\"upper left\", \"the face\", \"tile 2\") and to the image labels (A, B). If something cannot be judged from the pixels provided, say so."
                ),
                "Answer the question above.",
                (None, None),
            ),
            PresetId::QuickScore => p(
                "Quick score",
                "One MOS-like score from an IQA-tuned model (Q-ReAlign)",
                Arity::One,
                OutputKind::QuickScore,
                false,
                String::new(),
                QUICK_SCORE_QUESTION,
                (None, None),
            ),
            PresetId::VideoFrame => p(
                "Video frame quality",
                "Technical quality of a frame from a video",
                Arity::One,
                OutputKind::QualityJson,
                true,
                format!("{ROLE_VIDEO}\nAssess the technical quality of the frame as delivered video: capture, processing and encoding.\n{VIDEO_NOTES}"),
                "Assess the technical quality of video frame A and return the quality report.",
                (None, None),
            ),
            PresetId::VideoCompare => p(
                "Video A vs B frame",
                "The same video frame from two encodes or versions",
                Arity::Two,
                OutputKind::ComparisonJson,
                true,
                format!(
                    "{ROLE_VIDEO}\nYou compare the same frame from two versions of a video (for example two encodes, or an encode and its source). A is the reference unless stated otherwise. Describe differences, not absolute quality: where B lost detail or gained artifacts, or where it is better. Answer tie when the differences would be invisible at playback.\n{VIDEO_NOTES}"
                ),
                "Compare frame B with frame A and return the comparison report.",
                (Some("reference"), Some("test")),
            ),
        }
    }

    /// A preset from the settings' `custom_presets`.
    pub fn from_custom(c: &CustomPreset) -> Preset {
        let arity = if c.pair { Arity::Two } else { Arity::One };
        let output = match (c.structured, c.pair) {
            (false, _) => OutputKind::Markdown,
            (true, false) => OutputKind::QualityJson,
            (true, true) => OutputKind::ComparisonJson,
        };
        Preset {
            id: format!("custom:{}", c.name),
            builtin: None,
            name: c.name.clone(),
            description: "Custom preset".into(),
            arity,
            output,
            video: false,
            system: if c.system_prompt.trim().is_empty() { ROLE_ANALYST.to_string() } else { c.system_prompt.clone() },
            task: c.user_template.clone(),
            roles: if c.pair { (Some("reference"), Some("test")) } else { (None, None) },
        }
    }

    /// Whether the A/B swap-check applies (comparisons without a fixed
    /// reference role).
    pub fn supports_swap_check(&self) -> bool {
        matches!(self.builtin, Some(PresetId::Compare) | Some(PresetId::VideoCompare))
    }

    /// The full system prompt. `schema_in_prompt` adds the JSON schema as
    /// text, for servers that reject `response_format`.
    pub fn system_prompt(&self, conv: CoordConvention, schema_in_prompt: Option<&Value>) -> String {
        if self.output == OutputKind::QuickScore {
            return String::new();
        }
        let mut s = self.system.clone();
        if self.output.is_json() {
            s.push_str("\n\n");
            s.push_str(RUBRIC);
            s.push_str("\n\n");
            s.push_str(CATEGORIES_TEXT);
            if self.output == OutputKind::ComparisonJson {
                s.push_str("\nGive scores_a and scores_b for the same categories; detail_vs_reference is null for the reference itself.");
            }
            s.push_str("\n\n");
            s.push_str(&location_rules(conv));
        }
        s.push_str("\n\n");
        s.push_str(HONESTY);
        s.push_str("\n\n");
        s.push_str(METRICS_RULE);
        if let Some(schema) = schema_in_prompt {
            s.push_str("\n\nRespond with one JSON object and nothing else: no Markdown code fences, no text before or after it. It must follow this JSON Schema:\n");
            s.push_str(&serde_json::to_string(schema).unwrap_or_default());
        } else if self.output.is_json() {
            s.push_str("\n\nRespond with the JSON report only.");
        }
        s
    }
}

/// All presets: built-ins, then custom ones.
pub fn all_presets(custom: &[CustomPreset]) -> Vec<Preset> {
    PresetId::ALL.into_iter().map(Preset::builtin).chain(custom.iter().map(Preset::from_custom)).collect()
}

/// Finds a preset by id (`quality`, `custom:My preset`).
pub fn find_preset(id: &str, custom: &[CustomPreset]) -> Option<Preset> {
    if let Some(p) = PresetId::parse(id) {
        return Some(Preset::builtin(p));
    }
    let name = id.strip_prefix("custom:")?;
    custom.iter().find(|c| c.name == name).map(Preset::from_custom)
}

pub const QUICK_SCORE_QUESTION: &str = "How would you rate the quality of this image?";
pub const QUICK_SCORE_PREFIX: &str = "The quality of the image is";
/// Q-ReAlign rating words and their weights.
pub const QUICK_SCORE_LEVELS: [(&str, f64); 5] =
    [("excellent", 1.0), ("good", 0.75), ("fair", 0.5), ("poor", 0.25), ("bad", 0.0)];

/// Category keys, in report order, with display names.
pub const CATEGORIES: [(&str, &str); 6] = [
    ("sharpness_detail", "Sharpness and detail"),
    ("noise", "Noise"),
    ("compression_artifacts", "Compression artifacts"),
    ("color", "Color"),
    ("exposure_dynamic_range", "Exposure and dynamic range"),
    ("detail_vs_reference", "Detail vs reference"),
];

fn defs() -> Value {
    json!({"cat": {"type": "object", "additionalProperties": false, "required": ["score", "rationale"],
        "properties": {"score": {"type": "integer", "minimum": 1, "maximum": 10},
                       "rationale": {"type": "string", "maxLength": 300}}}})
}

fn categories_schema() -> Value {
    let keys: Vec<&str> = CATEGORIES.iter().map(|c| c.0).collect();
    let mut props = serde_json::Map::new();
    for k in &keys[..5] {
        props.insert(k.to_string(), json!({"$ref": "#/$defs/cat"}));
    }
    props.insert("detail_vs_reference".into(), json!({"anyOf": [{"$ref": "#/$defs/cat"}, {"type": "null"}]}));
    json!({"type": "object", "additionalProperties": false, "required": keys, "properties": props})
}

fn findings_schema(labels: &[&str], max_coord: u32) -> Value {
    let keys: Vec<&str> = CATEGORIES.iter().map(|c| c.0).collect();
    json!({"type": "array", "maxItems": 20, "items": {"type": "object", "additionalProperties": false,
        "required": ["category", "severity", "image", "tile", "bbox", "description"],
        "properties": {
            "category": {"enum": keys},
            "severity": {"enum": ["minor", "moderate", "severe"]},
            "image": {"enum": labels},
            "tile": {"type": "integer", "minimum": 0},
            "bbox": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": max_coord}, "minItems": 4, "maxItems": 4},
            "description": {"type": "string", "maxLength": 300}}}})
}

/// The strict quality report schema (plan 3.8). `labels` is `["A"]` for
/// one image or `["A", "B"]`; `max_coord` is 1000, or the largest sent
/// side for absolute pixel coordinates.
pub fn quality_schema(labels: &[&str], max_coord: u32) -> Value {
    json!({"type": "object", "additionalProperties": false,
        "required": ["overall", "categories", "findings", "summary"],
        "properties": {
            "overall": {"type": "integer", "minimum": 1, "maximum": 10},
            "categories": categories_schema(),
            "findings": findings_schema(labels, max_coord),
            "summary": {"type": "string", "maxLength": 800}},
        "$defs": defs()})
}

/// The comparison schema: the quality report extended with `preferred`,
/// `confidence` and per-image scores.
pub fn comparison_schema(max_coord: u32) -> Value {
    json!({"type": "object", "additionalProperties": false,
        "required": ["preferred", "confidence", "overall_a", "overall_b", "scores_a", "scores_b", "findings", "summary"],
        "properties": {
            "preferred": {"enum": ["A", "B", "tie"]},
            "confidence": {"type": "number", "minimum": 0, "maximum": 1},
            "overall_a": {"type": "integer", "minimum": 1, "maximum": 10},
            "overall_b": {"type": "integer", "minimum": 1, "maximum": 10},
            "scores_a": categories_schema(),
            "scores_b": categories_schema(),
            "findings": findings_schema(&["A", "B"], max_coord),
            "summary": {"type": "string", "maxLength": 800}},
        "$defs": defs()})
}

/// `response_format` for a schema.
pub fn response_format(name: &str, schema: &Value) -> Value {
    json!({"type": "json_schema", "json_schema": {"name": name, "strict": true, "schema": schema}})
}

fn fmt_value(v: f64) -> String {
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v.is_nan() {
        return "n/a".into();
    }
    let a = v.abs();
    let s = if a >= 100.0 || a == 0.0 {
        format!("{v:.1}")
    } else if a >= 10.0 {
        format!("{v:.2}")
    } else if a >= 1.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.4}")
    };
    // 38.20 -> 38.2, 1.000 -> 1
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// "Objective metrics (Lumen, B vs A, ROI): PSNR-Y 38.2 dB, SSIM 0.962".
/// Takes plain (name, value, unit) tuples. `None` when there are none.
pub fn metric_context<N: AsRef<str>, U: AsRef<str>>(scope: &str, metrics: &[(N, f64, U)]) -> Option<String> {
    if metrics.is_empty() {
        return None;
    }
    let items: Vec<String> = metrics
        .iter()
        .map(|(n, v, u)| {
            let u = u.as_ref().trim();
            let sep = if u.is_empty() || u == "%" { "" } else { " " };
            format!("{} {}{sep}{u}", n.as_ref().trim(), fmt_value(*v))
        })
        .collect();
    let scope = scope.trim();
    let head = if scope.is_empty() { "Objective metrics (Lumen)".to_string() } else { format!("Objective metrics (Lumen, {scope})") };
    Some(format!("{head}: {}.", items.join(", ")))
}

/// Where a frame comes from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameRef {
    /// Seconds from the start of the video.
    pub timestamp_s: Option<f64>,
    /// Zero-based frame number.
    pub frame_index: Option<u64>,
}

pub fn fmt_timestamp(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

impl FrameRef {
    /// "at 00:01:23.456 (frame 2001)".
    pub fn describe(&self) -> String {
        match (self.timestamp_s, self.frame_index) {
            (Some(t), Some(f)) => format!("at {} (frame {f})", fmt_timestamp(t)),
            (Some(t), None) => format!("at {}", fmt_timestamp(t)),
            (None, Some(f)) => format!("at frame {f}"),
            (None, None) => "at an unspecified time".into(),
        }
    }
}

/// The sentence telling the model the images are video frames.
pub fn frame_context(a: Option<&FrameRef>, b: Option<&FrameRef>, pair: bool) -> Option<String> {
    match (a, b) {
        (None, None) => None,
        (Some(fa), Some(fb)) if pair && fa != fb => Some(format!(
            "These are still frames from video: A is the frame {}, B the frame {}.",
            fa.describe(),
            fb.describe()
        )),
        (Some(f), _) | (None, Some(f)) if pair => {
            Some(format!("A and B are the same still frame of a video, taken {}, from two versions of the video.", f.describe()))
        }
        (Some(f), _) | (None, Some(f)) => Some(format!("Image A is a still frame from a video, taken {}.", f.describe())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve() {
        for id in PresetId::ALL {
            let p = Preset::builtin(id);
            assert_eq!(PresetId::parse(&p.id), Some(id));
            assert!(!p.name.is_empty() && !p.task.is_empty());
        }
        let custom = vec![CustomPreset { name: "Mine".into(), structured: true, pair: true, ..Default::default() }];
        let c = find_preset("custom:Mine", &custom).unwrap();
        assert_eq!((c.arity, c.output), (Arity::Two, OutputKind::ComparisonJson));
        assert!(find_preset("custom:Other", &custom).is_none());
        assert_eq!(all_presets(&custom).len(), 9);
        assert!(Preset::builtin(PresetId::Compare).supports_swap_check());
        assert!(!Preset::builtin(PresetId::Encode).supports_swap_check());
    }

    #[test]
    fn system_prompts_carry_rubric_and_rules() {
        let q = Preset::builtin(PresetId::Quality).system_prompt(CoordConvention::Relative1000, None);
        for needle in ["9-10: no visible defects at 100% zoom", "0 to 1000", "Do not invent defects", "Objective metrics", "JSON report only"] {
            assert!(q.contains(needle), "missing {needle}");
        }
        let abs = Preset::builtin(PresetId::Quality).system_prompt(CoordConvention::AbsolutePixels, None);
        assert!(abs.contains("pixel coordinates"));
        let v = Preset::builtin(PresetId::VideoFrame).system_prompt(CoordConvention::Relative1000, None);
        assert!(v.contains("Motion blur"));
        let schema = quality_schema(&["A"], 1000);
        let f = Preset::builtin(PresetId::Quality).system_prompt(CoordConvention::Relative1000, Some(&schema));
        assert!(f.contains("\"additionalProperties\":false") && f.contains("no Markdown code fences"));
        let md = Preset::builtin(PresetId::FreeForm).system_prompt(CoordConvention::Relative1000, None);
        assert!(!md.contains("JSON") && !md.contains("9-10"));
        assert!(Preset::builtin(PresetId::QuickScore).system_prompt(CoordConvention::Relative1000, None).is_empty());
    }

    #[test]
    fn schemas_match_the_plan() {
        let s = quality_schema(&["A", "B"], 1000);
        assert_eq!(s["required"], json!(["overall", "categories", "findings", "summary"]));
        assert_eq!(s["properties"]["categories"]["required"].as_array().unwrap().len(), 6);
        assert_eq!(s["properties"]["findings"]["items"]["properties"]["bbox"]["items"]["maximum"], 1000);
        assert_eq!(s["properties"]["findings"]["items"]["properties"]["image"]["enum"], json!(["A", "B"]));
        assert_eq!(s["$defs"]["cat"]["properties"]["score"]["maximum"], 10);
        let c = comparison_schema(2048);
        assert_eq!(c["properties"]["preferred"]["enum"], json!(["A", "B", "tie"]));
        assert_eq!(c["properties"]["findings"]["items"]["properties"]["bbox"]["items"]["maximum"], 2048);
        let rf = response_format("lumen_quality_report", &s);
        assert_eq!(rf["json_schema"]["strict"], true);
    }

    #[test]
    fn metric_text() {
        let m = [("PSNR-Y", 38.2, "dB"), ("SSIM", 0.96213, ""), ("CIEDE2000 mean", 1.4, ""), ("changed", 12.5, "%"), ("PSNR", f64::INFINITY, "dB")];
        assert_eq!(
            metric_context("B vs A, ROI", &m).unwrap(),
            "Objective metrics (Lumen, B vs A, ROI): PSNR-Y 38.2 dB, SSIM 0.9621, CIEDE2000 mean 1.4, changed 12.5%, PSNR inf dB."
        );
        let owned = vec![("VMAF".to_string(), 93.456, String::new())];
        assert_eq!(metric_context("", &owned).unwrap(), "Objective metrics (Lumen): VMAF 93.46.");
        assert!(metric_context::<&str, &str>("x", &[]).is_none());
    }

    #[test]
    fn frame_text() {
        let f = FrameRef { timestamp_s: Some(83.456), frame_index: Some(2001) };
        assert_eq!(f.describe(), "at 00:01:23.456 (frame 2001)");
        assert_eq!(frame_context(Some(&f), None, false).unwrap(), "Image A is a still frame from a video, taken at 00:01:23.456 (frame 2001).");
        assert!(frame_context(Some(&f), Some(&f), true).unwrap().starts_with("A and B are the same still frame"));
        assert!(frame_context(None, None, true).is_none());
        assert_eq!(fmt_timestamp(3723.5), "01:02:03.500");
    }
}
