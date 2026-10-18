//! Saving images, metadata preservation and lossless JPEG rotation.

use crate::formats::SaveFormat;
use anyhow::{anyhow, Result};
use image::RgbaImage;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Chroma {
    S444,
    S422,
    S420,
}

impl Chroma {
    pub fn label(self) -> &'static str {
        match self {
            Chroma::S444 => "4:4:4 (best color)",
            Chroma::S422 => "4:2:2",
            Chroma::S420 => "4:2:0 (smallest)",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SaveOptions {
    pub format: SaveFormat,
    pub jpeg_quality: u8,
    pub jpeg_progressive: bool,
    pub jpeg_chroma: Chroma,
    pub webp_quality: f32,
    pub webp_lossless: bool,
    pub png_best_compression: bool,
    pub keep_metadata: bool,
    #[serde(skip)]
    pub exif: Option<Vec<u8>>,
    #[serde(skip)]
    pub icc: Option<Vec<u8>>,
}

impl Default for SaveOptions {
    fn default() -> Self {
        SaveOptions {
            format: SaveFormat::Jpeg,
            jpeg_quality: 92,
            jpeg_progressive: false,
            jpeg_chroma: Chroma::S420,
            webp_quality: 88.0,
            webp_lossless: false,
            png_best_compression: false,
            keep_metadata: true,
            exif: None,
            icc: None,
        }
    }
}

fn has_alpha(img: &RgbaImage) -> bool {
    img.as_raw().iter().skip(3).step_by(4).any(|&a| a != 255)
}

/// Encode `img` to bytes in the requested format.
pub fn encode(img: &RgbaImage, opts: &SaveOptions) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let (w, h) = img.dimensions();
    match opts.format {
        SaveFormat::Jpeg => {
            if w > 65535 || h > 65535 {
                return Err(anyhow!("JPEG cannot store images larger than 65535 pixels per side"));
            }
            let mut enc = jpeg_encoder::Encoder::new(&mut out, opts.jpeg_quality.clamp(1, 100));
            enc.set_sampling_factor(match opts.jpeg_chroma {
                Chroma::S444 => jpeg_encoder::SamplingFactor::R_4_4_4,
                Chroma::S422 => jpeg_encoder::SamplingFactor::R_4_2_2,
                Chroma::S420 => jpeg_encoder::SamplingFactor::R_4_2_0,
            });
            enc.set_progressive(opts.jpeg_progressive);
            // Optimized Huffman tables make jpeg-encoder emit one scan per
            // component. That is valid for progressive JPEGs, but as *baseline*
            // it is mis-decoded by zune-jpeg (and some other decoders), so only
            // optimize when writing progressive files.
            enc.set_optimized_huffman_tables(opts.jpeg_progressive);
            if opts.keep_metadata {
                if let Some(exif) = &opts.exif {
                    let mut e = strip_exif_header(exif).to_vec();
                    exif_set_orientation(&mut e, 1);
                    if e.len() < 65000 {
                        let _ = enc.add_exif_metadata(&e);
                    }
                }
                if let Some(icc) = &opts.icc {
                    let _ = enc.add_icc_profile(icc);
                }
            }
            // JPEG has no alpha: composite onto white.
            let rgb = flatten_rgb(img, [255, 255, 255]);
            enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)?;
        }
        SaveFormat::WebP => {
            if w > 16383 || h > 16383 {
                return Err(anyhow!("WebP cannot store images larger than 16383 pixels per side"));
            }
            let enc = webp::Encoder::from_rgba(img.as_raw(), w, h);
            let mem = if opts.webp_lossless { enc.encode_lossless() } else { enc.encode(opts.webp_quality.clamp(1.0, 100.0)) };
            out.extend_from_slice(&mem);
        }
        SaveFormat::Png => {
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            use image::ImageEncoder;
            let comp = if opts.png_best_compression { CompressionType::Best } else { CompressionType::Default };
            let enc = PngEncoder::new_with_quality(&mut out, comp, FilterType::Adaptive);
            if has_alpha(img) {
                enc.write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)?;
            } else {
                let rgb = flatten_rgb(img, [0, 0, 0]);
                enc.write_image(&rgb, w, h, image::ExtendedColorType::Rgb8)?;
            }
            if opts.keep_metadata {
                if let Some(exif) = &opts.exif {
                    // EXIF in an eXIf chunk; the pixels are already upright.
                    let mut e = strip_exif_header(exif).to_vec();
                    exif_set_orientation(&mut e, 1);
                    if let Some(with) = png_with_exif(&out, e) {
                        out = with;
                    }
                }
            }
        }
        SaveFormat::Ico => {
            out = encode_ico(img)?;
        }
        other => {
            let fmt = match other {
                SaveFormat::Bmp => image::ImageFormat::Bmp,
                SaveFormat::Tiff => image::ImageFormat::Tiff,
                SaveFormat::Gif => image::ImageFormat::Gif,
                SaveFormat::Tga => image::ImageFormat::Tga,
                SaveFormat::Qoi => image::ImageFormat::Qoi,
                _ => unreachable!(),
            };
            let dynimg = if has_alpha(img) || matches!(other, SaveFormat::Gif | SaveFormat::Qoi) {
                image::DynamicImage::ImageRgba8(img.clone())
            } else {
                image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(w, h, flatten_rgb(img, [0, 0, 0])).unwrap())
            };
            dynimg.write_to(&mut std::io::Cursor::new(&mut out), fmt)?;
        }
    }
    Ok(out)
}

