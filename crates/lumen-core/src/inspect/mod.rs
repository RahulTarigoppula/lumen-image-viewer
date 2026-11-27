//! Full metadata inspection of one image file, for the Inspect panel.
//!
//! [`inspect_file`] / [`inspect_bytes`] read everything a camera or editor
//! stores in an image and return it as display-ready text in three parts:
//!
//! - **Capture**: what the camera did and which features fired (exposure,
//!   multi-frame HDR, night mode, portrait depth, motion photo...), in plain
//!   language and only for the fields that are present.
//! - **EXIF**: every EXIF tag, grouped by IFD.
//! - **Other**: file and encoding facts (JPEG quality estimate, chroma
//!   subsampling, PNG chunks...), the ICC profile, XMP, the Samsung SEF
//!   trailer, multi-picture (MPF) images, the maker note, PNG text chunks
//!   (including AI image generator settings) and comments.
//!
//! All parsing is our own, defensive and bounds checked (kamadak-exif reads
//! the EXIF block itself); problems become entries in
//! [`Inspection::warnings`] instead of errors, and a panic anywhere is caught.

mod capture;
mod containers;
mod exifview;
pub mod gainmap;
mod icc;
mod jpeg;
mod makernote;
mod samsung;
mod table;
mod xmp;
#[cfg(test)]
mod tests;

pub use table::table_csv;

use std::path::Path;

/// One displayed value. `note` is a plain-language hover explanation ("" when none).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    /// Label shown on the left, e.g. "Exposure" or "dc:creator".
    pub name: String,
    /// Display value with units, e.g. "1/50 s".
    pub value: String,
    /// Plain-language explanation for a hover tooltip; empty when there is none.
    pub note: String,
}

/// A named list of fields, shown as one section.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    /// Section title, e.g. "EXIF" or "Color profile (ICC)".
    pub name: String,
    /// Fields in display order.
    pub fields: Vec<Field>,
}

/// Everything [`inspect_file`] found in one image file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inspection {
    /// "Capture": what the camera did and which features fired, interpreted for people (see below).
    pub capture: Vec<Field>,
    /// Every EXIF tag: groups "Image", "EXIF", "GPS", "Interop", "Thumbnail" (tag name -> display value with units).
    pub exif: Vec<Group>,
    /// Everything else, in this order when present: "File", "Encoding", "Color profile (ICC)", "XMP", "Samsung trailer", "Multi-picture (MPF)", "HDR gain map", "Maker note", "PNG text", "Comments".
    pub other: Vec<Group>,
    /// Problems while reading (truncated segment, unreadable XMP, ...), shown in the UI.
    pub warnings: Vec<String>,
}

/// Largest file [`inspect_file`] reads; bigger files are inspected from their first 512 MB.
const MAX_READ: u64 = 512 * 1024 * 1024;

/// Read and inspect an image file. Never panics: unreadable files and
/// internal failures come back as an [`Inspection`] with warnings. Files
/// over 512 MB are inspected from their first 512 MB (with a warning).
pub fn inspect_file(path: &Path) -> Inspection {
    use std::io::Read;
    let ext = crate::formats::ext_of(path);
    let total = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return failed(format!("Cannot read the file: {e}")),
    };
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|f| f.take(MAX_READ).read_to_end(&mut bytes));
    if let Err(e) = read {
        return failed(format!("Cannot read the file: {e}"));
    }
    let mut out = guarded(|| inspect_impl(&bytes, &ext, total));
    if total > MAX_READ {
        out.warnings.push("Only the first 512 MB of this file were read; data stored at the end (trailers, embedded videos) is not shown.".into());
    }
    // Formats we do not parse ourselves: ask the decoders for the size.
    if !out.other.iter().any(|g| g.name == "File" && g.fields.iter().any(|f| f.name == "Pixel dimensions")) {
        if let Some((w, h)) = crate::decode::probe_dimensions(path) {
            if let Some(g) = out.other.iter_mut().find(|g| g.name == "File") {
                let size_at = g.fields.iter().position(|f| f.name == "File size").map_or(g.fields.len(), |i| i + 1);
                let mut extra = Vec::new();
                file_pixel_fields(&mut extra, w, h, total, None, "Reported by the image decoder (after applying the EXIF orientation).");
                for (k, f) in extra.into_iter().enumerate() {
                    g.fields.insert(size_at + k, f);
                }
            }
        }
    }
    out
}

