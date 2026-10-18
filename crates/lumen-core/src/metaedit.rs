//! Lossless metadata editing: JPEG comments (FastStone "D"), metadata
//! removal without re-encoding, file timestamps, EXIF date shifting and XMP
//! sidecars for ratings / labels. Every rewrite goes through
//! `encode::write_atomic` so a failure never damages the original.

#![allow(dead_code)]

use crate::encode::write_atomic;
use crate::formats;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{Duration, NaiveDateTime, TimeZone};
use img_parts::jpeg::{markers, Jpeg, JpegSegment};
use img_parts::{Bytes, ImageEXIF};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const EXIF_DATE_FMT: &str = "%Y:%m:%d %H:%M:%S";

/// Captured file times, restored after a rewrite when the caller asks to keep them.
struct Times {
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
}

fn capture_times(path: &Path) -> Times {
    let m = std::fs::metadata(path).ok();
    Times { created: m.as_ref().and_then(|m| m.created().ok()), modified: m.as_ref().and_then(|m| m.modified().ok()) }
}

/// Rewrite `path` with `bytes` atomically, optionally preserving its timestamps.
fn rewrite(path: &Path, bytes: &[u8], keep_times: bool) -> Result<()> {
    let t = capture_times(path);
    write_atomic(path, bytes)?;
    if keep_times {
        set_file_times(path, t.created, t.modified)?;
    }
    Ok(())
}

fn load_jpeg(path: &Path) -> Result<Jpeg> {
    let data = crate::decode::read_file(path)?;
    Jpeg::from_bytes(Bytes::from(data)).map_err(|e| anyhow!("Not a valid JPEG: {e}"))
}

fn jpeg_bytes(jpeg: Jpeg) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(jpeg.len());
    jpeg.encoder().write_to(&mut out)?;
    Ok(out)
}

pub fn is_jpeg(path: &Path) -> bool {
    formats::is_jpeg_ext(&formats::ext_of(path))
}

// ---------------------------------------------------------------- comments

/// The JPEG COM comment (the first one, if several), decoded as UTF-8 with a
/// Latin-1 fallback for legacy writers.
pub fn read_jpeg_comment(path: &Path) -> Result<Option<String>> {
    let jpeg = load_jpeg(path)?;
    Ok(jpeg.segment_by_marker(markers::COM).map(|s| decode_text(s.contents())))
}

fn decode_text(b: &[u8]) -> String {
    let b = b.strip_suffix(&[0]).unwrap_or(b);
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    }
}

/// Set (or with an empty string, remove) the JPEG comment, losslessly.
pub fn write_jpeg_comment(path: &Path, comment: &str, keep_times: bool) -> Result<()> {
    let mut jpeg = load_jpeg(path)?;
    set_comment(&mut jpeg, comment)?;
    rewrite(path, &jpeg_bytes(jpeg)?, keep_times)
}

fn set_comment(jpeg: &mut Jpeg, comment: &str) -> Result<()> {
    jpeg.remove_segments_by_marker(markers::COM);
    let text = comment.trim_end_matches('\0');
    if text.is_empty() {
        return Ok(());
    }
    if text.len() > 65533 {
        bail!("Comment is too long (JPEG comments are limited to 65533 bytes)");
    }
    let segs = jpeg.segments_mut();
    // After the APPn segments (JFIF/EXIF must stay first), before the tables.
    let pos = segs.iter().position(|s| !(0xE0..=0xEF).contains(&s.marker())).unwrap_or(segs.len());
    segs.insert(pos, JpegSegment::new_with_contents(markers::COM, Bytes::from(text.as_bytes().to_vec())));
    Ok(())
}

// ---------------------------------------------------------------- strip

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripOptions {
    /// Keep the embedded ICC color profile (recommended: colors change without it).
    pub keep_icc: bool,
    /// Keep the EXIF orientation (as a minimal EXIF block) so the photo stays upright.
    pub keep_orientation: bool,
    /// Keep the JPEG COM comment.
    pub keep_comment: bool,
}

