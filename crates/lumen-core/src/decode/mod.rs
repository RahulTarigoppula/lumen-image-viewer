//! Image decoding. Pure-Rust decoders handle the common formats; Windows Imaging
//! Component (WIC) handles everything else Windows has a codec for (HEIC, AVIF,
//! camera RAW, JPEG-XR, JPEG-XL, ...) and acts as a fallback when a native decode fails.

pub mod color;
pub mod ico;
pub mod metafile;
pub mod pcx;
pub mod pdf;
pub mod psd;
pub mod svg;
pub mod wic;

use crate::formats;
use anyhow::{anyhow, Context as _, Result};
use image::{AnimationDecoder, DynamicImage, ImageDecoder, RgbaImage};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub struct AnimFrame {
    pub image: Arc<RgbaImage>,
    pub delay_ms: u32,
}

/// A fully decoded image, already oriented and converted to 8-bit sRGB RGBA
/// (straight alpha).
#[derive(Clone)]
pub struct Decoded {
    pub image: Arc<RgbaImage>,
    /// Present only for animations with more than one frame (frame 0 == `image`).
    pub frames: Option<Arc<Vec<AnimFrame>>>,
    pub has_alpha: bool,
    pub format: String,
    pub decoder: &'static str,
    pub color_info: String,
    pub icc_profile: Option<Vec<u8>>,
    pub exif: Option<Vec<u8>>,
    /// EXIF orientation that was applied to the pixels (1 = none).
    pub orientation: u8,
    /// Size of the source before orientation was applied.
    pub source_size: (u32, u32),
    /// Number of frames/pages in the file.
    pub frame_count: usize,
    /// Bits per channel of the source (8, 16 or 32). The pixels here are
    /// always 8-bit; callers can use this to warn before a lossy 8-bit re-save.
    pub bit_depth: u8,
}

impl Decoded {
    pub fn width(&self) -> u32 {
        self.image.width()
    }
    pub fn height(&self) -> u32 {
        self.image.height()
    }
    pub fn from_rgba(image: RgbaImage, format: &str) -> Decoded {
        let size = image.dimensions();
        let has_alpha = image_has_alpha(&image);
        Decoded {
            image: Arc::new(image),
            frames: None,
            has_alpha,
            format: format.to_string(),
            decoder: "Lumen",
            color_info: "sRGB".into(),
            icc_profile: None,
            exif: None,
            orientation: 1,
            source_size: size,
            frame_count: 1,
            bit_depth: 8,
        }
    }

    /// True when `frames` are document pages (TIFF / PDF / ICO sizes) rather than
    /// an animation: every frame has `delay_ms == 0`. The viewer should not
    /// auto-play these; step with , and . instead.
    pub fn is_paged(&self) -> bool {
        self.frames.as_ref().map(|f| f.iter().all(|f| f.delay_ms == 0)).unwrap_or(false)
    }
}

/// Upper bound for one decoded RGBA buffer (1.5 GB). Headers claiming more
/// are rejected before anything is allocated.
pub const MAX_DECODE_BYTES: u64 = 1536 * 1024 * 1024;

/// One readable line for a decode error: the anyhow chain without repeats
/// (image crate errors embed their source text, so `{:#}` printed it twice),
/// and "Format error decoding Png: ..." phrased for people.
pub fn friendly_error(e: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in e.chain() {
        let m = c.to_string().trim().trim_end_matches('.').to_string();
        if m.is_empty() || parts.iter().any(|p| p.contains(&m)) {
            continue;
        }
        parts.push(m);
    }
    let mut s = parts.join(": ");
    if let Some(rest) = s.strip_prefix("Format error decoding ") {
        if let Some((fmt, detail)) = rest.split_once(':') {
            let detail = detail.trim().trim_end_matches('.');
            let detail = detail.strip_prefix("Invalid ").map(|d| d.to_lowercase()).unwrap_or_else(|| detail.to_lowercase());
            let detail = if detail.ends_with("signature") { "bad signature".to_string() } else { detail };
            s = format!("This file is not a valid {} image ({detail})", fmt.trim().to_uppercase());
        }
    }
    if !s.ends_with('.') {
        s.push('.');
    }
    s
}

pub fn too_large_msg(w: u32, h: u32) -> String {
    format!(
        "Image is too large to open ({w} x {h} pixels needs {:.1} GB; the limit is 1.5 GB)",
        w as f64 * h as f64 * 4.0 / (1u64 << 30) as f64
    )
}

