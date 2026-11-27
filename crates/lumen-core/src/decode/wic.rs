//! Windows Imaging Component decoding: anything Windows has a codec for.

use super::{apply_orientation, exif_orientation_from_file, image_has_alpha, Decoded};
use crate::formats;
use anyhow::{anyhow, Result};
use image::RgbaImage;
use std::cell::RefCell;
use std::path::Path;
use std::sync::Arc;
use windows::core::{Interface, GUID, HSTRING};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

thread_local! {
    static FACTORY: RefCell<Option<IWICImagingFactory>> = const { RefCell::new(None) };
}

/// Run `f` with a per-thread WIC factory (COM is initialised on first use).
pub fn with_factory<R>(f: impl FnOnce(&IWICImagingFactory) -> Result<R>) -> Result<R> {
    FACTORY.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            unsafe {
                // S_FALSE / RPC_E_CHANGED_MODE are fine: COM is usable either way.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let factory: IWICImagingFactory =
                    CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
                *slot = Some(factory);
            }
        }
        f(slot.as_ref().unwrap())
    })
}

/// Paths of 248+ characters need the `\\?\` prefix for most Win32 APIs.
pub fn long_path(path: &Path) -> std::path::PathBuf {
    let s = path.as_os_str().to_string_lossy();
    if s.len() < 248 || s.starts_with(r"\\?\") || !path.is_absolute() {
        return path.to_path_buf();
    }
    if let Some(unc) = s.strip_prefix(r"\\") {
        return std::path::PathBuf::from(format!(r"\\?\UNC\{unc}"));
    }
    std::path::PathBuf::from(format!(r"\\?\{s}"))
}

pub fn open_decoder(factory: &IWICImagingFactory, path: &Path) -> Result<IWICBitmapDecoder> {
    unsafe {
        let direct = factory.CreateDecoderFromFilename(
            &HSTRING::from(long_path(path).as_os_str()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        );
        match direct {
            Ok(d) => Ok(d),
            Err(e) => {
                // Sharing / lock violations (file open in another app without
                // read sharing) and path problems: retry from memory, since
                // std::fs::read opens with full sharing.
                let code = e.code().0 as u32;
                if !matches!(code, 0x8007_0020 | 0x8007_0021 | 0x8007_0003 | 0x8007_00CE | 0x8007_007B | 0x8007_0005) {
                    return Err(e.into());
                }
                let bytes = std::fs::read(path).map_err(|_| anyhow::Error::from(e.clone()))?;
                if bytes.is_empty() || bytes.len() >= u32::MAX as usize {
                    return Err(e.into());
                }
                let stream = windows::Win32::UI::Shell::SHCreateMemStream(Some(&bytes)).ok_or_else(|| anyhow!("Out of memory"))?;
                Ok(factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?)
            }
        }
    }
}

fn open_frame(factory: &IWICImagingFactory, path: &Path) -> Result<(IWICBitmapDecoder, IWICBitmapFrameDecode)> {
    unsafe {
        let decoder = open_decoder(factory, path)?;
        let frame = decoder.GetFrame(0)?;
        Ok((decoder, frame))
    }
}

/// Number of frames / pages in the file.
pub fn frame_count(path: &Path) -> Result<usize> {
    with_factory(|factory| unsafe { Ok(open_decoder(factory, path)?.GetFrameCount()? as usize) })
}

/// Decode frames `from..to` (pages of a multi-page TIFF), stopping once
/// `budget` bytes of RGBA have been produced. Gives up before a frame once
/// the decode is no longer wanted (see [`super::with_cancel`]).
pub fn decode_frames(path: &Path, from: usize, to: usize, color_manage: bool, budget: u64) -> Result<Vec<RgbaImage>> {
    with_factory(|factory| unsafe {
        let decoder = open_decoder(factory, path)?;
        let n = (decoder.GetFrameCount()? as usize).min(to);
        let mut out = Vec::new();
        let mut used = 0u64;
        for i in from..n {
            super::check_cancel()?;
            let frame = decoder.GetFrame(i as u32)?;
            let (mut w, mut h) = (0u32, 0u32);
            frame.GetSize(&mut w, &mut h)?;
            used += w as u64 * h as u64 * 4;
            if used > budget {
                break;
            }
            let source = if color_manage { color_managed(factory, &frame)?.0 } else { frame.cast()? };
            match source_to_rgba(&source) {
                Ok(img) => out.push(img),
                Err(_) => break,
            }
        }
        Ok(out)
    })
}

fn source_to_rgba(source: &IWICBitmapSource) -> Result<RgbaImage> {
    unsafe {
        let converted = WICConvertBitmapSource(&GUID_WICPixelFormat32bppRGBA, source)?;
        let (mut w, mut h) = (0u32, 0u32);
        converted.GetSize(&mut w, &mut h)?;
        super::check_alloc(w, h)?;
        let stride = w as usize * 4;
        let total = stride
            .checked_mul(h as usize)
            .ok_or_else(|| anyhow!("Image too large"))?;
        let mut buf = vec![0u8; total];
        // Copy in bands so huge images don't hit WIC's 32-bit buffer limits.
        let band_rows = ((256 * 1024 * 1024) / stride).max(1) as u32;
        let mut y = 0u32;
        while y < h {
            let rows = band_rows.min(h - y);
            let rect = WICRect { X: 0, Y: y as i32, Width: w as i32, Height: rows as i32 };
            let start = y as usize * stride;
            let end = start + rows as usize * stride;
            converted.CopyPixels(&rect, stride as u32, &mut buf[start..end])?;
            y += rows;
        }
        RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("Bad buffer"))
    }
}