/// Inspect an image held in memory. `ext` is the lowercase file extension
/// without the dot (e.g. "jpg"); it only matters for formats without a
/// signature and for the "wrong extension" warning. Never panics.
pub fn inspect_bytes(bytes: &[u8], ext: &str) -> Inspection {
    guarded(|| inspect_impl(bytes, ext, bytes.len() as u64))
}

fn failed(msg: String) -> Inspection {
    Inspection { warnings: vec![msg], ..Default::default() }
}

fn guarded(f: impl FnOnce() -> Inspection) -> Inspection {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .unwrap_or_else(|_| failed("Reading the metadata failed unexpectedly (the file may be corrupt).".into()))
}

impl Field {
    pub(crate) fn new(name: impl Into<String>, value: impl Into<String>, note: impl Into<String>) -> Field {
        Field { name: name.into(), value: value.into(), note: note.into() }
    }
}

/// Containers we parse ourselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Jpeg,
    Png,
    Gif,
    WebP,
    Heif,
    Tiff,
    Bmp,
    Jxl,
    Other,
}

/// What the container parsers collect for the shared stages.
#[derive(Default)]
pub(crate) struct Parts {
    pub format: String,
    pub format_note: String,
    /// Stored pixel dimensions from the file header.
    pub dims: Option<(u32, u32)>,
    pub dims_note: String,
    /// Raw TIFF-structured EXIF ("II*\0..." or "MM\0*...").
    pub exif_raw: Option<Vec<u8>>,
    /// EXIF already parsed by kamadak-exif's container reader (HEIF).
    pub exif_parsed: Option<exif::Exif>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<String>,
    /// Extended XMP (JPEG): number of chunks and the full length they declare.
    pub xmp_extended: Option<(usize, u64)>,
    pub encoding: Vec<Field>,
    /// MPF APP2 payload after "MPF\0" (TIFF structured).
    pub mpf: Option<Vec<u8>>,
    /// Where that payload starts in the file (MPF image offsets count from there).
    pub mpf_at: Option<usize>,
    pub comments: Vec<String>,
    pub png_text: Vec<Field>,
    /// End of the primary JPEG image (just after its EOI marker).
    pub image_end: Option<usize>,
    /// Coding-independent code points (HEIF nclx, PNG cICP) as text.
    pub cicp: Option<String>,
    /// HEIF auxiliary image types (urn strings).
    pub aux: Vec<String>,
    /// JPEG has an ISO 21496-1 gain map APP2 segment.
    pub iso_gain_map: bool,
    /// The first such segment's payload after the name (only a version in a main image).
    pub iso_gain_map_data: Option<Vec<u8>>,
    /// Color components in the JPEG frame header (0 when unknown).
    pub components: u8,
}

/// Everything the Capture and group builders read.
pub(crate) struct Ctx<'a> {
    pub exif: Option<&'a exif::Exif>,
    pub xmp: &'a [(String, String)],
    pub sef: Option<&'a samsung::Sef>,
    pub mpf: &'a [jpeg::MpImage],
    pub items: &'a [Vec<(String, String)>],
    pub video: Option<u64>,
    pub icc_desc: Option<&'a str>,
    pub cicp: Option<&'a str>,
    pub aux: &'a [String],
    pub apple: &'a exifview::AppleNote,
    pub iso_gain_map: bool,
    /// The decoded Samsung Type2 maker note.
    pub samsung: Option<&'a makernote::SamsungNote>,
    /// The EXIF maker note is Google's encrypted HDR+ note.
    pub google_hdrp: bool,
    /// The gain map, when one was found and its metadata read.
    pub gain: Option<&'a gainmap::Found>,
}