/// Standard icon sizes written into saved .ico files.
pub const ICO_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// Multi-resolution ICO: every standard size up to the source's long side
/// (at least 16), each a square PNG-compressed RGBA entry (Vista+ format).
pub fn encode_ico(img: &RgbaImage) -> Result<Vec<u8>> {
    use image::codecs::png::PngEncoder;
    use image::ImageEncoder;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(anyhow!("Cannot save an empty image as ICO"));
    }
    let long = w.max(h);
    let mut sizes: Vec<u32> = ICO_SIZES.iter().copied().filter(|&s| s <= long).collect();
    if sizes.is_empty() {
        sizes.push(16);
    }
    let mut entries: Vec<(u32, Vec<u8>)> = Vec::new();
    for &side in &sizes {
        let fitted = crate::imaging::resize_to_fit(img, side, side);
        // Center on a transparent square canvas.
        let mut canvas = RgbaImage::new(side, side);
        let ox = (side - fitted.width().min(side)) / 2;
        let oy = (side - fitted.height().min(side)) / 2;
        image::imageops::overlay(&mut canvas, &fitted, ox as i64, oy as i64);
        let mut png = Vec::new();
        PngEncoder::new(&mut png).write_image(canvas.as_raw(), side, side, image::ExtendedColorType::Rgba8)?;
        entries.push((side, png));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len();
    for (side, png) in &entries {
        let b = if *side >= 256 { 0 } else { *side as u8 };
        out.extend_from_slice(&[b, b, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in entries {
        out.extend_from_slice(&png);
    }
    Ok(out)
}

/// True when WebP `bytes` use the lossless (VP8L) codec, so callers can keep
/// lossless sources lossless when re-saving.
pub fn webp_is_lossless(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return false;
    }
    // Walk the chunks: simple files start with VP8 / VP8L, extended (VP8X)
    // ones carry the image chunk (or ANMF frames) later.
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        match id {
            b"VP8L" => return true,
            b"VP8 " => return false,
            b"ANMF" => {
                // Frame header is 16 bytes, then the frame's own chunks.
                let inner = pos + 8 + 16;
                if inner + 4 <= bytes.len() {
                    return &bytes[inner..inner + 4] == b"VP8L";
                }
                return false;
            }
            _ => {}
        }
        pos = match pos.checked_add(8 + len + (len & 1)) {
            Some(p) => p,
            None => return false,
        };
    }
    false
}

/// Path of a WebP file whose existing encoding is lossless.
pub fn webp_file_is_lossless(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = vec![0u8; 64 * 1024];
    let n = f.read(&mut head).unwrap_or(0);
    webp_is_lossless(&head[..n])
}

/// Save atomically (write temp file, then replace) so a failure never destroys the original.
pub fn save(img: &RgbaImage, path: &Path, opts: &SaveOptions) -> Result<()> {
    let bytes = encode(img, opts)?;
    write_atomic(path, &bytes)
}

/// Like [`save`], but when re-saving a JPEG over (or from) the JPEG `source`,
/// carry its comment, XMP and IPTC segments over (EXIF and ICC come from `opts`).
/// For any other format this is the same as [`save`].
pub fn save_from(img: &RgbaImage, path: &Path, opts: &SaveOptions, source: Option<&Path>) -> Result<()> {
    let mut bytes = encode(img, opts)?;
    if opts.format == SaveFormat::Jpeg && opts.keep_metadata {
        if let Some(orig) = source.filter(|s| crate::formats::is_jpeg_ext(&crate::formats::ext_of(s))).and_then(|s| std::fs::read(s).ok()) {
            if let Some(merged) = copy_jpeg_extras(&bytes, &orig) {
                bytes = merged;
            }
        }
    }
    write_atomic(path, &bytes)
}

fn png_with_exif(png: &[u8], exif: Vec<u8>) -> Option<Vec<u8>> {
    use img_parts::{png::Png, Bytes, ImageEXIF};
    if exif.len() < 8 || !(exif.starts_with(b"II") || exif.starts_with(b"MM")) {
        return None;
    }
    let mut p = Png::from_bytes(Bytes::copy_from_slice(png)).ok()?;
    p.set_exif(Some(Bytes::from(exif)));
    let mut buf = Vec::with_capacity(png.len() + 1024);
    p.encoder().write_to(&mut buf).ok()?;
    Some(buf)
}

/// True for the APP1 payloads that hold XMP (standard or extended), not EXIF.
fn is_xmp_app1(contents: &[u8]) -> bool {
    contents.starts_with(b"http://ns.adobe.com/xap/1.0/\0") || contents.starts_with(b"http://ns.adobe.com/xmp/extension/\0")
}

/// Copy COM (comment), APP1 XMP and APP13 (IPTC / Photoshop) segments of the
/// JPEG `original` into the freshly encoded JPEG `encoded`, after its own APPn
/// segments. Returns `None` when either file cannot be parsed or nothing to copy.
pub fn copy_jpeg_extras(encoded: &[u8], original: &[u8]) -> Option<Vec<u8>> {
    use img_parts::jpeg::{markers, Jpeg, JpegSegment};
    use img_parts::Bytes;
    let orig = Jpeg::from_bytes(Bytes::copy_from_slice(original)).ok()?;
    let extras: Vec<JpegSegment> = orig
        .segments()
        .iter()
        .filter(|s| match s.marker() {
            markers::COM | markers::APP13 => true,
            markers::APP1 => is_xmp_app1(s.contents()),
            _ => false,
        })
        .map(|s| JpegSegment::new_with_contents(s.marker(), s.contents().clone()))
        .collect();
    if extras.is_empty() {
        return None;
    }
    let mut out = Jpeg::from_bytes(Bytes::copy_from_slice(encoded)).ok()?;
    let segs = out.segments_mut();
    let at = segs.iter().position(|s| !(markers::APP0..=markers::APP15).contains(&s.marker())).unwrap_or(segs.len());
    for (i, s) in extras.into_iter().enumerate() {
        segs.insert(at + i, s);
    }
    let mut buf = Vec::with_capacity(encoded.len() + 1024);
    out.encoder().write_to(&mut buf).ok()?;
    Some(buf)
}

/// Write `bytes` to `path` without ever leaving a half-written or missing file:
/// write a uniquely named temp file next to it, flush it to disk, then swap it in.
/// An existing target keeps its attributes and creation date (ReplaceFileW).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}-{n}.lumen-tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("Cannot write {}: {e}", tmp.display()));
    }
    // A reader (thumbnailer, antivirus) holding the target briefly makes the
    // swap fail with a sharing error: retry a few times before giving up.
    let mut res = replace_file(&tmp, path);
    for attempt in 1..=5u64 {
        if res.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15 * attempt));
        res = replace_file(&tmp, path);
    }
    match res {
        Ok(()) => Ok(()),
        Err(e) => {
            // Only drop the temp copy while the original is still in place.
            if path.exists() {
                let _ = std::fs::remove_file(&tmp);
            }
            Err(anyhow!("Cannot replace {}: {e}", path.display()))
        }
    }
}