/// Fail if a `w` x `h` RGBA8 buffer would exceed [`MAX_DECODE_BYTES`].
pub fn check_alloc(w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 {
        return Err(anyhow!("Image has no pixels (0 x 0)"));
    }
    if w as u64 * h as u64 * 4 > MAX_DECODE_BYTES {
        return Err(anyhow!("{}", too_large_msg(w, h)));
    }
    Ok(())
}

/// Run a decoder that may panic on hostile input (third-party code).
pub fn guard<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err(anyhow!("The {what} decoder crashed on this file (it may be corrupt)")),
    }
}

#[derive(Clone, Copy)]
pub struct DecodeOptions {
    pub color_manage: bool,
    pub animations: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self { color_manage: true, animations: true }
    }
}

pub fn image_has_alpha(img: &RgbaImage) -> bool {
    // Sample-based check is fast enough even for huge images (stride through rows).
    let raw = img.as_raw();
    let step = if raw.len() > 64 * 1024 * 1024 { 4 * 7 } else { 4 };
    raw.iter().skip(3).step_by(step).any(|&a| a != 255)
}

/// Read the EXIF orientation (1..=8) from a raw EXIF blob.
pub fn exif_orientation(exif: &[u8]) -> u8 {
    let reader = exif::Reader::new();
    let Ok(parsed) = reader.read_raw(exif.to_vec()) else { return 1 };
    parsed
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v.clamp(1, 8) as u8)
        .unwrap_or(1)
}

pub fn apply_orientation(img: RgbaImage, orientation: u8) -> RgbaImage {
    use image::imageops;
    match orientation {
        2 => imageops::flip_horizontal(&img),
        3 => imageops::rotate180(&img),
        4 => imageops::flip_vertical(&img),
        5 => imageops::flip_horizontal(&imageops::rotate90(&img)),
        6 => imageops::rotate90(&img),
        7 => imageops::flip_horizontal(&imageops::rotate270(&img)),
        8 => imageops::rotate270(&img),
        _ => img,
    }
}

fn format_name(f: image::ImageFormat) -> &'static str {
    use image::ImageFormat as F;
    match f {
        F::Png => "PNG",
        F::Jpeg => "JPEG",
        F::Gif => "GIF",
        F::WebP => "WebP",
        F::Pnm => "PNM",
        F::Tiff => "TIFF",
        F::Tga => "TGA",
        F::Dds => "DDS",
        F::Bmp => "BMP",
        F::Ico => "ICO",
        F::Hdr => "Radiance HDR",
        F::OpenExr => "OpenEXR",
        F::Farbfeld => "Farbfeld",
        F::Avif => "AVIF",
        F::Qoi => "QOI",
        _ => "Image",
    }
}

fn color_type_name(ct: image::ColorType) -> String {
    use image::ColorType as C;
    match ct {
        C::L8 => "Grayscale 8-bit".into(),
        C::La8 => "Grayscale+Alpha 8-bit".into(),
        C::Rgb8 => "RGB 8-bit".into(),
        C::Rgba8 => "RGBA 8-bit".into(),
        C::L16 => "Grayscale 16-bit".into(),
        C::La16 => "Grayscale+Alpha 16-bit".into(),
        C::Rgb16 => "RGB 16-bit".into(),
        C::Rgba16 => "RGBA 16-bit".into(),
        C::Rgb32F => "RGB 32-bit float".into(),
        C::Rgba32F => "RGBA 32-bit float".into(),
        other => format!("{other:?}"),
    }
}

/// Decode the full image at `path`. Never panics: third-party decoder panics
/// become errors.
pub fn decode_file(path: &Path, opts: DecodeOptions) -> Result<Decoded> {
    guard("image", || decode_file_inner(path, opts))
}

/// Read a whole file with clear messages for the common failure cases.
pub fn read_file(path: &Path) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).map_err(|e| {
        let why = match e.raw_os_error() {
            Some(32) | Some(33) => "the file is locked by another program".to_string(),
            Some(2) | Some(3) => "the file no longer exists".to_string(),
            Some(5) => "access is denied".to_string(),
            _ => e.to_string(),
        };
        anyhow!("Cannot read {}: {why}", path.display())
    })?;
    if bytes.is_empty() {
        return Err(anyhow!("The file is empty (0 bytes)"));
    }
    Ok(bytes)
}

