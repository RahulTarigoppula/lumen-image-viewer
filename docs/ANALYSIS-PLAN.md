# Lumen Analysis: Objective Metrics and Remote AI Critique (Plan)

Status: architectural plan, October 2026. Target: Rust 1.99 MSVC, eframe/egui 0.37 (glow), single portable `lumen.exe`, offline by default. This document makes decisions; it contains no code beyond payload and signature sketches.

Builds on: `docs/VIDEO-RESEARCH.md` (metric math sections 2.1 to 2.5), `src/video/metrics.rs` (PSNR and FFmpeg-style SSIM on native YUV planes), `src/video/analyze.rs` (whole-file A/B job over `SeqDecoder`), `src/video/scopes.rs`, `src/edit/mod.rs` (`Histogram`), `src/ui/compare.rs` (RGBA difference and `DiffStats`), `src/ui/viewer.rs` (`CropState` with drag handles).

---

## 0. Decisions at a glance

| Decision | Choice | Why |
|---|---|---|
| Where metric code lives | New workspace crate `crates/lumen-analysis` (no egui, no FFmpeg) | Pure and testable; app keeps adapters |
| Where AI code lives | New crate `crates/lumen-ai` (behind cargo feature `ai`, default on, runtime opt-in off) | Only network code in the app; easy to audit |
| v1 FR metrics | PSNR (YUV and RGB), SSIM (fast and Gaussian), MS-SSIM, CIEDE2000 (plus CIE76/94), diff/SSIM/Delta-E heatmaps | All in-house pure Rust, cheap, well-defined golden values |
| v2 FR metrics | SSIMULACRA2 via `fast-ssim2` 0.9, butteraugli via `butteraugli` 0.9.3, FLIP via `flip-rs` 0.1.2, VMAF + CAMBI via `vmaf-head-sys` 0.2 (feature `vmaf`) | Pure Rust ports with reported parity; VMAF is the one C dependency, statically linked |
| VMAF fallback | Sidecar `ffmpeg.exe` with libvmaf if the user points to one | No build-time dependency when feature off |
| Neural inference (LPIPS, later learned NR) | `tract-onnx` 0.23 behind feature `nn` | Pure Rust, no DLL, no CPU baseline issue; ONNX Runtime (`ort` 2.0.0-rc.14) adds a large static lib and requires x86-64-v3 |
| Learned NR models (MUSIQ, TOPIQ, CLIP-IQA, MANIQA) | Not bundled; user-supplied ONNX only ("later") | pyiqa and most weights are non-commercial licensed; sizes 100 to 500 MB |
| BRISQUE / NIQE / PIQE | In-house pure Rust ports (v2) with embedded parameter tables | No Rust crate exists; parameters are small and permissively licensed |
| HTTP client | `ureq` 3.4 on a worker thread, `rustls` + `platform-verifier` | Blocking API fits a sync egui app; no tokio; about 1 to 1.5 MB |
| API key storage | Windows Credential Manager (`CredWriteW`) via existing `windows` crate; DPAPI file fallback | OS-encrypted per user, visible and removable by the user |
| Server settings | Host/IP + port (default 8000) + optional API key + HTTPS toggle; base URL derived as `http(s)://<host>:<port>/v1` | Matches how users run `vllm serve` on a LAN box |
| Structured output | `response_format: {type: "json_schema"}` (vLLM structured outputs; `guided_json` was removed in vLLM 0.12) | OpenAI-compatible, works with xgrammar/guidance |
| Default VLM recommendation | `Qwen/Qwen3.5-9B` (Apache-2.0, 24 GB GPU); IQA scorer `q-future/Q-ReAlign-Lite-4B` | Current, vLLM-supported, permissive |

---

## 1. Starting point in the codebase

- Video metrics already run on native planes (`YuvFrame`, `Plane`, `PlaneData::{U8,U16}`), with PSNR per plane, FFmpeg-weighted average, FFmpeg/x264 SSIM (8x8 windows, stride 4) and `scdet`-style scene score. `analyze::run` decodes A and B on two threads and streams `FrameMetrics` rows. It has a cancel flag but no frame range, ROI, or stride.
- `ssim_plane` reads through `Plane::get()` (a match per sample) and accumulates in `f64`; it is correct but about 5 to 10x slower than an integer, slice-typed version. Port it, do not wrap it.
- Image compare computes `|A - B|` on display RGBA 8-bit with `DiffStats` (mean, max, MSE, PSNR, changed %). It requires equal dimensions.
- The viewer already has a crop rectangle editor (`CropState`, handles, aspect lock). It is the basis for ROI selection; extract it into a shared `ui/roi.rs` widget.
- Settings live in `settings::data_dir()/settings.json`; cache in `settings::cache_dir()`.

---

## 2. Local objective analysis

### 2.1 Data model

`lumen-analysis` works on borrowed views, never on egui types:

- `FrameView` enum: `Yuv { planes: [PlaneView; 3], bit_depth, subsampling, matrix, range }` (from `YuvFrame`) or `Rgb { data: &[u8] | &[u16] | &[f32], width, height, stride, transfer: Srgb | Linear | Pq | Hlg }`.
- `Roi { x, y, w, h }` in luma/full-res pixel coordinates. For 4:2:0 and 4:2:2 the ROI is snapped outward to even coordinates and mapped to chroma by shifting. ROI is applied as a strided sub-view (no copy).
- `Derived` cache per frame (lazily computed, shared by all metrics in one run): luma `f32`, linear RGB `f32`, CIE Lab `f32` (D65). SSIMULACRA2 and butteraugli compute their own XYB internally. Each metric declares the input it needs (`InputKind::{NativePlanes, Luma, SrgbU8, LinearRgb, Lab}`), and the runner fills only what is required.
- Images: use the decoded 8-bit sRGB pixels after ICC conversion (what Lumen displays). Flag 16-bit and HDR sources as "measured on 8-bit display data" until a high-bit decode path exists (later).
- Video: native planes for PSNR/SSIM/MS-SSIM/VMAF; BT.601/709/2020-correct conversion to RGB (existing shader math, CPU path via `yuv` 0.8) for Delta-E, FLIP, SSIMULACRA2, butteraugli.
- Size mismatch (A vs B): refuse by default; offer "scale B to A" (Lanczos3 via `fast_image_resize` 6, already a dependency) and mark every result "B rescaled".

### 2.2 Full-reference (A vs B) metrics

| Metric | Version | Implementation | Reference / golden source | Est. cost per 4K frame (8-core AVX2) |
|---|---|---|---|---|
| PSNR Y/U/V/avg, RGB, per-channel; MSE, max abs | v1 | Port existing code; integer accumulation per row band | FFmpeg `psnr` filter | 2 to 4 ms |
| SSIM fast (8x8/stride 4) | v1 | Port existing; integer 4x4 block sums in two sliding rows | FFmpeg `ssim` filter | 5 to 8 ms |
| SSIM Gaussian 11x11, sigma 1.5 (Wang) + SSIM map | v1 | Separable f32 filter on 5 images in row strips with halo | scikit-image `structural_similarity(gaussian_weights=True, sigma=1.5, use_sample_covariance=False)` | 30 to 60 ms |
| MS-SSIM (5 scales, Wang 2003 weights) | v1 | Reuses Gaussian SSIM kernel per scale; clamp negative cs | libvmaf `float_ms_ssim`, TF `ssim_multiscale` | 45 to 80 ms |
| Delta-E CIE76 / CIE94 / CIEDE2000, map + mean/p95/max | v1 | sRGB -> linear via 256-entry LUT -> XYZ -> Lab (D65); CIEDE2000 per Sharma 2005; SoA f32 loops | Sharma, Wu, Dalal test set (34 pairs); `colour-science`; `palette` 0.7 as dev-dep oracle | CIE76 10 ms; CIEDE2000 50 to 80 ms |
| Abs/signed difference maps | v1 | Existing `difference` generalized to planes and ROI | trivial | 3 ms |
| SSIMULACRA2 + diffmap | v2 | `fast-ssim2` 0.9.0 (BSD-2, safe Rust, SIMD, `compute_ssimulacra2_maps`) | libjxl `ssimulacra2` tool | 1.0 to 1.5 s |
| Butteraugli (max, 3-norm) + diffmap | v2 | `butteraugli` 0.9.3 (BSD-3, pure Rust, validated vs libjxl) | libjxl `butteraugli_main` | about 0.9 s (README: 845 ms single thread at 3840x2160) |
| FLIP (LDR, HDR) + error map | v2 | `flip-rs` 0.1.2 (BSD-3, pure Rust, exact match on 201-case corpus vs FLIP v1.7) | NVIDIA `flip` Python/C++ | 0.2 to 0.5 s with rayon |
| VMAF v0.6.1 / 4K / NEG, plus CAMBI banding | v2 (feature `vmaf`) | `vmaf-head-sys` 0.2.0 (vendored libvmaf, static, MSVC in CI, built-in models) | libvmaf `vmaf` CLI | 50 to 120 ms (libvmaf AVX2, multithreaded) |
| LPIPS (AlexNet) | later (feature `nn`) | `tract-onnx` 0.23; exported LPIPS-alex ONNX (about 10 MB fp32, 5 MB fp16) | `lpips` Python package | 1 to 3 s full 4K; run on ROI or 1024-side tiles |
| PSNR-HVS-M, IW-SSIM | later | `iqa` 1.2.1 pure-Rust modules with `default-features = false` (its default pulls C++ bindings) | Ponomarenko MATLAB | 100 to 300 ms |