#[cfg(windows)]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Storage::FileSystem::{MoveFileExW, ReplaceFileW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, REPLACEFILE_IGNORE_ACL_ERRORS, REPLACEFILE_IGNORE_MERGE_ERRORS};
    let (src, dst) = (HSTRING::from(tmp.as_os_str()), HSTRING::from(path.as_os_str()));
    if path.exists() {
        // SAFETY: both strings are valid NUL-terminated wide paths for the call's duration.
        let r = unsafe { ReplaceFileW(&dst, &src, PCWSTR::null(), REPLACEFILE_IGNORE_MERGE_ERRORS | REPLACEFILE_IGNORE_ACL_ERRORS, None, None) };
        if r.is_ok() {
            return Ok(());
        }
    }
    // SAFETY: as above.
    unsafe { MoveFileExW(&src, &dst, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) }.map_err(|e| std::io::Error::other(e.message()))
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(tmp, path)
}

pub fn flatten_rgb(img: &RgbaImage, bg: [u8; 3]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((img.width() * img.height() * 3) as usize);
    for p in img.as_raw().chunks_exact(4) {
        let a = p[3] as u32;
        if a == 255 {
            rgb.extend_from_slice(&p[..3]);
        } else {
            for c in 0..3 {
                rgb.push(((p[c] as u32 * a + bg[c] as u32 * (255 - a) + 127) / 255) as u8);
            }
        }
    }
    rgb
}

