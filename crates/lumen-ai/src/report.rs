//! Parsing the model's JSON into an `AiReport`, mapping findings back to
//! source pixels, and rendering "Copy as Markdown".
//!
//! Parsing is tolerant: code fences and text around the object are
//! ignored, and a truncated object (`finish_reason: length`) is repaired
//! by closing open strings and brackets, dropping the last incomplete
//! member if needed. Values are validated and clamped; anything odd is
//! noted in `warnings` instead of failing the whole report.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::images::{ImageLabel, SentLayout, SourceBox};
use crate::prompts::{OutputKind, CATEGORIES};
use crate::settings::CoordConvention;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Minor,
    Moderate,
    Severe,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Minor => "minor",
            Severity::Moderate => "moderate",
            Severity::Severe => "severe",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CategoryScore {
    /// Schema key, e.g. `noise`.
    pub key: String,
    /// Display name, e.g. "Noise".
    pub label: String,
    /// `None` when the model returned null (detail_vs_reference without a
    /// reference) or nothing usable.
    pub score: Option<u8>,
    pub rationale: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub category: String,
    pub severity: Severity,
    pub image: ImageLabel,
    /// 0 = full view or composite, n = ROI tile n.
    pub tile: u32,
    /// As returned (0..1000 or sent pixels).
    pub bbox: [u32; 4],
    pub description: String,
    /// The box in source pixels, after `map_findings`.
    pub source: Option<SourceBox>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preference {
    A,
    B,
    Tie,
}

impl Preference {
    pub fn as_str(self) -> &'static str {
        match self {
            Preference::A => "A",
            Preference::B => "B",
            Preference::Tie => "tie",
        }
    }
    fn swapped(self) -> Preference {
        match self {
            Preference::A => Preference::B,
            Preference::B => Preference::A,
            Preference::Tie => Preference::Tie,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    pub preferred: Preference,
    pub confidence: f32,
    pub overall_a: Option<u8>,
    pub overall_b: Option<u8>,
    pub scores_a: Vec<CategoryScore>,
    pub scores_b: Vec<CategoryScore>,
}

/// A parsed, validated report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AiReport {
    pub overall: Option<u8>,
    pub categories: Vec<CategoryScore>,
    pub comparison: Option<Comparison>,
    pub findings: Vec<Finding>,
    pub summary: String,
    /// The JSON was truncated and repaired; some fields may be missing.
    pub partial: bool,
    /// Validation notes (clamped values, unknown categories...).
    pub warnings: Vec<String>,
}

/// Finds the JSON object in model output, repairing truncation.
/// Returns the value and whether it was repaired.
pub fn extract_json(text: &str) -> Option<(Value, bool)> {
    let body = strip_fences(text);
    let start = body.find('{')?;
    let s = &body[start..];
    if let Some(end) = matching_brace(s) {
        if let Ok(v) = serde_json::from_str::<Value>(&s[..=end]) {
            return Some((v, false));
        }
    }
    // Truncated: close what is open, dropping trailing members until it parses.
    if let Ok(v) = serde_json::from_str::<Value>(&close(s)) {
        return Some((v, true));
    }
    let cuts = cut_points(s);
    for &cut in cuts.iter().rev().take(400) {
        if let Ok(v) = serde_json::from_str::<Value>(&close(&s[..cut])) {
            return Some((v, true));
        }
    }
    None
}

fn strip_fences(text: &str) -> &str {
    let Some(i) = text.find("```") else { return text };
    let after = &text[i + 3..];
    let after = after.find('\n').map(|n| &after[n + 1..]).unwrap_or(after);
    match after.find("```") {
        Some(j) => &after[..j],
        None => after,
    }
}

/// Index of the brace closing the object that starts at 0.
fn matching_brace(s: &str) -> Option<usize> {
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, c) in s.char_indices() {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Positions just before each `,` and just after each `{` / `[` outside
/// strings: places where an incomplete member can be cut off.
fn cut_points(s: &str) -> Vec<usize> {
    let (mut in_str, mut esc) = (false, false);
    let mut out = Vec::new();
    for (i, c) in s.char_indices() {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            ',' => out.push(i),
            '{' | '[' => out.push(i + 1),
            _ => {}
        }
    }
    out
}

/// Closes an open string and all open brackets of a JSON prefix.
fn close(prefix: &str) -> String {
    let (mut in_str, mut esc) = (false, false);
    let mut stack = Vec::new();
    for c in prefix.chars() {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = prefix.trim_end().to_string();
    if esc {
        out.pop();
    }
    if in_str {
        out.push('"');
    }
    let trimmed = out.trim_end();
    if trimmed.ends_with(',') || trimmed.ends_with(':') {
        out = trimmed[..trimmed.len() - 1].to_string();
    }
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    out
}

fn get_u8(v: &Value, key: &str, lo: u8, hi: u8, warnings: &mut Vec<String>) -> Option<u8> {
    let n = v.get(key)?.as_f64()?;
    let c = n.round().clamp(lo as f64, hi as f64);
    if c != n {
        warnings.push(format!("{key} {n} clamped to {c}"));
    }
    Some(c as u8)
}

fn categories(v: Option<&Value>, warnings: &mut Vec<String>) -> Vec<CategoryScore> {
    CATEGORIES
        .iter()
        .map(|(key, label)| {
            let c = v.and_then(|o| o.get(*key)).filter(|c| c.is_object());
            CategoryScore {
                key: key.to_string(),
                label: label.to_string(),
                score: c.and_then(|c| get_u8(c, "score", 1, 10, warnings)),
                rationale: c.and_then(|c| c.get("rationale")).and_then(|r| r.as_str()).unwrap_or("").trim().to_string(),
            }
        })
        .collect()
}

fn label(v: Option<&Value>) -> Option<ImageLabel> {
    match v?.as_str()?.trim().to_ascii_uppercase().as_str() {
        "A" => Some(ImageLabel::A),
        "B" => Some(ImageLabel::B),
        _ => None,
    }
}

/// Parses model output into a report.
pub fn parse_report(text: &str, kind: OutputKind, conv: CoordConvention) -> Result<AiReport, String> {
    let (v, partial) = extract_json(text).ok_or_else(|| "the model output contains no JSON object".to_string())?;
    if !v.is_object() {
        return Err("the model output is not a JSON object".into());
    }
    let mut warnings = Vec::new();
    let max_coord = match conv {
        CoordConvention::Relative1000 => 1000.0,
        CoordConvention::AbsolutePixels => f64::from(u32::MAX),
    };
    let known: Vec<&str> = CATEGORIES.iter().map(|c| c.0).collect();
    let mut findings = Vec::new();
    for (i, f) in v.get("findings").and_then(|f| f.as_array()).map(|a| a.as_slice()).unwrap_or(&[]).iter().enumerate() {
        let n = i + 1;
        let Some(raw) = f.get("bbox").and_then(|b| b.as_array()).filter(|b| b.len() == 4) else {
            warnings.push(format!("finding {n} has no valid bbox and was skipped"));
            continue;
        };
        let nums: Vec<f64> = raw.iter().map(|x| x.as_f64().unwrap_or(0.0).clamp(0.0, max_coord)).collect();
        let bbox = [
            nums[0].min(nums[2]).round() as u32,
            nums[1].min(nums[3]).round() as u32,
            nums[0].max(nums[2]).round() as u32,
            nums[1].max(nums[3]).round() as u32,
        ];
        let category = f.get("category").and_then(|c| c.as_str()).unwrap_or("").to_string();
        if !known.contains(&category.as_str()) {
            warnings.push(format!("finding {n} has unknown category '{category}'"));
        }
        let severity = match f.get("severity").and_then(|s| s.as_str()).map(|s| s.to_ascii_lowercase()) {
            Some(s) if s == "minor" => Severity::Minor,
            Some(s) if s == "severe" => Severity::Severe,
            Some(s) if s == "moderate" => Severity::Moderate,
            _ => {
                warnings.push(format!("finding {n} has no valid severity; shown as moderate"));
                Severity::Moderate
            }
        };
        findings.push(Finding {
            category,
            severity,
            image: label(f.get("image")).unwrap_or(ImageLabel::A),
            tile: f.get("tile").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
            bbox,
            description: f.get("description").and_then(|d| d.as_str()).unwrap_or("").trim().to_string(),
            source: None,
        });
    }
    let comparison = (kind == OutputKind::ComparisonJson || v.get("preferred").is_some()).then(|| {
        let preferred = match v.get("preferred").and_then(|p| p.as_str()).map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "a" => Preference::A,
            Some(s) if s == "b" => Preference::B,
            _ => Preference::Tie,
        };
        let confidence = v.get("confidence").and_then(|c| c.as_f64()).unwrap_or(0.5).clamp(0.0, 1.0) as f32;
        Comparison {
            preferred,
            confidence,
            overall_a: get_u8(&v, "overall_a", 1, 10, &mut warnings),
            overall_b: get_u8(&v, "overall_b", 1, 10, &mut warnings),
            scores_a: categories(v.get("scores_a"), &mut warnings),
            scores_b: categories(v.get("scores_b"), &mut warnings),
        }
    });
    let overall = get_u8(&v, "overall", 1, 10, &mut warnings).or_else(|| comparison.as_ref().and_then(|c| c.overall_b));
    let categories = match v.get("categories") {
        Some(c) => categories(Some(c), &mut warnings),
        None => comparison.as_ref().map(|c| c.scores_b.clone()).unwrap_or_else(|| categories(None, &mut warnings)),
    };
    if partial {
        warnings.push("the output was cut off; the report is incomplete".into());
    }
    Ok(AiReport {
        overall,
        categories,
        comparison,
        findings,
        summary: v.get("summary").and_then(|s| s.as_str()).unwrap_or("").trim().to_string(),
        partial,
        warnings,
    })
}

impl AiReport {
    /// Fills `Finding::source` from the layout of what was sent.
    pub fn map_findings(&mut self, layout: &[SentLayout], conv: CoordConvention) {
        for f in &mut self.findings {
            let entry = layout
                .iter()
                .find(|e| e.label == Some(f.image) && e.tile == f.tile)
                .or_else(|| layout.iter().find(|e| e.label.is_none()))
                .or_else(|| layout.iter().find(|e| e.label == Some(f.image) && e.tile == 0));
            let b = f.bbox.map(|v| v as f64);
            f.source = entry.and_then(|e| e.map_bbox(f.image, b, conv));
            if let Some(s) = &f.source {
                f.image = s.label;
            }
        }
    }

    /// Converts a report from the swapped pass (B shown as A) back to the
    /// real labels.
    pub fn unswap(&mut self) {
        if let Some(c) = &mut self.comparison {
            c.preferred = c.preferred.swapped();
            std::mem::swap(&mut c.overall_a, &mut c.overall_b);
            std::mem::swap(&mut c.scores_a, &mut c.scores_b);
        }
        for f in &mut self.findings {
            f.image = f.image.other();
            if let Some(s) = &mut f.source {
                s.label = s.label.other();
            }
        }
    }

    /// "Copy as Markdown".
    pub fn to_markdown(&self, title: &str) -> String {
        let mut md = String::new();
        let cell = |s: &str| s.replace('|', "\\|").replace('\n', " ");
        let score = |s: Option<u8>| s.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
        match &self.comparison {
            Some(c) => {
                let verdict = match c.preferred {
                    Preference::Tie => "no visible preference (tie)".to_string(),
                    p => format!("{} preferred", p.as_str()),
                };
                md.push_str(&format!("## {title}: {verdict}, confidence {:.2}\n\n", c.confidence));
                md.push_str(&format!("Overall: A {}/10, B {}/10\n\n", score(c.overall_a), score(c.overall_b)));
                md.push_str("| Category | A | B | Notes (B) |\n|---|---|---|---|\n");
                for (a, b) in c.scores_a.iter().zip(&c.scores_b) {
                    if a.score.is_none() && b.score.is_none() {
                        continue;
                    }
                    let notes = if b.rationale.is_empty() { a.rationale.as_str() } else { b.rationale.as_str() };
                    md.push_str(&format!("| {} | {} | {} | {} |\n", a.label, score(a.score), score(b.score), cell(notes)));
                }
            }
            None => {
                md.push_str(&format!("## {title}: {}/10\n\n", score(self.overall)));
                md.push_str("| Category | Score | Notes |\n|---|---|---|\n");
                for c in self.categories.iter().filter(|c| c.score.is_some()) {
                    md.push_str(&format!("| {} | {} | {} |\n", c.label, score(c.score), cell(&c.rationale)));
                }
            }
        }
        if !self.findings.is_empty() {
            md.push_str("\n### Findings\n\n");
            for (i, f) in self.findings.iter().enumerate() {
                let cat = CATEGORIES.iter().find(|c| c.0 == f.category).map(|c| c.1).unwrap_or(&f.category);
                let place = if f.tile == 0 { format!("image {}", f.image.as_str()) } else { format!("image {}, tile {}", f.image.as_str(), f.tile) };
                let px = f
                    .source
                    .map(|s| format!(", pixels {:.0},{:.0} to {:.0},{:.0}", s.x0, s.y0, s.x1, s.y1))
                    .unwrap_or_default();
                md.push_str(&format!("{}. **{}** {} ({place}{px}): {}\n", i + 1, f.severity.as_str(), cat.to_lowercase(), f.description));
            }
        }
        if !self.summary.is_empty() {
            md.push_str(&format!("\n### Summary\n\n{}\n", self.summary));
        }
        if self.partial {
            md.push_str("\n_The model output was cut off; this report is incomplete._\n");
        }
        md
    }
}

/// Result of the Quick score preset.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuickScore {
    /// 0 (bad) to 1 (excellent).
    pub score: f64,
    /// Probabilities of excellent, good, fair, poor, bad.
    pub probs: [f64; 5],
}