fn decode_file_inner(path: &Path, opts: DecodeOptions) -> Result<Decoded> {
    let ext = formats::ext_of(path);
    if formats::WIC_EXTS.contains(&ext.as_str()) {
        if std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(false) {
            return Err(anyhow!("The file is empty (0 bytes)"));
        }
        return wic::decode_full(path, opts.color_manage).with_context(|| missing_codec_hint(&ext));
    }
    let bytes = read_file(path)?;
    if let Some(res) = decode_special(&bytes, &ext, Some(path), opts) {
        return res;
    }
    // zune-jpeg silently mis-decodes baseline JPEGs whose components are stored
    // in separate scans (written by some encoders); Windows' decoder is correct.
    if jpeg_is_noninterleaved_baseline(&bytes) {
        if let Ok(d) = wic::decode_full(path, opts.color_manage) {
            return Ok(d);
        }
    }
    let mut d = match decode_bytes(&bytes, &ext, opts) {
        Ok(d) => d,
        Err(native_err) => match wic::decode_full(path, opts.color_manage) {
            Ok(d) => d,
            Err(_) => return Err(native_err),
        },
    };
    if opts.animations && d.frames.is_none() && (matches!(ext.as_str(), "tif" | "tiff") || d.format.starts_with("TIFF")) {
        add_tiff_pages(path, &mut d, opts.color_manage);
    }
    Ok(d)
}

/// Multi-page TIFF: expose pages 2..N as frames (delay 0 = pages, no auto-play).
fn add_tiff_pages(path: &Path, d: &mut Decoded, color_manage: bool) {
    let Ok(n) = wic::frame_count(path) else { return };
    if n <= 1 {
        return;
    }
    let budget = MAX_DECODE_BYTES.saturating_sub(d.image.as_raw().len() as u64);
    let Ok(rest) = wic::decode_frames(path, 1, n, color_manage, budget) else { return };
    let mut frames = Vec::with_capacity(rest.len() + 1);
    frames.push(AnimFrame { image: d.image.clone(), delay_ms: 0 });
    frames.extend(rest.into_iter().map(|img| AnimFrame { image: Arc::new(img), delay_ms: 0 }));
    if frames.len() > 1 {
        d.frames = Some(Arc::new(frames));
    }
    d.frame_count = n;
    d.format = format!("TIFF ({n} pages)");
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let t = text.trim_start_matches('\u{feff}').trim_start();
    (t.starts_with("<?xml") || t.starts_with("<svg") || t.starts_with("<!--") || t.starts_with("<!DOCTYPE svg")) && text.contains("<svg")
}

/// Formats handled outside the image crate (by content sniffing first, then
/// by extension). Returns None when `bytes` is none of them.
fn decode_special(bytes: &[u8], ext: &str, path: Option<&Path>, opts: DecodeOptions) -> Option<Result<Decoded>> {
    if bytes.is_empty() {
        return Some(Err(anyhow!("The file is empty (0 bytes)")));
    }
    if psd::is_psd(bytes) {
        return Some(guard("PSD", || psd::decode(bytes, opts.color_manage)));
    }
    if bytes.starts_with(b"%PDF-") || (ext == "pdf" && pdf::is_pdf(bytes)) {
        return Some(guard("PDF", || pdf::decode(bytes, opts.animations)));
    }
    if metafile::is_emf(bytes) || (matches!(ext, "wmf" | "emf") && metafile::is_wmf(bytes)) || bytes.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A]) {
        return Some(guard("metafile", || metafile::decode(bytes)));
    }
    if matches!(ext, "svg" | "svgz") || looks_like_svg(bytes) {
        return Some(guard("SVG", || svg::decode(bytes, path)));
    }
    if ext == "pcx" {
        return Some(guard("PCX", || pcx::decode(bytes)));
    }
    if matches!(ext, "ico" | "cur") && ico::is_ico(bytes) {
        return Some(guard("icon", || ico::decode(bytes)));
    }
    match ext {
        "psd" | "psb" => Some(Err(anyhow!("Not a valid Photoshop file"))),
        "pdf" => Some(Err(anyhow!("Not a valid PDF file"))),
        "wmf" | "emf" => Some(Err(anyhow!("Not a valid Windows metafile"))),
        _ => None,
    }
}