impl Ctx<'_> {
    /// XMP value by exact property name (top level) or as the last part of a nested name.
    pub fn xget(&self, name: &str) -> Option<&str> {
        let suffix = format!(" / {name}");
        self.xmp.iter().find(|(k, _)| k == name || k.ends_with(&suffix)).map(|(_, v)| v.as_str())
    }

    /// The first XMP property (name, value) whose last name part is one of
    /// `names`, ignoring case (Google wrote "shot_log_data" and "ShotLogData").
    pub fn xfind(&self, names: &[&str]) -> Option<(&str, &str)> {
        self.xmp
            .iter()
            .find(|(k, _)| {
                let leaf = k.rsplit(" / ").next().unwrap_or(k.as_str());
                names.iter().any(|n| leaf.eq_ignore_ascii_case(n))
            })
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Whether any XMP property uses the namespace prefix (e.g. "hdrgm:").
    pub fn xhas_ns(&self, prefix: &str) -> bool {
        let nested = format!(" / {prefix}");
        self.xmp.iter().any(|(k, _)| k.starts_with(prefix) || k.contains(&nested))
    }

    /// Semantic of each GContainer item ("Primary", "GainMap", ...).
    pub fn item_semantics(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter_map(|it| it.iter().find(|(k, _)| k.ends_with(":Semantic")).map(|(_, v)| v.as_str()))
            .collect()
    }

    /// Samsung trailer entry names.
    pub fn sef_names(&self) -> Vec<&str> {
        self.sef.map(|s| s.entries.iter().map(|e| e.name.as_str()).collect()).unwrap_or_default()
    }
}

fn sniff(b: &[u8], ext: &str) -> (Kind, String) {
    let starts = |m: &[u8]| b.starts_with(m);
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return (Kind::Jpeg, "JPEG".into());
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return (Kind::Png, "PNG".into());
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return (Kind::Gif, "GIF".into());
    }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return (Kind::WebP, "WebP".into());
    }
    if b.len() >= 12 && &b[4..8] == b"ftyp" {
        return (Kind::Heif, "ISO media".into());
    }
    if starts(b"II*\0") || starts(b"MM\0*") {
        let raw = matches!(
            ext,
            "dng" | "nef" | "nrw" | "arw" | "srf" | "sr2" | "pef" | "3fr" | "erf" | "kdc" | "mos" | "iiq" | "rwl" | "srw" | "cr2"
        );
        let name = if b.len() > 10 && &b[8..10] == b"CR" {
            "CR2 (Canon raw, TIFF based)".to_string()
        } else if raw {
            format!("Camera raw ({}, TIFF based)", ext.to_uppercase())
        } else {
            "TIFF".to_string()
        };
        return (Kind::Tiff, name);
    }
    if starts(b"II+\0") || starts(b"MM\0+") {
        return (Kind::Other, "BigTIFF".into());
    }
    if starts(b"IIRO") || starts(b"IIRS") {
        return (Kind::Other, "ORF (Olympus raw)".into());
    }
    if starts(b"IIU\0") {
        return (Kind::Other, "RW2 (Panasonic raw)".into());
    }
    if starts(b"FUJIFILMCCD-RAW") {
        return (Kind::Other, "RAF (Fujifilm raw)".into());
    }
    if starts(b"BM") && b.len() >= 30 {
        return (Kind::Bmp, "BMP".into());
    }
    if starts(&[0xFF, 0x0A]) || starts(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A]) {
        return (Kind::Jxl, "JPEG XL".into());
    }
    let other = |s: &str| (Kind::Other, s.to_string());
    if starts(b"8BPS") {
        return other("PSD (Photoshop)");
    }
    if starts(b"%PDF") {
        return other("PDF");
    }
    if starts(&[0, 0, 1, 0]) {
        return other("ICO (Windows icon)");
    }
    if starts(&[0, 0, 2, 0]) {
        return other("CUR (Windows cursor)");
    }
    if starts(b"qoif") {
        return other("QOI");
    }
    if starts(&[0x76, 0x2F, 0x31, 0x01]) {
        return other("OpenEXR");
    }
    if starts(b"#?RADIANCE") || starts(b"#?RGBE") {
        return other("Radiance HDR");
    }
    if starts(b"DDS ") {
        return other("DDS");
    }
    if starts(b"P1") || starts(b"P2") || starts(b"P3") || starts(b"P4") || starts(b"P5") || starts(b"P6") {
        if matches!(ext, "pbm" | "pgm" | "ppm" | "pnm") {
            return other("Netpbm");
        }
    }
    let head = &b[..b.len().min(4096)];
    if find(head, b"<svg").is_some() {
        return other("SVG");
    }
    if ext == "pcx" && starts(&[0x0A]) {
        return other("PCX");
    }
    if ext == "tga" {
        return other("TGA");
    }
    if ext.is_empty() {
        other("Unknown")
    } else {
        (Kind::Other, format!("Unknown ({})", ext.to_uppercase()))
    }
}

