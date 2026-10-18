# Lumen Video Mode: Research and Recommendations

Status: research note, October 2026. Target: Rust 1.99 (MSVC), eframe/egui 0.37 (glow), single self-contained `lumen.exe`.

---

## 0. TL;DR

- **Decoder:** FFmpeg 9.0, **statically linked** from **vcpkg** (`x64-windows-static-md`, LGPL build with no `gpl` feature, plus `dav1d` for AV1). The Rust side uses `ffmpeg-next 9.0` / `ffmpeg-sys-next 9.0.1` with the `static` feature. Both added FFmpeg 9.0 support in Jul-Oct 2026, and vcpkg's `ffmpeg` port is at 9.0.2. Media Foundation is ruled out: it can't report frame types or packet sizes, its codec coverage depends on Store extensions, and its seeking isn't exact.
- **Rendering:** upload the Y/U/V (or NV12/P010) planes as GL textures and convert YUV→RGB in a fragment shader. The A/B compare modes (wipe, difference, checkerboard, flicker) run in the same shader, so they cost almost nothing. CPU conversion with the `yuv` crate is only for snapshots, metrics, and fallback.
- **Audio:** `cpal` 0.18 (WASAPI), audio resampled to the device format with FFmpeg's `swresample`, and an SPSC ring buffer. The **audio clock is the master clock**, and playback falls back to a monotonic system clock when there's no audio.
- **Metrics:** pure Rust (rayon plus autovectorized integer loops). PSNR per plane, SSIM (FFmpeg-compatible 8x8/stride-4 for speed, Wang Gaussian 11x11 for accuracy), and MS-SSIM. **VMAF is phase 3**: through an optional external `ffmpeg.exe` that has libvmaf, or a pure-Rust port later. vcpkg's `libvmaf` port is marked `!windows`.

---

## 1. What existing tools do (feature survey)

| Tool | Notable features worth copying |
|---|---|
| **pixop/video-compare** (C++, FFmpeg + SDL2) | Split-screen wipe slider that follows the mouse. Subtraction (difference) modes, with a luma-only option. hstack/vstack. Right-video time shift (`+`/`-` = 1 frame, Ctrl = 10, Alt = 100). In-buffer frame stepping (`A`/`D`), Shift+D to decode the next frame, Shift+A to seek back. Pixel value under cursor (`P`). Metrics printout (`M`). Histogram, vectorscope, and waveform windows (F1-F3). Zoom presets 4-9 and wheel zoom at the cursor. Magnifier loupes (`Z`/`C`). Swap (`S`). Per-side crop. Copy/paste timestamp. Multiple right-hand videos against one reference. HDR→SDR tone mapping. **No audio.** |
| **MSU VQMT** | PSNR variants, SSIM/MS-SSIM/3SSIM/stSSIM, VMAF, and no-reference blur/blocking/noise metrics. Per-frame and per-component results. **Metric visualization maps** (per-pixel heatmaps). **Worst-frame extraction.** ROI masks. Multiple distorted files against one reference. CSV/JSON export. 1-16-bit and float, Rec.2020/ICtCp. |
| **Netflix VMAF / libvmaf** | Fusion of VIF (4 scales) + ADM/DLM + motion features, regressed with an SVM model (built-in JSON models). BSD-2-Clause-Patent. The meson build targets Unix/MinGW. |
| **Elecard StreamEye** | Frame-size bar chart colored by frame type (I/P/B) with a thumbnail tooltip (type, PTS, QP, offset, POC, size). GOP view. Macroblock/CTU overlays (QP, partitions, motion vectors), DPB info. Lumen should copy the bar chart and tooltip. Block-level overlays need codec internals, so they are out of scope. |
| **VQProbe / VQ Analyzer** (ViCueSoft) | Same class of tool as StreamEye: bitstream syntax trees and per-block overlays. Out of scope except for the "frame list with type and size" idea. |
| **mpv / IINA** | `.`/`,` frame step and back step (back step decodes from the previous keyframe). Precise seeks with `hr-seek`. `i` / `I` stats overlay (decoder, hwdec, frame timings, color params). Speed control `[`/`]`. Screenshots (`s`). A-B loop (`l`). |
| **DaVinci Resolve scopes** | Waveform (luma or RGB overlay), RGB parade, vectorscope with a graticule (75%/100% targets, skin-tone line), histogram, CIE chromaticity. Lumen should use the same scope conventions. |
| **FFmpeg filters** | `psnr`, `ssim` (8x8 box sums with stride 4, not Gaussian), `signalstats` (YMIN/YAVG/YMAX, SATAVG, BRNG = out-of-range pixels, TOUT = temporal outliers), `blend=all_mode=difference`, `waveform`, `vectorscope`, `histogram`, `scdet`. Lumen reimplements the math natively rather than linking avfilter. |
| **VapourSynth previewers** (vs-preview, VSEdit) | Several outputs switched by number keys at the *same frame* (instant A/B flicker). Frame-number-first navigation. Pixel picker that shows raw YUV values. "Copy frame number". This is the main UX model encoder developers rely on. |
| **AviDemux** | Keyframe-only seek buttons, frame type and size shown in the status bar, A/B markers. |

### Prioritised feature list for Lumen Video mode

