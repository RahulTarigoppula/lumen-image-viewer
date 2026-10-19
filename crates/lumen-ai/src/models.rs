//! Model discovery: `/v1/models` entries, the vision-name heuristic, the
//! probe verdict cache, automatic model choice and the context estimate.
//!
//! vLLM reports no modality, so vision support is a guess from the id (and
//! `root`) until the "Test vision" probe answers. A guess never blocks a
//! run: unknown models show "vision unknown, will try".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One entry of `/v1/models` (vLLM shape; unknown fields are ignored).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCard {
    pub id: String,
    /// The Hugging Face path when `--served-model-name` renamed the model.
    pub root: Option<String>,
    /// Set for LoRA adapters.
    pub parent: Option<String>,
    pub max_model_len: Option<u64>,
    pub owned_by: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisionHint {
    /// The name matches a known vision-language family.
    Likely,
    /// No match; the model may still take images.
    Unknown,
}

impl VisionHint {
    pub fn label(self) -> &'static str {
        match self {
            VisionHint::Likely => "probably vision",
            VisionHint::Unknown => "vision unknown, will try",
        }
    }
}

/// Substrings of known vision-language model ids (after lowercasing and
/// removing `-`, `_` and spaces, so `Qwen-3.8` reads as `qwen3.8`).
const VISION_NAMES: &[&str] = &[
    "vision",
    "qwen3",
    "qwen2.5vl",
    "qwen2vl",
    "qvq",
    "gemma3",
    "gemma4",
    "pixtral",
    "mistralsmall3",
    "internvl",
    "llava",
    "llama4",
    "qrealign",
    "visualquality",
    "minicpmv",
    "minicpmo",
    "glm4v",
    "glm4.5v",
    "kimivl",
    "molmo",
    "idefics",
    "smolvlm",
    "phi3.5vision",
    "phi4multimodal",
    "omni",
    "iqa",
];

fn squash(s: &str) -> String {
    s.to_ascii_lowercase().chars().filter(|c| !matches!(c, '-' | '_' | ' ')).collect()
}

/// True when a `vl` token stands alone in the id (`Qwen2.5-VL-7B`, `x_vl`).
fn has_vl_token(s: &str) -> bool {
    s.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| t == "vl" || (t.ends_with("vl") && t.len() > 2 && t[..t.len() - 2].ends_with(|c: char| c.is_ascii_digit())))
}

/// Name heuristic over a model id.
pub fn vision_hint_for(id: &str) -> VisionHint {
    // Path prefixes like "Qwen/" never matter; look at the whole id anyway
    // so "q-future/..." still matches.
    let s = squash(id);
    if VISION_NAMES.iter().any(|n| s.contains(n)) || has_vl_token(id) {
        VisionHint::Likely
    } else {
        VisionHint::Unknown
    }
}

impl ModelCard {
    pub fn vision_hint(&self) -> VisionHint {
        match vision_hint_for(&self.id) {
            VisionHint::Likely => VisionHint::Likely,
            VisionHint::Unknown => self.root.as_deref().map(vision_hint_for).unwrap_or(VisionHint::Unknown),
        }
    }

    pub fn is_adapter(&self) -> bool {
        self.parent.is_some()
    }

    /// An IQA scorer for the Quick score preset.
    pub fn is_quick_scorer(&self) -> bool {
        let s = squash(&self.id) + &self.root.as_deref().map(squash).unwrap_or_default();
        s.contains("qrealign") || s.contains("qalign")
    }
}

/// Result of the "Test vision" probe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VisionVerdict {
    Yes,
    /// The server said the model takes no images.
    No(String),
    /// Inconclusive (timeout, other error); runs stay allowed.
    Unknown(String),
}

impl VisionVerdict {
    pub fn label(&self) -> &'static str {
        match self {
            VisionVerdict::Yes => "vision",
            VisionVerdict::No(_) => "text only",
            VisionVerdict::Unknown(_) => "vision unknown, will try",
        }
    }
}

/// Probe verdicts per (server, model id, root). Serializable so the app can
/// keep it in its cache directory.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeCache {
    entries: BTreeMap<String, VisionVerdict>,
}

impl ProbeCache {
    fn key(server_key: &str, card: &ModelCard) -> String {
        format!("{server_key}|{}|{}", card.id, card.root.as_deref().unwrap_or(""))
    }
    pub fn get(&self, server_key: &str, card: &ModelCard) -> Option<&VisionVerdict> {
        self.entries.get(&Self::key(server_key, card))
    }
    /// Inconclusive verdicts are not cached, so the next test tries again.
    pub fn insert(&mut self, server_key: &str, card: &ModelCard, v: VisionVerdict) {
        if !matches!(v, VisionVerdict::Unknown(_)) {
            self.entries.insert(Self::key(server_key, card), v);
        }
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// The model to select after listing: `current` when still served, else
/// the first base model whose name looks like vision, else the first
/// vision-looking adapter, else the first model.
pub fn pick_model(models: &[ModelCard], current: Option<&str>) -> Option<String> {
    if let Some(c) = current.filter(|c| !c.is_empty()) {
        if models.iter().any(|m| m.id == c) {
            return Some(c.to_string());
        }
    }
    let likely = |m: &&ModelCard| m.vision_hint() == VisionHint::Likely && !m.is_quick_scorer();
    models
        .iter()
        .filter(|m| !m.is_adapter())
        .find(likely)
        .or_else(|| models.iter().find(likely))
        .or_else(|| models.iter().find(|m| m.vision_hint() == VisionHint::Likely))
        .or_else(|| models.first())
        .map(|m| m.id.clone())
}

/// Visual tokens for an image of `w` x `h` sent pixels (Qwen3.x: one token
/// per 32x32 block after the 2x2 merge, plus start and end markers).
pub fn image_tokens(w: u32, h: u32) -> u64 {
    (w.div_ceil(32) as u64) * (h.div_ceil(32) as u64) + 2
}

/// Rough prompt size: images plus text at about 3.5 characters per token,
/// plus chat-template overhead.
pub fn estimate_prompt_tokens(images: &[(u32, u32)], text_chars: usize) -> u64 {
    images.iter().map(|&(w, h)| image_tokens(w, h)).sum::<u64>() + (text_chars as f64 / 3.5).ceil() as u64 + 32
}

/// A warning when prompt plus output may not fit `max_model_len`.
pub fn context_warning(card: &ModelCard, prompt_tokens: u64, max_completion: u32) -> Option<String> {
    let limit = card.max_model_len?;
    let need = prompt_tokens + max_completion as u64;
    if need > limit {
        Some(format!(
            "About {prompt_tokens} prompt tokens plus {max_completion} output tokens may exceed {}'s context of {limit}. Lower the overview size or the number of tiles.",
            card.id
        ))
    } else if need as f64 > limit as f64 * 0.9 {
        Some(format!("The request uses about {} percent of the model's context ({limit} tokens).", need * 100 / limit))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str) -> ModelCard {
        ModelCard { id: id.into(), ..Default::default() }
    }

    #[test]
    fn heuristic_matches_known_families() {
        for id in [
            "Qwen/Qwen3.5-9B",
            "qwen-3.8",
            "Qwen3.8-VL-30B",
            "qwen3",
            "Qwen/Qwen3-VL-8B-Instruct-FP8",
            "Qwen/Qwen2.5-VL-7B-Instruct",
            "google/gemma-3-27b-it",
            "google/gemma-4-31B-it",
            "mistralai/Pixtral-12B-2409",
            "mistralai/Mistral-Small-3.2-24B-Instruct-2506",
            "OpenGVLab/InternVL3_5-8B-HF",
            "llava-hf/llava-1.5-7b-hf",
            "meta-llama/Llama-4-Scout-17B-16E-Instruct",
            "q-future/Q-ReAlign-Lite-4B",
            "TianheWu/VisualQuality-R1-7B",
            "my_custom_vl",
            "foo-vision-1",
        ] {
            assert_eq!(vision_hint_for(id), VisionHint::Likely, "{id}");
        }
        for id in ["meta-llama/Llama-3.1-8B-Instruct", "mistral-7b", "deepseek-r1", "devlin", "evaluator"] {
            assert_eq!(vision_hint_for(id), VisionHint::Unknown, "{id}");
        }
        // A renamed model is recognized through root.
        let renamed = ModelCard { id: "critic".into(), root: Some("Qwen/Qwen3.5-9B".into()), ..Default::default() };
        assert_eq!(renamed.vision_hint(), VisionHint::Likely);
        assert_eq!(card("critic").vision_hint().label(), "vision unknown, will try");
    }

    #[test]
    fn picks_a_vision_model() {
        let models = vec![
            card("meta-llama/Llama-3.1-8B-Instruct"),
            ModelCard { id: "lora-critic".into(), parent: Some("Qwen/Qwen3.5-9B".into()), ..Default::default() },
            card("q-future/Q-ReAlign-Lite-4B"),
            card("Qwen/Qwen3.5-9B"),
        ];
        assert_eq!(pick_model(&models, None).as_deref(), Some("Qwen/Qwen3.5-9B"));
        assert_eq!(pick_model(&models, Some("lora-critic")).as_deref(), Some("lora-critic"));
        assert_eq!(pick_model(&models, Some("gone")).as_deref(), Some("Qwen/Qwen3.5-9B"));
        // Nothing looks like vision: still pick something, never block.
        assert_eq!(pick_model(&[card("x"), card("y")], None).as_deref(), Some("x"));
        assert_eq!(pick_model(&[], None), None);
        // Only a scorer: pick it.
        assert_eq!(pick_model(&[card("x"), card("q-future/Q-ReAlign-Lite-4B")], None).as_deref(), Some("q-future/Q-ReAlign-Lite-4B"));
    }

    #[test]
    fn probe_cache_keys_on_server_and_root() {
        let mut c = ProbeCache::default();
        let m = card("m");
        c.insert("http://a:8000", &m, VisionVerdict::Yes);
        c.insert("http://a:8000", &card("n"), VisionVerdict::Unknown("timeout".into()));
        assert_eq!(c.get("http://a:8000", &m), Some(&VisionVerdict::Yes));
        assert_eq!(c.get("http://b:8000", &m), None);
        assert_eq!(c.get("http://a:8000", &card("n")), None);
        let rooted = ModelCard { root: Some("other".into()), ..m };
        assert_eq!(c.get("http://a:8000", &rooted), None);
    }

    #[test]
    fn context_estimate() {
        assert_eq!(image_tokens(1536, 864), 48 * 27 + 2);
        let est = estimate_prompt_tokens(&[(1536, 864), (1536, 864)], 3500);
        assert_eq!(est, 2 * (1296 + 2) + 1000 + 32);
        let small = ModelCard { max_model_len: Some(2048), ..card("m") };
        assert!(context_warning(&small, est, 1500).unwrap().contains("exceed"));
        let big = ModelCard { max_model_len: Some(32768), ..card("m") };
        assert!(context_warning(&big, est, 1500).is_none());
        assert!(context_warning(&card("m"), est, 1500).is_none());
    }
}
