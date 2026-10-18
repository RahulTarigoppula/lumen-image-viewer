<p align="center"><img src="assets/lumen-256.png" width="96" alt="Lumen icon"></p>

<h1 align="center">Lumen</h1>
<p align="center"><b>A fast, private, modern image &amp; video viewer for Windows — a FastStone Image Viewer replacement with a built-in video analysis mode.</b><br>
One portable <code>Lumen.exe</code>. No installer, no account, no telemetry, no network access.</p>

<p align="center"><a href="https://github.com/RahulTarigoppula/lumen-image-viewer/releases/latest"><b>⬇ Download Lumen.exe</b></a></p>

---

## Why Lumen

FastStone Image Viewer is loved for being fast and complete, but it shows its age: a dated UI, a cramped tiny-preview editing workflow, no real video support and an unclear future. Lumen keeps everything that makes FastStone great — the browser/viewer workflow, keyboard-first operation, batch tools — and rebuilds it on a modern, GPU-accelerated engine:

- **Instant on huge images.** A tiled GPU renderer with a mip pyramid shows 100+ megapixel photos and pans/zooms at 60 fps. Neighbours are preloaded, so stepping through a folder is instant.
- **Any format.** JPEG, PNG/APNG, GIF (animated), WebP (animated), TIFF, BMP, ICO, TGA, PNM, QOI, DDS, HDR, OpenEXR natively; HEIC/HEIF, AVIF, JPEG XL, JPEG-XR and camera RAW (DNG, CR2/CR3, NEF, ARW, ORF, RW2, RAF, …) through Windows' codecs.
- **Correct color.** Embedded ICC profiles (Adobe RGB, Display P3, ProPhoto…) are converted to sRGB; EXIF orientation is honored everywhere.
- **Live, full-screen editing.** Every adjustment previews on the real image (not a postage stamp), hold to compare with the original, unlimited undo.
- **Lossless JPEG rotation**, atomic saves that never corrupt originals, and EXIF preserved on save.
- **Video mode** with frame-accurate stepping, A/B comparison (side-by-side, wipe, flicker, difference, overlay, checkerboard), PSNR/SSIM per frame and whole-file analysis with CSV export, I/P/B frame-size timeline, scopes (waveform, RGB parade, vectorscope, histogram) and full media info.
- **Private by design.** Ratings, flags and labels live in a local file — never written into your photos, never uploaded.

## Features

**Browser** — folder tree with favorites & drives · thumbnail grid (persistent thumbnail cache) and details view · natural sort by name/size/date/date taken/dimensions/rating · instant filter · flag, 0–5 star ratings and color labels · copy/move/rename/delete (Recycle Bin) · drag files onto folders · copy/paste files with Explorer · preview pane with info · type-ahead navigation.

**Viewer** — fit modes (fit large, fit all, fill, width, height, 100%) · smooth animated zoom at the cursor · press-and-hold magnifier · animated GIF/WebP/APNG playback with frame stepping · filmstrip · info panel with histogram, full EXIF and GPS · full screen with auto-hiding controls.

**Editing** — crop (aspect presets, rule of thirds) · rotate/flip · resize (Lanczos & more) · canvas size · auto enhance · adjust colors (exposure, brightness, contrast, gamma, saturation, vibrance, hue, temperature, tint) · levels · curves · sharpen · blur · effects · straighten · borders · text · watermark · undo/redo · save / save as with live file-size estimate.

**Tools** — batch convert & resize · batch rename with tokens and preview · contact sheets · duplicate & similar-image finder · side-by-side compare of 2–4 images with synced zoom · slideshow with transitions · set as wallpaper · print · command palette (Ctrl+K) for everything.

**Video** — play with audio (A/V synced) · `,` `.` frame stepping (also backwards) · go to frame/time (G) · A-B loop · speed control · compare two encodes with offset · PSNR-Y/U/V, SSIM · scopes · snapshots.

## Keyboard

Press **F1** in the app for the full list, or **Ctrl+K** to search every command. Highlights:

| | Browser | Viewer | Video |
|---|---|---|---|
| Open | Enter / double-click | — | drop files |
| Navigate | arrows, Home/End | ←/→, Space, PgUp/PgDn, wheel | `,` `.` step, Shift+←/→ ±1 s |
| Zoom | Ctrl+wheel (thumb size) | +/−, Ctrl+wheel, Z, `/` fit | wheel, 0 fit, 1 100% |
| Rate / flag | 0–5 / Space | 0–5 / `` ` `` | — |
| Edit | — | X crop, Ctrl+E colors, Ctrl+R resize, L/R rotate | — |
| Compare | P | P | S W D O C Tab, X swap, +/− offset |

## Install

1. Download **Lumen.exe** from [Releases](https://github.com/RahulTarigoppula/lumen-image-viewer/releases/latest).
2. Run it. That's it — it is fully portable. Settings live in `%APPDATA%\Lumen`; create an empty `portable.txt` next to the exe to keep everything beside it instead.
3. Optional: Settings → Integration → *Register file types* adds "Open with Lumen" and an Explorer "Browse with Lumen" folder menu (per-user, no admin rights).

For HEIC/AVIF/RAW, install the free Microsoft Store extensions (*HEIF Image Extensions*, *AV1 Video Extension*, *Raw Image Extension*) — Lumen picks them up automatically.

## Build from source

Requirements: Rust (stable, MSVC), Visual Studio Build Tools, and for video: vcpkg + libclang.

```powershell
git clone https://github.com/microsoft/vcpkg C:\vcpkg; C:\vcpkg\bootstrap-vcpkg.bat
C:\vcpkg\vcpkg install "ffmpeg[core,avcodec,avformat,swscale,swresample,dav1d]:x64-windows-static-md"
$env:VCPKG_ROOT="C:\vcpkg"; $env:LIBCLANG_PATH="C:\Program Files\LLVM\bin"
cargo build --release          # → target\release\lumen.exe
cargo build --release --no-default-features   # image-only build without FFmpeg
```

See [docs/VIDEO-RESEARCH.md](docs/VIDEO-RESEARCH.md) for the design of the video mode.

## Privacy

Lumen never opens a network connection. The only exception is explicit: clicking *Open location in browser* for a photo with GPS data opens OpenStreetMap in your default browser.

## License

MIT — see [LICENSE](LICENSE) and [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) (FFmpeg is LGPL-2.1+).
