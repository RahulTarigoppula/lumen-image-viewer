//! Known file formats and their capabilities.

use std::path::Path;

/// Extensions decoded by the built-in (pure Rust) decoders.
pub const NATIVE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "apng", "gif", "webp", "bmp", "dib", "ico", "cur", "tif",
    "tiff", "tga", "pbm", "pgm", "ppm", "pnm", "pam", "hdr", "exr", "qoi", "dds", "ff",
];

/// Extensions decoded via Windows Imaging Component (codecs installed in Windows).
pub const WIC_EXTS: &[&str] = &[
    "heic", "heif", "hif", "avif", "jxr", "wdp", "hdp", "jxl", // HEIF / AV1 / JPEG-XR / JPEG-XL extensions
    // Camera RAW (Windows "Raw Image Extension")
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef",
    "srw", "x3f", "3fr", "erf", "kdc", "dcr", "mrw", "mos", "iiq", "rwl",
];

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_image_ext(ext: &str) -> bool {
    NATIVE_EXTS.contains(&ext) || WIC_EXTS.contains(&ext) || ext == "svg" || ext == "psd"
}

pub fn is_image_path(path: &Path) -> bool {
    is_image_ext(&ext_of(path))
}

pub fn is_raw_ext(ext: &str) -> bool {
    matches!(
        ext,
        "dng" | "cr2" | "cr3" | "crw" | "nef" | "nrw" | "arw" | "srf" | "sr2" | "orf" | "rw2"
            | "raf" | "pef" | "srw" | "x3f" | "3fr" | "erf" | "kdc" | "dcr" | "mrw" | "mos"
            | "iiq" | "rwl"
    )
}

pub fn is_jpeg_ext(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "jpe" | "jfif")
}

/// Human readable type name for the details view.
pub fn type_name(ext: &str) -> String {
    match ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => "JPEG image".into(),
        "png" => "PNG image".into(),
        "apng" => "Animated PNG".into(),
        "gif" => "GIF image".into(),
        "webp" => "WebP image".into(),
        "bmp" | "dib" => "Bitmap image".into(),
        "ico" | "cur" => "Icon".into(),
        "tif" | "tiff" => "TIFF image".into(),
        "heic" | "heif" | "hif" => "HEIF image".into(),
        "avif" => "AVIF image".into(),
        "jxl" => "JPEG XL image".into(),
        "psd" => "Photoshop document".into(),
        "svg" => "SVG vector image".into(),
        e if is_raw_ext(e) => format!("{} camera RAW", e.to_uppercase()),
        "" => "File".into(),
        e => format!("{} image", e.to_uppercase()),
    }
}

/// Formats offered in "Save as" / batch conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SaveFormat {
    Jpeg,
    Png,
    WebP,
    Bmp,
    Tiff,
    Gif,
    Tga,
    Ico,
    Qoi,
}

impl SaveFormat {
    pub const ALL: [SaveFormat; 9] = [
        SaveFormat::Jpeg,
        SaveFormat::Png,
        SaveFormat::WebP,
        SaveFormat::Bmp,
        SaveFormat::Tiff,
        SaveFormat::Gif,
        SaveFormat::Tga,
        SaveFormat::Ico,
        SaveFormat::Qoi,
    ];
    pub fn ext(self) -> &'static str {
        match self {
            SaveFormat::Jpeg => "jpg",
            SaveFormat::Png => "png",
            SaveFormat::WebP => "webp",
            SaveFormat::Bmp => "bmp",
            SaveFormat::Tiff => "tif",
            SaveFormat::Gif => "gif",
            SaveFormat::Tga => "tga",
            SaveFormat::Ico => "ico",
            SaveFormat::Qoi => "qoi",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SaveFormat::Jpeg => "JPEG",
            SaveFormat::Png => "PNG",
            SaveFormat::WebP => "WebP (lossless)",
            SaveFormat::Bmp => "BMP",
            SaveFormat::Tiff => "TIFF",
            SaveFormat::Gif => "GIF",
            SaveFormat::Tga => "TGA",
            SaveFormat::Ico => "ICO",
            SaveFormat::Qoi => "QOI",
        }
    }
    pub fn from_ext(ext: &str) -> Option<SaveFormat> {
        Some(match ext {
            "jpg" | "jpeg" | "jpe" | "jfif" => SaveFormat::Jpeg,
            "png" => SaveFormat::Png,
            "webp" => SaveFormat::WebP,
            "bmp" => SaveFormat::Bmp,
            "tif" | "tiff" => SaveFormat::Tiff,
            "gif" => SaveFormat::Gif,
            "tga" => SaveFormat::Tga,
            "ico" => SaveFormat::Ico,
            "qoi" => SaveFormat::Qoi,
            _ => return None,
        })
    }
}
