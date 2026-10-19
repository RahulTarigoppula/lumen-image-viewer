//! Lumen analysis: objective image and video quality metrics.
//!
//! Pure Rust, no egui, no FFmpeg. The app wraps decoded pixels in
//! [`FrameView`]s (RGBA8 images or native YUV planes), picks metrics by id
//! from the [`metrics`] registry and runs them through [`job`] (image jobs,
//! video range jobs fed by the app's decoder), reading back
//! [`report::AnalysisReport`]s and [`heatmap::Heatmap`]s. See `API.md`.

// Hot loops index several equally long slices on purpose (it is what
// autovectorizes); iterator zips of five or more slices read worse.
#![allow(clippy::needless_range_loop)]

pub mod cache;
pub mod color;
mod colormaps;
pub mod frame;
pub mod heatmap;
pub mod job;
pub mod metrics;
pub mod report;
mod simd;
pub mod stats;

pub use frame::{FrameCtx, FrameSource, FrameView, Matrix, OwnedFrame, OwnedPlane, Plane, PlaneView, Range, RgbData, RgbView, Roi, Transfer, YuvView};
pub use heatmap::{Colormap, ColorizeOptions, Heatmap, RangeMode};
pub use job::{Analyzer, CancelToken, FrameResult, ImageSpec, JobControl, JobEvent, JobHandle, SharedFrame, VideoSpec};
pub use metrics::{Metric, MetricOptions, MetricResult, Unit, Value, PSNR_CAP_DB};
pub use report::{AnalysisReport, FrameRange, Scope, ScopeKind};
