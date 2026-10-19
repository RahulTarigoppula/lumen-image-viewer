# Lumen architecture

Lumen is a single native Windows executable written in Rust. The UI is
immediate-mode (egui 0.37 on OpenGL via eframe); every expensive operation runs
off the UI thread so the interface stays at display refresh rate on folders with
tens of thousands of files and on images of hundreds of megapixels.

## Crates

The repository is a Cargo workspace:

| Crate | Path | Contents | Depends on |
|---|---|---|---|
| `lumen` (binary) | `src/` | UI (egui/eframe), viewer, browser, batch tools, GPU renderer, loader and thumbnail pools, shell integration, FFmpeg video engine (`video` feature), `build.rs` (resources, FFmpeg link libraries) | `lumen-core`, egui, eframe, FFmpeg |
| `lumen-analysis` | `crates/lumen-analysis/` | Objective image and video metrics, heatmaps, jobs, reports, cache (see its API.md) | rayon, blake3, fast-ssim2, butteraugli, flip-rs |
| `lumen-ai` | `crates/lumen-ai/` | The only network code: OpenAI-compatible (vLLM) client, prompts, image preparation, secrets (see its API.md); behind the `ai` feature | ureq + rustls, windows |
| `lumen-core` (library `lumen_core`) | `crates/lumen-core/` | Everything with no UI and no FFmpeg: formats, decoders, color, resizing, encoders, metadata, catalog, tags, settings data, edit pixel operations, video metrics and scopes | image, WIC (`windows`), small pure-Rust codecs |
| `lumen-ai` (library `lumen_ai`, feature `ai`) | `crates/lumen-ai/` | The only network code: OpenAI-compatible (vLLM) client, Test connection, prompts, image preparation, reports, API key storage. See its `API.md` | ureq (rustls), image |

`lumen-core` must never depend on egui, eframe or FFmpeg. The app imports
the core modules at its crate root (`use lumen_core::{decode, settings, ...}`
in `main.rs`, `pub use lumen_core::edit::*` in `edit/mod.rs`, and
`pub use lumen_core::video::...` in `video/mod.rs`), so app code keeps using
`crate::decode`, `crate::edit::filters`, `crate::video::metrics` and so on.
Two seams connect the crates at run time:

- `lumen_core::video::set_thumbnailer`: the app installs the FFmpeg
  thumbnailer at startup when built with `video`; without it video
  thumbnails report "no video support".
- `lumen_core::meta::MetaService::new(repaint)`: takes a repaint callback
  instead of an `egui::Context`.

Splitting the crates keeps incremental builds of the app small: editing UI
code recompiles only `lumen`, and `lumen-core` is rebuilt only when its own
sources change. Profiles live in the workspace root `Cargo.toml`; shared
dependency versions are in `[workspace.dependencies]`.

## Module map

| Area | Crate | Modules | Responsibility |
|---|---|---|---|
| Shell | `lumen` | `main.rs`, `app.rs`, `shell.rs`, `devhook.rs`, `fileops.rs`, `bg.rs` | Startup, single instance (named pipe), mode switching, command dispatch, dialogs, file operations, dev automation hook |
| Commands and input | `lumen` | `ui/commands.rs`, `ui/keymap.rs`, `ui/palette.rs` | One `Command` enum drives menus, toolbar, shortcuts and the Ctrl+K palette; remappable keymap with presets |
| Browser | `lumen` | `ui/browser.rs`, `ui/tree.rs`, `thumbs.rs` | Grid and details views, folder tree, thumbnail pool and GPU texture cache |
| Catalog and tags | `lumen-core` | `catalog.rs`, `tags.rs`, `settings.rs` | Folder listing, sorting, filtering, ratings/flags/labels, settings data and storage paths |
| Viewer | `lumen` | `ui/viewer.rs`, `render.rs`, `loader.rs` | Zoom/pan, magnifier, crop, tool hosting, animation, info panel; tiled GPU renderer; decode cache with preloading |
| Decoding | `lumen-core` | `decode/*`, `formats.rs`, `imaging.rs`, `meta.rs` | Pure-Rust decoders, WIC fallback (HEIC/AVIF/RAW/JXL), ICC to sRGB, resizing, EXIF |
| Encoding | `lumen-core` | `encode.rs`, `metaedit.rs` | JPEG/PNG/WebP/TIFF/... with metadata preservation, atomic writes, lossless JPEG orientation, metadata editing |
| Editing (pixels) | `lumen-core` | `edit/mod.rs`, `edit/{ops,filters,effects,geom,tone,text,mask,raster,retouch}.rs` | Undo session, histogram data, filters, effects, geometry, tone curves, text, draw board rasterization, retouch |
| Editing (UI) | `lumen` | `edit/mod.rs`, `edit/tools.rs`, `edit/tools/*` | `EditTool` trait, tool contexts, histogram painting, every adjustment dialog |
| Batch and tools | `lumen` | `ui/batch*`, `ui/tools*` | Batch convert pipeline, batch rename, contact sheet, duplicates, output tools, printing |
| Modes | `lumen` | `ui/slideshow.rs`, `ui/compare.rs`, `video/*` | Slideshow with GPU transitions, 2-4 image compare, video playback and A/B analysis |
| Video (pure) | `lumen-core` | `video/mod.rs`, `video/metrics.rs`, `video/scopes.rs` | Video extensions, PSNR/SSIM, scopes, thumbnailer hook |
| Video (FFmpeg) | `lumen` | `video/engine.rs`, `video/audio.rs`, `video/analyze.rs`, `video/mode.rs` | FFmpeg demux/decode worker, WASAPI audio master clock, whole-file analysis, UI |
| Analysis panel | `lumen` | `analysis_ui/*` | Right-side panel of the viewer, compare and video modes (Ctrl+Q): Metrics tab (`metrics_view.rs`, state in `Core::metrics`, Shift+Q runs it; `heatmap_view.rs` overlays, `plots.rs` graphs under the video timeline, `adapters.rs` from decoded pixels and `SeqDecoder` ranges to `lumen_analysis` jobs), AI tab, send preview, ROI (R) and finding overlays. Modes only publish where they draw images (`analysis_ui::canvas::publish`); video mode also hands the Metrics tab its streams each frame |