pub fn missing_codec_hint(ext: &str) -> String {
    match ext {
        "heic" | "heif" | "hif" => "No HEIF codec found. Install \"HEIF Image Extensions\" and \"HEVC Video Extensions\" from the Microsoft Store.".into(),
        "avif" => "No AVIF codec found. Install \"AV1 Video Extension\" from the Microsoft Store.".into(),
        "jxl" => "No JPEG XL codec found. Install the \"JPEG XL Image Extension\" from the Microsoft Store.".into(),
        e if formats::is_raw_ext(e) => "No RAW codec found. Install \"Raw Image Extension\" from the Microsoft Store.".into(),
        _ => "Windows has no decoder for this format.".into(),
    }
}

/// Decode an in-memory image. `ext` is a hint used when the content is not
/// recognisable on its own.
pub fn decode_bytes(bytes: &[u8], ext: &str, opts: DecodeOptions) -> Result<Decoded> {
    if let Some(res) = decode_special(bytes, ext, None, opts) {
        return res;
    }
    guard("image", || decode_bytes_native(bytes, ext, opts))
}

fn decode_bytes_native(bytes: &[u8], ext: &str, opts: DecodeOptions) -> Result<Decoded> {
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let format = reader
        .format()
        .or_else(|| image::ImageFormat::from_extension(ext))
        .ok_or_else(|| anyhow!("Unrecognised image format"))?;

    if opts.animations {
        if let Some(anim) = try_decode_animation(bytes, format)? {
            return Ok(anim);
        }
    }

    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.no_limits();
    let mut decoder = reader.into_decoder()?;
    let icc = decoder.icc_profile().ok().flatten();
    let exif = decoder.exif_metadata().ok().flatten();
    let orientation = exif.as_deref().map(exif_orientation).unwrap_or(1);
    let color_type = decoder.color_type();
    let (sw, sh) = decoder.dimensions();
    check_alloc(sw, sh)?;
    // The decoder's own buffer (16-bit / float) plus our RGBA8 copy.
    if decoder.total_bytes().saturating_add(sw as u64 * sh as u64 * 4) > 2 * MAX_DECODE_BYTES {
        return Err(anyhow!("{}", too_large_msg(sw, sh)));
    }
    let bit_depth = (color_type.bits_per_pixel() / color_type.channel_count().max(1) as u16).clamp(1, 32) as u8;
    let dynimg = DynamicImage::from_decoder(decoder)?;
    let has_alpha_channel = color_type.has_alpha();
    let mut rgba = to_rgba8(dynimg);
    let mut color_info = color_type_name(color_type);

    if opts.color_manage {
        if let Some(profile) = icc.as_deref() {
            match color::to_srgb_in_place(&mut rgba, profile) {
                color::Outcome::Converted(name) => color_info = format!("{color_info} · {name} → sRGB"),
                color::Outcome::AlreadySrgb => color_info = format!("{color_info} · sRGB"),
                color::Outcome::Unsupported => color_info = format!("{color_info} · ICC (unmanaged)"),
            }
        }
    }
    let rgba = apply_orientation(rgba, orientation);
    let has_alpha = has_alpha_channel && image_has_alpha(&rgba);
    Ok(Decoded {
        image: Arc::new(rgba),
        frames: None,
        has_alpha,
        format: format_name(format).to_string(),
        decoder: "Native",
        color_info,
        icc_profile: icc,
        exif,
        orientation,
        source_size: (sw, sh),
        frame_count: 1,
        bit_depth: bit_depth.max(8),
    })
}