Rejected: `dssim` / `dssim-core` (AGPL-3.0, incompatible with an MIT exe); `nv-flip` 0.1.2 (C++ bindings, stale since 2023, superseded by `flip-rs`); `ssimulacra2` 0.5.1 (rust-av, last release Dec 2024, slower, no diffmap); `oximedia-quality` ("VMAF-like", no parity claim with libvmaf).

**VMAF decision.** No pure-Rust VMAF port with libvmaf parity exists in late 2026 (searches and crates.io show only bindings). The vcpkg `libvmaf` port is still `!windows`. `vmaf-head-sys` (xnorpx/rust-vmaf) vendors libvmaf master (commit f85a853, 2026-08-14; upstream release v3.2.1, 2026-09-14), statically links it, embeds models via `built-in-models`, and its CI builds `x86_64-pc-windows-msvc`. Build needs Meson, Ninja, NASM 2.14+ and `xxd` (ships with Git for Windows). Recommendation: put it behind cargo feature `vmaf` (on in release CI, off for quick local builds). Exe cost: about 2 to 4 MB. Feed it native Y planes (8 or 10 bit) from `SeqDecoder` directly. Keep the `ffmpeg.exe` sidecar (gyan.dev full builds include libvmaf) as a runtime fallback when the feature is off. A native Rust port (VIF + ADM + motion + SVR, 2 to 4k lines) stays "later, only if the C build becomes a burden".

### 2.3 No-reference diagnostics (single image or frame)

All v1 items are in-house, pure Rust, on luma f32 (or Lab), ROI-aware.

| Diagnostic | Version | Algorithm | Output | Est. 4K cost |
|---|---|---|---|---|
| Sharpness: variance of Laplacian | v1 | 3x3 Laplacian on luma, variance (Pech-Pacheco 2000) | scalar + per-tile map (64 px tiles) | 5 ms |
| Sharpness: Tenengrad | v1 | Sobel gradient magnitude squared, mean above threshold (Krotkov 1987) | scalar + tile map | 10 ms |
| Edge width (blur) | v2 | Marziliano 2002 edge width along vertical Sobel edges (same idea as FFmpeg `blurdetect`) | mean edge width px | 20 ms |
| MTF50 slanted edge | later | ISO 12233 e-SFR on a user ROI containing a slanted edge (sfrmat algorithm) | MTF curve, MTF50 cy/px | 10 ms per ROI |
| Noise: Immerkaer fast noise variance | v1 | Convolve with [1,-2,1; -2,4,-2; 1,-2,1], sigma = sqrt(pi/2) / (6 (W-2)(H-2)) * sum abs; mask out edges (Sobel above p90) to avoid texture bias | sigma for Y, Cb, Cr (or L, a, b) | 5 ms per plane |
| Noise: wavelet MAD | v2 | Haar HH1 subband, sigma = median(abs(HH)) / 0.6745 (Donoho 1995), median via 4096-bin histogram | sigma per plane, per-tile map | 15 ms |
| Exposure | v1 | Reuse `Histogram`; add per-channel clip % (0 and 255, or limited-range 16/235 for video), mean/median L*, p0.5/p99.5, dynamic range in stops = log2(p99.5 / max(p0.5, eps)) on linear luminance, 11-zone histogram (Adams zones on L*) | table + histogram widget | 3 ms |
| Color cast | v1 | Mean a*, b* over near-neutral pixels (chroma < 10, L* 20..90), fallback gray-world; magnitude as Delta-E from neutral and hue angle | "cast: +3.1 b* (warm), Delta-E 3.4" | 10 ms (shares Lab) |
| Blockiness | v1 | 8x8 boundary-to-interior gradient ratio, horizontal and vertical, with grid offset search 0..7 (Wang, Sheikh, Bovik 2002 family; FFmpeg `blockdetect` is similar) | score, detected grid offset, per-tile map | 10 ms |
| Ringing | v2 | Energy of small oscillations in a band around strong edges vs flat-area energy | score + map | 25 ms |
| Banding | v1 simple, v2 CAMBI | v1: low-gradient regions with quantized step contours (runs of 1-code-value steps over smooth areas); v2: Netflix CAMBI via libvmaf (`vmaf` feature) | score + mask overlay | v1 15 ms; CAMBI 30 to 60 ms |
| BRISQUE | v2 | MSCN (7x7 Gaussian, sigma 7/6) at 2 scales, GGD + 4 AGGD fits = 36 features, scale with range file, RBF SVR | score 0..100 (lower better) | 60 to 100 ms |
| NIQE | v2 | 96x96 patches, 2 scales, 18 features each (as scikit-video), MVG distance to pristine model | score (lower better) | 120 to 200 ms |
| PIQE | v2 | Block-wise (16x16) MSCN activity, noticeable distortion and noise criteria (Venkatanath 2015); training-free | score 0..100 + block map | 40 ms |
| Temporal (video) | v1 | Frame-to-frame mean luma delta (flicker), exact/near duplicate frames (freeze, MAD < eps), scene score (existing) | per-frame series | 2 ms |

BRISQUE/NIQE parameter provenance: OpenCV contrib `quality` module (Apache-2.0) samples `brisque_model_live.yml` and `brisque_range_live.yml` (LIBSVM model trained on LIVE IQA; several hundred support vectors x 36 features). Convert once with a script in `tools/` to a compact little-endian f32 blob embedded via `include_bytes!` (about 120 KB). NIQE: scikit-video's `niqe_image_params.mat` (`pop_mu`, `pop_cov`, from the LIVE NIQE release, redistributed under its permissive notice); embed as 36 + 36x36 f64 (about 11 KB) and ship the notice in THIRD-PARTY-LICENSES. Golden values for both from reference implementations (scikit-video for NIQE, OpenCV `QualityBRISQUE` for BRISQUE). Note: NIQE needs min side >= 193 px, and both are trained on natural photos; label them "experimental for synthetic/graphics content".

Learned NR models ("later"): MUSIQ (about 27M params, 110 MB), TOPIQ-NR (ResNet50 backbone, about 100 MB), CLIP-IQA+ (CLIP RN50, about 100 MB), MANIQA (ViT-B, about 500 MB). Sizes are approximate. The pyiqa toolbox is PolyForm Noncommercial 1.0.0, and many weights are trained on KonIQ/SPAQ under research-only terms. Decision: never bundle; provide a generic "custom ONNX NR model" slot (input size, normalization, output scalar, all declared in a small sidecar JSON) executed by `tract`. The VLM path (section 3) is the recommended way to get learned perceptual judgements.