impl QuickScore {
    /// MOS-like 1..5 scale.
    pub fn mos(&self) -> f64 {
        1.0 + 4.0 * self.score
    }

    /// Softmax over the logprobs of the five rating words in the first
    /// generated token's `top_logprobs` (OpenAI shape).
    pub fn from_response(raw: &Value) -> Result<QuickScore, String> {
        let content = raw
            .pointer("/choices/0/logprobs/content/0")
            .ok_or_else(|| "the response has no logprobs (is logprobs enabled on the server?)".to_string())?;
        let mut best = [f64::NEG_INFINITY; 5];
        let mut consider = |tok: &str, lp: f64| {
            let t = tok.trim().trim_start_matches('\u{0120}').to_ascii_lowercase();
            if let Some(i) = crate::prompts::QUICK_SCORE_LEVELS.iter().position(|(w, _)| *w == t) {
                best[i] = best[i].max(lp);
            }
        };
        if let Some(arr) = content.get("top_logprobs").and_then(|t| t.as_array()) {
            for e in arr {
                if let (Some(t), Some(lp)) = (e.get("token").and_then(|t| t.as_str()), e.get("logprob").and_then(|l| l.as_f64())) {
                    consider(t, lp);
                }
            }
        }
        if let (Some(t), Some(lp)) = (content.get("token").and_then(|t| t.as_str()), content.get("logprob").and_then(|l| l.as_f64())) {
            consider(t, lp);
        }
        let max = best.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if !max.is_finite() {
            return Err("none of excellent/good/fair/poor/bad is among the top tokens; is this a Q-ReAlign style model?".into());
        }
        let exps: Vec<f64> = best.iter().map(|&l| if l.is_finite() { (l - max).exp() } else { 0.0 }).collect();
        let sum: f64 = exps.iter().sum();
        let mut probs = [0.0; 5];
        for (p, e) in probs.iter_mut().zip(&exps) {
            *p = e / sum;
        }
        let score = probs.iter().zip(crate::prompts::QUICK_SCORE_LEVELS.iter()).map(|(p, (_, w))| p * w).sum();
        Ok(QuickScore { score, probs })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{"overall": 7, "categories": {
        "sharpness_detail": {"score": 8, "rationale": "crisp"},
        "noise": {"score": 6, "rationale": "chroma | grain"},
        "compression_artifacts": {"score": 7, "rationale": "slight banding"},
        "color": {"score": 9, "rationale": "neutral"},
        "exposure_dynamic_range": {"score": 12, "rationale": "fine"},
        "detail_vs_reference": null},
      "findings": [{"category": "noise", "severity": "moderate", "image": "A", "tile": 1, "bbox": [500, 100, 200, 300], "description": "chroma noise in shadows"},
                   {"category": "blur", "severity": "huge", "image": "a", "tile": 0, "bbox": [0, 0, 2000, 10], "description": "x"},
                   {"category": "noise", "severity": "minor", "image": "A", "tile": 0, "bbox": [1, 2], "description": "bad bbox"}],
      "summary": "Good overall."}"#;

    #[test]
    fn parses_and_validates() {
        let r = parse_report(&format!("Here you go:\n```json\n{FULL}\n```\nDone."), OutputKind::QualityJson, CoordConvention::Relative1000).unwrap();
        assert_eq!(r.overall, Some(7));
        assert_eq!(r.categories.len(), 6);
        assert_eq!(r.categories[4].score, Some(10), "clamped");
        assert_eq!(r.categories[5].score, None);
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.findings[0].bbox, [200, 100, 500, 300], "sorted");
        assert_eq!(r.findings[1].bbox[2], 1000, "clamped");
        assert_eq!(r.findings[1].severity, Severity::Moderate);
        assert!(!r.partial);
        assert!(r.warnings.len() >= 3, "{:?}", r.warnings);
        let md = r.to_markdown("Quality critique");
        assert!(md.starts_with("## Quality critique: 7/10"));
        assert!(md.contains("chroma \\| grain"));
        assert!(md.contains("1. **moderate** noise (image A, tile 1): chroma noise in shadows"));
        assert!(md.contains("### Summary\n\nGood overall."));
    }