/// HDR / 16-bit / float images are tonemapped down to 8-bit sRGB for display.
fn to_rgba8(img: DynamicImage) -> RgbaImage {
    match img {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            // Simple Reinhard-ish tonemap for HDR content, then sRGB encode.
            let f = img.to_rgba32f();
            let (w, h) = f.dimensions();
            let mut out = RgbaImage::new(w, h);
            for (o, p) in out.pixels_mut().zip(f.pixels()) {
                let map = |v: f32| {
                    let v = v.max(0.0);
                    let t = v / (1.0 + v * 0.25);
                    let s = if t <= 0.0031308 { 12.92 * t } else { 1.055 * t.powf(1.0 / 2.4) - 0.055 };
                    (s.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
                };
                o.0 = [map(p.0[0]), map(p.0[1]), map(p.0[2]), (p.0[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8];
            }
            out
        }
        other => other.into_rgba8(),
    }
}

fn try_decode_animation(bytes: &[u8], format: image::ImageFormat) -> Result<Option<Decoded>> {
    use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
    let frames: Vec<image::Frame> = match format {
        image::ImageFormat::Gif => {
            let dec = GifDecoder::new(Cursor::new(bytes))?;
            let (w, h) = dec.dimensions();
            check_alloc(w, h)?;
            collect_frames_limited(dec.into_frames())?
        }
        image::ImageFormat::Png => {
            let dec = PngDecoder::new(Cursor::new(bytes))?;
            if !dec.is_apng()? {
                return Ok(None);
            }
            let (w, h) = dec.dimensions();
            check_alloc(w, h)?;
            collect_frames_limited(dec.apng()?.into_frames())?
        }
        image::ImageFormat::WebP => {
            let dec = WebPDecoder::new(Cursor::new(bytes))?;
            if !dec.has_animation() {
                return Ok(None);
            }
            let (w, h) = dec.dimensions();
            check_alloc(w, h)?;
            collect_frames_limited(dec.into_frames())?
        }
        _ => return Ok(None),
    };
    if frames.is_empty() {
        return Err(anyhow!("Animation contains no frames"));
    }
    let count = frames.len();
    let anim: Vec<AnimFrame> = frames
        .into_iter()
        .map(|f| {
            let (n, d) = f.delay().numer_denom_ms();
            let mut ms = if d == 0 { 100 } else { n / d.max(1) };
            // Browsers clamp tiny delays to 100ms; follow the same convention.
            if ms <= 10 {
                ms = 100;
            }
            AnimFrame { image: Arc::new(f.into_buffer()), delay_ms: ms }
        })
        .collect();
    let first = anim[0].image.clone();
    let size = first.dimensions();
    let has_alpha = anim.iter().take(4).any(|f| image_has_alpha(&f.image));
    Ok(Some(Decoded {
        image: first,
        frames: if count > 1 { Some(Arc::new(anim)) } else { None },
        has_alpha,
        format: if count > 1 { format!("{} (animated)", format_name(format)) } else { format_name(format).into() },
        decoder: "Native",
        color_info: "RGBA 8-bit".into(),
        icc_profile: None,
        exif: None,
        orientation: 1,
        source_size: size,
        frame_count: count,
        bit_depth: 8,
    }))
}

fn collect_frames_limited(frames: image::Frames<'_>) -> Result<Vec<image::Frame>> {
    // Cap total memory for very long animations (~1.5 GB of RGBA).
    const BUDGET: usize = 1536 * 1024 * 1024;
    let mut used = 0usize;
    let mut out = Vec::new();
    for f in frames {
        let f = f?;
        used += f.buffer().as_raw().len();
        out.push(f);
        if used > BUDGET {
            break;
        }
    }
    Ok(out)
}

/// Decode a reduced-size version of the image whose longest side is at most
/// `max_side` (used for thumbnails and instant first-paint previews).
pub fn decode_thumbnail(path: &Path, max_side: u32) -> Result<RgbaImage> {
    guard("thumbnail", || decode_thumbnail_inner(path, max_side))
}

fn decode_thumbnail_inner(path: &Path, max_side: u32) -> Result<RgbaImage> {
    let ext = formats::ext_of(path);
    if crate::video::is_video_ext(&ext) {
        return crate::video::thumbnail(path, max_side);
    }
    // Vector and document formats render straight at the requested size.
    match ext.as_str() {
        "svg" | "svgz" => return svg::render(&read_file(path)?, Some(path), Some(max_side)),
        "pdf" => return pdf::first_page(&read_file(path)?, max_side),
        "wmf" | "emf" => return metafile::render(&read_file(path)?, Some(max_side)).map(|r| r.0),
        _ => {}
    }
    // WIC can scale JPEGs during decode (DCT scaling) and pull embedded RAW previews,
    // which is dramatically faster than a full decode for large photos.
    if formats::is_jpeg_ext(&ext) || formats::WIC_EXTS.contains(&ext.as_str()) || ext == "tif" || ext == "tiff" {
        if let Ok(img) = wic::decode_scaled(path, max_side) {
            return Ok(img);
        }
    }
    let decoded = decode_file(path, DecodeOptions { color_manage: true, animations: false })?;
    Ok(crate::imaging::resize_to_fit(&decoded.image, max_side, max_side))
}

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(_) => return None,
        }
    }
    buf.truncate(got);
    Some(buf)
}

/// Cheap header-only probe of image dimensions.
pub fn probe_dimensions(path: &Path) -> Option<(u32, u32)> {
    guard("probe", || Ok(probe_dimensions_inner(path))).ok().flatten()
}