Neural runtime decision (for LPIPS and custom models): `tract-onnx` 0.23.9. Reasons: pure Rust, statically compiled, no DLL; adds about 3 to 5 MB. `ort` 2.0.0-rc.14 is faster (roughly 2 to 4x on CNNs), but its prebuilt static ONNX Runtime adds roughly 15 to 30 MB, requires x86-64-v3 (excludes pre-Haswell CPUs), and is still a release candidate. `ort` also offers an `ort-tract` alternative backend, so switching later is cheap if the model code goes through a thin `NnBackend` trait. `candle` 0.11 would require reimplementing each network graph by hand. LPIPS licensing: code BSD-2 (richzhang/PerceptualSimilarity), AlexNet weights BSD-3 (torchvision); bundling the fp16 ONNX (about 5 MB) under feature `nn` is acceptable.

### 2.4 Heatmaps and colormaps

- One `Heatmap { w, h, data: Vec<f32>, range: (lo, hi), units }` type produced by any metric that has a map (abs diff, SSIM, Delta-E, FLIP, SSIMULACRA2 diffmap, butteraugli diffmap, tile maps for NR).
- Colormaps as 256-entry RGBA LUTs: `magma` (default, matches FLIP), `viridis`, `turbo`, `gray`, and a diverging `RdBu` for signed difference. Range modes: auto (p1..p99), fixed (metric-specific defaults, e.g. Delta-E 0..10, SSIM 0.5..1 inverted), user-set.
- Display: upload as an egui texture at map resolution (downsample with max-pooling to at most 4096 px side so hotspots are not averaged away), overlay on the image with an opacity slider, threshold mode (show only pixels above T), and a legend bar. Tile maps are drawn as rectangles directly.
- Hover readout: the value at the cursor plus A/B pixel values (reuse the compare pixel inspector).

### 2.5 Scopes: full frame, ROI, frame range

- **Image:** whole image or ROI.
- **Video current frame:** the paused frame (A, or A and B). Runs instantly on demand.
- **Video range:** `FrameRange { start, end, stride }` in frame indices resolved through the existing index (`build_index`). `SeqDecoder` gets two additions: `seek_to_frame(idx)` (keyframe seek via the index, then decode and discard) and `skip(n)`, which decodes without copying planes (avoid `copy_planes` for skipped frames). Stride > 1 still decodes every frame (inter-frame dependency) but skips metric work; when "keyframes only" is checked, decode only keyframes.
- **ROI on video:** fixed rectangle across the range (no tracking).
- Every result records its scope (`file`, `frames`, `roi`, `rescaled`), so exports are self-describing.

### 2.6 Outputs

- Results panel: one row per metric, with value, units, direction ("higher is better"), and a sparkline for video.
- Video graphs with `egui_plot` 0.37: one plot per metric family, A/B and multiple Bs as series, click to seek, drag-select to set a new frame range, horizontal threshold line.
- Worst-frame list: N lowest per selected metric (default PSNR-Y or VMAF), with thumbnails; click to seek; "export worst frames as PNG pairs".
- Export: CSV (one row per frame, one column per metric, header with scope metadata as `#` comment lines) and JSON (`AnalysisReport` serde struct, versioned `schema: "lumen.analysis/1"`). Heatmap export as PNG (colorized) and as 32-bit float TIFF (raw values) via `image`.
- Per-file history: results cached (section 2.7) and listed in the panel with timestamps; AI critiques stored alongside.

### 2.7 Performance design