/// Wrap the frame in a color transform to sRGB when it carries a non-sRGB profile.
fn color_managed(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode) -> Result<(IWICBitmapSource, String)> {
    color_managed_source(factory, frame, frame.cast()?)
}

/// Wrap `source` (the frame, or a scaled copy of it) in a color transform to
/// sRGB when `frame` carries a non-sRGB profile.
fn color_managed_source(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode, source: IWICBitmapSource) -> Result<(IWICBitmapSource, String)> {
    unsafe {
        let mut count = 0u32;
        if frame.GetColorContexts(&mut [], &mut count).is_err() || count == 0 {
            return Ok((source, String::new()));
        }
        let mut contexts: Vec<Option<IWICColorContext>> = Vec::with_capacity(count as usize);
        for _ in 0..count {
            contexts.push(Some(factory.CreateColorContext()?));
        }
        let mut actual = 0u32;
        frame.GetColorContexts(&mut contexts, &mut actual)?;
        let Some(src_ctx) = contexts.into_iter().flatten().next() else {
            return Ok((source, String::new()));
        };
        let mut label = String::from("ICC");
        if src_ctx.GetType().ok() == Some(WICColorContextProfile) {
            let mut size = 0u32;
            if src_ctx.GetProfileBytes(&mut [], &mut size).is_ok() && size > 0 {
                let mut bytes = vec![0u8; size as usize];
                if src_ctx.GetProfileBytes(&mut bytes, &mut size).is_ok() {
                    if let Some(name) = super::color::profile_description(&bytes) {
                        if name.to_ascii_lowercase().contains("srgb") {
                            return Ok((source, "sRGB".into()));
                        }
                        label = name;
                    }
                }
            }
        }
        let dst_ctx = factory.CreateColorContext()?;
        dst_ctx.InitializeFromExifColorSpace(1)?; // 1 == sRGB
        let transform = factory.CreateColorTransformer()?;
        match transform.Initialize(&source, &src_ctx, &dst_ctx, &GUID_WICPixelFormat32bppRGBA) {
            Ok(()) => Ok((transform.cast()?, format!("{label} → sRGB"))),
            Err(_) => Ok((source, label)),
        }
    }
}

fn container_name(decoder: &IWICBitmapDecoder) -> String {
    unsafe {
        if let Ok(info) = decoder.GetDecoderInfo() {
            let mut len = 0u32;
            if info.GetFriendlyName(&mut [], &mut len).is_ok() && len > 0 {
                let mut buf = vec![0u16; len as usize];
                if info.GetFriendlyName(&mut buf, &mut len).is_ok() {
                    let s = String::from_utf16_lossy(&buf[..(len as usize).saturating_sub(1)]);
                    return s.replace(" Decoder", "").replace(" decoder", "");
                }
            }
        }
    }
    "Image".into()
}

/// HEIF/AVIF decoders already apply the container's rotation; EXIF orientation
/// in those files must not be applied a second time.
pub(crate) fn needs_exif_orientation(path: &Path) -> bool {
    let ext = formats::ext_of(path);
    !matches!(ext.as_str(), "heic" | "heif" | "hif" | "avif" | "jxl")
}