fn probe_dimensions_inner(path: &Path) -> Option<(u32, u32)> {
    let ext = formats::ext_of(path);
    match ext.as_str() {
        "psd" | "psb" => return psd::probe(&read_head(path, 26)?),
        "pcx" => return pcx::probe(&read_head(path, 128)?),
        "ico" | "cur" => return ico::probe(&read_file(path).ok()?),
        "svg" | "svgz" => return svg::probe(&read_file(path).ok()?),
        "wmf" | "emf" => return metafile::probe(&read_file(path).ok()?),
        "pdf" => return pdf::probe(&read_file(path).ok()?),
        _ => {}
    }
    if formats::NATIVE_EXTS.contains(&ext.as_str()) {
        if let Ok(r) = image::ImageReader::open(path).and_then(|r| r.with_guessed_format()) {
            if let Ok(dec) = r.into_decoder() {
                let (w, h) = dec.dimensions();
                let swap = matches!(exif_orientation_from_file(path), 5..=8);
                return Some(if swap { (h, w) } else { (w, h) });
            }
        }
    }
    wic::probe_dimensions(path).ok()
}

pub fn exif_orientation_from_file(path: &Path) -> u8 {
    let Ok(file) = std::fs::File::open(path) else { return 1 };
    let mut reader = std::io::BufReader::new(file);
    let Ok(parsed) = exif::Reader::new().read_from_container(&mut reader) else { return 1 };
    parsed
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v.clamp(1, 8) as u8)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn friendly_errors_dedupe_and_rephrase() {
        // A fake PNG through the image crate.
        let e = anyhow::Error::from(image::load_from_memory_with_format(b"not a png at all", image::ImageFormat::Png).unwrap_err());
        let m = friendly_error(&e);
        assert_eq!(m.matches("signature").count(), 1, "{m}");
        assert!(m.starts_with("This file is not a valid PNG image"), "{m}");
        let e = anyhow!("outer: inner detail").context("Cannot open");
        assert_eq!(friendly_error(&e), "Cannot open: outer: inner detail.");
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lumen-decode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn png_bytes(img: DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    fn jpeg_bytes(w: u16, h: u16) -> Vec<u8> {
        let rgb: Vec<u8> = (0..w as usize * h as usize).flat_map(|i| [(i * 7) as u8, 128, 200]).collect();
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 90).encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
        out
    }

    #[test]
    fn empty_and_truncated_files() {
        let d = tmpdir("trunc");
        for ext in ["png", "jpg", "psd", "svg", "pdf", "pcx", "emf", "heic", "tif"] {
            let p = d.join(format!("empty.{ext}"));
            std::fs::write(&p, b"").unwrap();
            let e = decode_file(&p, DecodeOptions::default()).err().unwrap_or_else(|| panic!("{ext} decoded"));
            assert!(format!("{e:#}").contains("empty"), "{ext}: {e:#}");
            assert!(decode_thumbnail(&p, 64).is_err());
            assert_eq!(probe_dimensions(&p), None);
        }
        let png = png_bytes(DynamicImage::ImageRgb8(image::RgbImage::new(64, 64)));
        let jpg = jpeg_bytes(64, 64);
        for (ext, data) in [("png", &png), ("jpg", &jpg)] {
            for cut in [8usize, 30, data.len() / 2] {
                let p = d.join(format!("cut{cut}.{ext}"));
                std::fs::write(&p, &data[..cut]).unwrap();
                // Must not panic; a partial decode via WIC is acceptable.
                let _ = decode_file(&p, DecodeOptions::default());
                let _ = decode_thumbnail(&p, 32);
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn huge_claimed_dimensions_are_refused() {
        // A PNG header claiming 60000 x 60000 (13.4 GB of RGBA).
        let mut png = png_bytes(DynamicImage::ImageRgb8(image::RgbImage::new(1, 1)));
        png[16..20].copy_from_slice(&60000u32.to_be_bytes());
        png[20..24].copy_from_slice(&60000u32.to_be_bytes());
        // Fix the IHDR CRC so decoders accept the header.
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let e = decode_bytes(&png, "png", DecodeOptions::default()).err().expect("must fail");
        assert!(format!("{e}").contains("too large"), "{e}");
        let d = tmpdir("huge");
        let p = d.join("huge.png");
        std::fs::write(&p, &png).unwrap();
        let e = decode_file(&p, DecodeOptions::default()).err().expect("must fail");
        assert!(format!("{e}").contains("too large"), "{e}");
        assert!(check_alloc(30000, 30000).is_err());
        assert!(check_alloc(16000, 16000).is_ok());
        assert!(check_alloc(0, 10).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn crc32(b: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &x in b {
            c ^= x as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    }

    #[test]
    fn png_variants() {
        // 16-bit RGBA
        let mut img16 = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::new(3, 2);
        for p in img16.pixels_mut() {
            p.0 = [65535, 32768, 0, 65535];
        }
        let d = decode_bytes(&png_bytes(DynamicImage::ImageRgba16(img16)), "png", DecodeOptions::default()).unwrap();
        assert_eq!(d.bit_depth, 16);
        assert_eq!(d.image.get_pixel(0, 0).0, [255, 128, 0, 255]);
        assert!(!d.has_alpha);
        // Grayscale + alpha
        let la = image::GrayAlphaImage::from_pixel(2, 2, image::LumaA([90, 40]));
        let d = decode_bytes(&png_bytes(DynamicImage::ImageLumaA8(la)), "png", DecodeOptions::default()).unwrap();
        assert_eq!(d.image.get_pixel(1, 1).0, [90, 90, 90, 40]);
        assert!(d.has_alpha);
        assert_eq!(d.bit_depth, 8);
        // Palette with tRNS
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 1);
            enc.set_color(png::ColorType::Indexed);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_palette(vec![255, 0, 0, 0, 0, 255]);
            enc.set_trns(vec![255, 0]);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[0, 1]).unwrap();
        }
        let d = decode_bytes(&out, "png", DecodeOptions::default()).unwrap();
        assert_eq!(d.image.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(d.image.get_pixel(1, 0).0[3], 0);
        assert!(d.has_alpha);
    }

    #[test]
    fn cmyk_jpeg_colors() {
        // Pure cyan, pure black, white (CMYK input; jpeg-encoder writes Adobe APP14).
        let cmyk = [255u8, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0];
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 100).encode(&cmyk, 3, 1, jpeg_encoder::ColorType::Cmyk).unwrap();
        let d = decode_bytes(&out, "jpg", DecodeOptions::default()).unwrap();
        let near = |a: [u8; 4], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() < 40);
        assert!(near(d.image.get_pixel(0, 0).0, [0, 255, 255]), "cyan {:?}", d.image.get_pixel(0, 0));
        assert!(near(d.image.get_pixel(1, 0).0, [0, 0, 0]), "black {:?}", d.image.get_pixel(1, 0));
        assert!(near(d.image.get_pixel(2, 0).0, [255, 255, 255]), "white {:?}", d.image.get_pixel(2, 0));
    }

    #[test]
    fn unicode_long_and_locked_paths() {
        let d = tmpdir("paths");
        // Unicode names.
        let uni = d.join("\u{1F600} caf\u{e9} \u{0444}\u{043e}\u{0442}\u{043e}.jpg");
        std::fs::write(&uni, jpeg_bytes(40, 20)).unwrap();
        assert_eq!(decode_file(&uni, DecodeOptions::default()).unwrap().image.dimensions(), (40, 20));
        assert!(decode_thumbnail(&uni, 16).is_ok());
        assert_eq!(probe_dimensions(&uni), Some((40, 20)));
        // Long path (> MAX_PATH).
        let mut deep = d.clone();
        while deep.as_os_str().len() < 300 {
            deep = deep.join("a_rather_long_directory_name_for_testing");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let long = deep.join("photo.jpg");
        std::fs::write(&long, jpeg_bytes(40, 20)).unwrap();
        assert_eq!(decode_file(&long, DecodeOptions::default()).unwrap().image.dimensions(), (40, 20));
        assert_eq!(decode_thumbnail(&long, 16).unwrap().width(), 16);
        assert_eq!(wic::decode_full(&long, true).unwrap().image.dimensions(), (40, 20));
        // Open for writing by "another program" that only shares reading.
        let locked = d.join("locked.jpg");
        std::fs::write(&locked, jpeg_bytes(40, 20)).unwrap();
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _hold = std::fs::OpenOptions::new().read(true).write(true).share_mode(1).open(&locked).unwrap();
            assert!(decode_file(&locked, DecodeOptions::default()).is_ok());
            assert!(wic::decode_scaled(&locked, 16).is_ok());
        }
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _hold = std::fs::OpenOptions::new().read(true).share_mode(0).open(&locked).unwrap();
            let e = decode_file(&locked, DecodeOptions::default()).err().expect("exclusive lock");
            assert!(format!("{e:#}").contains("locked"), "{e:#}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn multipage_tiff_pages_become_frames() {
        let d = tmpdir("tiff");
        let p = d.join("pages.tif");
        {
            let f = std::fs::File::create(&p).unwrap();
            let mut enc = tiff::encoder::TiffEncoder::new(f).unwrap();
            for (i, side) in [(0u8, 8u32), (1, 8), (2, 6)] {
                let data = vec![i * 100; (side * side * 3) as usize];
                enc.write_image::<tiff::encoder::colortype::RGB8>(side, side, &data).unwrap();
            }
        }
        let dec = decode_file(&p, DecodeOptions::default()).unwrap();
        assert_eq!(dec.frame_count, 3);
        assert_eq!(dec.format, "TIFF (3 pages)");
        assert!(dec.is_paged());
        let frames = dec.frames.as_ref().unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1].image.get_pixel(0, 0).0, [100, 100, 100, 255]);
        assert_eq!(frames[2].image.dimensions(), (6, 6));
        // Thumbnails / no-animation decodes stay single-page.
        let single = decode_file(&p, DecodeOptions { color_manage: true, animations: false }).unwrap();
        assert!(single.frames.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn new_formats_via_files() {
        let d = tmpdir("formats");
        let svg = d.join("a.svg");
        std::fs::write(&svg, br##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50" fill="#00f"/></svg>"##).unwrap();
        assert_eq!(decode_file(&svg, DecodeOptions::default()).unwrap().format, "SVG");
        assert_eq!(decode_thumbnail(&svg, 64).unwrap().dimensions(), (64, 32));
        assert_eq!(probe_dimensions(&svg), Some((100, 50)));
        let psd_path = d.join("a.psd");
        std::fs::write(&psd_path, psd::tests::build(false, 3, 8, 4, 2, &[vec![9; 8], vec![8; 8], vec![7; 8]], true, false)).unwrap();
        assert_eq!(decode_file(&psd_path, DecodeOptions::default()).unwrap().image.get_pixel(3, 1).0, [9, 8, 7, 255]);
        assert_eq!(probe_dimensions(&psd_path), Some((4, 2)));
        assert_eq!(decode_thumbnail(&psd_path, 2).unwrap().width(), 2);
        let pdf_path = d.join("a.pdf");
        std::fs::write(&pdf_path, pdf::tests::sample_pdf(2)).unwrap();
        assert_eq!(decode_thumbnail(&pdf_path, 100).unwrap().height(), 100);
        assert_eq!(decode_file(&pdf_path, DecodeOptions::default()).unwrap().frame_count, 2);
        assert!(probe_dimensions(&pdf_path).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// True for a baseline/extended sequential JPEG (SOF0/SOF1) with more than one
/// component whose first scan does not contain all components.
pub fn jpeg_is_noninterleaved_baseline(d: &[u8]) -> bool {
    if d.len() < 4 || d[0] != 0xFF || d[1] != 0xD8 {
        return false;
    }
    let mut sof_components = 0u8;
    let mut baseline = false;
    let mut i = 2;
    while i + 4 <= d.len() {
        if d[i] != 0xFF {
            return false;
        }
        let m = d[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
        match m {
            0xC0 | 0xC1 => {
                baseline = true;
                sof_components = *d.get(i + 9).unwrap_or(&0);
            }
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return false,
            0xDA => {
                let scan_components = *d.get(i + 4).unwrap_or(&0);
                return baseline && sof_components > 1 && scan_components < sof_components;
            }
            _ => {}
        }
        i += 2 + len;
    }
    false
}

#[cfg(test)]
mod jpeg_layout_tests {
    #[test]
    fn detects_noninterleaved_baseline() {
        let img = image::RgbaImage::from_fn(32, 32, |x, y| image::Rgba([x as u8 * 8, y as u8 * 8, 77, 255]));
        let rgb = crate::encode::flatten_rgb(&img, [0, 0, 0]);
        let mut bad = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut bad, 90);
        enc.set_optimized_huffman_tables(true);
        enc.encode(&rgb, 32, 32, jpeg_encoder::ColorType::Rgb).unwrap();
        assert!(super::jpeg_is_noninterleaved_baseline(&bad));
        let mut good = Vec::new();
        jpeg_encoder::Encoder::new(&mut good, 90).encode(&rgb, 32, 32, jpeg_encoder::ColorType::Rgb).unwrap();
        assert!(!super::jpeg_is_noninterleaved_baseline(&good));
        assert!(!super::jpeg_is_noninterleaved_baseline(b"not a jpeg"));
    }
}
