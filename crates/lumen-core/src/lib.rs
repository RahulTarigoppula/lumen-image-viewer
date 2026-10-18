//! Lumen core: the UI-free, FFmpeg-free half of Lumen. File formats and
//! decoders (pure Rust plus WIC), color management, resizing, encoders with
//! metadata preservation, metadata editing, folder catalog, tags, settings
//! data, the pure pixel operations behind the editor, and video metrics and
//! scopes. Nothing here depends on egui or eframe; the `lumen` app crate
//! builds the UI on top.

pub mod catalog;
pub mod decode;
pub mod edit;
pub mod encode;
pub mod formats;
pub mod imaging;
pub mod meta;
pub mod metaedit;
pub mod settings;
pub mod tags;
pub mod video;