pub fn decode_full(path: &Path, color_manage: bool) -> Result<Decoded> {
    with_factory(|factory| unsafe {
        let (decoder, frame) = open_frame(factory, path)?;
        let frame_count = decoder.GetFrameCount().unwrap_or(1) as usize;
        let (mut sw, mut sh) = (0u32, 0u32);
        frame.GetSize(&mut sw, &mut sh)?;
        let pf = frame.GetPixelFormat().unwrap_or(GUID::zeroed());
        let (source, color_label) = if color_manage {
            color_managed(factory, &frame)?
        } else {
            (frame.cast()?, String::new())
        };
        super::check_cancel()?;
        let rgba = source_to_rgba(&source)?;
        super::check_cancel()?;
        let orientation = if needs_exif_orientation(path) { exif_orientation_from_file(path) } else { 1 };
        let rgba = apply_orientation(rgba, orientation);
        let has_alpha = pixel_format_has_alpha(&pf) && image_has_alpha(&rgba);
        let mut info = pixel_format_name(&pf);
        if !color_label.is_empty() {
            info = format!("{info} · {color_label}");
        }
        Ok(Decoded {
            image: Arc::new(rgba),
            frames: None,
            has_alpha,
            format: container_name(&decoder),
            decoder: "Windows (WIC)",
            color_info: info,
            icc_profile: None,
            exif: None,
            orientation,
            source_size: (sw, sh),
            frame_count,
            bit_depth: pixel_format_bit_depth(&pf),
        })
    })
}

/// Decode scaled so the longest side is ≤ `max_side`. Uses embedded thumbnails
/// when they are large enough and DCT-domain scaling for JPEG.
pub fn decode_scaled(path: &Path, max_side: u32) -> Result<RgbaImage> {
    decode_scaled_sized(path, max_side).map(|r| r.0)
}

/// [`decode_scaled`] plus the source size as shown (after EXIF orientation, as
/// [`probe_dimensions`] reports it), from the same open of the file.
pub fn decode_scaled_sized(path: &Path, max_side: u32) -> Result<(RgbaImage, (u32, u32))> {
    with_factory(|factory| unsafe {
        let (_decoder, frame) = open_frame(factory, path)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        let orientation = if needs_exif_orientation(path) { exif_orientation_from_file(path) } else { 1 };
        let size = super::oriented_size((w, h), orientation);
        let img = match embedded_preview(&frame, (w, h), max_side)? {
            Some(img) => img,
            None => scaled_rgba(factory, &frame, (w, h), max_side)?,
        };
        Ok((apply_orientation(img, orientation), size))
    })
}

/// [`decode_scaled`] only where it is cheap: an embedded preview at least
/// `max_side` big, or a decoder that scales natively to half the size or less
/// (JPEG). Ok(None) when the image would have to be decoded at full size.
pub fn decode_scaled_cheap(path: &Path, max_side: u32) -> Result<Option<RgbaImage>> {
    with_factory(|factory| unsafe {
        let (_decoder, frame) = open_frame(factory, path)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        let img = match embedded_preview(&frame, (w, h), max_side)? {
            Some(img) => img,
            None if scales_natively(&frame, (w, h), max_side) => scaled_rgba(factory, &frame, (w, h), max_side)?,
            None => return Ok(None),
        };
        let orientation = if needs_exif_orientation(path) { exif_orientation_from_file(path) } else { 1 };
        Ok(Some(apply_orientation(img, orientation)))
    })
}

/// The frame's embedded preview (RAW files, many JPEGs) fitted into `max_side`,
/// when it is at least that big and has the main image's aspect (some cameras pad).
fn embedded_preview(frame: &IWICBitmapFrameDecode, (w, h): (u32, u32), max_side: u32) -> Result<Option<RgbaImage>> {
    unsafe {
        let Ok(thumb) = frame.GetThumbnail() else { return Ok(None) };
        let (mut tw, mut th) = (0u32, 0u32);
        if thumb.GetSize(&mut tw, &mut th).is_err() || tw.max(th) < max_side || tw == 0 {
            return Ok(None);
        }
        let a1 = w as f32 / h.max(1) as f32;
        let a2 = tw as f32 / th.max(1) as f32;
        if (a1 - a2).abs() >= 0.03 && (a1 - 1.0 / a2).abs() >= 0.03 {
            return Ok(None);
        }
        let img = source_to_rgba(&thumb)?;
        Ok(Some(crate::imaging::resize_to_fit(&img, max_side, max_side)))
    }
}