fn strip_exif_header(exif: &[u8]) -> &[u8] {
    exif.strip_prefix(b"Exif\0\0").unwrap_or(exif)
}

/// Patch the Orientation tag (0x0112) in IFD0 of a raw TIFF/EXIF blob. Returns
/// false when the tag is absent.
pub fn exif_set_orientation(tiff: &mut [u8], value: u16) -> bool {
    let off = if tiff.starts_with(b"Exif\0\0") { 6 } else { 0 };
    let t = &mut tiff[off..];
    if t.len() < 8 {
        return false;
    }
    let le = match &t[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return false,
    };
    let rd16 = |b: &[u8], o: usize| -> u16 {
        if le { u16::from_le_bytes([b[o], b[o + 1]]) } else { u16::from_be_bytes([b[o], b[o + 1]]) }
    };
    let rd32 = |b: &[u8], o: usize| -> u32 {
        if le { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) } else { u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
    };
    let ifd = rd32(t, 4) as usize;
    if ifd + 2 > t.len() {
        return false;
    }
    let count = rd16(t, ifd) as usize;
    for i in 0..count {
        let e = ifd + 2 + i * 12;
        if e + 12 > t.len() {
            return false;
        }
        if rd16(t, e) == 0x0112 {
            let bytes = if le { value.to_le_bytes() } else { value.to_be_bytes() };
            t[e + 8] = bytes[0];
            t[e + 9] = bytes[1];
            return true;
        }
    }
    false
}

/// Compose an EXIF orientation with an extra transform (applied after it).
pub fn compose_orientation(current: u8, op: crate::edit::Transform) -> u8 {
    // Brute force with a tiny asymmetric probe image: apply `current`, then `op`,
    // and find the single orientation producing the same pixels.
    let mut probe = RgbaImage::new(3, 2);
    for (i, p) in probe.pixels_mut().enumerate() {
        p.0 = [i as u8, 0, 0, 255];
    }
    let target = op.apply(&crate::decode::apply_orientation(probe.clone(), current));
    for o in 1..=8u8 {
        if crate::decode::apply_orientation(probe.clone(), o) == target {
            return o;
        }
    }
    1
}

/// Rotate/flip a JPEG losslessly by rewriting its EXIF orientation (no recompression).
pub fn lossless_jpeg_transform(path: &Path, op: crate::edit::Transform) -> Result<u8> {
    use img_parts::jpeg::Jpeg;
    use img_parts::{Bytes, ImageEXIF};
    let data = std::fs::read(path)?;
    let mut jpeg = Jpeg::from_bytes(Bytes::from(data)).map_err(|e| anyhow!("Not a valid JPEG: {e}"))?;
    let current = jpeg.exif().map(|e| crate::decode::exif_orientation(&e)).unwrap_or(1);
    let new = compose_orientation(current, op);
    let mut exif = match jpeg.exif() {
        Some(e) => e.to_vec(),
        None => minimal_exif(1),
    };
    if !exif_set_orientation(&mut exif, new as u16) {
        // EXIF present but without an orientation tag: replace with a minimal block
        // only when it is tiny, otherwise refuse to avoid losing metadata.
        if exif.len() > 64 {
            return Err(anyhow!("This JPEG's metadata has no orientation tag; use regular rotation instead"));
        }
        exif = minimal_exif(new as u16);
    }
    jpeg.set_exif(Some(Bytes::from(exif)));
    let mut out = Vec::new();
    jpeg.encoder().write_to(&mut out)?;
    write_atomic(path, &out)?;
    Ok(new)
}

/// A tiny little-endian TIFF block containing only the orientation tag.
pub(crate) fn minimal_exif(orientation: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"II*\0");
    v.extend_from_slice(&8u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // one entry
    v.extend_from_slice(&0x0112u16.to_le_bytes());
    v.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&orientation.to_le_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&0u32.to_le_bytes()); // next IFD
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Transform;

    #[test]
    fn orientation_composition() {
        assert_eq!(compose_orientation(1, Transform::Rotate90), 6);
        assert_eq!(compose_orientation(6, Transform::Rotate90), 3);
        assert_eq!(compose_orientation(3, Transform::Rotate90), 8);
        assert_eq!(compose_orientation(8, Transform::Rotate90), 1);
        assert_eq!(compose_orientation(1, Transform::Rotate270), 8);
        assert_eq!(compose_orientation(1, Transform::FlipH), 2);
        assert_eq!(compose_orientation(2, Transform::FlipH), 1);
    }

    #[test]
    fn patch_orientation() {
        let mut e = minimal_exif(1);
        assert!(exif_set_orientation(&mut e, 6));
        assert_eq!(crate::decode::exif_orientation(&e), 6);
    }

    #[test]
    fn webp_lossless_detection() {
        let img = RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]));
        let lossy = encode(&img, &SaveOptions { format: SaveFormat::WebP, webp_lossless: false, ..Default::default() }).unwrap();
        let lossless = encode(&img, &SaveOptions { format: SaveFormat::WebP, webp_lossless: true, ..Default::default() }).unwrap();
        assert!(!webp_is_lossless(&lossy));
        assert!(webp_is_lossless(&lossless));
        assert!(!webp_is_lossless(b"RIFF"));
        assert!(!webp_is_lossless(&[]));
        assert_eq!(SaveFormat::WebP.label(), "WebP");
    }

    #[test]
    fn ico_has_all_sizes() {
        let img = RgbaImage::from_pixel(300, 150, image::Rgba([10, 200, 30, 255]));
        let bytes = encode(&img, &SaveOptions { format: SaveFormat::Ico, ..Default::default() }).unwrap();
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 7);
        let d = crate::decode::decode_bytes(&bytes, "ico", Default::default()).unwrap();
        assert_eq!(d.image.dimensions(), (256, 256));
        assert_eq!(d.frame_count, 7);
        // Letterboxed: top rows transparent, middle opaque.
        assert_eq!(d.image.get_pixel(128, 2).0[3], 0);
        assert_eq!(d.image.get_pixel(128, 128).0, [10, 200, 30, 255]);
        let small = encode(&RgbaImage::new(20, 20), &SaveOptions { format: SaveFormat::Ico, ..Default::default() }).unwrap();
        assert_eq!(u16::from_le_bytes([small[4], small[5]]), 1);
    }

    #[test]
    fn roundtrip_formats() {
        let mut img = RgbaImage::new(17, 9);
        for (x, y, p) in img.enumerate_pixels_mut() {
            p.0 = [(x * 15) as u8, (y * 25) as u8, 128, 255];
        }
        for f in SaveFormat::ALL {
            let opts = SaveOptions { format: f, ..Default::default() };
            let bytes = encode(&img, &opts).unwrap_or_else(|e| panic!("{f:?}: {e}"));
            let back = if f == SaveFormat::Tga {
                image::load_from_memory_with_format(&bytes, image::ImageFormat::Tga)
            } else {
                image::load_from_memory(&bytes)
            }
            .unwrap_or_else(|e| panic!("{f:?}: {e}"));
            if f != SaveFormat::Ico {
                assert_eq!((back.width(), back.height()), (17, 9), "{f:?}");
            }
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lumen-encode-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let d = temp_dir("atomic");
        let p = d.join("a.bin");
        write_atomic(&p, b"one").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        write_atomic(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
        // Concurrent writers in one folder never collide on temp names.
        std::thread::scope(|s| {
            for i in 0..8u8 {
                let d = &d;
                s.spawn(move || {
                    for _ in 0..4 {
                        write_atomic(&d.join(format!("c{i}.bin")), &[i; 64]).unwrap();
                    }
                });
            }
        });
        assert_eq!(std::fs::read(d.join("c3.bin")).unwrap(), vec![3u8; 64]);
        let names: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        assert_eq!(names.len(), 9, "{names:?}");
        assert!(names.iter().all(|n| !n.ends_with(".lumen-tmp")), "{names:?}");
        // A failed write (target folder missing) leaves nothing behind.
        assert!(write_atomic(&d.join("missing").join("b.bin"), b"x").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn jpeg_resave_keeps_comment_xmp_and_iptc() {
        use img_parts::jpeg::{markers, Jpeg, JpegSegment};
        use img_parts::Bytes;
        let img = RgbaImage::from_pixel(16, 16, image::Rgba([90, 120, 30, 255]));
        let opts = SaveOptions { format: SaveFormat::Jpeg, keep_metadata: true, exif: Some(minimal_exif(6)), ..Default::default() };
        let base = encode(&img, &opts).unwrap();
        let mut orig = Jpeg::from_bytes(Bytes::from(base.clone())).unwrap();
        let xmp = Bytes::from_static(b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta/>");
        orig.segments_mut().insert(1, JpegSegment::new_with_contents(markers::APP1, xmp.clone()));
        orig.segments_mut().insert(1, JpegSegment::new_with_contents(markers::APP13, Bytes::from_static(b"Photoshop 3.0\0iptc")));
        orig.segments_mut().insert(1, JpegSegment::new_with_contents(markers::COM, Bytes::from_static(b"hello")));
        let mut orig_bytes = Vec::new();
        orig.encoder().write_to(&mut orig_bytes).unwrap();

        let merged = copy_jpeg_extras(&base, &orig_bytes).unwrap();
        let j = Jpeg::from_bytes(Bytes::from(merged.clone())).unwrap();
        assert_eq!(j.segments_by_marker(markers::COM).next().unwrap().contents().as_ref(), b"hello");
        assert!(j.segments_by_marker(markers::APP13).next().is_some());
        assert!(j.segments_by_marker(markers::APP1).any(|s| s.contents() == &xmp));
        // Our own EXIF is still there, exactly once, and the file still decodes.
        assert_eq!(j.segments_by_marker(markers::APP1).filter(|s| s.contents().starts_with(b"Exif")).count(), 1);
        assert_eq!(image::load_from_memory(&merged).unwrap().width(), 16);
        // Nothing to carry over: None.
        assert!(copy_jpeg_extras(&base, &base).is_none());
    }

    #[test]
    fn png_keeps_exif() {
        use img_parts::{png::Png, Bytes, ImageEXIF};
        let img = RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let opts = SaveOptions { format: SaveFormat::Png, keep_metadata: true, exif: Some(minimal_exif(6)), ..Default::default() };
        let bytes = encode(&img, &opts).unwrap();
        let p = Png::from_bytes(Bytes::from(bytes.clone())).unwrap();
        assert_eq!(crate::decode::exif_orientation(&p.exif().unwrap()), 1);
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 4);
    }
}

#[cfg(test)]
mod jpeg_interop {
    use super::*;

    /// Every JPEG variant Lumen writes must decode back to the right colors
    /// with our own native decoder (zune-jpeg), not just with libjpeg.
    #[test]
    fn saved_jpegs_decode_correctly() {
        let img = RgbaImage::from_fn(64, 48, |x, y| image::Rgba([(x * 4) as u8, (y * 5) as u8, 90, 255]));
        for progressive in [false, true] {
            for chroma in [Chroma::S444, Chroma::S422, Chroma::S420] {
                let opts = SaveOptions { format: SaveFormat::Jpeg, jpeg_progressive: progressive, jpeg_chroma: chroma, ..Default::default() };
                let bytes = encode(&img, &opts).unwrap();
                assert!(!crate::decode::jpeg_is_noninterleaved_baseline(&bytes), "progressive={progressive} {chroma:?}");
                let back = image::load_from_memory(&bytes).unwrap().to_rgba8();
                let (a, b) = (img.get_pixel(40, 30).0, back.get_pixel(40, 30).0);
                for c in 0..3 {
                    assert!((a[c] as i32 - b[c] as i32).abs() < 12, "progressive={progressive} {chroma:?}: {a:?} vs {b:?}");
                }
            }
        }
    }
}