/// The container an extension promises, for the mismatch warning.
fn ext_kind(ext: &str) -> Option<Kind> {
    Some(match ext {
        "jpg" | "jpeg" | "jpe" | "jfif" => Kind::Jpeg,
        "png" | "apng" => Kind::Png,
        "gif" => Kind::Gif,
        "webp" => Kind::WebP,
        "heic" | "heif" | "hif" | "avif" => Kind::Heif,
        "tif" | "tiff" => Kind::Tiff,
        "bmp" | "dib" => Kind::Bmp,
        "jxl" => Kind::Jxl,
        _ => return None,
    })
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Jpeg => "JPEG",
        Kind::Png => "PNG",
        Kind::Gif => "GIF",
        Kind::WebP => "WebP",
        Kind::Heif => "HEIF/AVIF",
        Kind::Tiff => "TIFF",
        Kind::Bmp => "BMP",
        Kind::Jxl => "JPEG XL",
        Kind::Other => "other",
    }
}

/// The whole inspection without the panic guard (tests call this directly so
/// a panic fails the test instead of being swallowed).
pub(crate) fn inspect_impl(b: &[u8], ext: &str, total: u64) -> Inspection {
    let mut warn: Vec<String> = Vec::new();
    if b.is_empty() {
        return failed("The file is empty (0 bytes).".into());
    }
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let (kind, format) = sniff(b, &ext);
    let mut p = Parts { format, ..Default::default() };
    match kind {
        Kind::Jpeg => jpeg::parse(b, &mut p, &mut warn),
        Kind::Png => containers::png(b, &mut p, &mut warn),
        Kind::Gif => containers::gif(b, &mut p, &mut warn),
        Kind::WebP => containers::webp(b, &mut p, &mut warn),
        Kind::Heif => containers::heif(b, &mut p, &mut warn),
        Kind::Bmp => containers::bmp(b, &mut p, &mut warn),
        Kind::Jxl => containers::jxl(b, &mut p, &mut warn),
        Kind::Tiff => p.exif_raw = Some(b.to_vec()),
        Kind::Other => {}
    }
    if let Some(want) = ext_kind(&ext) {
        if want != kind && !(want == Kind::Tiff && p.format.contains("TIFF")) {
            if kind == Kind::Other && p.format.starts_with("Unknown") {
                warn.push(format!("The file is named .{ext} but its content is not recognized as {}.", kind_name(want)));
            } else {
                warn.push(format!("The file is named .{ext} but contains {} data.", p.format));
            }
        }
    }

    // EXIF. The raw block is kept for maker notes that count offsets from
    // its start (a TIFF file is its own block).
    let exif_block = if kind == Kind::Tiff { None } else { p.exif_raw.clone() };
    let exif = match (p.exif_parsed.take(), p.exif_raw.take()) {
        (Some(e), _) => Some(e),
        (None, Some(raw)) => exifview::read_raw(raw, kind == Kind::Tiff, &mut warn),
        (None, None) => None,
    };
    if kind == Kind::Tiff {
        containers::tiff_after_exif(exif.as_ref(), &mut p);
    }
    let tiff_block: Option<&[u8]> = if kind == Kind::Tiff { Some(b) } else { exif_block.as_deref() };
    let maker_note = exif.as_ref().and_then(exifview::maker_note_bytes);
    let make = exif.as_ref().and_then(|e| exifview::Ex(e).text(exif::Tag::Make));
    let samsung_note = maker_note.and_then(|m| makernote::samsung(m, tiff_block, make.as_deref(), &mut warn));
    let google_hdrp = maker_note.and_then(makernote::google_hdrp).is_some();

    // XMP: containers that have no dedicated slot get a raw packet search.
    if p.xmp.is_none() && !matches!(kind, Kind::Jpeg | Kind::Png | Kind::WebP) {
        p.xmp = xmp::find_raw(b);
    }
    let xmp_doc = p.xmp.as_deref().map(xmp::Doc::parse);
    let xmp_fields = xmp_doc.as_ref().map(|d| d.properties()).unwrap_or_default();
    let items = xmp_doc.as_ref().map(|d| d.container_items()).unwrap_or_default();
    if let (Some(text), Some(doc)) = (p.xmp.as_deref(), xmp_doc.as_ref()) {
        if xmp_fields.is_empty() && !doc.has_rdf() {
            warn.push(format!("The XMP packet ({}) could not be parsed.", crate::catalog::human_size(text.len() as u64)));
        }
    }

    // Trailers and embedded media (anything can carry a Samsung trailer, but only JPEGs do in practice).
    let sef = samsung::parse(b, &mut warn);
    let video = match (kind, p.image_end) {
        (Kind::Jpeg, Some(end)) => jpeg::embedded_video(b, end, sef.as_ref()),
        _ => None,
    };
    let mpf = p.mpf.as_deref().map(|m| jpeg::parse_mpf(m, &mut warn)).unwrap_or_default();
    let apple = exif.as_ref().map(exifview::apple_note).unwrap_or_default();
    let icc_info = p.icc.as_deref().map(|d| icc::group(d, &mut warn));

    // HDR gain map: where it is and what its metadata says (no pixels decoded here).
    let gain = if kind == Kind::Jpeg {
        let orientation = exif.as_ref().and_then(|e| exifview::Ex(e).uint(exif::Tag::Orientation)).map_or(1, |o| o.clamp(1, 8) as u8);
        let pr = gainmap::Primary { mpf: &mpf, mpf_at: p.mpf_at, end: p.image_end, items: &items, xmp: &xmp_fields, orientation, apple: &apple, iso: p.iso_gain_map };
        match (pr.announced() || mpf.len() > 1).then(|| gainmap::locate_in(b, &pr)) {
            Some(Ok(f)) => Some(f),
            Some(Err(e)) => {
                if e.announced {
                    warn.push(e.reason);
                }
                None
            }
            None => None,
        }
    } else {
        None
    };

    let ctx = Ctx {
        exif: exif.as_ref(),
        xmp: &xmp_fields,
        sef: sef.as_ref(),
        mpf: &mpf,
        items: &items,
        video: video.map(|v| v as u64),
        icc_desc: icc_info.as_ref().and_then(|(_, d)| d.as_deref()),
        cicp: p.cicp.as_deref(),
        aux: &p.aux,
        apple: &apple,
        iso_gain_map: p.iso_gain_map,
        samsung: samsung_note.as_ref(),
        google_hdrp,
        gain: gain.as_ref(),
    };

    let mut out = Inspection { capture: capture::build(&ctx), ..Default::default() };
    if let Some(e) = exif.as_ref() {
        out.exif = exifview::groups(e);
    }

    // Other groups, in the documented order.
    out.other.push(file_group(&p, b, total, exif.as_ref()));
    if !p.encoding.is_empty() {
        out.other.push(Group { name: "Encoding".into(), fields: std::mem::take(&mut p.encoding) });
    }
    if let Some((g, _)) = icc_info.clone() {
        out.other.push(g);
    }
    if p.xmp.is_some() || p.xmp_extended.is_some() {
        out.other.push(xmp_group(p.xmp.as_deref(), p.xmp_extended, &xmp_fields));
    }
    if let Some(s) = sef.as_ref() {
        out.other.push(samsung::group(s, b));
    }
    if !mpf.is_empty() || !items.is_empty() {
        out.other.push(jpeg::mpf_group(&ctx));
    }
    if let Some(g) = gain.as_ref() {
        out.other.push(gainmap::group(g));
    }
    if let Some(e) = exif.as_ref() {
        let google_blocks: Vec<&str> = xmp::GOOGLE_ENCRYPTED.iter().filter(|(names, _)| ctx.xfind(names).is_some()).map(|(_, what)| *what).collect();
        if let Some(g) = exifview::maker_group(e, &apple, samsung_note.as_ref(), &google_blocks) {
            out.other.push(g);
        }
    }
    if !p.png_text.is_empty() {
        out.other.push(Group { name: "PNG text".into(), fields: std::mem::take(&mut p.png_text) });
    }
    if !p.comments.is_empty() {
        let n = p.comments.len();
        let fields = p
            .comments
            .iter()
            .enumerate()
            .map(|(i, c)| Field::new(if n == 1 { "Comment".to_string() } else { format!("Comment {}", i + 1) }, c.clone(), ""))
            .collect();
        out.other.push(Group { name: "Comments".into(), fields });
    }
    warn.dedup();
    out.warnings = warn;
    out
}