    #[test]
    fn repairs_truncated_output() {
        let cut = &FULL[..FULL.find("\"summary\"").unwrap() - 20];
        let r = parse_report(cut, OutputKind::QualityJson, CoordConvention::Relative1000).unwrap();
        assert!(r.partial);
        assert_eq!(r.overall, Some(7));
        assert!(!r.findings.is_empty());
        // Cut inside a string.
        let r = parse_report(r#"{"overall": 4, "summary": "Heavy block"#, OutputKind::QualityJson, CoordConvention::Relative1000).unwrap();
        assert_eq!((r.overall, r.summary.as_str()), (Some(4), "Heavy block"));
        // Cut after a key.
        let r = parse_report(r#"{"overall": 5, "categories": {"noise": {"score": 3, "rationale": "x"}, "color": "#, OutputKind::QualityJson, CoordConvention::Relative1000).unwrap();
        assert_eq!(r.categories[1].score, Some(3));
        assert!(parse_report("no json here", OutputKind::QualityJson, CoordConvention::Relative1000).is_err());
    }

    #[test]
    fn comparison_and_unswap() {
        let text = r#"{"preferred": "B", "confidence": 0.8, "overall_a": 6, "overall_b": 8,
          "scores_a": {"noise": {"score": 5, "rationale": "noisy"}}, "scores_b": {"noise": {"score": 8, "rationale": "clean"}},
          "findings": [{"category": "noise", "severity": "severe", "image": "A", "tile": 0, "bbox": [0,0,10,10], "description": "grain"}],
          "summary": "B is cleaner."}"#;
        let mut r = parse_report(text, OutputKind::ComparisonJson, CoordConvention::Relative1000).unwrap();
        let c = r.comparison.clone().unwrap();
        assert_eq!((c.preferred, c.overall_b, c.scores_b[1].score), (Preference::B, Some(8), Some(8)));
        assert_eq!(r.overall, Some(8));
        let md = r.to_markdown("A vs B");
        assert!(md.starts_with("## A vs B: B preferred, confidence 0.80"));
        assert!(md.contains("| Noise | 5 | 8 | clean |"));
        r.unswap();
        let c = r.comparison.unwrap();
        assert_eq!((c.preferred, c.overall_a, c.scores_a[1].score), (Preference::A, Some(8), Some(8)));
        assert_eq!(r.findings[0].image, ImageLabel::B);
    }

    #[test]
    fn quick_score_softmax() {
        let raw = serde_json::json!({"choices": [{"logprobs": {"content": [{"token": " good", "logprob": -0.1,
            "top_logprobs": [{"token": " good", "logprob": -0.1}, {"token": " excellent", "logprob": -2.4},
                             {"token": " fair", "logprob": -3.0}, {"token": "Ġpoor", "logprob": -6.0}, {"token": " the", "logprob": -1.0}]}]}}]});
        let q = QuickScore::from_response(&raw).unwrap();
        assert!((q.probs.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(q.probs[1] > q.probs[0] && q.probs[4] == 0.0);
        assert!(q.score > 0.7 && q.score < 0.85, "{}", q.score);
        assert!((q.mos() - (1.0 + 4.0 * q.score)).abs() < 1e-12);
        assert!(QuickScore::from_response(&serde_json::json!({"choices": [{}]})).is_err());
    }
}