**P0: MVP (viewer)**
1. Open a video from the browser and play/pause with **audio**, A/V sync, volume/mute.
2. Timeline scrubber with time and frame number. Seek ±1 s / ±10 s.
3. **Frame-accurate step forward and back** (with a back-step GOP cache) and **jump to frame or time** (`G`).
4. Zoom/pan using the same view model as image mode: fit, 100%, wheel at cursor.
5. Metadata panel (container, codec, profile/level, resolution, SAR/DAR, fps (avg/real, VFR flag), bitrate, duration, frame count, pixel format and bit depth, color primaries/transfer/matrix/range, chroma location, HDR mastering and CLL, rotation, audio streams).
6. Snapshot of the current frame (PNG, native resolution) and copy to clipboard.
7. Correct YUV→RGB for BT.601/709/2020, limited and full range, 8-12 bit.

**P1: compare and analysis core**
8. Open B (and later C, D...), with **sync by timestamp or by frame index**, plus a **time offset** (±1/10/100 frames).
9. Compare modes: **side-by-side** (H/V), **wipe slider** (vertical/horizontal, mouse-driven), **flicker/toggle** (hold key or auto N ms), **difference** (|A−B| × gain 1-64, luma-only or RGB, signed mode, heatmap), **checkerboard** (tile size). Zoom and pan are always shared between views.
10. **Pixel inspector**: raw Y/Cb/Cr code values, converted RGB, and the A−B delta under the cursor. Loupe magnifier.
11. **Per-frame metrics** PSNR-Y/U/V/avg and SSIM (current frame instantly). A background "analyze whole file" job produces a graph you can click to seek, a worst-N frames list, and CSV/JSON export.
12. **Frame index**: per-frame PTS, DTS, key flag, packet size, and frame type I/P/B. Shown as a StreamEye-style frame-size bar chart and a **bitrate graph** (1 s sliding window).
13. Thumbnail filmstrip on the timeline.

**P2: scopes and polish**
14. Scopes: luma waveform, RGB parade, vectorscope (with graticule), histogram (Y and RGB). Computed on the displayed frame and on demand.
15. Scene-change detection (markers on the timeline, next/prev scene keys).
16. MS-SSIM, metric heatmap overlay (per-pixel SSIM or squared error map, as in VQMT).
17. HDR (PQ/HLG) tone mapping to SDR in the shader, and gamut mapping from 2020 to 709.
18. Hardware decode (D3D11VA, copy-back) for smooth 4K/8K HEVC/AV1.
19. A-B loop, playback speed (audio muted when not at 1×), `signalstats`-like broadcast-range warnings.

**P3**
20. VMAF (via sidecar `ffmpeg.exe` or a native port), more than two compared videos, crop per side, scale/align B to A (Lanczos), and audio waveform.

---

## 2. Metric math (implementation-ready)

All metrics run on **decoded native planes** (YUV code values), not on display RGB. If A and B differ in size or subsampling, scale B to A first with swscale (bicubic or Lanczos) and flag the result as "rescaled" in the UI.

### 2.1 PSNR

For plane *p* with width W_p and height H_p, and bit depth *b* (MAX = 2^b − 1; 255 for 8-bit, 1023 for 10-bit):

```
MSE_p  = (1 / (W_p*H_p)) * Σ (A_p(x,y) − B_p(x,y))²
PSNR_p = 10 * log10(MAX² / MSE_p)          (MSE_p = 0 → +inf; display as "inf" or cap at 100 dB)
```

