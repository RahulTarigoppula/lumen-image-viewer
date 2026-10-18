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

fn open_frame(factory: &IWICImagingFactory, path: &Path) -> Result<(IWICBitmapDecoder, IWICBitmapFrameDecode)> {
    unsafe {
        let decoder = factory.CreateDecoderFromFilename(
            &HSTRING::from(path.as_os_str()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        Ok((decoder, frame))
    }
}

fn source_to_rgba(source: &IWICBitmapSource) -> Result<RgbaImage> {
    unsafe {
        let converted = WICConvertBitmapSource(&GUID_WICPixelFormat32bppRGBA, source)?;
        let (mut w, mut h) = (0u32, 0u32);
        converted.GetSize(&mut w, &mut h)?;
        if w == 0 || h == 0 {
            return Err(anyhow!("Empty image"));
        }
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

/// Wrap `source` in a color transform to sRGB when the frame carries a non-sRGB profile.
fn color_managed(factory: &IWICImagingFactory, frame: &IWICBitmapFrameDecode) -> Result<(IWICBitmapSource, String)> {
    unsafe {
        let source: IWICBitmapSource = frame.cast()?;
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
fn needs_exif_orientation(path: &Path) -> bool {
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
        let rgba = source_to_rgba(&source)?;
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
        })
    })
}

/// Decode scaled so the longest side is ≤ `max_side`. Uses embedded thumbnails
/// when they are large enough and DCT-domain scaling for JPEG.
pub fn decode_scaled(path: &Path, max_side: u32) -> Result<RgbaImage> {
    with_factory(|factory| unsafe {
        let (_decoder, frame) = open_frame(factory, path)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        let orientation = if needs_exif_orientation(path) { exif_orientation_from_file(path) } else { 1 };

        // Embedded preview (RAW files, many JPEGs) when it is big enough.
        if let Ok(thumb) = frame.GetThumbnail() {
            let (mut tw, mut th) = (0u32, 0u32);
            if thumb.GetSize(&mut tw, &mut th).is_ok() && tw.max(th) >= max_side && tw > 0 {
                // Only trust it when its aspect matches the main image (some cameras pad).
                let a1 = w as f32 / h.max(1) as f32;
                let a2 = tw as f32 / th.max(1) as f32;
                if (a1 - a2).abs() < 0.03 || (a1 - 1.0 / a2).abs() < 0.03 {
                    let img = source_to_rgba(&thumb)?;
                    let img = crate::imaging::resize_to_fit(&img, max_side, max_side);
                    return Ok(apply_orientation(img, orientation));
                }
            }
        }

        let (source, _) = color_managed(factory, &frame)?;
        let scale = (max_side as f32 / w.max(h) as f32).min(1.0);
        let source: IWICBitmapSource = if scale < 1.0 {
            let tw = ((w as f32 * scale).round() as u32).max(1);
            let th = ((h as f32 * scale).round() as u32).max(1);
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(&source, tw, th, WICBitmapInterpolationModeFant)?;
            scaler.cast()?
        } else {
            source
        };
        let img = source_to_rgba(&source)?;
        Ok(apply_orientation(img, orientation))
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
