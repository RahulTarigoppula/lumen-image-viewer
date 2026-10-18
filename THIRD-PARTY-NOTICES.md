# Third-party notices

Lumen itself is MIT licensed. The single executable statically includes the
following components:

| Component | License | Notes |
|---|---|---|
| [FFmpeg](https://ffmpeg.org) 9.0 (libavcodec, libavformat, libavutil, libswscale, libswresample) | LGPL-2.1-or-later | Built without `--enable-gpl` / `--enable-nonfree` via vcpkg (see `.github/workflows/build.yml`). You may relink Lumen against your own FFmpeg build: the complete Lumen source and build recipe are in this repository. FFmpeg source: https://ffmpeg.org/download.html |
| [dav1d](https://code.videolan.org/videolan/dav1d) | BSD-2-Clause | AV1 decoding |
| [egui / eframe](https://github.com/emilk/egui) | MIT OR Apache-2.0 | UI |
| [image-rs](https://github.com/image-rs/image) and codecs | MIT OR Apache-2.0 | Image decoding/encoding |
| [libwebp](https://chromium.googlesource.com/webm/libwebp) (via `webp` crate) | BSD-3-Clause | WebP encoding |
| [fast_image_resize](https://github.com/Cykooz/fast_image_resize) | MIT OR Apache-2.0 | Resampling |
| [qcms](https://github.com/FirefoxGraphics/qcms) | MIT | ICC color management |
| [kamadak-exif](https://github.com/kamadak/exif-rs) | BSD-2-Clause | EXIF parsing |
| [cpal](https://github.com/RustAudio/cpal) | Apache-2.0 | Audio output |
| [Phosphor Icons](https://phosphoricons.com) | MIT | Icon font (`assets/PHOSPHOR-LICENSE-MIT.txt`) |
| [windows-rs](https://github.com/microsoft/windows-rs) | MIT OR Apache-2.0 | Windows APIs (WIC, shell, GDI) |

Codecs such as H.264/HEVC may be covered by patents in some countries; this is
the same situation as for VLC, mpv and other open-source players.

Fonts are not bundled: Lumen uses the fonts installed with Windows (Segoe UI).