- **Combined YUV PSNR**, the FFmpeg `psnr` convention: weight the *MSE* by the plane's share of total samples, then convert. For 4:2:0 the weights are Y 4/6, U 1/6, V 1/6: `MSE_avg = Σ w_p·MSE_p`, `PSNR_avg = 10·log10(MAX²/MSE_avg)`.
- Some encoder papers use the alternative `(6·PSNR_Y + PSNR_U + PSNR_V)/8`. Report that only as an option and label it as such.
- **Sequence PSNR:** keep per-frame MSE and report both the *mean of per-frame PSNR* (what FFmpeg's "average" prints) and the *PSNR of the mean MSE* (the "global" value). The two differ.
- **RGB PSNR:** the same formula on R, G, and B after conversion. Combine with equal weights (1/3 each).
- **Implementation:** process rows in parallel with `par_chunks` per row band. 8-bit: `d = a as i32 - b as i32; acc += (d*d) as u32` per row, then sum into a `u64` (this autovectorizes to `vpmaddwd` on AVX2). 16-bit: accumulate `i64`. A 4K 4:2:0 frame (12.4 M samples) takes about 1-2 ms on 8 cores.

### 2.2 SSIM (Wang, Bovik, Sheikh, Simoncelli 2004)

For each local window with means μ, variances σ², and covariance σ_xy:

```
SSIM(x,y) = ((2 μx μy + C1) (2 σxy + C2)) / ((μx² + μy² + C1)(σx² + σy² + C2))
C1 = (K1·L)²,  C2 = (K2·L)²,  K1 = 0.01, K2 = 0.03,  L = 2^b − 1 (255 for 8-bit)
Frame SSIM = mean of the SSIM map over all valid windows (no padding)
SSIM dB    = 10·log10(1 / (1 − SSIM))   (FFmpeg reports this)
```

**Two variants. Offer both:**

1. **Fast, FFmpeg/x264-compatible (default for whole-file analysis).** Use unweighted **8x8 windows at stride 4**, built from 4x4 block sums. For each 4x4 block, compute `s1=Σa, s2=Σb, ss=Σa²+Σb², s12=Σab`. Each 8x8 window sums a 2x2 group of neighboring blocks. Then, with N = 64:
   ```
   vars  = ss·64 − s1² − s2²
   covar = s12·64 − s1·s2
   c1 = 0.01²·L²·64 ,  c2 = 0.03²·L²·64·63
   ssim = (2·s1·s2 + c1)(2·covar + c2) / ((s1² + s2² + c1)(vars + c2))
   ```
   Average over `((W>>2)−1)·((H>>2)−1)` windows. Combine planes with the same pixel-count weights as PSNR. All sums are integers, so 8-bit fits in `i32` per 4x4 block. This is very fast: one pass, two rows of block sums kept in a sliding buffer.
2. **Accurate (Wang reference / MATLAB `ssim_index`).** Use an **11x11 Gaussian, σ = 1.5**, normalized to sum to 1. Apply it as a separable filter (two 1-D 11-tap passes) to five f32 images: x, y, x², y², xy. Then `σx² = E[x²] − μx²`, and so on. Keep only "valid" outputs, without padding. The original also suggests downsampling by `f = max(1, round(min(W,H)/256))` with an f×f box filter first. Make that a toggle, because many tools (libvmaf `float_ssim`) apply it. Cost is about 110 MAC per pixel per image, roughly 30-60 ms for 4K luma with rayon + f32 SIMD. That's fine for the current frame, but slower for a whole file.

Compute SSIM on **Y only** by default. Y, U, and V plus the weighted "All" value are optional.

### 2.3 MS-SSIM (Wang, Simoncelli, Bovik 2003)

- Use M = 5 scales. Between scales, low-pass with a 2x2 average and downsample by 2.
- The contrast·structure term `cs_j = (2σxy + C2)/(σx²+σy²+C2)` is computed at every scale. The luminance term `l_M` is computed at the coarsest scale only.
- `MS-SSIM = l_M^{β5} · Π_{j=1..5} cs_j^{β_j}`, with β = [0.0448, 0.2856, 0.3001, 0.2363, 0.1333].
- Gaussian 11x11, σ = 1.5 at each scale. Clamp negative `cs` to 0 before applying the power, as libvmaf and TF do. This requires min(W,H) ≥ 176 (11·2⁴).

### 2.4 Scene-change score (FFmpeg `scdet` style)

On downscaled luma (for example, ¼ resolution):
- `mafd = SAD(Y_n, Y_{n−1}) / (pixels)`, normalized to 0-100 by dividing by MAX and multiplying by 100.
- `diff = |mafd − mafd_prev|`.
- `score = clamp(min(mafd, diff), 0, 100)`.
- Mark a scene change when `score > 10` (configurable).

### 2.5 VMAF feasibility

- **License:** libvmaf is BSD-2-Clause-Patent, which is compatible with MIT.
- **Static link from MSVC:** not realistic today. The build is meson + nasm + `xxd` aimed at Unix/MinGW. vcpkg's `libvmaf` 3.2.1 port declares `"supports": "!windows"`. The Rust crates (`libvmaf-sys` 0.4.4, 2023; `rust-vmaf` 0.2, 2024) are stale and expect MinGW. Mixing MinGW static archives into an MSVC link is fragile.
- **Practical path (phase 3):** if the user has an `ffmpeg.exe` built with `--enable-libvmaf` (gyan.dev "full" builds include it), run it as a sidecar process for the whole-file VMAF job. `ffmpeg-sidecar` 2.6 is one option, or plain `std::process::Command`:
  ```
  ffmpeg -i dist.mp4 -i ref.mp4 -lavfi "[0:v][1:v]libvmaf=log_fmt=json:log_path=vmaf.json:n_threads=8" -f null -
  ```
  Parse the per-frame JSON into the same graph used for PSNR and SSIM.
- **Native port (optional, later):** VMAF v0.6.1 = VIF at 4 scales + ADM + motion2, followed by nu-SVR with the JSON model. That's roughly 2-4k lines of Rust, and the main cost is validating it to within about 1e-3 of libvmaf.

---

## 3. Decoding backend decision

### Options compared

| | **FFmpeg (static, vcpkg)** | **Media Foundation (IMFSourceReader)** | libmpv / GStreamer |
|---|---|---|---|
| Codec/container coverage | Everything (H.264, HEVC, VP9, AV1 via dav1d, ProRes, DNxHD, FFV1, MPEG-2, MKV/MP4/MOV/TS/AVI/WebM/Y4M/raw) | Native MP4/MOV/AVI/ASF/3GP. MKV source since Win10. HEVC needs the paid Store "HEVC Video Extensions". VP9/AV1 need Store extensions. No ProRes/FFV1/Y4M. Depends on the user's machine. | Full, via FFmpeg |
| Frame type, packet size, PTS/DTS | Yes (`AVPacket.size/flags/pts/dts`, `AVFrame.pict_type`, parser `pict_type`) | **No.** Only decoded samples with timestamps. No bitstream access. | mpv: per-frame stats only. Pixels are not accessible for metrics. |
| Frame-accurate seeking | Yes: seek to keyframe, then decode and discard. Exact with a demux index. | `SetCurrentPosition` documents seeking to the nearest *preceding keyframe* without a guarantee, so you must read forward. There are reports of `ReadSample` stalls on Win11 with high-fps files. | mpv `hr-seek` is exact, but it's a black box |
| Bit-exact output for metrics | Yes (software decoders are deterministic) | HW decoders may differ slightly, and output is usually only NV12 | n/a |
| Single exe | Yes, static | Yes (OS component) | **No**: libmpv-2.dll / GStreamer runtime, so not single-exe |
| Size cost | +15-35 MB (estimate; trimmable) | ~0 | n/a |
| License | LGPL-2.1+ (no `gpl` feature) | OS | mpv GPL/LGPL, GStreamer LGPL plus plugins |

**Recommendation: FFmpeg, statically linked from vcpkg.** Media Foundation can't provide the analysis features (frame types, packet sizes, bitstream info), and its codec support varies with which Store extensions a user has installed. libmpv and GStreamer break the single-exe goal, and they don't expose decoded planes for metrics. A possible later extra is an optional MF/D3D11 *hardware playback* path, but FFmpeg's own `d3d11va` hwaccel already covers that.

### Rust crate choice (as of Oct 2026)

| Crate | Latest | FFmpeg support | Notes |
|---|---|---|---|
| `ffmpeg-next` / `ffmpeg-sys-next` | 9.0.0 / **9.0.1** (2026-10-09) | 3.4 → **9.0** ("Add support for FFmpeg 9.0", Jul 2026) | "Maintenance mode", but releases still ship. The build.rs supports vcpkg and FFMPEG_DIR. **Recommended.** |
| `ffmpeg-the-third` | 6.0.0+ffmpeg-9.0 (2026-08) | 5.1 → 9.0 | More actively maintained fork with newtype enums. A good alternative if ffmpeg-next stalls. |
| `rsmpeg` / `rusty_ffmpeg` | 0.18 (FFmpeg 8.0) / 0.17 (8.1) | Lags FFmpeg 9 | Has a vcpkg feature. Mostly raw API. |
| `ffmpeg-sidecar` | 2.6 | CLI | Spawns ffmpeg.exe. Only useful for the VMAF sidecar. |

Pin exact versions, because ffmpeg-next minor versions can break the API. Use the safe wrapper for demux and decode, and drop to `ffmpeg_sys_next` for `pict_type`, side data (mastering display, CLL, Dolby Vision), the hwaccel setup, and `av_parser_*`.

**How ffmpeg-sys-next finds FFmpeg** (from its `build.rs`): it checks `build` feature → `FFMPEG_DIR` → **vcpkg (MSVC only)** → pkg-config, in that order. The `static` feature (`CARGO_FEATURE_STATIC`) switches FFmpeg linking to static. Without `static`, the build script sets `VCPKGRS_DYNAMIC=1`, which selects the DLL triplet. In the vcpkg + static path it emits `ole32`, `secur32`, `ws2_32`, `bcrypt`, and `user32`. **In the `FFMPEG_DIR` path it emits no system libs and no transitive libs (dav1d, zlib)**, so prefer the vcpkg path (`VCPKG_ROOT`), which also links the port's dependencies. The `vcpkg` crate's default triplet for `x86_64-pc-windows-msvc` is `x64-windows-static-md` (static libs, dynamic CRT /MD). That matches Lumen's current build, which has no `crt-static`.

**Additional system libs.** Add these from Lumen's `build.rs` to cover avcodec's MediaFoundation wrapper, d3d11va/dxva2, and swscale:
`mfplat mfuuid strmiids ole32 oleaut32 user32 gdi32 shlwapi advapi32 bcrypt secur32 ws2_32 psapi`. Emitting them more than once is harmless. If the link fails, the unresolved symbols name whatever is still missing.

### Licensing (MIT app + static LGPL FFmpeg)

- Build **without** the `gpl`, `version3`, and `nonfree` features. That way libavcodec stays LGPL-2.1+ and dav1d stays BSD-2. Never add x264/x265 (GPL); you only need decoders anyway.
- FFmpeg's legal checklist recommends dynamic linking as the *safe* route. FFmpeg developers and the FSF FAQ agree that static linking also complies under LGPL §6, provided users can **relink** against a modified FFmpeg. Because Lumen is MIT open source with a public build recipe, anyone can rebuild with their own FFmpeg, which satisfies this.
- Also: ship the LGPL text and an FFmpeg notice in About/LICENSES, state the FFmpeg version and configure line (from `avcodec_configuration()`), and link to or mirror the exact FFmpeg source tarball on each GitHub release. (Not legal advice.)
- Codec patents (H.264/HEVC) are the same situation as for VLC and mpv. Mention them in the docs. They don't block an open-source release.

### Size and build time (estimates)

- With the default vcpkg port (all decoders, encoders, muxers, demuxers, protocols), static linking adds about 25-35 MB to the release exe. Every registered codec is referenced from `allcodecs.c`, so LTO can't strip unused ones.
- **Trim** with a vcpkg *overlay port* that appends `--disable-encoders --disable-muxers --disable-devices --disable-filters --disable-network --disable-protocols --enable-protocol=file --disable-programs`. That should bring the cost to about 10-15 MB.
- A first vcpkg build of ffmpeg + dav1d takes about 10-25 min (configure/make runs under the msys2 that vcpkg downloads). Later builds come from the binary cache. The Rust bindgen step is about 30 s.

---

## 4. Audio and A/V sync

- **Output:** `cpal` 0.18 uses WASAPI shared mode by default on Windows. Open the default output device and use its `default_output_config()` (usually 48 kHz f32 stereo).
- **Format conversion:** use FFmpeg `swresample` (already linked) to convert any decoded audio to device rate, channel layout, and f32 interleaved. That avoids pulling in `rubato`.
- **Transport:** run an audio decode thread that pushes `(pts_of_first_sample, samples)` chunks into an SPSC ring (`ringbuf` 0.5, about 200-500 ms capacity). The cpal callback pops samples (and writes silence on underrun). After each callback it publishes `clock = chunk_pts + consumed/rate − output_latency`. `output_latency` comes from `OutputCallbackInfo::timestamp()` (`playback − callback`). Store it in an `AtomicU64` (f64 bits) together with an `Instant`, so the UI can interpolate between callbacks.
- **Master clock:** audio when an audio stream exists and speed is 1×. Otherwise use `Instant` plus the accumulated pause offset. Muting is not the same as no-audio: keep feeding the device zeros while muted so the clock keeps running.
- **Video presentation:** the video decode thread fills a bounded queue (about 4-8 frames) of decoded frames. Each repaint, the UI picks the newest frame with `pts ≤ clock`, drops the frames older than it, and calls `ctx.request_repaint_after(next_pts − clock)`. Lumen's display is vsync-locked, so expect normal 24p-on-60 Hz cadence judder (the same as mpv's default `video-sync=audio`).
- **Seeking:** pause the clock, flush both decoders (`avcodec_flush_buffers`), clear the rings, then `av_seek_frame(stream, ts, AVSEEK_FLAG_BACKWARD)`. Decode and discard until video `pts ≥ target` and trim audio samples before the target. Unmute only when both sides are ready. While dragging the scrubber, seek to keyframes only (no discard loop) for responsiveness, then do one exact seek on release.
- **Speed other than 1×:** mute audio (or add `avfilter` atempo later) and switch to the system clock.

---

## 5. Rendering decoded frames in egui/glow

**Recommended: GPU YUV→RGB.**
- Keep the decoded `AVFrame` in its native format (yuv420p, nv12, yuv420p10le, p010, yuv444p, ...). Upload each plane to its own texture with `glTexSubImage2D`: `GL_R8` (Y, U, V), `GL_RG8` (NV12 UV), or `GL_R16` for >8-bit. Use 2-3 round-robin **PBOs** for asynchronous upload.
- 4K 4:2:0 8-bit is 12.4 MB per frame, about 750 MB/s at 60 fps. That's comfortable even on iGPUs. CPU→RGBA would be 33 MB per frame (2 GB/s) plus about 4-8 ms of conversion per frame.
- Draw with `egui::PaintCallback` + `egui_glow::CallbackFn` (see the `custom3d_glow` demo; the callback gets `painter.gl()`). Alternatively, render into an FBO-backed RGBA texture registered with `frame.register_native_glow_texture()` and display it as a normal `egui::Image`, which reuses the image-mode zoom/pan code. **The FBO route is preferred:** you get one texture per side and can reuse it for scopes and snapshots.
- Fragment shader inputs: the matrix (from `AVFrame.colorspace`) and range (`color_range`), a bit-depth scale (`65535/1023` for low-bit-packed 10-bit; ≈1 for P010), and chroma siting. When the matrix is unspecified, use BT.709 if height ≥ 720, otherwise BT.601 (the mpv/FFmpeg heuristic).

YUV→RGB math. With Kg = 1 − Kr − Kb and Y′∈[0,1], Cb,Cr∈[−0.5,0.5]:
```
Limited 8-bit:  Y′ = (Y − 16)/219,   Cb = (U − 128)/224,  Cr = (V − 128)/224   (×2^(b−8) for b-bit codes)
Full range:     Y′ = Y/255,          Cb = (U − 128)/255,  Cr = (V − 128)/255
R = Y′ + 2(1−Kr)·Cr
G = Y′ − (2Kb(1−Kb)/Kg)·Cb − (2Kr(1−Kr)/Kg)·Cr
B = Y′ + 2(1−Kb)·Cb
BT.601  Kr=0.299  Kb=0.114   → R=Y+1.402Cr,  G=Y−0.344136Cb−0.714136Cr, B=Y+1.772Cb
BT.709  Kr=0.2126 Kb=0.0722  → R=Y+1.5748Cr, G=Y−0.187324Cb−0.468124Cr, B=Y+1.8556Cb
BT.2020 Kr=0.2627 Kb=0.0593  → R=Y+1.4746Cr, G=Y−0.164553Cb−0.571353Cr, B=Y+1.8814Cb
```
The result is gamma-encoded R′G′B′. Write it to an sRGB-agnostic RGBA8 target as-is, which matches what players show for SDR. For PQ/HLG, linearize, tone map (BT.2390 EETF or Hable), apply the 2020→709 matrix, and re-encode.

**Compare modes in the same shader** (two YUV inputs → one output):
- `wipe`: pick by `dot(uv − p, n) > 0` (any angle), with a 1 px divider line.
- `checker`: pick by `(floor(x/T) + floor(y/T)) & 1`.
- `diff`: `abs(A − B)·gain`, on luma or RGB. Variants: signed `0.5 + (A − B)·gain`, a heatmap ramp, and a threshold mask.
- `flicker`: a uniform bool toggled on a timer or a held key.
- Side-by-side uses two quads with the same view transform. B is sampled in A's coordinate space (with an optional scale-to-fit), so zoom and pan are always synchronized. Use nearest-neighbour filtering at zoom ≥ 2× so individual pixels are visible.

**CPU path (still needed):** snapshots, pixel inspector RGB values, scopes, and the fallback. Use the `yuv` crate (yuvutils-rs 0.8.x: AVX2/SSE4.1/AVX-512 runtime dispatch, BT.601/709/2020, limited/full, 8-16 bit; libyuv-class speed). For format and size normalization of B (and odd pixel formats such as yuv422p10, gbrp, or paletted), use `sws_scale` → yuv420p/yuv444p16 before metrics or upload. Don't use sws for display conversion, because it's slower than the shader.

**Scopes (CPU, on a ¼-res copy, rayon):**
- Waveform: a 2D histogram (columns × 256 or 1024 levels) mapped with a log/gamma intensity curve into an R8 texture.
- Parade: three waveforms side by side.
- Vectorscope: a 2D histogram over (Cb, Cr) with a graticule (75% color targets, skin-tone line at about 123°).
- Histogram: 256 bins for Y/R/G/B.
- Each takes about 1-2 ms at ¼ res. Recompute only when the frame or settings change.

**Hardware decode (P2):** `av_hwdevice_ctx_create(AV_HWDEVICE_TYPE_D3D11VA)` + `av_hwframe_transfer_data` → NV12/P010 in system memory, then the same upload path. Zero-copy D3D11↔GL interop (`WGL_NV_DX_interop2`) isn't worth the complexity. Keep software decode as the default in compare mode for bit-exactness. FFmpeg's `threads=0` (auto) frame threading plus dav1d already handle 4K60 H.264/HEVC/AV1 on 8+ cores.

---

## 6. Recommended architecture

```
src/video/
  mod.rs          VideoMode state, keymap, mode switch from image browser
  source.rs       Source = one opened file: demuxer + video/audio decoder threads (ffmpeg-next)
  index.rs        background demux-only pass: Vec<FrameEntry{pts,dts,key,size,pos,pict_type}>
  clock.rs        MasterClock (audio atomic / system fallback), pause, speed
  audio.rs        cpal stream, swresample, ringbuf, latency estimate
  cache.rs        decoded-frame cache: forward queue + back-step GOP cache (memory-budgeted)
  sync.rs         A/B pairing: by pts (+offset) or by frame index
  render/
    yuv_gl.rs     plane textures, PBO upload, FBO, shaders (convert + compare modes + HDR)
    scopes.rs     waveform/parade/vectorscope/histogram (CPU → R8 textures)
  metrics/
    psnr.rs ssim.rs msssim.rs scdet.rs   pure Rust, rayon
    job.rs        whole-file analysis worker (decode A and B as fast as possible, emit per-frame rows)
  ui/
    timeline.rs   scrubber, filmstrip thumbnails, frame-size bars (I red / P blue / B green), bitrate line, scene markers, metric graph
    panels.rs     metadata, frame info, pixel inspector, metrics table, worst-N frames
```

Key decisions:
- **Per-source threads.** One demux thread routes packets to a video decode thread and an audio decode thread through `crossbeam-channel` (already a dependency). The UI thread only uploads textures and draws.
- **Index first.** On open, run a demux-only pass in the background: `av_read_frame`, no decoding, typically several hundred MB/s. It gives exact frame count, frame number↔PTS mapping (VFR-safe), keyframe positions, packet sizes, and the bitrate graph. Frame types come from `av_parser_parse2` (`AVCodecParserContext.pict_type`) for H.264/HEVC/MPEG-2, after `h264_mp4toannexb`/`hevc_mp4toannexb` for MP4/MKV. For AV1/VP9 the frame type is only key vs inter. Decoded `AVFrame.pict_type` overwrites the entry whenever a frame is actually decoded.
- **Frame-accurate stepping.** Forward step = next frame from the decode queue. Back step = if the frame is in the GOP cache, show it instantly. Otherwise seek to the keyframe ≤ (target − 1) using the index, decode forward to the target, and **keep every frame of that GOP** in the back cache (budget default 1 GB, about 80 frames at 4K 4:2:0 8-bit). The cache stores refcounted `AVFrame`s, with no copies.
- **Jump to frame/time** accepts `1234`, `1234f`, `00:01:02.500`, `62.5s`, and `+10f`/`-2s`. It resolves through the index to a PTS, then does an exact seek.
- **Compare pairing.** The A timeline is the master. B shows the frame with the largest `pts_B ≤ pts_A + offset` (timestamp mode) or `index_A + offset_frames` (index mode). Index mode is the default when both files have the same frame count, which is typical for encoder comparisons. Both sources decode in lockstep while paused. During playback, B follows A's clock.
- **Metrics.** "Current frame" metrics compute on demand (fast SSIM < 5 ms at 4K). Whole-file jobs run in a separate worker that opens its own decoder instances, so playback isn't disturbed. Results go into `Vec<FrameMetrics>`, which the timeline graph and CSV/JSON export read.
- **Thumbnails.** A separate low-priority decoder seeks to N evenly spaced keyframes, decodes one frame each, and scales with `sws_scale` to a height of 90 px. The results go into the existing `lru` thumbnail cache.

---

## 7. UX recommendations

What works in the best tools:
- **Instant flicker** between A and B at the *same frame* and the *same zoom* (VapourSynth previewers, video-compare). The eye catches differences in flicker that it misses side by side.
- The **wipe slider follows the mouse** (no click), with a key to freeze it (video-compare).
- **Difference mode needs amplification.** Without gain, the image just looks black.
- Raw code values under the cursor.
- Frame number is a first-class concept everywhere (HUD, copy, jump).
- Metric graph and frame-size chart *on the timeline*, and clicking them seeks.
- Worst-frames list.
- Swap A/B.
- Metadata of A and B side by side, with mismatches highlighted (resolution, fps, matrix, range). A wrong matrix or range is the most common cause of a "fake" difference.

Proposed keymap. It merges mpv, video-compare, and NLE (JKL) conventions, and stays consistent with Lumen's image-mode zoom keys.

| Key | Action |
|---|---|
| Space / K | Play/pause |
| `.` / `,` and Right / Left (when paused) | Next / previous frame (repeat while held) |
| Shift+Right/Left | ±1 s; Ctrl+Right/Left ±10 s; Up/Down ±1 min |
| PgDn / PgUp | Next / previous keyframe; Ctrl+PgDn/PgUp next/prev scene change |
| Home / End | First / last frame |
| J / L | Shuttle reverse (by stepping) / forward; repeat to go faster |
| `[` / `]`, Backspace | Speed −/+; reset to 1× |
| G | Go to frame/time dialog |
| Ctrl+C / Ctrl+V | Copy timecode+frame / paste and seek (video-compare) |
| 0 / 1 / wheel / drag | Fit / 100% / zoom at cursor / pan (same as image mode) |
| Tab (hold) or `` ` `` | Flicker: show B while held / toggle A/B |
| W | Wipe (cycle vertical / horizontal); mouse moves the divider, Shift freezes it |
| S / Shift+S | Side-by-side H / V |
| D | Difference (Up/Down with Alt = gain ×2/÷2; Y cycles abs/signed/heatmap; U luma-only) |
| C | Checkerboard (Alt+wheel = tile size) |
| X | Swap A/B |
| `+` / `-` | B offset ±1 frame (Ctrl ×10, Alt ×100), as in video-compare |
| I | Info panel (metadata + frame info); Shift+I stats overlay (decode ms, dropped frames, queue) |
| P | Pixel inspector / loupe |
| M | Metrics panel (current frame); Shift+M run whole-file analysis |
| F2-F5 | Waveform / parade / vectorscope / histogram |
| F (Ctrl+F) | Save snapshot of A (both / composited view) |
| Ctrl+L | A-B loop: first press sets in, second sets out, third clears (mpv `l` semantics) |

---

## 8. Build recipe

### 8.1 Local: Windows 11 + VS2019 BuildTools (MSVC 14.29)

```powershell
# 1. Prerequisites: LLVM (libclang for bindgen), git
winget install -e --id LLVM.LLVM
setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"

# 2. vcpkg (pin a commit for reproducibility; vcpkg auto-detects VS2019 via vswhere)
git clone https://github.com/microsoft/vcpkg C:\vcpkg
C:\vcpkg\bootstrap-vcpkg.bat -disableMetrics
setx VCPKG_ROOT C:\vcpkg

# 3. FFmpeg 9.0.x, LGPL, static libs + dynamic CRT, decoders incl. dav1d
C:\vcpkg\vcpkg install "ffmpeg[core,avcodec,avformat,swscale,swresample,dav1d]:x64-windows-static-md"
#   (~10-25 min first time; result in C:\vcpkg\installed\x64-windows-static-md\{lib,include})

# 4. New shell (so env vars apply), then:
cargo build --release
```

`Cargo.toml` additions:
```toml
[dependencies]
ffmpeg-next = { version = "=9.0.0", default-features = false, features = ["codec", "format", "software-scaling", "software-resampling", "static"] }
ffmpeg-sys-next = { version = "=9.0.1", default-features = false, features = ["avcodec", "avformat", "swscale", "swresample", "static"] }
cpal = "0.18"
ringbuf = "0.5"
yuv = "0.8"
```

`build.rs` addition (alongside the existing winresource code):
```rust
#[cfg(windows)]
for lib in ["mfplat","mfuuid","strmiids","ole32","oleaut32","user32","gdi32",
            "shlwapi","advapi32","bcrypt","secur32","ws2_32","psapi"] {
    println!("cargo:rustc-link-lib=dylib={lib}");
}
```
Notes:
- Don't set `FFMPEG_DIR`. If it's set, it takes priority over vcpkg and skips the transitive `dav1d.lib`.
- Don't set `VCPKGRS_DYNAMIC`.
- If you ever switch to `+crt-static`, use the `x64-windows-static` triplet instead and set `VCPKGRS_TRIPLET=x64-windows-static`.
- For a smaller exe, create `vcpkg-overlays/ffmpeg` (a copy of the port with the extra `--disable-*` options from §3) and install with `--overlay-ports=vcpkg-overlays`.

### 8.2 GitHub Actions (`windows-latest` = Windows Server 2025 + VS 2026)

```yaml
jobs:
  build:
    runs-on: windows-latest
    env:
      VCPKG_ROOT: ${{ github.workspace }}\vcpkg
      VCPKG_BINARY_SOURCES: clear;files,${{ github.workspace }}\vcpkg-bincache,readwrite
      VCPKG_COMMIT: <pinned-vcpkg-commit-sha>
    steps:
      - uses: actions/checkout@v4
      - name: Cache vcpkg binaries
        uses: actions/cache@v4
        with:
          path: vcpkg-bincache
          key: vcpkg-${{ env.VCPKG_COMMIT }}-ffmpeg-core-avcodec-avformat-swscale-swresample-dav1d-x64-windows-static-md
      - name: Install FFmpeg via vcpkg
        shell: pwsh
        run: |
          New-Item -ItemType Directory -Force vcpkg-bincache | Out-Null
          git clone https://github.com/microsoft/vcpkg $env:VCPKG_ROOT
          git -C $env:VCPKG_ROOT checkout $env:VCPKG_COMMIT
          & "$env:VCPKG_ROOT\bootstrap-vcpkg.bat" -disableMetrics
          & "$env:VCPKG_ROOT\vcpkg.exe" install "ffmpeg[core,avcodec,avformat,swscale,swresample,dav1d]:x64-windows-static-md"
      - uses: dtolnay/rust-toolchain@stable
      - name: Ensure libclang
        shell: pwsh
        run: |
          if (-not (Test-Path "C:\Program Files\LLVM\bin\libclang.dll")) { choco install llvm -y }
          "LIBCLANG_PATH=C:\Program Files\LLVM\bin" >> $env:GITHUB_ENV
      - run: cargo build --release
      - uses: actions/upload-artifact@v4
        with: { name: lumen, path: target/release/lumen.exe }
```

CI notes:
- vcpkg removed the `x-gha` binary-cache provider in 2025.06. The `files` provider plus `actions/cache` (above), or NuGet on GitHub Packages, are the supported replacements.
- Pin `VCPKG_COMMIT` so both the FFmpeg version and the cache key stay stable. Bump it deliberately when upgrading FFmpeg, together with the `ffmpeg-sys-next` version.
- Release job: attach the FFmpeg source tarball and a `THIRD-PARTY-LICENSES` file (FFmpeg LGPL-2.1, dav1d BSD-2) to the release.

---

## 9. Implementation order (suggested)

1. Get vcpkg FFmpeg linking, open a file, and print metadata. Index pass.
2. Video decode thread, NV12/yuv420p upload, YUV shader, play/pause without audio, then with the system clock.
3. cpal audio + audio master clock + exact seeking + frame stepping + GOP back cache.
4. Second source, sync/offset, the shader compare modes, synced zoom, pixel inspector.
5. PSNR/SSIM current frame, then the whole-file job with graph and worst-frames list.
6. Timeline frame-size bars, bitrate graph, thumbnails, scenes.
7. Scopes, HDR tone mapping, hwaccel, MS-SSIM, VMAF sidecar.

---

## Sources

- pixop/video-compare README (features, keys): https://github.com/pixop/video-compare
- MSU VQMT: https://www.compression.ru/video/quality_measure/video_measurement_tool.html
- Netflix libvmaf README and license: https://github.com/Netflix/vmaf/blob/master/libvmaf/README.md
- vcpkg `libvmaf` port (supports `!windows`): https://github.com/microsoft/vcpkg/blob/master/ports/libvmaf/vcpkg.json
- Elecard StreamEye: https://www.elecard.com/products/video-analysis/streameye
- FFmpeg `vf_ssim.c` / `vf_psnr.c`: https://github.com/FFmpeg/FFmpeg/tree/master/libavfilter
- "A Hitchhiker's Guide to Structural Similarity" (SSIM variants compared): https://arxiv.org/pdf/2101.06354
- TensorFlow `tf.image.ssim` (11x11, σ=1.5, K1/K2): https://www.tensorflow.org/api_docs/python/tf/image/ssim
- FFmpeg legal / LGPL checklist: https://ffmpeg.org/legal.html
- FFmpeg-devel thread on static linking and LGPL: https://ffmpeg.org/pipermail/ffmpeg-devel/2012-May/124935.html
- ffmpeg-next (FFmpeg 3.4 to 8.0/9.0, maintenance mode): https://github.com/zmwangx/rust-ffmpeg ; sys build.rs: https://github.com/zmwangx/rust-ffmpeg-sys/blob/master/build.rs
- ffmpeg-the-third (FFmpeg 5.1 to 9.0): https://github.com/shssoichiro/ffmpeg-the-third
- rsmpeg: https://github.com/larksuite/rsmpeg
- vcpkg ffmpeg port (9.0.2, features): https://github.com/microsoft/vcpkg/blob/master/ports/ffmpeg/vcpkg.json
- vcpkg-rs (default triplet `x64-windows-static-md`, env vars): https://docs.rs/vcpkg
- vcpkg x-gha removal: https://devblogs.microsoft.com/cppblog/whats-new-in-vcpkg-june-2025/ and https://learn.microsoft.com/en-us/vcpkg/consume/binary-caching-github-actions-cache
- GitHub runner images (windows-latest = WS2025 + VS2026): https://github.com/actions/runner-images
- Media Foundation supported formats: https://learn.microsoft.com/en-us/windows/win32/medfound/supported-media-formats-in-media-foundation ; MKV: https://learn.microsoft.com/en-us/windows/win32/medfound/mkv-support ; SetCurrentPosition seek semantics: https://learn.microsoft.com/en-us/windows/win32/api/mfreadwrite/nf-mfreadwrite-imfsourcereader-setcurrentposition
- yuvutils-rs (`yuv` crate): https://github.com/awxkee/yuvutils-rs
- egui `custom3d_glow` demo and `Frame::register_native_glow_texture`: https://github.com/emilk/egui
- gyan.dev FFmpeg builds (include libvmaf): https://www.gyan.dev/ffmpeg/builds/
- ffmpeg-sidecar: https://github.com/nathanbabcock/ffmpeg-sidecar
- Crate versions checked via the crates.io API on 2026-10-18: ffmpeg-next 9.0.0, ffmpeg-sys-next 9.0.1, ffmpeg-the-third 6.0.0, rsmpeg 0.18.0, cpal 0.18.2, ringbuf 0.5.3, yuv 0.8.19, libmpv2 6.0.0, egui/eframe 0.37.0.