Paths in the `lumen-core` rows are relative to `crates/lumen-core/src/`, the
others to `src/`.

## Threading model

- **UI thread**: input, layout, texture uploads with a per-frame time budget. It never decodes, never resizes large images and never touches the network.
- **Loader pool** (`loader.rs`, 2-4 threads): full decodes plus mip pyramid build. Priority 0 is the image on screen, then the next and previous neighbours. A memory budget (default 1.5 GB) evicts least-recently-used images, never the current one. A fast scaled preview (WIC DCT scaling) is shown first for big files.
- **Thumbnail pool** (`thumbs.rs`, cores-2 threads): newest request first, stale requests skipped, results written to a bounded disk cache (1.5 GB) and an LRU of GPU textures (400).
- **Metadata pool** (`meta.rs`): dimensions and capture dates for sorting and the details view.
- **Video**: one worker per open file (demux, video decode, audio decode/resample), an index thread (demux-only pass for frame count, timestamps, packet sizes), and the cpal audio callback which is the master clock.
- **Ad-hoc jobs**: histograms, EXIF panels, tool previews, final renders, batch jobs and analysis run on rayon or dedicated threads and report back through channels (`bg::BgSlot` for "latest value for key" results).

All workers are panic-isolated (`catch_unwind`) so a corrupt file produces an error message, not a crash.

## Rendering

`render::GpuImage` keeps a CPU mip pyramid (each level half the previous) and uploads 1024 px tiles of the level that matches the current zoom, only for tiles that are visible, center-out, within a 12 ms per-frame budget. The coarsest level (at most 1024 px) is always resident, so panning never shows holes. Opaque rows are copied straight into texture memory without per-pixel conversion.

Video frames are converted by swscale with the correct BT.601/709/2020 matrix and range at the on-screen size (not native size when zoomed out), so a 4K stream in a 1080p window uploads 1080p frames.

## Data safety

- All writes go to a temporary file first and are renamed over the target.
- Deletes go to the Recycle Bin.
- Ratings, flags and labels are stored in `%APPDATA%\Lumen\tags.json` and never written into images.
- Unsaved edits are guarded on navigation and on window close.

## Privacy

There is no network code in the image or video paths. The optional AI critique (crate `lumen-ai`, feature `ai`) is off by default, LAN-only by default, previews every request, and sends only re-encoded pixels the user chooses. `--no-default-features --features video` builds an exe with no network code.

## Performance budgets (release build)

| Scenario | Budget |
|---|---|
| Cold start to first frame | under 1.5 s |
| Idle CPU | 0% (no repaint when nothing changes) |
| Browser frame, 10,000-file folder | under 4 ms |
| Next image (preloaded) | under 16 ms |
| Open a 100 MP JPEG | under 1.5 s to full resolution, instant preview |

The dev hook (`LUMEN_DEV_DIR`) provides `bench <frames>` (per-section frame timings) and startup milestones in `state` output for verifying these budgets.

## Testing

Unit tests cover pure logic (sorting, metadata patching, encoders round-trip, metrics, edit operations, rename planning, keymap). UI flows are exercised through the dev automation hook, which can run commands, open files, and capture window screenshots from inside the app.

Most unit tests live in `lumen-core` next to the code they cover. Run everything with `cargo test --workspace --no-default-features`; `cargo test -p lumen-core` runs only the core tests and does not build the UI.