fn file_group(p: &Parts, b: &[u8], total: u64, exif: Option<&exif::Exif>) -> Group {
    let mut f = Vec::new();
    f.push(Field::new("Format", p.format.clone(), p.format_note.clone()));
    f.push(Field::new(
        "File size",
        format!("{} ({} bytes)", crate::catalog::human_size(total), thousands(total)),
        "",
    ));
    let mut dims = p.dims;
    let mut dims_note = p.dims_note.clone();
    if dims.is_none() {
        // TIFF-based files: the image size lives in IFD0.
        if let Some(e) = exif {
            let get = |t| e.get_field(t, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0));
            if let (Some(w), Some(h)) = (get(exif::Tag::ImageWidth), get(exif::Tag::ImageLength)) {
                dims = Some((w, h));
                dims_note = "From the first image directory (raw files often store a preview there).".into();
            }
        }
    }
    if let Some((w, h)) = dims.filter(|&(w, h)| w > 0 && h > 0) {
        // Bits per pixel of the primary image only: trailers and embedded videos are not image data.
        let image_bytes = p.image_end.map(|e| e as u64).filter(|&e| e < total);
        file_pixel_fields(&mut f, w, h, total, image_bytes, &dims_note);
    }
    let _ = b;
    Group { name: "File".into(), fields: f }
}