- **Thread pools:** a dedicated `rayon::ThreadPool` for analysis (`num_threads = cores - 1`, threads set to `THREAD_PRIORITY_BELOW_NORMAL` via `windows` `SetThreadPriority`) so thumbnails, decode and UI stay responsive.
- **Two granularities:** current-frame and image jobs use intra-frame parallelism (row bands). Whole-range video jobs use frame-level parallelism: decoder threads push `(idx, A, B)` into a bounded channel, workers run the whole metric set single-threaded per frame via `par_bridge`, and results are reordered by index. This scales better for cheap metrics and avoids rayon nesting overhead.
- **SIMD:** keep inner loops on contiguous typed slices (`&[u8]`, `&[u16]`, `&[f32]`), fixed-width chunks of 16/32, no per-sample enum match, integer accumulators where exact (PSNR, fast SSIM). Rely on autovectorization at `opt-level=3`, plus runtime-dispatched `#[target_feature(enable = "avx2,fma")]` wrappers for the 5 hot kernels (separable Gaussian, Lab conversion, CIEDE2000, Laplacian, block sums). Third-party crates (fast-ssim2, butteraugli) already dispatch at runtime.
- **Preview vs final:** heavy metrics (SSIMULACRA2, butteraugli, FLIP, LPIPS, NIQE) first run on a 1/4-area preview (from the existing image pyramid) with a "preview" badge, then on full resolution in the background, replacing the value when done. Cheap v1 metrics always run at full res.
- **Strip processing:** Gaussian SSIM and Lab maps run in horizontal strips of 128 rows plus a 5-row halo. Peak extra memory per 4K frame stays under 20 MB per worker, instead of about 170 MB for five full f32 images.
- **Memory budget:** a job declares its working set from frame size, and the scheduler caps in-flight frames to `budget / (2 * frame_bytes + per_worker_scratch)`. The default total budget is 1 GB. 4K 4:2:0 8-bit is 12.4 MB per frame (A+B 25 MB), so about 8 frame pairs fit in flight. 8K 4:2:0 10-bit stored as u16 is 99.5 MB per frame (A+B 200 MB), so 2 to 3 pairs and workers = 3.
- **Cancellation:** `Arc<AtomicBool>` checked per frame and per strip; crates without cancellation (butteraugli, fast-ssim2) are bounded by one frame of latency. A new request with the same key supersedes the old one (generation counter, as compare's `DiffState` does).
- **Progress:** `AtomicU64` frames done plus an ETA; UI polls each repaint and calls `request_repaint_after(100 ms)` while running.
- **Cache:** key = blake3 of (canonical path, file size, mtime, metric id, metric version, params, roi, range, B identity). Store `cache_dir()/analysis/<key>.json`; LRU by total size (default 200 MB). Heatmaps are not persisted (cheap to recompute), except on explicit export.

### 2.8 Local metric roadmap

- **v1 (ship first):** PSNR (YUV planes and RGB), SSIM fast + Gaussian with map, MS-SSIM, Delta-E 76/94/2000 with map and mean/p95/max, abs/signed diff heatmaps, sharpness (Laplacian variance, Tenengrad), Immerkaer noise per plane, exposure/clipping/DR/zones, color cast, blockiness, simple banding, temporal flicker/freeze/scene; ROI and frame ranges; graphs, worst frames, CSV/JSON, cache.
- **v2:** SSIMULACRA2 (`fast-ssim2` 0.9), butteraugli (`butteraugli` 0.9.3), FLIP (`flip-rs` 0.1.2), VMAF + CAMBI (`vmaf-head-sys` 0.2, feature `vmaf`; sidecar fallback), BRISQUE, NIQE, PIQE, wavelet MAD noise, edge width, ringing.
- **Later:** LPIPS via `tract-onnx` (feature `nn`), custom ONNX NR slot, MTF50 slanted edge, PSNR-HVS-M / IW-SSIM, HDR-aware metrics (HDR-FLIP, PU21-encoded PSNR/SSIM), high-bit-depth image decode path for metrics.

---

## 3. Remote AI subjective analysis (vLLM, OpenAI-compatible)

### 3.1 Privacy model (non-negotiable)

- This is the only network feature in Lumen. It is compiled in (feature `ai`) but **disabled at runtime by default**. Enabling it requires ticking "Enable remote AI analysis" in Settings > AI, which shows a one-paragraph explanation of what is sent.
- **LAN-only by default:** before connecting, resolve the host and allow only loopback, RFC 1918 (10/8, 172.16/12, 192.168/16), link-local, IPv6 ULA (fc00::/7) and, optionally, CGNAT 100.64/10 (Tailscale). Public addresses require unticking "Only allow local network servers" plus HTTPS.
- **No redirects** (`max_redirects(0)`), so data cannot be forwarded to another host. No proxy unless the user sets one in Lumen: build the agent with an explicit `proxy(None)` so `HTTPS_PROXY` in the environment is ignored.
- **Pixels only:** images are re-encoded from decoded, sRGB-converted pixels by Lumen's own encoders (PNG via `image`, JPEG via `jpeg-encoder`), which write no EXIF, XMP, GPS or ICC. File names and paths are not sent unless "Include file name" is ticked. An EXIF summary (camera, lens, exposure; never GPS) is sent only if "Include capture settings" is ticked.
- **Send preview:** the first request to a server shows a confirmation sheet, and so does every request when "Always confirm" is on (the default). The sheet lists the destination URL, the model, each image (thumbnail, pixel size, encoded bytes, and what it is: "A full view, downscaled 2.5x", "A ROI tile 2/4, native"), and the exact text of the system and user prompts. Buttons: Send, Cancel, and "Do not ask again for this server".
- **Indicator:** while a request is in flight, the status bar shows a "Sending to 192.168.1.20:8000" network badge. The result card records exactly what was sent (stored locally only).
- Warn when an API key would travel over plain HTTP to a non-loopback host.

### 3.2 Settings

Stored in `settings.json` under `ai` (the key is never stored there):

| Field | Default | Notes |
|---|---|---|
| `enabled` | false | Master switch |
| `host` | "" | IP or hostname, e.g. `192.168.1.20` or `workstation` |
| `port` | 8000 | vLLM default |
| `https` | false | Base URL becomes `https://host:port/v1` |
| `api_key_ref` | none | `credman` or `dpapi`; secret stored outside JSON |
| `model` | none | Picked from `/v1/models` |
| `preset` | "Quality critique" | See 3.8 |
| `custom_presets` | [] | Name, system prompt, user template, schema on/off |
| `timeout_connect_s` / `timeout_first_token_s` / `timeout_total_s` | 5 / 120 / 600 | First-token covers model warmup and large prefill |
| `max_completion_tokens` | 1500 | |
| `temperature` | 0.2 | Low for repeatable rubric scores |
| `max_side_overview` | 1536 | Long side of downscaled full view |
| `tile_side` | 1024 | Native ROI tile side (multiple of 32) |
| `max_images` | 6 | Per request; must be <= server `--limit-mm-per-prompt` |
| `encoding` | PNG | PNG (lossless, default) or JPEG q95 4:4:4 for slow links |
| `disable_thinking` | true | Sends `chat_template_kwargs.enable_thinking=false` |
| `coord_convention` | relative-1000 | relative-1000 (Qwen3/3.5, Gemma) or absolute pixels (Qwen2.5-VL) |
| `custom_ca_pem` | none | For self-signed HTTPS on the workstation |
| `lan_only`, `always_confirm`, `include_filename`, `include_exif` | true, true, false, false | Privacy |

User flow:
1. Type the IP, and optionally the port and key.
2. Press **Test connection**.
3. Pick a model from the auto-fetched list.
4. Pick a preset and run.

Test connection runs these steps in order:
1. TCP connect with a 5 s timeout.
2. `GET http(s)://host:port/health` (vLLM returns 200 with an empty body).
3. `GET /version` (shows the vLLM version).
4. `GET /v1/models` with the key.
5. Optionally, a "Test vision" probe on the selected model.

Each step reports a precise, human-readable message (connection refused, TLS failure, 401 bad key, no models).

**Secret storage:** use Windows Credential Manager through the existing `windows` 0.62 crate with feature `Win32_Security_Credentials`:
- Save with `CredWriteW`: `CRED_TYPE_GENERIC`, `TargetName = "Lumen/ai/<host>:<port>"`, `Persist = CRED_PERSIST_LOCAL_MACHINE` (per user, survives logoff, not roaming).
- Read with `CredReadW` + `CredFree`; "Forget key" calls `CredDeleteW`.
- The OS encrypts the key with the user's DPAPI key, and the user can see and delete it in Control Panel > Credential Manager.
- Fallback when `CredWriteW` fails (locked-down profiles): `CryptProtectData` (feature `Win32_Security_Cryptography`, `CRYPTPROTECT_UI_FORBIDDEN`, user scope, optional entropy constant) writing to `data_dir()/ai-key.bin`.
- `keyring` 4.x would also work, but it adds a dependency for what is about 80 lines with `windows`.
- Keep the key in memory in a `zeroize`-on-drop wrapper and never log headers.

### 3.3 Model discovery

`GET /v1/models` (header `Authorization: Bearer <key>` when set). vLLM response shape (from `vllm/entrypoints/openai/engine/protocol.py`):

```json
{"object": "list",
 "data": [{"id": "Qwen/Qwen3.5-9B", "object": "model", "created": 1760800000,
           "owned_by": "vllm", "root": "Qwen/Qwen3.5-9B", "parent": null,
           "max_model_len": 32768,
           "permission": [{"id": "modelperm-...", "object": "model_permission", "...": "..."}]}]}
```

LoRA adapters appear as extra entries with `parent` set. There is **no modality field**, so vision capability is detected two ways:
1. **Name heuristic:** the id matches `vl`, `vision`, `qwen3.5`, `qwen3-vl`, `gemma-3`, `gemma-4`, `pixtral`, `mistral-small-3`, `internvl`, `llava`, `llama-4`, `q-realign` or `visualquality`. The UI shows this as "probably vision".
2. **"Test vision" probe:** send a 64x64 gray PNG with `max_completion_tokens: 1`. HTTP 200 means vision. A 400 whose message mentions multimodal or image support means text-only.

Cache the verdict per (server, model id, root). Use `max_model_len` to warn when the estimated prompt (images x tokens per image + text) exceeds the context.

### 3.4 Request payload

`POST /v1/chat/completions`:

```json
{
  "model": "Qwen/Qwen3.5-9B",
  "messages": [
    {"role": "system", "content": "<preset system prompt with rubric>"},
    {"role": "user", "content": [
      {"type": "text", "text": "Image A (reference): full view, 1536x864, downscaled 2.5x from 3840x2160."},
      {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0..."}},
      {"type": "text", "text": "Image B (encoded): full view, same scaling."},
      {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0..."}},
      {"type": "text", "text": "Image A ROI tile 1/2: native pixels x=1200..2224, y=400..1424."},
      {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}},
      {"type": "text", "text": "Image B ROI tile 1/2: same region."},
      {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}},
      {"type": "text", "text": "Objective metrics (Lumen, B vs A, ROI): PSNR-Y 38.2 dB, SSIM 0.962, CIEDE2000 mean 1.4 / p95 3.9, blockiness B 0.31 (A 0.05). Task: <preset user text>"}
    ]}
  ],
  "max_completion_tokens": 1500,
  "temperature": 0.2,
  "top_p": 0.9,
  "seed": 42,
  "stream": true,
  "stream_options": {"include_usage": true},
  "response_format": {"type": "json_schema",
                      "json_schema": {"name": "lumen_quality_report", "strict": true, "schema": {"...": "see 3.8"}}},
  "chat_template_kwargs": {"enable_thinking": false}
}
```

Notes:
- `max_tokens` is deprecated in vLLM in favor of `max_completion_tokens`; send the latter (OpenAI compatible). `top_k`, `min_p`, `repetition_penalty`, `chat_template_kwargs`, `continue_final_message` are vLLM extensions accepted in the same body; send them only when set.
- Multiple images: vLLM's `--limit-mm-per-prompt` now defaults to 999 per modality, but many launch recipes set it low (e.g. `--limit-mm-per-prompt.image 2` or `'{"image": 4}'`). On a 400 that mentions the image limit, Lumen automatically retries once with a composite (see 3.7) and remembers the limit for that model.
- Per-request `mm_processor_kwargs` (e.g. `max_pixels`) are rejected unless the server runs with `--trust-request-mm-kwargs`, so Lumen controls resolution by resizing client-side.
- When the server runs a reasoning parser, structured output is enforced only after the reasoning section, so thinking models still produce valid JSON in `content`.
- Objective metrics are included as plain text so the VLM can reconcile its visual judgement with the numbers. A preset toggle "Blind (no metrics)" removes them to avoid anchoring.

### 3.5 Streaming, cancellation, errors

- **SSE parsing:** use a hand-written parser (about 60 lines) rather than `eventsource-stream`/`sse-stream`, which are built on async streams. It works like this:
  - Read the body through `BufReader` line by line.
  - Ignore lines starting with `:`.
  - Collect `data:` lines until a blank line; `data: [DONE]` ends the stream.
  - Each chunk carries `choices[0].delta.content` (answer text), `choices[0].delta.reasoning` (also accept the legacy `reasoning_content`) and `choices[0].finish_reason` (`stop`, `length`).
  - A final chunk carries `usage` (prompt and completion tokens, `prompt_tokens_details`).
- **egui rendering:** the worker sends `AiEvent::{Connected, Reasoning(String), Delta(String), Usage(Usage), Done(Result<Report, AiError>)}` over a `crossbeam-channel`. It coalesces deltas to at most about 30 events per second and calls `ctx.request_repaint()`. The UI drains the channel each frame into a `String`.
  - Free-text presets render progressively as Markdown with `egui_commonmark` 0.26 (supports egui 0.37).
  - JSON presets show a live monospace "raw output" with a token count, then parse into result cards on `Done`.
  - Reasoning text goes into a collapsed "Model reasoning" section.
- **Cancellation:** the Cancel button sets an `AtomicBool`. The worker checks it between SSE lines and drops the response, which closes the socket, and vLLM aborts the request when the client disconnects. The UI marks the job cancelled immediately and ignores any late events (generation id).
- **Timeouts (ureq 3 config):** `timeout_connect` 5 s, `timeout_recv_response` = first-token timeout (default 120 s), `timeout_recv_body` none (streaming), `timeout_global` = total cap (600 s). Non-streaming probes use 15 s.
- **Errors:** parse `{"error": {"message", "type", "param", "code"}}` and the older top-level `{"object": "error", "message": ...}` shape. Map them as follows:

| Condition | Message or action |
|---|---|
| Connection refused, DNS failure, TLS failure | "Server not reachable" |
| 401 / 403 | "API key rejected" |
| 404 | "Model not loaded on server" (refresh the list) |
| 400 with "maximum context length" | "Request too large: reduce max image side or tiles" |
| 400 image limit | Composite fallback |
| 413 | Reduce size |
| `finish_reason: length` | "Output truncated, raise max tokens"; keep the partial text (the JSON may be unparseable) |

- **Retries:** only for connect failures, 429, 502, 503 and 504, and only before the first streamed token: 2 retries with 1 s and 4 s backoff, honoring `Retry-After`. Never retry after partial output.

### 3.6 HTTP client decision

Use **`ureq` 3.4.2** with `default-features = false, features = ["rustls", "platform-verifier"]`, on a dedicated worker thread per request. Reasons:
- Blocking I/O fits Lumen's thread-and-channel architecture naturally (no tokio runtime).
- Responses can be streamed via `Body::as_reader()`.
- rustls with the `ring` provider builds cleanly on MSVC.
- `platform-verifier` validates against the Windows certificate store, so corporate CAs work; `custom_ca_pem` adds a self-signed workstation CA.
- Binary cost is about 1 to 1.5 MB.

`reqwest` 0.13 would pull in tokio + hyper (roughly 3 to 5 MB more) and force an async runtime into a sync app for no benefit. Use `base64` 0.23 for encoding and the existing `serde_json` for JSON. No gzip: traffic is LAN and mostly incompressible base64 images.

### 3.7 Image preparation and tiling

- **Encoding:** default PNG (lossless) so the VLM judges the real artifacts and not Lumen's JPEG; JPEG q95 4:4:4 option for slow links. A 1536x864 PNG photo is 2 to 3 MB (3 to 4 MB base64), which is acceptable on LAN. Show bytes in the send preview.
- **Overview:** downscale the full image to `max_side_overview` (1536) with Lanczos3, with dimensions rounded to multiples of 32. Qwen3/3.5 use 16 px patches with a 2x2 merge, so 32 px per visual token (about 1300 tokens for 1536x864). This avoids the server's processor rescaling again.
- **ROI at native resolution:**
  - If the ROI fits in `tile_side` (1024), send it unscaled, padded to a multiple of 32 by edge replication (not resampling).
  - Larger ROIs are split into a grid of tiles of at most 1024x1024 with 32 px overlap.
  - If the tile count exceeds the budget (`max_images` minus overviews, default 4 per side), downscale the ROI by the smallest factor that fits and say so in the text part ("downscaled 1.6x").
  - Each tile is preceded by a text part stating the image, tile index and source pixel bounds, so findings can be mapped back.
- **A vs B: separate images (default).** Each image is labeled by a preceding text part, A and B use the same tile grid, and tiles are interleaved A-tile-1, B-tile-1, and so on (adjacent images help models compare). A **composite** (A | B side by side with burned-in "A" and "B" labels and a 16 px gap) is used only as the automatic fallback for servers limited to one image, or when chosen explicitly. It halves effective resolution per side, and models sometimes confuse the halves.
- **Mapping findings back:** findings carry `image`, `tile` and `bbox` in the convention set by `coord_convention` (default: normalized 0..1000 relative to the image as sent). Lumen maps them through the tile transform into source pixels and draws labeled boxes on the viewer, compare panes or video frame.

### 3.8 Prompt design

Presets are editable and stored in settings:
- **Quality critique** (single image)
- **A vs B comparison**
- **Encode review** (B is an encode of reference A; focus on artifacts and detail loss)
- **Photo technical review** (exposure, focus, noise, color for a camera image)
- **Free-form** (user prompt, Markdown output)
- **Quick score** (Q-ReAlign style, below)

System prompt (Quality critique, condensed): "You are an expert image quality analyst (broadcast QC and photo editing). Judge only what is visible in the provided pixels. Score each category from 1 (unacceptable) to 10 (flawless, indistinguishable from a pristine source) using these anchors: 9-10 no visible defects at 100% zoom; 7-8 minor defects visible only on close inspection; 5-6 clearly visible defects that do not ruin the image; 3-4 distracting defects; 1-2 severe. Categories: sharpness_detail (focus, resolution, fine texture), noise (luma and chroma grain, denoise smearing), compression_artifacts (blocking, ringing, mosquito noise, banding, posterization), color (accuracy, casts, saturation, grading consistency), exposure_dynamic_range (clipping, crushed shadows, flat contrast, tone mapping), detail_vs_reference (only when a reference is given, else null). Report findings with a location: the image label, the tile index, and a bounding box [x0, y0, x1, y1] in 0..1000 coordinates relative to that image. Do not invent defects; if unsure, say so in the finding and lower its severity. Objective metrics computed by software are provided as context; mention when your visual judgement disagrees with them."

JSON schema (strict):

```json
{"type": "object", "additionalProperties": false,
 "required": ["overall", "categories", "findings", "summary"],
 "properties": {
  "overall": {"type": "integer", "minimum": 1, "maximum": 10},
  "categories": {"type": "object", "additionalProperties": false,
    "required": ["sharpness_detail", "noise", "compression_artifacts", "color", "exposure_dynamic_range", "detail_vs_reference"],
    "properties": {
      "sharpness_detail": {"$ref": "#/$defs/cat"}, "noise": {"$ref": "#/$defs/cat"},
      "compression_artifacts": {"$ref": "#/$defs/cat"}, "color": {"$ref": "#/$defs/cat"},
      "exposure_dynamic_range": {"$ref": "#/$defs/cat"},
      "detail_vs_reference": {"anyOf": [{"$ref": "#/$defs/cat"}, {"type": "null"}]}}},
  "findings": {"type": "array", "maxItems": 20, "items": {"type": "object", "additionalProperties": false,
    "required": ["category", "severity", "image", "tile", "bbox", "description"],
    "properties": {
      "category": {"enum": ["sharpness_detail", "noise", "compression_artifacts", "color", "exposure_dynamic_range", "detail_vs_reference"]},
      "severity": {"enum": ["minor", "moderate", "severe"]},
      "image": {"enum": ["A", "B"]}, "tile": {"type": "integer", "minimum": 0},
      "bbox": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 1000}, "minItems": 4, "maxItems": 4},
      "description": {"type": "string", "maxLength": 300}}}},
  "summary": {"type": "string", "maxLength": 800}},
 "$defs": {"cat": {"type": "object", "additionalProperties": false, "required": ["score", "rationale"],
   "properties": {"score": {"type": "integer", "minimum": 1, "maximum": 10}, "rationale": {"type": "string", "maxLength": 300}}}}}
```

Tile 0 means the overview image.

The comparison preset extends the schema with `"preferred": {"enum": ["A", "B", "tie"]}`, `"confidence": {"type": "number", "minimum": 0, "maximum": 1}`, and per-category scores for both images (`scores_a`, `scores_b`). It instructs: "A is the reference unless stated otherwise; describe differences, not absolute quality, and point to regions where B lost detail or gained artifacts." To reduce position bias, a "Swap-check" option runs the comparison twice with the A and B order swapped and reports agreement.

**Quick score preset (IQA-tuned scorer):** for `q-future/Q-ReAlign-*` models.
- Request: the image with the user text "How would you rate the quality of this image?", plus an assistant message "The quality of the image is". Set `continue_final_message: true`, `add_generation_prompt: false`, `max_completion_tokens: 1`, `temperature: 0`, `logprobs: true`, `top_logprobs: 20`.
- Scoring: softmax the logprobs of " excellent", " good", " fair", " poor" and " bad", then compute score = sum(w_i p_i) with w = [1.0, 0.75, 0.5, 0.25, 0.0].
- This gives a numeric MOS-like score per frame, cheap enough to plot over a video range (sampled frames). It is shown next to the local NR metrics.

### 3.9 Recommended models (vLLM 0.32.0 is current)

| Use | Model (Hugging Face) | License | GPU memory (bf16 unless noted) | Launch sketch |
|---|---|---|---|---|
| Default critique | `Qwen/Qwen3.5-9B` | Apache-2.0 | about 19 GB weights; 24 GB GPU at 32k context | `vllm serve Qwen/Qwen3.5-9B --max-model-len 32768 --limit-mm-per-prompt '{"image": 8}' --reasoning-parser qwen3 --gpu-memory-utilization 0.92 --api-key <key>` |
| Small GPU (12 to 16 GB) | `Qwen/Qwen3.5-4B` or `Qwen/Qwen3-VL-8B-Instruct-FP8` | Apache-2.0 | 9 to 10 GB | same flags, `--max-model-len 16384` |
| Higher quality, 48 GB | `Qwen/Qwen3.5-35B-A3B-FP8` (MoE, 3B active, fast) | Apache-2.0 | about 36 GB | add `--max-model-len 32768`; `--tensor-parallel-size 2` on 2x24 GB |
| Highest quality, 80 GB | `Qwen/Qwen3.5-27B` or `google/gemma-4-31B-it` | Apache-2.0 | 54 to 62 GB | Gemma 4 needs no reasoning parser; QAT `gemma-4-31B-it-qat-w4a16-ct` fits 24 GB |
| Alternative families | `OpenGVLab/InternVL3_5-8B-HF`, `mistralai/Mistral-Small-3.2-24B-Instruct-2506` | Apache-2.0 | 17 GB / 48 GB | Mistral: `--tokenizer-mode mistral` |
| IQA scorer | `q-future/Q-ReAlign-Lite-4B` (Qwen3.5 backbone, 0.8B and 9B siblings) | Apache-2.0 | about 9 GB | `vllm serve q-future/Q-ReAlign-Lite-4B --max-model-len 8192` |
| IQA reasoning (research) | `TianheWu/VisualQuality-R1-7B` (Qwen2.5-VL-7B), `zibuyu-02/IQA-T1` (Qwen3-VL-4B) | check model cards | 16 GB / 9 GB | set `coord_convention` to absolute for Qwen2.5-VL |

Rules of thumb:
- Weights take params x 2 bytes (bf16), x 1 (FP8) or x about 0.55 (4-bit). Add 3 to 8 GB for the KV cache and the vision encoder at 32k context.
- For Qwen3.5, use non-thinking mode (`enable_thinking: false`) with temperature 0.2 for rubric scoring. Offer thinking as a preset toggle; it is slower but sometimes better at subtle artifacts.
- Llama 4 Scout/Maverick (109B/400B MoE) is not practical on a single workstation.
- Older IQA-tuned models (Q-Instruct, Co-Instruct, Q-Align on mPLUG-Owl2) use architectures vLLM does not support, and Q-ReAlign supersedes them.
- Always confirm against the vLLM supported-models page for the user's vLLM version.

---

## 4. UI/UX integration

- **Analysis panel:** a right-side `egui::Panel` ("Analysis") available in viewer, compare and video modes. `Q` (currently unbound) toggles it, `Shift+Q` runs the full selected metric set, and `Ctrl+Shift+Q` starts AI analysis. In video mode the planned `M` metrics panel becomes a tab of the same panel.
- **Panel layout, top to bottom:**
  1. Scope bar: Whole / ROI / Current frame / Range, with start, end and stride fields and a "use A-B loop" button.
  2. Metric picker: a checklist grouped FR / NR / Temporal, with presets "Quick", "Encode QA" and "Photo technical", and a cost hint per metric.
  3. Run / Cancel, with a progress bar and ETA.
  4. Results table.
  5. Heatmap selector: metric, colormap, range, opacity, threshold.
  6. AI tab: server status dot, model dropdown, preset dropdown, "What will be sent" expander, Run, live output, result cards.
  7. History: previous runs for this file.
  8. Export menu.
- **ROI interaction:** press `R` while the panel is open (or use the ROI button) and drag on the image to draw a rectangle. Reuse the crop handles for resizing and moving; `Shift` constrains to a square, and `Esc` clears. In compare mode the ROI is shared across panes in image coordinates (views are already synchronized). Show the ROI size, and warn when it is below a metric's minimum (MS-SSIM 176 px, NIQE 193 px). Multiple ROIs: later.
- **Result presentation:** each metric row shows a value, unit, direction arrow and a color chip (green/amber/red) with metric-specific thresholds, for example:
  - PSNR-Y: >= 40 dB green, < 30 dB red.
  - CIEDE2000 mean: < 1 green, > 3 red.
  - VMAF: >= 93 green.

  Clicking a row that has a map shows its heatmap overlay. In video mode, plots sit under the timeline (reusing the timeline area): click to seek, with the worst-frames list beside them.
- **AI result cards:** an overall score badge, six category bars with rationale tooltips, a findings list (click to zoom to the bbox; boxes are drawn on the image with numbered labels and severity colors), the summary text, and footer metadata (model, server, duration, tokens, what was sent). Actions: "Copy as Markdown" and "Export JSON".
- **History and caching per file:** results are stored in the cache (section 2.7) and indexed by file key. The History list shows date, scope, and the metric set or AI model and preset. Selecting an entry re-displays it without recomputation. "Clear analysis history" lives in Settings > Privacy.
- **Export:** CSV and JSON for metrics; PNG/TIFF heatmaps; a "Report bundle" folder with JSON, CSV, heatmap PNGs, AI cards as Markdown, and worst-frame PNG pairs.

---

## 5. Architecture

### 5.1 Layout

```
Cargo.toml                 [workspace] members = [".", "crates/lumen-analysis", "crates/lumen-ai"]
crates/lumen-analysis/     no egui, no ffmpeg; deps: rayon, serde, bytemuck, blake3, optional fast-ssim2, butteraugli, flip-rs, vmaf-head-sys, tract-onnx
  src/frame.rs             FrameView, PlaneView, Roi, ColorInfo, Derived cache
  src/color.rs             sRGB/linear LUTs, XYZ, Lab, Delta-E formulas
  src/metrics/mod.rs       Metric trait, MetricId, registry, MetricResult
  src/metrics/reference/   psnr.rs ssim.rs msssim.rs deltae.rs diff.rs ssimulacra2.rs butteraugli.rs flip.rs vmaf.rs lpips.rs
  src/metrics/noref/       sharpness.rs noise.rs exposure.rs cast.rs blockiness.rs banding.rs brisque.rs niqe.rs piqe.rs temporal.rs
  src/heatmap.rs           Heatmap, colormap LUTs, pooling downsample
  src/job.rs               Job, JobSpec, scheduler, progress, cancellation, memory budget
  src/report.rs            AnalysisReport (serde), CSV/JSON writers
  src/cache.rs             key hashing, on-disk JSON cache, LRU
  data/                    brisque.bin, niqe.bin, (feature nn) lpips_alex_fp16.onnx
crates/lumen-ai/           deps: ureq, serde, serde_json, base64, image (png), jpeg-encoder, windows (credentials), zeroize
  src/settings.rs          AiSettings, base URL derivation from host/port/https, LAN checks
  src/secret.rs            Credential Manager + DPAPI fallback
  src/client.rs            Agent build, health/version/models, chat (stream and non-stream), SSE parser, errors, retries
  src/models.rs            ModelCard types, vision heuristics, probe cache
  src/prompts.rs           presets, schemas, user-message builder, metric context text
  src/images.rs            overview resize, tiling, composite, encoding, coordinate mapping
  src/report.rs            AiReport parsing and validation
src/analysis_ui/           panel.rs roi.rs plots.rs heatmap_view.rs ai_view.rs send_preview.rs (app crate; adapters from LoadedImage/YuvFrame)
```

`src/video/metrics.rs` becomes a thin re-export of `lumen_analysis::metrics::reference::{psnr, ssim}` so existing callers keep compiling; `analyze.rs` becomes a `JobSpec` producer.

### 5.2 Core traits and types (signatures only)

```
pub enum MetricKind { FullReference, NoReference, Temporal }
pub trait Metric: Send + Sync {
    fn id(&self) -> MetricId;            // stable string id, e.g. "ssim.gauss"
    fn name(&self) -> &str;
    fn kind(&self) -> MetricKind;
    fn version(&self) -> u32;            // bump when numerics change (invalidates cache)
    fn input(&self) -> InputKind;        // what Derived data it needs
    fn min_size(&self) -> (u32, u32);
    fn cost_hint(&self, w: u32, h: u32) -> Cost; // for UI and scheduling
    fn compute(&self, a: &FrameCtx, b: Option<&FrameCtx>, roi: Option<Roi>,
               opts: &MetricOptions, cancel: &CancelToken) -> Result<MetricResult, MetricError>;
}
pub struct MetricResult { pub values: Vec<(String, f64, Unit)>, pub map: Option<Heatmap>, pub notes: Vec<String> }
pub trait TemporalMetric: Send { fn push(&mut self, idx: u64, cur: &FrameCtx) -> Vec<(String, f64)>; }
```

`FrameCtx` wraps a `FrameView` plus its `Derived` cache (`OnceLock` fields). Metrics are stateless and registered in a static registry, so the UI lists them generically.

### 5.3 Jobs and threading

- `JobSpec { source: Image(path) | Video { a, b, offset, range }, roi, metrics, preview_first }`. The app's `AnalysisService` owns one scheduler thread, the analysis rayon pool, and an `mpsc` channel of `JobEvent::{Progress, Row(idx, results), Partial(MetricResult), Done(Report), Failed}` that the panel consumes each frame.
- Image job: build `FrameCtx` for A (and B), then run metrics in parallel (`par_iter` over metrics, each internally parallel for big maps) with the preview-then-full policy.
- Video job: generalizes `analyze::run`. It uses two decoder threads (A, B) with frame range and stride, a bounded channel sized by the memory budget, a worker stage (`par_bridge`) and a reorder buffer. Temporal metrics run sequentially in the scheduler thread in index order. VMAF runs as its own temporal consumer, because libvmaf takes frames sequentially and is internally threaded.
- AI jobs run on their own thread in `lumen-ai`, never in the analysis pool, and can include the latest metric results as context.

### 5.4 Testing strategy

- **Golden vectors:** a `tools/golden/` Python script (dev only, not shipped) generates distortions of a small, permissively licensed image set: Kodak-style 512x512 crops plus synthetic gradients and text, with JPEG q10..90, Gaussian blur, Gaussian noise and color shifts. It records reference outputs from:
  - FFmpeg `psnr`/`ssim` (fast variants).
  - scikit-image `structural_similarity` and `peak_signal_noise_ratio`.
  - TF `ssim_multiscale`.
  - `colour-science` `delta_E` (CIE1976/1994/2000), plus Sharma's 34-pair table.
  - The libvmaf CLI (VMAF, CAMBI, float_ms_ssim).
  - libjxl `ssimulacra2` and `butteraugli_main`.
  - NVIDIA `flip`.
  - scikit-video `niqe` and OpenCV `QualityBRISQUE`.
  - The `lpips` package.

  Results are committed as JSON. Rust tests assert per-metric tolerances: PSNR 1e-6 dB, SSIM 1e-4, MS-SSIM 1e-4, CIEDE2000 1e-4, VMAF 0.05, NIQE/BRISQUE 1e-2 relative. pyiqa (non-commercial) is used only for informal cross-checks, never as the committed source of truth.
- **Property tests:** identical inputs give PSNR inf, SSIM 1 and Delta-E 0; scores are monotonic in noise sigma and blur radius; ROI equals cropping then computing; chroma ROI mapping is correct for 4:2:0 odd offsets.
- **Performance tests:** `criterion`-style benches (feature-gated) at 1080p and 4K with budgets from section 2.2; CI fails on a regression of more than 25%.
- **AI client:** tests run against a mock OpenAI-compatible server (a tiny `std::net::TcpListener` responder) that replays recorded vLLM responses:
  - the models list;
  - an SSE stream with reasoning and usage chunks;
  - structured JSON output;
  - a 400 context-length error and a 401;
  - an image-limit error (composite fallback);
  - a slow first token (timeout);
  - a mid-stream cancel.

  Unit tests cover LAN-range checks, base URL derivation from host/port/https, tiling and coordinate-mapping round trips, and metadata stripping: encoded PNG/JPEG must contain no eXIf/iTXt/iCCP/APP1 chunks.

---

## 6. Milestones (one developer, estimates)

| # | Milestone | Content | Effort |
|---|---|---|---|
| M1 | Foundation | Workspace crates, `FrameView`/`Roi`/`Derived`, Metric trait and registry, port PSNR + fast SSIM with typed fast paths, job scheduler with cancel/progress/budget, cache | 1.5 weeks |
| M2 | FR v1 | Gaussian SSIM with map, MS-SSIM, Delta-E 76/94/2000 with maps, diff maps, heatmap overlay and colormaps, golden tests | 2 weeks |
| M3 | NR v1 | Sharpness, Immerkaer noise, exposure/zones/DR, color cast, blockiness, simple banding, temporal; tile maps | 1.5 weeks |
| M4 | Panel and video ranges | Analysis panel in all three modes, ROI widget, frame range/stride in `SeqDecoder`, `egui_plot` graphs, worst frames, CSV/JSON export, history | 2 weeks |
| M5 | AI client | Settings (host/port/key/https), Test connection, Credential Manager, models list and vision probe, LAN checks, ureq streaming client, SSE, errors, retries, send preview, status badge | 1.5 weeks |
| M6 | AI analysis | Presets, schemas, image overview and tiling, composite fallback, metric context, result cards and bbox overlays, Quick score (logprobs), mock-server tests | 1.5 weeks |
| M7 | v2 metrics | fast-ssim2, butteraugli, flip-rs integration with maps; VMAF + CAMBI via vmaf-head-sys (feature, CI toolchain: meson, ninja, nasm); sidecar fallback; BRISQUE, NIQE, PIQE ports; wavelet noise, edge width, ringing | 3 weeks |
| M8 | Later | tract LPIPS (feature `nn`), custom ONNX NR slot, MTF50, HDR metrics, high-bit image path | 2 to 3 weeks |

Total to v1 local + AI (M1 to M6): about 10 weeks. v2: +3 weeks.

---

## 7. Risks and mitigations

- **VMAF build on CI:** meson/nasm/xxd on `windows-latest` add setup steps. Mitigate with feature gating and the sidecar fallback, and cache the built static lib.
- **Metric drift vs references:** golden tests with explicit tolerances, and a per-metric `version()` that invalidates the cache when numerics change.
- **VLM hallucinated defects and position bias:** use low temperature, rubric anchors, "say when unsure", objective metrics as context, and swap-check for comparisons. Always present AI output as "opinion" beside the measured numbers.
- **Large payloads and context overflow:** client-side resizing, a tile budget, a token estimate from `max_model_len`, and clear error mapping.
- **Privacy regressions:** a unit test asserts that encoded images carry no metadata. Network code is isolated in one crate, and the feature can be compiled out (`--no-default-features --features video`).
- **Licensing:** avoid AGPL (dssim) and non-commercial (pyiqa weights) components. Ship notices for libvmaf (BSD-2-Clause-Patent), butteraugli (BSD-3), fast-ssim2 (BSD-2), flip-rs (BSD-3), the LIVE BRISQUE/NIQE parameters, and LPIPS (BSD-2).

---

## Sources

- Lumen `docs/VIDEO-RESEARCH.md` (metric math, VMAF feasibility, FFmpeg build), and the source files listed at the top.
- crates.io API, checked 2026-10-18: fast-ssim2 0.9.0, ssimulacra2 0.5.1, butteraugli 0.9.3, flip-rs 0.1.2, nv-flip 0.1.2, iqa 1.2.1, dssim-core 3.5.1 (AGPL-3.0), zensim 0.2.7, oximedia-quality 0.2.1, vmaf-head-sys 0.2.0, shiguredo_vmaf 2026.1.0, libvmaf-sys 0.4.4, rust-vmaf 0.2.0, ort 2.0.0-rc.14, tract-onnx 0.23.9, candle-core 0.11.0, palette 0.7.7, yuv 0.8.19, ureq 3.4.2, reqwest 0.13.5, keyring 4.2.0, windows 0.62.2, egui_plot 0.37.0, egui_commonmark 0.26.0, base64 0.23.1, blake3 1.8.7: https://crates.io
- fast-ssim2: https://github.com/imazen/fast-ssim2 ; butteraugli (Rust): https://github.com/imazen/butteraugli ; zensim: https://github.com/imazen/zensim
- flip-rs (Rust port of FLIP v1.7): https://github.com/Tavrin/flip-rs ; NVIDIA FLIP: https://github.com/NVlabs/flip ; Andersson et al., "FLIP: A Difference Evaluator for Alternating Images", HPG 2020
- iqa-rs: https://github.com/justin13888/iqa-rs ; oximedia-quality: https://docs.rs/oximedia-quality
- rust-vmaf / vmaf-head-sys (vendored libvmaf, MSVC CI): https://github.com/xnorpx/rust-vmaf ; shiguredo vmaf-rs: https://github.com/shiguredo/vmaf-rs ; Netflix VMAF releases (v3.2.1): https://github.com/Netflix/vmaf/releases ; CAMBI: Tandon et al., "CAMBI: Contrast-aware Multiscale Banding Index", PCS 2021
- ort linking and prebuilt binaries: https://ort.pyke.io/setup/linking , https://ort.pyke.io/misc/prebuilt-binaries , https://ort.pyke.io/backends/tract ; tract: https://github.com/sonos/tract
- LPIPS: Zhang et al., "The Unreasonable Effectiveness of Deep Features as a Perceptual Metric", CVPR 2018, https://github.com/richzhang/PerceptualSimilarity
- pyiqa / IQA-PyTorch (PolyForm Noncommercial 1.0.0): https://github.com/chaofengc/IQA-PyTorch
- OpenCV contrib quality module (BRISQUE models): https://github.com/opencv/opencv_contrib/tree/4.x/modules/quality ; Mittal, Moorthy, Bovik, "No-Reference Image Quality Assessment in the Spatial Domain" (BRISQUE), IEEE TIP 2012
- scikit-video NIQE: https://github.com/scikit-video/scikit-video/blob/master/skvideo/measure/niqe.py ; Mittal, Soundararajan, Bovik, "Making a Completely Blind Image Quality Analyzer" (NIQE), IEEE SPL 2013
- Venkatanath N. et al., "Blind Image Quality Evaluation Using Perception Based Features" (PIQE), NCC 2015 ; MathWorks brisque/niqe/piqe docs: https://www.mathworks.com/help/images/image-quality-metrics.html
- Wang, Bovik, Sheikh, Simoncelli, "Image Quality Assessment: From Error Visibility to Structural Similarity", IEEE TIP 2004 ; Wang, Simoncelli, Bovik, "Multi-scale Structural Similarity", Asilomar 2003
- Sharma, Wu, Dalal, "The CIEDE2000 Color-Difference Formula: Implementation Notes, Supplementary Test Data, and Mathematical Observations", CR&A 2005
- Immerkaer, "Fast Noise Variance Estimation", CVIU 1996 ; Donoho and Johnstone, "Ideal spatial adaptation by wavelet shrinkage", Biometrika 1994 ; Pech-Pacheco et al., "Diatom autofocusing in brightfield microscopy", ICPR 2000 ; Marziliano et al., "A no-reference perceptual blur metric", ICIP 2002 ; Wang, Sheikh, Bovik, "No-reference perceptual quality assessment of JPEG compressed images", ICIP 2002 ; ISO 12233:2017 (e-SFR)
- vLLM docs: structured outputs https://docs.vllm.ai/en/latest/features/structured_outputs.html ; multimodal inputs https://docs.vllm.ai/en/latest/features/multimodal_inputs.html ; source of request/response types: https://github.com/vllm-project/vllm (vllm/entrypoints/openai/chat_completion/protocol.py, vllm/entrypoints/openai/engine/protocol.py, vllm/config/multimodal.py) ; vLLM releases (v0.32.0, 2026-10-18): https://github.com/vllm-project/vllm/releases
- OpenAI API reference, chat completions, structured outputs and streaming: https://platform.openai.com/docs/api-reference/chat/create
- Hugging Face model cards: https://huggingface.co/Qwen/Qwen3.5-9B , https://huggingface.co/Qwen/Qwen3-VL-8B-Instruct , https://huggingface.co/google/gemma-4-31B-it , https://huggingface.co/OpenGVLab/InternVL3_5-8B-HF , https://huggingface.co/mistralai/Mistral-Small-3.2-24B-Instruct-2506 , https://huggingface.co/q-future/Q-ReAlign-Lite-4B , https://huggingface.co/TianheWu/VisualQuality-R1-7B , https://huggingface.co/zibuyu-02/IQA-T1
- VisualQuality-R1: https://arxiv.org/abs/2505.14460 ; Q-Insight: https://arxiv.org/abs/2503.22679 ; Q-Tacit: https://arxiv.org/abs/2603.22641
- ureq docs: https://docs.rs/ureq ; Windows Credential Manager API (CredWriteW/CredReadW): https://learn.microsoft.com/en-us/windows/win32/api/wincred/ ; DPAPI CryptProtectData: https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata
