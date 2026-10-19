//! Lumen remote AI critique, against a user's own OpenAI-compatible server
//! (vLLM). This is the only network code in Lumen; it has no egui
//! dependency and talks to the UI through plain types, a channel of
//! `AiEvent`s and a repaint callback. See `API.md` for the integration
//! guide and `docs/ANALYSIS-PLAN.md` section 3 for the design.
//!
//! Flow: type the IP, `spawn_test_connection` (lists models and picks a
//! vision model), pick a preset, `prepare` (send preview), `spawn_run`.

pub mod client;
pub mod error;
pub mod images;
pub mod models;
pub mod prompts;
pub mod report;
pub mod request;
pub mod secret;
pub mod settings;
pub mod worker;

pub use client::{CancelToken, Client, ClientConfig, ConnectionReport, StepResult, StepStatus, TestStep, Usage};
pub use error::{AiError, TimeoutKind};
pub use images::{ImageLabel, Roi, SentLayout, SourceBox};
pub use models::{pick_model, ModelCard, ProbeCache, VisionHint, VisionVerdict};
pub use prompts::{all_presets, find_preset, metric_context, Arity, FrameRef, OutputKind, Preset, PresetId};
pub use report::{AiReport, CategoryScore, Comparison, Finding, Preference, QuickScore, Severity};
pub use request::{prepare, AbLayout, InputImage, PreparedRequest, PreviewImage, RequestInput, SentRecord};
pub use secret::{ApiKey, KeyStore, SecretStore};
pub use settings::{AiSettings, CoordConvention, CustomPreset, Endpoint, ImageEncoding, NetPolicy};
pub use worker::{run_blocking, spawn_list_models, spawn_run, spawn_test_connection, spawn_vision_probe, AiEvent, Repaint, RunResult, SwapCheck, Task};