/// Pixel dimensions, megapixels, aspect ratio and bits per pixel.
fn file_pixel_fields(f: &mut Vec<Field>, w: u32, h: u32, total: u64, image_bytes: Option<u64>, dims_note: &str) {
    f.push(Field::new("Pixel dimensions", format!("{w} x {h}"), dims_note));
    let px = w as u64 * h as u64;
    f.push(Field::new("Megapixels", format!("{:.1} MP", px as f64 / 1e6), ""));
    f.push(Field::new("Aspect ratio", aspect(w, h), ""));
    let bytes = image_bytes.unwrap_or(total);
    let bpp = bytes as f64 * 8.0 / px as f64;
    let mut note = "File size x 8 / pixels: a compression indicator. Uncompressed 8-bit RGB is 24; typical phone JPEGs are 2 to 5; lower means stronger compression or a simpler scene.".to_string();
    if image_bytes.is_some() {
        note.push_str(" Counts only the primary image (up to the JPEG end marker), not extra images, videos or trailers after it.");
    }
    f.push(Field::new("Bits per pixel", format!("{bpp:.2}"), note));
}

fn aspect(w: u32, h: u32) -> String {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let g = gcd(w, h).max(1);
    let (a, b) = (w / g, h / g);
    if a <= 32 && b <= 32 {
        format!("{a}:{b}")
    } else {
        let r = w as f64 / h as f64;
        let named = [(4.0 / 3.0, "4:3"), (3.0 / 2.0, "3:2"), (16.0 / 9.0, "16:9"), (1.0, "1:1"), (3.0 / 4.0, "3:4"), (2.0 / 3.0, "2:3"), (9.0 / 16.0, "9:16")];
        match named.iter().find(|(v, _)| (r - v).abs() < 0.01) {
            Some((_, n)) => format!("{r:.2}:1 (about {n})"),
            None => format!("{r:.2}:1"),
        }
    }
}

