//! Image decoding. Pure-Rust decoders handle the common formats; Windows Imaging
//! Component (WIC) handles everything else Windows has a codec for (HEIC, AVIF,
//! camera RAW, JPEG-XR, JPEG-XL, …) and acts as a fallback when a native decode fails.

pub mod color;
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
        }
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

/// Decode the full image at `path`.
pub fn decode_file(path: &Path, opts: DecodeOptions) -> Result<Decoded> {
    let ext = formats::ext_of(path);
    if formats::WIC_EXTS.contains(&ext.as_str()) {
        return wic::decode_full(path, opts.color_manage)
            .with_context(|| missing_codec_hint(&ext));
    }
    if ext == "svg" {
        return Err(anyhow!("SVG rendering is not supported yet"));
    }
    let bytes = std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    match decode_bytes(&bytes, &ext, opts) {
        Ok(d) => Ok(d),
        Err(native_err) => match wic::decode_full(path, opts.color_manage) {
            Ok(d) => Ok(d),
            Err(_) => Err(native_err),
        },
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

pub fn decode_bytes(bytes: &[u8], ext: &str, opts: DecodeOptions) -> Result<Decoded> {
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
            collect_frames_limited(dec.into_frames())?
        }
        image::ImageFormat::Png => {
            let dec = PngDecoder::new(Cursor::new(bytes))?;
            if !dec.is_apng()? {
                return Ok(None);
            }
            collect_frames_limited(dec.apng()?.into_frames())?
        }
        image::ImageFormat::WebP => {
            let dec = WebPDecoder::new(Cursor::new(bytes))?;
            if !dec.has_animation() {
                return Ok(None);
            }
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
    let ext = formats::ext_of(path);
    if crate::video::is_video_ext(&ext) {
        return crate::video::thumbnail(path, max_side);
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

/// Cheap header-only probe of image dimensions.
pub fn probe_dimensions(path: &Path) -> Option<(u32, u32)> {
    let ext = formats::ext_of(path);
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
