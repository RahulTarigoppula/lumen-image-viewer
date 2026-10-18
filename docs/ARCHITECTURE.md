# Lumen architecture

Lumen is a single native Windows executable written in Rust. The UI is
immediate-mode (egui 0.37 on OpenGL via eframe); every expensive operation runs
off the UI thread so the interface stays at display refresh rate on folders with
tens of thousands of files and on images of hundreds of megapixels.

## Module map

| Area | Modules | Responsibility |
|---|---|---|
| Shell | `main.rs`, `app.rs`, `shell.rs`, `devhook.rs` | Startup, single instance (named pipe), mode switching, command dispatch, dialogs, dev automation hook |
| Commands and input | `ui/commands.rs`, `ui/keymap.rs`, `ui/palette.rs` | One `Command` enum drives menus, toolbar, shortcuts and the Ctrl+K palette; remappable keymap with presets |
| Browser | `ui/browser.rs`, `ui/tree.rs`, `catalog.rs`, `tags.rs` | Folder listing, sorting, filtering, grid and details views, folder tree, ratings/flags/labels |
| Viewer | `ui/viewer.rs`, `render.rs`, `loader.rs` | Zoom/pan, magnifier, crop, tool hosting, animation, info panel; tiled GPU renderer; decode cache with preloading |
| Decoding | `decode/*`, `formats.rs`, `meta.rs` | Pure-Rust decoders, WIC fallback (HEIC/AVIF/RAW/JXL), ICC to sRGB, EXIF |
| Encoding | `encode.rs` | JPEG/PNG/WebP/TIFF/... with metadata preservation, atomic writes, lossless JPEG orientation |
| Editing | `edit/*` | `EditTool` trait, undo session, filters, effects, geometry, tone curves, text, draw board, retouch |
| Batch and tools | `ui/batch*`, `ui/tools*` | Batch convert pipeline, batch rename, contact sheet, duplicates, output tools, printing |
| Modes | `ui/slideshow.rs`, `ui/compare.rs`, `video/*` | Slideshow with GPU transitions, 2-4 image compare, video playback and A/B analysis |
| Video | `video/engine.rs`, `video/audio.rs`, `video/metrics.rs`, `video/scopes.rs`, `video/analyze.rs`, `video/mode.rs` | FFmpeg demux/decode worker, WASAPI audio master clock, PSNR/SSIM, scopes, whole-file analysis, UI |

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

There is no network code in the image or video paths. The optional AI critique feature (see `docs/ANALYSIS-PLAN.md`) is off by default, LAN-oriented, and sends only pixels the user explicitly chooses.

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