fn xmp_group(text: Option<&str>, ext: Option<(usize, u64)>, fields: &[(String, String)]) -> Group {
    let mut f = Vec::new();
    if let Some(t) = text {
        f.push(Field::new("Packet size", crate::catalog::human_size(t.len() as u64), "Size of the XMP metadata packet."));
    }
    if let Some((n, len)) = ext {
        f.push(Field::new(
            "Extended XMP",
            format!("{} in {n} segment{}", crate::catalog::human_size(len), if n == 1 { "" } else { "s" }),
            "XMP too large for one JPEG segment (often a depth map or gain map description); its content is not shown.",
        ));
    }
    for (k, v) in fields {
        let value = if xmp::is_google_encrypted(k) { xmp::encrypted_value(v) } else { v.clone() };
        f.push(Field::new(k.clone(), value, xmp::note_for(k)));
    }
    Group { name: "XMP".into(), fields: f }
}

// ---- Shared helpers ----

pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        match hay[i..hay.len() - needle.len() + 1].iter().position(|&c| c == first) {
            None => return None,
            Some(k) => {
                let at = i + k;
                if &hay[at..at + needle.len()] == needle {
                    return Some(at);
                }
                i = at + 1;
            }
        }
    }
    None
}

pub(crate) fn be16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o.checked_add(2)?).map(|s| u16::from_be_bytes([s[0], s[1]]))
}
pub(crate) fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o.checked_add(4)?).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
pub(crate) fn le16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o.checked_add(2)?).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
pub(crate) fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o.checked_add(4)?).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// 1234567 -> "1,234,567".
pub(crate) fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Truncate to at most `max` characters, adding "..." when cut.
pub(crate) fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

/// Text when the bytes are printable UTF-8 (tabs and newlines allowed), else None.
pub(crate) fn printable(b: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(b).ok()?;
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if s.chars().all(|c| !c.is_control() || c == '\n' || c == '\r' || c == '\t') {
        Some(s.to_string())
    } else {
        None
    }
}

/// Names for ITU-T H.273 code points (HEIF nclx, PNG cICP).
pub(crate) fn cicp_text(prim: u16, transfer: u16, matrix: u16, full: bool) -> String {
    let p = match prim {
        1 => "BT.709 / sRGB",
        9 => "BT.2020",
        11 => "DCI-P3",
        12 => "Display P3",
        2 => "unspecified",
        _ => "other",
    };
    let t = match transfer {
        1 | 6 | 14 | 15 => "BT.709",
        13 => "sRGB",
        8 => "linear",
        16 => "PQ (HDR)",
        18 => "HLG (HDR)",
        2 => "unspecified",
        _ => "other",
    };
    format!(
        "primaries {prim} ({p}), transfer {transfer} ({t}), matrix {matrix}, {} range",
        if full { "full" } else { "limited" }
    )
}
