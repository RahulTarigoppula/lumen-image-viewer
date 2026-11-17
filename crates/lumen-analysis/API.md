# lumen-analysis: integration guide

Objective image and video quality metrics for Lumen. Pure Rust: no egui, no
FFmpeg. The app hands in decoded pixels, picks metrics by id, runs a job and
reads back an `AnalysisReport` plus heatmaps. Everything below is public API;
you should not need the internals.

```toml
# app Cargo.toml
lumen-analysis.workspace = true
```

```rust
use lumen_analysis::{metrics, Analyzer, ImageSpec, VideoSpec, JobEvent, FrameView, RgbView, RgbData,
                     YuvView, Matrix, Range, Transfer, OwnedFrame, SharedFrame, Roi, Scope, FrameRange,
                     AnalysisReport, ColorizeOptions, RangeMode, Colormap};
// SharedFrame = Arc<dyn FrameSource>
```

## 1. Frames

A `FrameView<'a>` borrows pixels without copying. Two kinds:

- `FrameView::Rgb(RgbView)`: interleaved RGB or RGBA (alpha ignored), samples
  `u8`, `u16` or `f32`, a row stride in samples, and a `Transfer`
  (`Srgb` default, `Linear`, `Pq`, `Hlg`).
- `FrameView::Yuv(YuvView)`: native planes (Y, optional Cb/Cr), `u8` or
  `u16` samples, `bit_depth`, `Matrix` (`Bt601`/`Bt709`/`Bt2020`), `Range`
  (`Limited`/`Full`), `Transfer`. Chroma subsampling is derived from the
  plane sizes (4:4:4, 4:2:2, 4:2:0, 4:4:0, 4:1:1, odd sizes round up).

### Images (display RGBA8)