impl Default for StripOptions {
    fn default() -> Self {
        StripOptions { keep_icc: true, keep_orientation: true, keep_comment: false }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StripReport {
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// Remove EXIF (including the embedded thumbnail and GPS), XMP, IPTC /
/// Photoshop, MPF preview images and other application segments from a JPEG
/// without re-encoding. JFIF (APP0) and Adobe (APP14, needed for CMYK colors)
/// are always kept.
pub fn strip_jpeg_metadata(path: &Path, opts: StripOptions, keep_times: bool) -> Result<StripReport> {
    let data = crate::decode::read_file(path)?;
    let before = data.len();
    let out = strip_jpeg_bytes(data, opts)?;
    let after = out.len();
    rewrite(path, &out, keep_times)?;
    Ok(StripReport { bytes_before: before, bytes_after: after })
}

pub fn strip_jpeg_bytes(data: Vec<u8>, opts: StripOptions) -> Result<Vec<u8>> {
    let mut jpeg = Jpeg::from_bytes(Bytes::from(data)).map_err(|e| anyhow!("Not a valid JPEG: {e}"))?;
    let orientation = jpeg.exif().map(|e| crate::decode::exif_orientation(&e)).unwrap_or(1);
    let had_mpf = jpeg.segments().iter().any(|s| s.marker() == markers::APP2 && s.contents().starts_with(b"MPF\0"));
    jpeg.segments_mut().retain(|s| {
        let m = s.marker();
        match m {
            markers::APP0 => true,
            markers::APP14 => true, // Adobe: color transform flag
            markers::APP2 => opts.keep_icc && s.contents().starts_with(b"ICC_PROFILE\0"),
            0xE1..=0xEF => false,
            markers::COM => opts.keep_comment,
            _ => true,
        }
    });
    if opts.keep_orientation && orientation != 1 {
        jpeg.set_exif(Some(Bytes::from(crate::encode::minimal_exif(orientation as u16))));
    }
    let mut out = jpeg_bytes(jpeg)?;
    if had_mpf {
        // MPF secondary images are appended after the primary EOI: cut them off.
        truncate_after_primary_eoi(&mut out);
    }
    Ok(out)
}

/// Drop anything after the EOI that ends the first image. Entropy-coded data
/// cannot contain FF D9 (0xFF bytes are stuffed), so the first one after SOS
/// is the primary image's end.
fn truncate_after_primary_eoi(b: &mut Vec<u8>) {
    let mut i = 2usize;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if m == 0xDA {
            let start = i + 2 + len;
            if let Some(p) = b.get(start..).and_then(|e| e.windows(2).position(|w| w == [0xFF, 0xD9])) {
                b.truncate(start + p + 2);
            }
            return;
        }
        i += 2 + len;
    }
}

/// Lossless metadata removal for PNG: drops text / XMP (tEXt, zTXt, iTXt),
/// eXIf, time and other ancillary chunks while keeping everything that
/// affects how the pixels look.
pub fn strip_png_metadata(path: &Path, keep_icc: bool, keep_times: bool) -> Result<StripReport> {
    use img_parts::png::Png;
    let data = crate::decode::read_file(path)?;
    let before = data.len();
    let mut png = Png::from_bytes(Bytes::from(data)).map_err(|e| anyhow!("Not a valid PNG: {e}"))?;
    png.chunks_mut().retain(|c| {
        let k = c.kind();
        matches!(&k, b"IHDR" | b"PLTE" | b"IDAT" | b"IEND" | b"tRNS" | b"gAMA" | b"cHRM" | b"sRGB" | b"sBIT" | b"pHYs" | b"acTL" | b"fcTL" | b"fdAT" | b"cICP" | b"mDCV" | b"cLLI")
            || (keep_icc && &k == b"iCCP")
    });
    let mut out = Vec::new();
    png.encoder().write_to(&mut out)?;
    let after = out.len();
    rewrite(path, &out, keep_times)?;
    Ok(StripReport { bytes_before: before, bytes_after: after })
}

/// Strip metadata from any supported file (JPEG or PNG).
pub fn strip_metadata(path: &Path, opts: StripOptions, keep_times: bool) -> Result<StripReport> {
    match formats::ext_of(path).as_str() {
        e if formats::is_jpeg_ext(e) => strip_jpeg_metadata(path, opts, keep_times),
        "png" | "apng" => strip_png_metadata(path, opts.keep_icc, keep_times),
        _ => bail!("Removing metadata is supported for JPEG and PNG files"),
    }
}

pub fn can_strip(path: &Path) -> bool {
    let e = formats::ext_of(path);
    formats::is_jpeg_ext(&e) || e == "png" || e == "apng"
}

// ---------------------------------------------------------------- file times

/// Set the file's creation and/or modification time (SetFileTime under the
/// hood). Only FILE_WRITE_ATTRIBUTES access is requested, so read-only files
/// work too.
pub fn set_file_times(path: &Path, created: Option<SystemTime>, modified: Option<SystemTime>) -> Result<()> {
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    if created.is_none() && modified.is_none() {
        return Ok(());
    }
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    let f = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .open(path)
        .with_context(|| format!("Cannot open {}", path.display()))?;
    let mut t = std::fs::FileTimes::new();
    if let Some(m) = modified {
        t = t.set_modified(m);
    }
    if let Some(c) = created {
        t = t.set_created(c);
    }
    f.set_times(t).with_context(|| format!("Cannot change the dates of {}", path.display()))
}

/// Interpret a naive local date-time (as EXIF stores it) as a SystemTime.
pub fn local_to_system(dt: NaiveDateTime) -> Option<SystemTime> {
    let local = chrono::Local.from_local_datetime(&dt).earliest()?;
    Some(SystemTime::from(local))
}

/// Parse "YYYY-MM-DD HH:MM:SS", "YYYY:MM:DD HH:MM:SS" or "YYYY-MM-DD HH:MM".
pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    for f in ["%Y-%m-%d %H:%M:%S", EXIF_DATE_FMT, "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, f) {
            return Some(d);
        }
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// EXIF "date taken" (DateTimeOriginal, then Digitized, then DateTime).
pub fn exif_date_taken(path: &Path) -> Option<NaiveDateTime> {
    let ts = crate::meta::read_exif(path)?.taken_ts?;
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.naive_utc())
}

/// Set the file's modified (and optionally created) time to its EXIF date taken.
pub fn file_times_from_exif(path: &Path, also_created: bool) -> Result<NaiveDateTime> {
    let dt = exif_date_taken(path).ok_or_else(|| anyhow!("No EXIF date taken in {}", file_name(path)))?;
    let t = local_to_system(dt).ok_or_else(|| anyhow!("Invalid local time {dt}"))?;
    set_file_times(path, also_created.then_some(t), Some(t))?;
    Ok(dt)
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------- EXIF dates

pub const TAG_DATETIME: u16 = 0x0132;
pub const TAG_DATETIME_ORIGINAL: u16 = 0x9003;
pub const TAG_DATETIME_DIGITIZED: u16 = 0x9004;

/// Find ASCII date tags in IFD0 and the EXIF sub-IFD of a TIFF/EXIF blob.
/// Returns (tag, byte offset of the 19-character value).
fn find_date_fields(t: &[u8]) -> Vec<(u16, usize)> {
    let mut out = Vec::new();
    if t.len() < 8 {
        return out;
    }
    let le = match &t[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return out,
    };
    let rd16 = |o: usize| -> Option<u16> {
        let b = t.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let rd32 = |o: usize| -> Option<u32> {
        let b = t.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) })
    };
    let mut ifds = vec![rd32(4).unwrap_or(0) as usize];
    let mut seen = Vec::new();
    while let Some(ifd) = ifds.pop() {
        if ifd == 0 || ifd >= t.len() || seen.contains(&ifd) || seen.len() > 4 {
            continue;
        }
        seen.push(ifd);
        let Some(count) = rd16(ifd) else { continue };
        for i in 0..count as usize {
            let e = ifd + 2 + i * 12;
            let (Some(tag), Some(ty), Some(n), Some(val)) = (rd16(e), rd16(e + 2), rd32(e + 4), rd32(e + 8)) else { break };
            match tag {
                0x8769 => ifds.push(val as usize),
                TAG_DATETIME | TAG_DATETIME_ORIGINAL | TAG_DATETIME_DIGITIZED if ty == 2 && n >= 19 => {
                    let off = val as usize;
                    if off + 19 <= t.len() {
                        out.push((tag, off));
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Apply `f` to every parseable EXIF date in place. Returns how many changed.
fn edit_exif_dates(exif: &mut [u8], mut f: impl FnMut(u16, NaiveDateTime) -> NaiveDateTime) -> usize {
    let base = if exif.starts_with(b"Exif\0\0") { 6 } else { 0 };
    let t = &mut exif[base..];
    let fields = find_date_fields(t);
    let mut n = 0;
    for (tag, off) in fields {
        let Ok(s) = std::str::from_utf8(&t[off..off + 19]) else { continue };
        let Ok(dt) = NaiveDateTime::parse_from_str(s, EXIF_DATE_FMT) else { continue };
        let new = f(tag, dt).format(EXIF_DATE_FMT).to_string();
        if new.len() == 19 {
            t[off..off + 19].copy_from_slice(new.as_bytes());
            n += 1;
        }
    }
    n
}

/// Which EXIF dates to touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTags {
    pub original: bool,
    pub digitized: bool,
    pub modified: bool,
}

impl Default for DateTags {
    fn default() -> Self {
        DateTags { original: true, digitized: true, modified: true }
    }
}

impl DateTags {
    fn wants(&self, tag: u16) -> bool {
        match tag {
            TAG_DATETIME_ORIGINAL => self.original,
            TAG_DATETIME_DIGITIZED => self.digitized,
            _ => self.modified,
        }
    }
}

fn edit_file_exif(path: &Path, keep_times: bool, edit: impl FnOnce(&mut Vec<u8>) -> usize) -> Result<usize> {
    let data = crate::decode::read_file(path)?;
    let ext = formats::ext_of(path);
    let mut img = img_parts::DynImage::from_bytes(Bytes::from(data))
        .map_err(|e| anyhow!("Cannot parse {}: {e}", file_name(path)))?
        .ok_or_else(|| anyhow!("EXIF dates can be changed in JPEG, PNG and WebP files ({} is .{ext})", file_name(path)))?;
    let mut exif = img.exif().ok_or_else(|| anyhow!("{} has no EXIF data", file_name(path)))?.to_vec();
    let n = edit(&mut exif);
    if n == 0 {
        bail!("{} has no EXIF dates", file_name(path));
    }
    img.set_exif(Some(Bytes::from(exif)));
    let mut out = Vec::new();
    img.encoder().write_to(&mut out)?;
    rewrite(path, &out, keep_times)?;
    Ok(n)
}

/// Shift EXIF dates by `offset_secs` (patched in place, no re-encode).
pub fn shift_exif_dates(path: &Path, offset_secs: i64, tags: DateTags, keep_times: bool) -> Result<usize> {
    let d = Duration::seconds(offset_secs);
    edit_file_exif(path, keep_times, |e| edit_exif_dates(e, |tag, dt| if tags.wants(tag) { dt + d } else { dt }))
}

/// Set EXIF dates to an absolute value (only tags that already exist).
pub fn set_exif_dates(path: &Path, value: NaiveDateTime, tags: DateTags, keep_times: bool) -> Result<usize> {
    edit_file_exif(path, keep_times, |e| edit_exif_dates(e, |tag, dt| if tags.wants(tag) { value } else { dt }))
}

// ---------------------------------------------------------------- XMP sidecar

pub fn sidecar_path(path: &Path) -> PathBuf {
    path.with_extension("xmp")
}

const LABELS: [&str; 6] = ["", "Red", "Yellow", "Green", "Blue", "Purple"];

/// Write (or update) `name.xmp` next to the image with xmp:Rating, xmp:Label
/// and lumen:Flagged. Existing sidecars (e.g. from Lightroom) are edited in
/// place so their other contents survive, and only the properties Lumen has
/// a value for are written: an unrated / unlabelled file never overwrites a
/// rating or label stored by another program.
pub fn export_xmp_sidecar(path: &Path, tag: crate::tags::Tag) -> Result<PathBuf> {
    let side = sidecar_path(path);
    let rating = tag.rating.min(5).to_string();
    let label = LABELS.get(tag.label as usize).copied().unwrap_or("");
    let flagged = if tag.flagged { "True" } else { "False" };
    let xml = match std::fs::read_to_string(&side) {
        Ok(existing) if existing.contains("<rdf:Description") => {
            let mut s = existing;
            if tag.rating > 0 {
                s = set_xmp_property(s, "xmp", "http://ns.adobe.com/xap/1.0/", "Rating", &rating);
            }
            if !label.is_empty() {
                s = set_xmp_property(s, "xmp", "http://ns.adobe.com/xap/1.0/", "Label", label);
            }
            // lumen:Flagged is Lumen's own property: keep it in sync when present.
            if tag.flagged || find_xmp_value(&s, "lumen:Flagged").is_some() {
                s = set_xmp_property(s, "lumen", LUMEN_NS, "Flagged", flagged);
            }
            s
        }
        _ => {
            let mut props = String::new();
            if tag.rating > 0 {
                props.push_str(&format!("\n   xmp:Rating=\"{rating}\""));
            }
            if !label.is_empty() {
                props.push_str(&format!("\n   xmp:Label=\"{label}\""));
            }
            if tag.flagged {
                props.push_str("\n   lumen:Flagged=\"True\"");
            }
            format!(
            "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Lumen\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\"\n\
    xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n\
    xmlns:lumen=\"{LUMEN_NS}\"{props}/>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>\n"
            )
        }
    };
    write_atomic(&side, xml.as_bytes())?;
    Ok(side)
}

const LUMEN_NS: &str = "https://github.com/RahulTarigoppula/lumen-image-viewer/ns/1.0/";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;")
}

/// Byte range of the value of attribute `key` (e.g. `xmp:Rating`), with
/// either quote style and optional spaces around `=`. The key must start at
/// a name boundary so `foo:xmp:Rating` or `myxmp:Rating` do not match.
fn find_xmp_attr(s: &str, key: &str) -> Option<(usize, usize, char)> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(off) = s[from..].find(key) {
        let i = from + off;
        from = i + key.len();
        if i > 0 && !b[i - 1].is_ascii_whitespace() {
            continue;
        }
        let mut j = i + key.len();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != b'=' {
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let q = match b.get(j) {
            Some(b'"') => '"',
            Some(b'\'') => '\'',
            _ => continue,
        };
        let start = j + 1;
        let len = s[start..].find(q)?;
        return Some((start, start + len, q));
    }
    None
}

/// Value of `key` as an attribute (either quote style) or a simple element.
fn find_xmp_value(s: &str, key: &str) -> Option<String> {
    if let Some((a, b, _)) = find_xmp_attr(s, key) {
        return Some(s[a..b].to_string());
    }
    let open = format!("<{key}>");
    let a = s.find(&open)? + open.len();
    let b = s[a..].find('<')?;
    Some(s[a..a + b].trim().to_string())
}

/// Set `prefix:name` on the first rdf:Description, as an attribute or by
/// replacing an existing element / attribute value.
fn set_xmp_property(mut s: String, prefix: &str, ns: &str, name: &str, value: &str) -> String {
    let mut value = xml_escape(value);
    if let Some((start, end, q)) = find_xmp_attr(&s, &format!("{prefix}:{name}")) {
        if q == '\'' {
            value = value.replace('\'', "&apos;");
        }
        s.replace_range(start..end, &value);
        return s;
    }
    let open = format!("<{prefix}:{name}>");
    let close = format!("</{prefix}:{name}>");
    if let (Some(a), Some(b)) = (s.find(&open), s.find(&close)) {
        if b > a {
            s.replace_range(a + open.len()..b, &value);
            return s;
        }
    }
    let Some(d) = s.find("<rdf:Description") else { return s };
    let insert_at = d + "<rdf:Description".len();
    let mut add = String::new();
    if !s.contains(&format!("xmlns:{prefix}=")) {
        add.push_str(&format!(" xmlns:{prefix}=\"{ns}\""));
    }
    add.push_str(&format!(" {prefix}:{name}=\"{value}\""));
    s.insert_str(insert_at, &add);
    s
}

/// Read rating / label / flag back from a sidecar (for importing ratings).
pub fn read_xmp_sidecar(path: &Path) -> Option<crate::tags::Tag> {
    let s = std::fs::read_to_string(sidecar_path(path)).ok()?;
    let get = |key: &str| find_xmp_value(&s, key);
    let rating = get("xmp:Rating").and_then(|r| r.trim().parse::<i32>().ok()).unwrap_or(0).clamp(0, 5) as u8;
    let label = get("xmp:Label").map(|l| LABELS.iter().position(|n| !n.is_empty() && n.eq_ignore_ascii_case(l.trim())).unwrap_or(0) as u8).unwrap_or(0);
    let flagged = get("lumen:Flagged").map(|f| f.eq_ignore_ascii_case("true")).unwrap_or(false);
    Some(crate::tags::Tag { rating, flagged, label })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lumen-metaedit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// EXIF with orientation, DateTime in IFD0 and DateTimeOriginal in the EXIF IFD.
    fn exif_blob(orientation: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"II*\0");
        v.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 at 8: 3 entries -> 2 + 36 + 4 = 42 bytes -> ends at 50
        let date0 = 50u32;
        let exif_ifd = date0 + 20;
        let date1 = exif_ifd + 2 + 12 + 4;
        v.extend_from_slice(&3u16.to_le_bytes());
        let mut entry = |tag: u16, ty: u16, n: u32, val: u32| {
            v.extend_from_slice(&tag.to_le_bytes());
            v.extend_from_slice(&ty.to_le_bytes());
            v.extend_from_slice(&n.to_le_bytes());
            v.extend_from_slice(&val.to_le_bytes());
        };
        entry(0x0112, 3, 1, orientation as u32);
        entry(TAG_DATETIME, 2, 20, date0);
        entry(0x8769, 4, 1, exif_ifd);
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"2020:01:31 23:30:00\0");
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&TAG_DATETIME_ORIGINAL.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&20u32.to_le_bytes());
        v.extend_from_slice(&date1.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"2020:01:31 23:30:00\0");
        v
    }

    fn sample_jpeg() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(16, 8, image::Rgba([200, 30, 30, 255]));
        let mut out = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
        enc.add_exif_metadata(&exif_blob(6)).unwrap();
        enc.add_icc_profile(&[0u8; 200]).unwrap();
        enc.encode(&crate::encode::flatten_rgb(&img, [0, 0, 0]), 16, 8, jpeg_encoder::ColorType::Rgb).unwrap();
        // Add XMP (APP1) and IPTC (APP13) segments.
        let mut jpeg = Jpeg::from_bytes(Bytes::from(out)).unwrap();
        let segs = jpeg.segments_mut();
        segs.insert(1, JpegSegment::new_with_contents(markers::APP1, Bytes::from_static(b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta/>")));
        segs.insert(1, JpegSegment::new_with_contents(markers::APP13, Bytes::from_static(b"Photoshop 3.0\08BIM")));
        jpeg_bytes(jpeg).unwrap()
    }

    #[test]
    fn comment_roundtrip_unicode_path() {
        let d = tmpdir("com");
        let p = d.join("caf\u{e9} \u{65e5}\u{672c}.jpg");
        std::fs::write(&p, sample_jpeg()).unwrap();
        assert_eq!(read_jpeg_comment(&p).unwrap(), None);
        write_jpeg_comment(&p, "Hello, world 1", true).unwrap();
        assert_eq!(read_jpeg_comment(&p).unwrap().as_deref(), Some("Hello, world 1"));
        write_jpeg_comment(&p, "second", false).unwrap();
        assert_eq!(read_jpeg_comment(&p).unwrap().as_deref(), Some("second"));
        let j = load_jpeg(&p).unwrap();
        assert_eq!(j.segments_by_marker(markers::COM).count(), 1);
        // JFIF/APP segments stay before the comment.
        assert_ne!(j.segments()[0].marker(), markers::COM);
        write_jpeg_comment(&p, "", false).unwrap();
        assert_eq!(read_jpeg_comment(&p).unwrap(), None);
        // Still decodes.
        assert!(crate::decode::decode_file(&p, Default::default()).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn strip_keeps_icc_and_orientation() {
        let data = sample_jpeg();
        let out = strip_jpeg_bytes(data.clone(), StripOptions::default()).unwrap();
        let j = Jpeg::from_bytes(Bytes::from(out.clone())).unwrap();
        assert!(j.segments_by_marker(markers::APP13).next().is_none());
        assert!(!j.segments().iter().any(|s| s.contents().starts_with(b"http://ns.adobe.com/xap")));
        assert!(j.segments().iter().any(|s| s.contents().starts_with(b"ICC_PROFILE")));
        let exif = j.exif().unwrap();
        assert_eq!(crate::decode::exif_orientation(&exif), 6);
        assert!(find_date_fields(exif.strip_prefix(b"Exif\0\0").unwrap_or(&exif)).is_empty());
        // Pixels unchanged: no re-encode.
        let pa = image::load_from_memory(&data).unwrap().into_rgb8();
        let pb = image::load_from_memory(&out).unwrap().into_rgb8();
        assert_eq!(pa, pb);
        // Trailing MPF-style data after the primary EOI is cut.
        let mut with_tail = out.clone();
        with_tail.extend_from_slice(b"\xFF\xD8 second image");
        let mut t = with_tail.clone();
        truncate_after_primary_eoi(&mut t);
        assert_eq!(t, out);

        let all = strip_jpeg_bytes(out, StripOptions { keep_icc: false, keep_orientation: false, keep_comment: false }).unwrap();
        let j = Jpeg::from_bytes(Bytes::from(all.clone())).unwrap();
        assert!(j.exif().is_none());
        assert!(j.segments_by_marker(markers::APP2).next().is_none());
        assert!(image::load_from_memory(&all).is_ok());
    }

    #[test]
    fn shift_and_set_exif_dates() {
        let d = tmpdir("dates");
        let p = d.join("a.jpg");
        std::fs::write(&p, sample_jpeg()).unwrap();
        let n = shift_exif_dates(&p, 3600, DateTags::default(), false).unwrap();
        assert_eq!(n, 2);
        let s = crate::meta::read_exif(&p).unwrap();
        assert_eq!(s.taken.as_deref(), Some("2020-02-01 00:30:00"));
        let only_mod = DateTags { original: false, digitized: false, modified: true };
        set_exif_dates(&p, parse_datetime("2001-02-03 04:05:06").unwrap(), only_mod, false).unwrap();
        let s = crate::meta::read_exif(&p).unwrap();
        assert_eq!(s.taken.as_deref(), Some("2020-02-01 00:30:00"));
        assert!(s.all.iter().any(|(_, k, v)| k == "DateTime" && v.contains("2001-02-03")));
        // File times from EXIF.
        let dt = file_times_from_exif(&p, true).unwrap();
        let m = std::fs::metadata(&p).unwrap().modified().unwrap();
        assert_eq!(m, local_to_system(dt).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_times_and_keep_times() {
        let d = tmpdir("times");
        let p = d.join("b.jpg");
        std::fs::write(&p, sample_jpeg()).unwrap();
        let t = local_to_system(parse_datetime("2015-06-07 08:09:10").unwrap()).unwrap();
        set_file_times(&p, Some(t), Some(t)).unwrap();
        let md = std::fs::metadata(&p).unwrap();
        assert_eq!(md.modified().unwrap(), t);
        assert_eq!(md.created().unwrap(), t);
        // Rewrites can preserve both.
        write_jpeg_comment(&p, "keep", true).unwrap();
        let md = std::fs::metadata(&p).unwrap();
        assert_eq!(md.modified().unwrap(), t);
        assert_eq!(md.created().unwrap(), t);
        // Read-only files still accept new times.
        let mut perm = md.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm.clone()).unwrap();
        set_file_times(&p, None, Some(SystemTime::now())).unwrap();
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn xmp_sidecar_roundtrip_and_merge() {
        let d = tmpdir("xmp");
        let p = d.join("c.jpg");
        std::fs::write(&p, b"x").unwrap();
        let tag = crate::tags::Tag { rating: 4, flagged: true, label: 3 };
        let side = export_xmp_sidecar(&p, tag).unwrap();
        assert_eq!(side, d.join("c.xmp"));
        assert_eq!(read_xmp_sidecar(&p), Some(tag));
        // An existing foreign sidecar keeps its contents.
        let lr = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"r\"><rdf:Description rdf:about=\"\" xmlns:crs=\"c\" crs:Exposure2012=\"+0.50\"><xmp:Rating>2</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        std::fs::write(&side, lr).unwrap();
        export_xmp_sidecar(&p, crate::tags::Tag { rating: 5, flagged: false, label: 1 }).unwrap();
        let s = std::fs::read_to_string(&side).unwrap();
        assert!(s.contains("crs:Exposure2012=\"+0.50\""));
        assert!(s.contains("<xmp:Rating>5</xmp:Rating>"));
        assert_eq!(read_xmp_sidecar(&p), Some(crate::tags::Tag { rating: 5, flagged: false, label: 1 }));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn xmp_export_keeps_foreign_values_and_single_quotes() {
        let d = tmpdir("xmpq");
        let p = d.join("q.jpg");
        std::fs::write(&p, b"x").unwrap();
        let side = sidecar_path(&p);
        // Single-quoted attributes (valid XML) are updated, not duplicated.
        let lr = "<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='r'><rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='3' xmp:Label='Red'/></rdf:RDF></x:xmpmeta>";
        std::fs::write(&side, lr).unwrap();
        export_xmp_sidecar(&p, crate::tags::Tag { rating: 5, flagged: false, label: 0 }).unwrap();
        let s = std::fs::read_to_string(&side).unwrap();
        assert_eq!(s.matches("xmp:Rating").count(), 1, "{s}");
        assert!(s.contains("xmp:Rating='5'"));
        // Lumen has no label: the existing one survives.
        assert!(s.contains("xmp:Label='Red'"));
        assert!(!s.contains("lumen:Flagged"));
        assert_eq!(read_xmp_sidecar(&p).map(|t| (t.rating, t.label)), Some((5, 1)));
        // Untagged export to a fresh sidecar writes no rating / label.
        let _ = std::fs::remove_file(&side);
        export_xmp_sidecar(&p, crate::tags::Tag::default()).unwrap();
        let s = std::fs::read_to_string(&side).unwrap();
        assert!(!s.contains("xmp:Rating") && !s.contains("xmp:Label"), "{s}");
        // Spaces around '=' and a prefix that merely ends in the key.
        assert_eq!(find_xmp_value("<a myxmp:Rating=\"1\" xmp:Rating = \"4\"/>", "xmp:Rating").as_deref(), Some("4"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn errors_are_clean() {
        let d = tmpdir("err");
        let p = d.join("empty.jpg");
        std::fs::write(&p, b"").unwrap();
        assert!(read_jpeg_comment(&p).is_err());
        assert!(strip_metadata(&p, StripOptions::default(), false).is_err());
        let q = d.join("junk.jpg");
        std::fs::write(&q, b"\xFF\xD8garbage").unwrap();
        assert!(write_jpeg_comment(&q, "x", false).is_err());
        assert_eq!(std::fs::read(&q).unwrap(), b"\xFF\xD8garbage");
        let _ = std::fs::remove_dir_all(&d);
    }
}