/// Size of a `w` x `h` frame scaled to fit `max_side` (None when it already fits).
fn scaled_size((w, h): (u32, u32), max_side: u32) -> Option<(u32, u32)> {
    let scale = (max_side as f32 / w.max(h) as f32).min(1.0);
    if scale >= 1.0 {
        return None;
    }
    Some((((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1)))
}

/// True when the decoder itself can produce the frame at half its size or less
/// for `max_side` (JPEG decodes at 1/2, 1/4 or 1/8 in the DCT domain).
fn scales_natively(frame: &IWICBitmapFrameDecode, (w, h): (u32, u32), max_side: u32) -> bool {
    let long = w.max(h) as u64;
    if max_side as u64 * 2 > long {
        return false;
    }
    let Some((mut tw, mut th)) = scaled_size((w, h), max_side) else { return false };
    unsafe {
        let Ok(t) = frame.cast::<IWICBitmapSourceTransform>() else { return false };
        t.GetClosestSize(&mut tw, &mut th).is_ok() && tw.max(th) as u64 * 2 <= long
    }
}

/// The frame fitted into `max_side` and converted to sRGB. It is scaled first and
/// the small result color-converted: a color transform between the decoder and
/// the scaler hides the decoder's own scaling (JPEG DCT scaling), so the whole
/// image would be decoded and converted at full size.
fn scaled_rgba(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode, (w, h): (u32, u32), max_side: u32) -> Result<RgbaImage> {
    let Some((tw, th)) = scaled_size((w, h), max_side) else {
        let (source, _) = color_managed(factory, frame)?;
        return source_to_rgba(&source);
    };
    if let Ok(img) = scale_then_convert(factory, frame, tw, th) {
        return Ok(img);
    }
    // Pixel formats the scaler cannot take straight from the decoder: convert
    // first, as before.
    unsafe {
        let (source, _) = color_managed(factory, frame)?;
        let scaler = factory.CreateBitmapScaler()?;
        scaler.Initialize(&source, tw, th, WICBitmapInterpolationModeFant)?;
        source_to_rgba(&scaler.cast()?)
    }
}

fn scale_then_convert(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode, tw: u32, th: u32) -> Result<RgbaImage> {
    unsafe {
        let scaler = factory.CreateBitmapScaler()?;
        let source: IWICBitmapSource = frame.cast()?;
        scaler.Initialize(&source, tw, th, WICBitmapInterpolationModeFant)?;
        let (source, _) = color_managed_source(factory, frame, scaler.cast()?)?;
        source_to_rgba(&source)
    }
}

/// Pixel size of the first frame as stored (before EXIF orientation).
pub fn stored_size(path: &Path) -> Result<(u32, u32)> {
    with_factory(|factory| unsafe {
        let (_d, frame) = open_frame(factory, path)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        Ok((w, h))
    })
}

pub fn probe_dimensions(path: &Path) -> Result<(u32, u32)> {
    with_factory(|factory| unsafe {
        let (_d, frame) = open_frame(factory, path)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        let o = if needs_exif_orientation(path) { exif_orientation_from_file(path) } else { 1 };
        Ok(if (5..=8).contains(&o) { (h, w) } else { (w, h) })
    })
}

fn pixel_format_has_alpha(pf: &GUID) -> bool {
    let alpha = [
        GUID_WICPixelFormat32bppRGBA,
        GUID_WICPixelFormat32bppBGRA,
        GUID_WICPixelFormat32bppPRGBA,
        GUID_WICPixelFormat32bppPBGRA,
        GUID_WICPixelFormat64bppRGBA,
        GUID_WICPixelFormat64bppBGRA,
        GUID_WICPixelFormat64bppPRGBA,
        GUID_WICPixelFormat128bppRGBAFloat,
        GUID_WICPixelFormat64bppRGBAHalf,
        GUID_WICPixelFormat8bppIndexed,
        GUID_WICPixelFormat4bppIndexed,
        GUID_WICPixelFormat2bppIndexed,
        GUID_WICPixelFormat1bppIndexed,
    ];
    alpha.contains(pf)
}

fn pixel_format_bit_depth(pf: &GUID) -> u8 {
    let n = pixel_format_name(pf);
    if n.contains("16-bit") {
        16
    } else if n.contains("32-bit") {
        32
    } else {
        8
    }
}

fn pixel_format_name(pf: &GUID) -> String {
    let table: [(GUID, &str); 14] = [
        (GUID_WICPixelFormat24bppBGR, "RGB 8-bit"),
        (GUID_WICPixelFormat24bppRGB, "RGB 8-bit"),
        (GUID_WICPixelFormat32bppBGR, "RGB 8-bit"),
        (GUID_WICPixelFormat32bppBGRA, "RGBA 8-bit"),
        (GUID_WICPixelFormat32bppRGBA, "RGBA 8-bit"),
        (GUID_WICPixelFormat48bppRGB, "RGB 16-bit"),
        (GUID_WICPixelFormat48bppBGR, "RGB 16-bit"),
        (GUID_WICPixelFormat64bppRGBA, "RGBA 16-bit"),
        (GUID_WICPixelFormat8bppGray, "Grayscale 8-bit"),
        (GUID_WICPixelFormat16bppGray, "Grayscale 16-bit"),
        (GUID_WICPixelFormat32bppCMYK, "CMYK 8-bit"),
        (GUID_WICPixelFormat64bppCMYK, "CMYK 16-bit"),
        (GUID_WICPixelFormat8bppIndexed, "Indexed 8-bit"),
        (GUID_WICPixelFormat128bppRGBAFloat, "RGBA 32-bit float"),
    ];
    table
        .iter()
        .find(|(g, _)| g == pf)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| "Color".into())
}