Use the decoded, ICC-converted 8-bit sRGB pixels Lumen displays
(`image::RgbaImage` or the loader's buffer):

```rust
let view = FrameView::Rgb(RgbView::rgba8(img.as_raw(), w as usize, h as usize)?);
// strided buffer: RgbView::new(RgbData::U8(bytes), w, h, stride_in_samples, 4)?
```

For jobs that outlive the borrow, pass shared frames (`SharedFrame =
Arc<dyn FrameSource>`):

```rust
let a: SharedFrame = Arc::new(OwnedFrame::rgba8(w, h, rgba_bytes));      // takes the Vec
// or wrap your own buffer without copying:
struct Loaded(Arc<LoadedImage>);
impl lumen_analysis::FrameSource for Loaded {
    fn view(&self) -> FrameView<'_> {
        FrameView::Rgb(RgbView::rgba8(&self.0.rgba, self.0.w, self.0.h).unwrap())
    }
}
```

When A and B differ in size, metrics refuse (`Mismatch`). Scale B to A in
the app (Lanczos3 via `fast_image_resize`) and set `scope.rescaled = true`.
For 16-bit / HDR sources measured on 8-bit display data, set
`scope.measured_on = Some("8-bit display data".into())`.

### Video (native YUV planes)

`lumen_core::video::metrics::YuvFrame` already implements `FrameSource`
(BT.709 limited range assumed), so decoded frames go straight in:

```rust
let a: SharedFrame = Arc::new(yuv_frame_a);            // or an existing Arc<YuvFrame>
let view = yuv_frame_a.analysis_view().unwrap();       // borrowed FrameView
```

From raw planes (e.g. an FFmpeg frame, stride in samples):

```rust
use lumen_analysis::frame::{Plane, PlaneView};
let y = PlaneView::U8(Plane::new(y_bytes, w, h, y_stride)?);
let u = PlaneView::U8(Plane::new(u_bytes, (w + 1) / 2, (h + 1) / 2, u_stride)?);
let v = PlaneView::U8(Plane::new(v_bytes, (w + 1) / 2, (h + 1) / 2, v_stride)?);
let yuv = YuvView::new(y, Some((u, v)), 8)?.with_color(Matrix::Bt709, Range::Limited, Transfer::Srgb);
// 10-bit: PlaneView::U16(...) with bit_depth 10. Gray: YuvView::new(y, None, 8).
let view = FrameView::Yuv(yuv);
```

Native-plane metrics (`psnr`, `ssim.fast`) need A and B with the same
layout (size, bit depth, subsampling). Color metrics convert YUV to RGB with
the frame's matrix and range.

### ROI

`Roi { x, y, w, h }` in full-resolution pixels. It is applied as a strided
sub-view (no copy). For subsampled YUV it is snapped outward to the chroma
grid; the ROI actually used is reported in `report.scope.roi` and by
`FrameView::crop(roi) -> (view, used_roi)`. Results equal cropping first and
computing on the crop. Heatmaps of a ROI run are positioned in full-frame
coordinates (see section 5).

## 2. Metrics

Registry: `lumen_analysis::metrics::all()` (frame metrics, display order),
`metrics::get(id)`, `metrics::temporal::all()`, and presets
`metrics::preset("quick" | "encode_qa" | "photo_technical")`. Each metric
has `info()`: `id`, `name`, `kind` (FullReference / NoReference /
Temporal), `version` (part of cache keys), `min_size`, `cost_ms(w, h)`
(estimate for the UI), `heavy`, `has_map`, `direction`, `headline` and
`description`. `headline` lists the keys of the headline value in order of
preference (`metrics::headline_key(id, keys)` picks one, `MetricResult::main()`
returns it); it is never just the first value, which for PSNR on RGB is R.

Values are reported as `<id>.<key>`. dB values are capped at 100
(`PSNR_CAP_DB`), never infinity. "levels" are 8-bit-equivalent code values.

| id | kind | keys (headline in bold) | map |
|---|---|---|---|
| `psnr` | FR | YUV: **`y`** `u v avg`, `mse_*`, `maxabs_*` (code values); RGB: `r g b` **`avg`** ... | - |
| `psnr.rgb` | FR | `r g b` **`avg`**, `mse_*`, `maxabs_*` (YUV converted, 8-bit levels) | - |
| `ssim.fast` | FR | **`y`**, YUV also `u v all`, `y_db` (FFmpeg/x264 8x8 stride 4) | - |
| `ssim.gauss` | FR | **`y`**, `db`, `cs` (11x11 Gaussian, matches scikit-image) | SSIM, inverted magma |
| `ms_ssim` | FR | **`y`**, `cs1..cs4`, `ssim5` (needs 176x176) | - |
| `deltae.76` / `.94` / `.2000` | FR | **`mean`** `median p95 max above_jnd_pct` (Delta-E > 2.3) | Delta-E |
| `diff.abs` | FR | **`mean`** `p99 max changed_pct` (RGB: largest channel; YUV: luma) | abs diff |
| `diff.signed` | FR | **`mean`** `std min max` (luma B - A) | signed, RdBu |
| `ssimulacra2` (feature) | FR, heavy | **`score`** (100 = identical, higher is better) | - |
| `butteraugli` (feature) | FR, heavy | **`score`** (max-norm, below 1 good), `pnorm3` | diffmap |
| `flip` (feature) | FR, heavy | **`mean`** `median max` (0..1, LDR, 67 ppd) | error map |
| `sharpness.laplacian` | NR | **`p90`** (the sharpest tiles, 90th percentile of the tile variances) `var` (whole frame), levels^2 | tiles |
| `sharpness.tenengrad` | NR | **`mean_g2`**, `edge_pct` | tiles |
| `noise` | NR | **`y`** `cb cr` (Immerkaer sigma, levels, edges masked) | - |
| `visual_noise` | NR | **`index`** `l chroma a b` (CIELAB units), `flat_pct` + a text note | 64 px tiles, each measured tile's index |
| `exposure` | NR | **`clip_pct`** (the larger of) `clip_lo_pct clip_hi_pct`; RGB also `clip_lo_{r,g,b}_pct clip_hi_{r,g,b}_pct clip_lo_any_pct clip_hi_all_pct`; `l_mean l_median l_p0_5 l_p1 l_p5 l_p95 l_p99 l_p99_5`, `dr_stops`, `zone_0..zone_10` (%) | 32 px tiles, clipped share in % (+ highlights, - shadows) |
| `color_cast` | NR | **`delta_e`** (Delta C*) `a b hue neutral_pct gray_world` + a text note | 64 px tiles, Delta C* of each tile's near-neutral pixels |
| `blockiness` | NR | **`score`** `ratio_h ratio_v offset_x offset_y` | tiles |
| `banding` | NR | **`score`** `smooth_pct contour_pct` | 64 px tiles, each tile's score (clear below 1%) |
| `temporal.flicker` | T | **`flicker`** `delta mean_luma` | - |
| `temporal.freeze` | T | **`duplicate`** `mad run` | - |
| `temporal.scene` | T | **`score`** `cut` | - |

The three heavy metrics come from pure-Rust crates behind cargo features
`ssimulacra2` (fast-ssim2, BSD-2-Clause), `butteraugli` (BSD-3-Clause) and
`flip` (flip-rs, BSD-3-Clause), all on by default; they cost roughly 0.2 s
at 1080p and 0.8 to 1 s at 4K, so image jobs emit a `Partial` result on a
1/4-area preview first (`ImageSpec.preview_first`, frames of 1 MP and up).
Keep them out of video range jobs unless the user asks. Their licenses need
entries in THIRD-PARTY-NOTICES.md.

### Camera metrics (no reference)

For comparing phone cameras on real scenes; all are in the
`photo_technical` preset.

- `visual_noise`: noise measured where it is visible, on flat areas. Per
  64 px tile (`MetricOptions::tile`) a plane is fitted to L*, a* and b* and
  removed (gradients such as sky are not noise); the residual standard
  deviations are the tile's noise. Flat tiles: under 1% clipped, L*
  residual at most twice the cleanest tenth's (never under 0.3) and at most
  6 L*. Pooled over the flat tiles: `l` (L* std), `a`, `b`, `chroma` =
  sqrt(var a* + var b*) and the headline `index` = sqrt(var L* + 0.852 var
  a* + 0.323 var b*) (CPIQ / ISO 15739 weights, no contrast sensitivity
  filter or viewing model, so not a certified CPIQ value). `flat_pct` is the
  share of the picture measured; 0 means no flat area was found and the
  value comes from the least textured tenth (a note says so; the summary
  does not grade it). Lower is better; about 1 is hard to see at 100%.
- White balance on neutral areas is `color_cast` (extended, version 2): mean
  a*, b* of near-neutral pixels (chroma < 10, 20 < L* < 90, which also keeps
  clipped pixels out), `delta_e` = Delta C* of that mean from gray, `hue` in
  degrees (0 red, 90 yellow, 180 green, 270 blue,
  `metrics::noref::cast::describe(hue)` gives words), `neutral_pct` used and
  `gray_world` = 1 when fewer than 2% were neutral and all pixels were
  averaged instead. The map shows the cast per tile of the neutral pixels,
  so mixed light shows up.
- Clipping per channel is `exposure` (extended, version 4): a sample is
  clipped within one 8-bit level of either end (>= 254 / <= 1 in 8-bit
  terms, also for 16-bit and float RGB; YUV luma within a level of 16 / 235
  or 0 / max). `clip_hi_pct` is any channel high, `clip_lo_pct` every
  channel low, `clip_hi_all_pct` / `clip_lo_any_pct` the other readings,
  `clip_{lo,hi}_{r,g,b}_pct` each channel. The map is the clipped share of
  each 32 px tile in percent, positive for highlights, negative for shadows
  (whichever is larger); `find_regions` reads it for clipped areas.

### Just-noticeable differences

`lumen_analysis::jnd` words the gap between two images' values of one metric
in approximate JNDs. Rules of thumb, deliberately conservative, never
pass / fail limits: Delta-E 2000 / 94 2.0 per JND, Delta-E 76 and
`color_cast.delta_e` 2.3, PSNR 1.5 dB (nothing above 45 dB), `visual_noise`
and `noise` 25% relative change (on value + 0.5 CIELAB / + 1 level).

```rust
use lumen_analysis::jnd;
jnd::jnds("visual_noise.index", a, b);          // Some(2.3): positive when B is worse
jnd::describe_gap("visual_noise", 1.0, 2.0);    // Some("B is about 2 JND noisier than A")
jnd::describe_gap_named("color_cast.delta_e", 7.0, 1.0, "C", "D"); // "C is about 3 JND more tinted than D"
jnd::step_at("visual_noise.index", best);        // metric gap of about 1 JND near `best`
```

Keys are full (`"deltae.2000.mean"`) or a bare metric id for its headline;
metrics without a rule (sharpness, counts, shares) return `None`.

Full-reference metrics are skipped (with a report note) when B is missing;
no-reference metrics run on A. Temporal metrics run only in video jobs; with
B present they also report B's sequence with a `_b` suffix
(`temporal.freeze.duplicate_b`).

`MetricOptions` (in `ImageSpec.options` / `VideoSpec.options`): `want_map`
(image default true, video default false), `tile` (64), `ssim_downsample`
(Wang auto downsampling, off), `tenengrad_threshold`, `noise_mask_edges`
(true), `diff_threshold`, `freeze_eps` (0.1 levels), `scene_threshold` (10).

## 3. Image job

```rust
// Once, e.g. in the app state. Pool: cores - 1 threads, lowest thread priority (the app passes cores - 2).
let ctx2 = ctx.clone();
let analyzer = Analyzer::new(None, Some(Arc::new(move || ctx2.request_repaint())));

let spec = ImageSpec {
    metrics: metrics::preset("encode_qa").iter().map(|s| s.to_string()).collect(),
    roi: Some(Roi::new(x, y, w, h)),          // or None
    scope: Scope { file_a: Some(path_a), file_b: Some(path_b), ..Default::default() },
    ..Default::default()                      // options, preview_first: true
};
let mut job = analyzer.submit_image(spec, a, Some(b));   // JobHandle; B optional
```

Each UI frame:

```rust
while let Some(ev) = job.try_recv() {
    match ev {
        JobEvent::Partial(r) => { /* preview result of a heavy metric (r.preview) */ }
        JobEvent::Metric(r)  => { /* one finished metric: r.id, r.values, r.map */ }
        JobEvent::Done(report) => { /* final AnalysisReport */ }
        JobEvent::Failed(msg) => { /* e.g. unknown metric id */ }
        JobEvent::Cancelled => {}
        JobEvent::Row(_) => {}                // video only
    }
}
let p = job.progress();   // p.done(), p.total(), p.fraction(), p.eta()
job.cancel();             // or drop the handle: dropping an unfinished job cancels it
```

A new `submit_image` cancels the previous image job (same for video).
`job.generation` vs `analyzer.latest_generation()` tells whether a handle is
stale. Blocking variant (tests, batch): `job.wait() -> Result<AnalysisReport, String>`
(or `job.wait_timeout(limit)`, which gives up and cancels after `limit` without an event),
or `job::run_image(&pool, &spec, &view_a, Some(&view_b), &JobControl::default(), &|ev| ...)`.

## 4. Video range job

The app decodes (with its own threads, range, stride, keyframes-only) and
feeds `(frame_index, A, Option<B>)` in display order. Any `IntoIterator`
works; a bounded crossbeam channel from the decoder is the usual choice:

```rust
let (tx, rx) = crossbeam_channel::bounded::<(u64, SharedFrame, Option<SharedFrame>)>(8);
// decoder thread: for each wanted frame, tx.send((idx, Arc::new(fa), Some(Arc::new(fb))))
let spec = VideoSpec {
    metrics: vec!["psnr".into(), "ssim.fast".into(), "temporal.freeze".into(), "temporal.scene".into()],
    total_frames: Some(n_frames),             // progress / ETA
    roi: None,
    scope: Scope { file_a: Some(a_path), file_b: Some(b_path),
                   frames: Some(FrameRange { start, end, stride }), ..Default::default() },
    ..Default::default()                      // memory_budget: 1 GB
};
let mut job = analyzer.submit_video(spec, rx);
```

- Frames are processed in parallel (one task per frame) and reordered; you get
  one `JobEvent::Row(FrameResult { idx, values })` per frame in input order,
  then `Done(report)`.
- In-flight frames are capped at `(memory_budget - queued_frames x frame
  bytes) / (bytes of A + B + their derived images)` (and at the pool size
  plus 2); the iterator is not pulled faster than that, so a bounded channel
  back-pressures the decoder. `queued_frames` (default 12) is what your side
  holds: the channel's pairs and frames your decoders have handed over.
- Temporal metrics compare consecutive frames *as fed*; with stride > 1 they
  measure the sampled sequence.
- Cancel with `job.cancel()`: in-flight frames finish within one frame of
  latency and the job ends with `Cancelled`.

## 5. Results

`AnalysisReport` (serde, schema `"lumen.analysis/1"`):

- `scope`: `kind` (image / video_frame / video_range), files, `width`,
  `height`, `roi` used, `frames`, `rescaled`, `measured_on`.
- Image: `metrics: Vec<MetricEntry>` in registry order (`id`, `name`,
  `version`, `values: Vec<ReportValue { key, value: Option<f64>, unit, direction }>`,
  `notes`, `elapsed_ms`, `map`). Helpers: `report.value("ssim.gauss.y")`,
  `report.metric("noise")`, `report.map("deltae.2000")`.
- Video: `columns` (key, unit, direction), `frames: Vec<FrameRow { idx,
  values }>` aligned to columns, `summary` per column (`count mean min max
  worst_idx`) plus `psnr.global` (PSNR of the mean MSE). Helpers:
  `series(key)`, `summary_of(key)`, `worst_frames(key, n)` (lowest first when
  higher is better).
- `notes`: skipped metrics and why (too small, no reference, budget).
- Undefined values are `None` (JSON `null`, empty CSV cell). `Unit::label()`
  and `Unit::decimals()` give display text; `Direction` says which way is good.

## 6. Heatmaps

`Heatmap { width, height, data: Vec<f32>, label, unit, fixed_range, min_span,
colormap, invert, pool, x0, y0, cell_w, cell_h }`. Cell `(i, j)` covers image
pixels `x0 + i * cell_w ..` (frame coordinates, ROI offset included). SSIM
maps start at (5, 5) (valid windows); tile maps have 64 px cells (the exposure clip map 32 px); NaN means
"no value".

```rust
let m = report.map("ssim.gauss").unwrap().for_display();  // max side <= 4096, worst value kept
let opts = ColorizeOptions { range: RangeMode::Auto, alpha: 160, ..m.default_options() };
let rgba = m.colorize(&opts);                       // width * height * 4 bytes, NaN transparent
let tex = ctx.load_texture("heat", egui::ColorImage::from_rgba_unmultiplied(
    [m.width as usize, m.height as usize], &rgba), egui::TextureOptions::NEAREST);
let (x, y, w, h) = m.extent();                      // where to draw it, in image pixels
let hover = m.value_at(img_x, img_y);               // Option<f32> for the readout
let bar = lumen_analysis::heatmap::legend(opts.colormap, 256, opts.invert);
```

- `RangeMode::Auto`: maps with an ideal value (`min_span > 0`: differences,
  Delta-E, SSIM, perceptual maps) run from that value (the clean end of
  `fixed_range`) to the 99th percentile (SSIM: 1st), at least `min_span` wide,
  so near-identical images look nearly clean and real differences still fill
  the colormap; other maps p1..p99. Symmetric for signed maps. `Fixed` = the
  metric default (`fixed_range`: Delta-E 0..10, abs diff 0..64 levels, signed
  diff -32..32, SSIM 0.7..1, Butteraugli 0..3, FLIP 0..1), `User { lo, hi }`.
- `threshold: Some(t)` draws only cells at or beyond `t` (below `t` when
  `invert`), the rest transparent.
- Colormaps: `Magma` (default), `Viridis`, `Turbo`, `Gray`, `RdBu`
  (`Colormap::ALL`, `name()`, `lut()`).

## 7. Export

```rust
report.write_json(&path)?;    // pretty JSON, atomic (temp file + rename)
report.write_csv(&path)?;     // "#" comment header with scope, then rows
m.write_png(&path, &opts)?;   // colorized RGBA PNG
m.write_tiff(&path)?;         // raw values, 32-bit float gray TIFF
```

CSV: video reports have `frame,<column>...` with one row per frame; image
reports have `key,value,unit,direction` rows. dB is capped at 100.0000.

## 8. Cache

```rust
use lumen_analysis::cache::{CacheKey, DiskCache, DEFAULT_MAX_BYTES};
let cache = DiskCache::new(settings::cache_dir().join("analysis"), DEFAULT_MAX_BYTES); // 200 MB LRU
let key = CacheKey::builder()
    .file("a", &path_a).file("b", &path_b)          // canonical path, size, mtime
    .metric("ssim.gauss", metrics::get("ssim.gauss").unwrap().info().version)
    .params(&spec.options).roi(spec.roi).range(None)
    .finish();
if let Some(r) = cache.get::<AnalysisReport>(&key) { /* history hit, maps are not stored */ }
cache.put(&key, &report)?;    // evicts least recently used entries beyond the limit
cache.clear();                // Settings > Privacy
```

## 9. Notes

- Never call blocking functions (`run_image`, `run_video`, `wait`) on the UI
  thread; use `Analyzer` and poll `try_recv`.
- Jobs are panic-isolated: a panic inside a metric ends the job with `Failed`.
- `lumen_core::video::metrics::{mse_plane, ssim_plane, compare}` now delegate
  to this crate, so existing callers get the faster kernels unchanged.
- Golden references: `tools/golden/gen_ssim_golden.py` (scikit-image SSIM and
  PSNR, numpy MS-SSIM) writes `tests/golden/`; `tools/golden/gen_colormaps.py`
  regenerates `src/colormaps.rs`. Timings:
  `cargo test -p lumen-analysis --release --test perf -- --ignored --nocapture`.
