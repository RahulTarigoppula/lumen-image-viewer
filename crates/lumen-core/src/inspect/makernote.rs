//! Maker notes decoded beyond a summary: Samsung "Type2" (Galaxy phones and
//! Samsung cameras) and Google's HDR+ note, which is encrypted and only
//! recognized.
//!
//! Samsung Type2, as ExifTool's Samsung.pm reads it: an IFD with no header,
//! in the byte order of the EXIF block, whose first entry is tag 0x0001
//! (MakerNoteVersion: 4 undefined bytes such as "0100"). Values longer than
//! 4 bytes sit elsewhere, at offsets Samsung counts from the maker note on
//! some models and from the TIFF header on others; like ExifTool's FixBase,
//! the base that puts the values right after the directory wins. Tag names
//! and value meanings follow ExifTool's Samsung Type2 table; other tags are
//! shown as stored.

use super::capture::{fmt_exposure, fmt_num};
use super::{be16, be32, le16, le32, printable, Field};

/// One maker note value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Int(Vec<i64>),
    Real(Vec<f64>),
    Text(String),
    Bytes(Vec<u8>),
}

impl Val {
    pub fn int(&self) -> Option<i64> {
        match self {
            Val::Int(v) => v.first().copied(),
            Val::Real(v) => v.first().filter(|x| x.is_finite() && x.fract() == 0.0).map(|&x| x as i64),
            _ => None,
        }
    }

    pub fn real(&self) -> Option<f64> {
        match self {
            Val::Int(v) => v.first().map(|&x| x as f64),
            Val::Real(v) => v.first().copied(),
            _ => None,
        }
        .filter(|x| x.is_finite())
    }

    fn ints(&self) -> &[i64] {
        match self {
            Val::Int(v) => v,
            _ => &[],
        }
    }

    fn text(&self) -> Option<String> {
        match self {
            Val::Text(s) => Some(s.clone()),
            Val::Bytes(b) => printable(b).filter(|s| !s.is_empty()),
            _ => None,
        }
    }
}

/// A decoded Samsung Type2 maker note.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SamsungNote {
    /// (tag, value) in directory order.
    pub tags: Vec<(u16, Val)>,
    /// Long values were found at offsets from the TIFF header (else from the maker note).
    pub absolute: bool,
}

impl SamsungNote {
    pub fn get(&self, tag: u16) -> Option<&Val> {
        self.tags.iter().find(|(t, _)| *t == tag).map(|(_, v)| v)
    }

    pub fn int(&self, tag: u16) -> Option<i64> {
        self.get(tag)?.int()
    }

    pub fn real(&self, tag: u16) -> Option<f64> {
        self.get(tag)?.real()
    }
}

/// Tag numbers the Capture section reads.
pub(crate) mod tag {
    pub const WHITE_BALANCE: u16 = 0x0041;
    pub const FACE_DETECT: u16 = 0x0100;
    pub const FACE_RECOGNITION: u16 = 0x0120;
    pub const SMART_RANGE: u16 = 0xA012;
    pub const EXPOSURE_COMP: u16 = 0xA013;
    pub const ISO: u16 = 0xA014;
    pub const EXPOSURE_TIME: u16 = 0xA018;
    pub const F_NUMBER: u16 = 0xA019;
}

/// Byte order of a TIFF-style structure (true = little-endian).
#[derive(Clone, Copy)]
struct Order(bool);

impl Order {
    fn u16(self, b: &[u8], o: usize) -> Option<u16> {
        if self.0 { le16(b, o) } else { be16(b, o) }
    }
    fn u32(self, b: &[u8], o: usize) -> Option<u32> {
        if self.0 { le32(b, o) } else { be32(b, o) }
    }
}

/// Bytes per value of a TIFF field type (None for unknown types).
fn unit(typ: u16) -> Option<usize> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

/// Decode `cnt` values of type `typ` from exactly the bytes `d`.
fn decode(e: Order, typ: u16, cnt: usize, d: &[u8]) -> Val {
    let ints = |size: usize, f: &dyn Fn(usize) -> Option<i64>| Val::Int((0..cnt).filter_map(|i| f(i * size)).collect());
    let ratio = |n: f64, den: f64| if den == 0.0 { f64::NAN } else { n / den };
    match typ {
        1 => Val::Int(d.iter().map(|&x| x as i64).collect()),
        6 => Val::Int(d.iter().map(|&x| x as i8 as i64).collect()),
        2 => Val::Text(String::from_utf8_lossy(d).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()),
        3 => ints(2, &|o| e.u16(d, o).map(|v| v as i64)),
        8 => ints(2, &|o| e.u16(d, o).map(|v| v as i16 as i64)),
        4 | 13 => ints(4, &|o| e.u32(d, o).map(|v| v as i64)),
        9 => ints(4, &|o| e.u32(d, o).map(|v| v as i32 as i64)),
        5 => Val::Real((0..cnt).filter_map(|i| Some(ratio(e.u32(d, 8 * i)? as f64, e.u32(d, 8 * i + 4)? as f64))).collect()),
        10 => Val::Real((0..cnt).filter_map(|i| Some(ratio(e.u32(d, 8 * i)? as i32 as f64, e.u32(d, 8 * i + 4)? as i32 as f64))).collect()),
        11 => Val::Real((0..cnt).filter_map(|i| e.u32(d, 4 * i)).map(|v| f32::from_bits(v) as f64).collect()),
        12 => Val::Real(
            (0..cnt)
                .filter_map(|i| {
                    let (a, b) = (e.u32(d, 8 * i)? as u64, e.u32(d, 8 * i + 4)? as u64);
                    Some(f64::from_bits(if e.0 { (b << 32) | a } else { (a << 32) | b }))
                })
                .collect(),
        ),
        _ => Val::Bytes(d.to_vec()),
    }
}

/// Offset of the entry for `tag` in the IFD at `ifd` (None when absent).
fn find_entry(t: &[u8], e: Order, ifd: usize, tag: u16) -> Option<usize> {
    let n = e.u16(t, ifd)? as usize;
    (0..n.min(1000)).map(|i| ifd + 2 + 12 * i).find(|&o| e.u16(t, o) == Some(tag))
}

/// Where the MakerNote value starts in a TIFF-structured EXIF block.
fn maker_note_pos(t: &[u8]) -> Option<usize> {
    let e = match t.get(0..2)? {
        b"II" => Order(true),
        b"MM" => Order(false),
        _ => return None,
    };
    let ifd0 = e.u32(t, 4)? as usize;
    let exif_ifd = e.u32(t, find_entry(t, e, ifd0, 0x8769)? + 8)? as usize;
    let f = find_entry(t, e, exif_ifd, 0x927C)?;
    if e.u32(t, f + 4)? <= 4 { Some(f + 8) } else { e.u32(t, f + 8).map(|v| v as usize) }
}

/// The Samsung Type2 signature: a plausible entry count, then tag 0x0001 of
/// type UNDEFINED with 4 digits ("0100").
fn type2_order(m: &[u8]) -> Option<Order> {
    [Order(true), Order(false)].into_iter().find(|&e| {
        e.u16(m, 0).is_some_and(|n| (1..1000).contains(&n))
            && e.u16(m, 2) == Some(1)
            && e.u16(m, 4) == Some(7)
            && e.u32(m, 6) == Some(4)
            && m.get(10..14).is_some_and(|v| v.iter().all(u8::is_ascii_digit))
    })
}

/// Decode a Samsung Type2 maker note `m`. `tiff` is the EXIF block it came
/// from (to resolve offsets counted from the TIFF header); `make` the EXIF
/// Make (another maker's note with a similar start is left alone). None when
/// `m` is not one; values that cannot be found become a warning.
pub(crate) fn samsung(m: &[u8], tiff: Option<&[u8]>, make: Option<&str>, warn: &mut Vec<String>) -> Option<SamsungNote> {
    if make.is_some_and(|mk| !mk.to_ascii_lowercase().contains("samsung")) {
        return None;
    }
    let e = type2_order(m)?;
    let declared = e.u16(m, 0)? as usize;
    let n = declared.min(m.len().saturating_sub(2) / 12);
    if n < declared {
        warn.push(format!("Samsung maker note: its directory is cut short ({n} of {declared} entries read)."));
    }
    let table_end = 2 + 12 * n;
    // Where the maker note sits in the TIFF block (for offsets counted from the TIFF header).
    let mn_pos = tiff.and_then(|t| maker_note_pos(t).filter(|&p| t.get(p..p.saturating_add(m.len())) == Some(m)));
    struct Raw {
        tag: u16,
        typ: u16,
        cnt: usize,
        field: usize,
        size: usize,
    }
    let raws: Vec<Raw> = (0..n)
        .filter_map(|i| {
            let o = 2 + 12 * i;
            let (tag, typ, cnt) = (e.u16(m, o)?, e.u16(m, o + 2)?, e.u32(m, o + 4)? as usize);
            let size = unit(typ)?.checked_mul(cnt)?;
            Some(Raw { tag, typ, cnt, field: o + 8, size })
        })
        .collect();
    // Choose the base of the long values: the one where most of them fit
    // inside the maker note after its directory, the lowest closest to it.
    let offsets: Vec<(usize, usize)> = raws.iter().filter(|r| r.size > 4).filter_map(|r| Some((e.u32(m, r.field)? as usize, r.size))).collect();
    let score = |shift: usize| -> (usize, usize) {
        let mut fit = 0;
        let mut lowest = usize::MAX;
        for &(off, size) in &offsets {
            if let Some(p) = off.checked_sub(shift) {
                if p >= table_end && p.saturating_add(size) <= m.len() {
                    fit += 1;
                    lowest = lowest.min(p);
                }
            }
        }
        (fit, lowest.saturating_sub(table_end))
    };
    let rel = score(0);
    let absolute = mn_pos.filter(|&p| p > 0).is_some_and(|p| {
        let abs = score(p);
        abs.0 > rel.0 || (abs.0 == rel.0 && abs.0 > 0 && abs.1 < rel.1)
    });
    let shift = if absolute { mn_pos.unwrap_or(0) } else { 0 };
    let mut tags = Vec::new();
    let mut skipped = 0;
    for r in &raws {
        let data = if r.size <= 4 {
            m.get(r.field..r.field + r.size)
        } else {
            e.u32(m, r.field)
                .and_then(|off| (off as usize).checked_sub(shift))
                .filter(|&p| p >= table_end)
                .and_then(|p| m.get(p..p.checked_add(r.size)?))
        };
        match data {
            Some(d) => tags.push((r.tag, decode(e, r.typ, r.cnt, d))),
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        warn.push(format!(
            "Samsung maker note: {skipped} value{} point{} outside the maker note and {} not read.",
            if skipped == 1 { "" } else { "s" },
            if skipped == 1 { "s" } else { "" },
            if skipped == 1 { "was" } else { "were" }
        ));
    }
    Some(SamsungNote { tags, absolute })
}

/// Google's HDR+ maker note ("HDRP", a version byte, then encrypted data):
/// the version byte, or None when `m` is not one.
pub(crate) fn google_hdrp(m: &[u8]) -> Option<u8> {
    m.starts_with(b"HDRP").then(|| m.get(4).copied().unwrap_or(0))
}

// ---- Display ----

/// A value as stored: short lists in full, long ones as a count.
fn plain(v: &Val) -> String {
    let list = |items: Vec<String>| if items.len() <= 8 { items.join(", ") } else { format!("({} values)", super::thousands(items.len() as u64)) };
    match v {
        Val::Int(x) => list(x.iter().map(|n| n.to_string()).collect()),
        Val::Real(x) => list(x.iter().map(|&n| if n.is_finite() { trim_num(n) } else { "?".into() }).collect()),
        Val::Text(s) if s.is_empty() => "(empty)".into(),
        Val::Text(s) => super::clip(s, 200),
        Val::Bytes(b) => match printable(b) {
            Some(t) if !t.is_empty() && t.chars().all(|c| !c.is_control()) => super::clip(&t, 200),
            _ if b.len() <= 16 => b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" "),
            _ => format!("({} bytes)", super::thousands(b.len() as u64)),
        },
    }
}

/// Up to 4 decimals, trailing zeros removed.
fn trim_num(n: f64) -> String {
    let s = format!("{n:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

fn pick(v: &Val, names: &[(i64, &str)]) -> String {
    match v.int().and_then(|i| names.iter().find(|(k, _)| *k == i)) {
        Some((_, n)) => n.to_string(),
        None => plain(v),
    }
}

const OFF_ON: [(i64, &str); 2] = [(0, "Off"), (1, "On")];
const WB_NOTE: &str = "White balance multipliers for the red, green, green and blue sensor channels.";
const MATRIX_NOTE: &str = "Color conversion matrix used when developing the sensor data.";
const CURVE_NOTE: &str = "Tone curve used when developing the sensor data.";

/// Display name, value and hover note of one Samsung Type2 tag.
fn describe(t: u16, v: &Val) -> (String, String, &'static str) {
    let named = |name: &str, value: String, note: &'static str| (name.to_string(), value, note);
    let count = || match v {
        Val::Int(x) => format!("({} values)", x.len()),
        Val::Real(x) => format!("({} values)", x.len()),
        _ => plain(v),
    };
    match t {
        0x0001 => named("Maker note version", v.text().unwrap_or_else(|| plain(v)), "Version of Samsung's maker note layout."),
        0x0002 => {
            let s = match v.int() {
                Some(0x1000) => "Compact camera".to_string(),
                Some(0x2000) => "NX camera (interchangeable lens)".to_string(),
                Some(0x3000) => "HMX video camera".to_string(),
                Some(0x12000) => "Phone".to_string(),
                Some(0x300000) => "SMX video camera".to_string(),
                Some(x) => format!("Code 0x{x:X}"),
                None => plain(v),
            };
            named("Device type", s, "Kind of Samsung device that took the photo.")
        }
        0x0003 => named(
            "Model ID",
            v.int().map_or_else(|| plain(v), |x| format!("0x{x:08X}")),
            "Samsung's internal model code. Samsung cameras have published names for it; phones only show the code.",
        ),
        0x0020 => named("Smart album color", plain(v), "Gallery color tag set by the camera."),
        0x0021 => {
            let modes = [
                (0, "Standard"),
                (1, "Vivid"),
                (2, "Portrait"),
                (3, "Landscape"),
                (4, "Forest"),
                (5, "Retro"),
                (6, "Cool"),
                (7, "Calm"),
                (8, "Classic"),
                (9, "Custom 1"),
                (10, "Custom 2"),
                (11, "Custom 3"),
                (255, "None"),
            ];
            named("Picture Wizard", pick(v, &modes), "Samsung's picture style (colors, saturation, sharpness and contrast presets).")
        }
        0x0030 => named("Local location name", plain(v), ""),
        0x0031 => named("Location name", plain(v), ""),
        0x0035 => named("Preview image", "(stored)".into(), "A preview image stored inside the maker note."),
        0x0040 => named("Raw data byte order", plain(v), "Byte order of the sensor data (raw files)."),
        0x0041 => named("White balance setup", pick(v, &[(0, "Auto"), (1, "Manual")]), "White balance chosen in the camera: Auto, or Manual (a preset or a set color temperature)."),
        0x0043 => named(
            "Camera temperature",
            v.real().map_or_else(|| plain(v), |c| format!("{c:.1} \u{00B0}C")),
            "Temperature inside the camera when the photo was taken; heat adds sensor noise.",
        ),
        0x0050 => named("Raw data color pattern", plain(v), "Layout of the color filters on the sensor (raw files)."),
        0x0100 => named("Face detection", pick(v, &OFF_ON), "Whether face detection was on: faces then guide focus and exposure."),
        0x0120 => named("Face recognition", pick(v, &OFF_ON), "Whether detected faces were matched against saved names."),
        0x0123 => named("Face name", plain(v), ""),
        0xA001 => named("Firmware", plain(v), "Camera firmware or image processor (ISP) build that processed the photo."),
        0xA002 => named("Serial number", plain(v), ""),
        0xA003 => {
            let s = match v.int() {
                Some(0) => "Built-in or manual lens".to_string(),
                Some(x) => format!("Lens code {x}"),
                None => plain(v),
            };
            named("Lens type", s, "Lens code; 0 is a built-in lens (phones and compact cameras) or a manual lens.")
        }
        0xA004 => named("Lens firmware", plain(v), ""),
        0xA005 => named("Lens serial number", plain(v), ""),
        0xA010 => {
            let a = v.ints();
            let s = if a.len() >= 8 && a[2] > a[0] && a[3] > a[1] && a[6] > a[4] && a[7] > a[5] {
                format!("Full {} x {}, used {} x {} (from {}, {})", a[2] - a[0], a[3] - a[1], a[6] - a[4], a[7] - a[5], a[4], a[5])
            } else {
                plain(v)
            };
            named("Sensor area", s, "The sensor's full area and the part used for the image, in sensor pixels.")
        }
        0xA011 => named("Color space", pick(v, &[(0, "sRGB"), (1, "Adobe RGB")]), "Color space chosen in the camera."),
        0xA012 => named("Smart Range", pick(v, &OFF_ON), "Samsung's dynamic range boost: keeps more highlight and shadow detail."),
        0xA013 => named("Exposure compensation", v.real().map_or_else(|| plain(v), |b| format!("{b:+.1} EV")), "Exposure compensation as Samsung stored it."),
        0xA014 => named("ISO", v.int().map_or_else(|| plain(v), |i| format!("ISO {i}")), "ISO sensitivity as Samsung stored it."),
        0xA018 => named("Exposure time", v.real().filter(|&x| x > 0.0).map_or_else(|| plain(v), fmt_exposure), "Shutter time as Samsung stored it."),
        0xA019 => named("F-number", v.real().filter(|&x| x > 0.0).map_or_else(|| plain(v), |n| format!("f/{}", fmt_num(n))), "Aperture as Samsung stored it."),
        0xA01A => named(
            "Focal length (35 mm equivalent)",
            v.real().filter(|&x| x > 0.0).map_or_else(|| plain(v), |f| format!("{} mm", fmt_num(f / 10.0))),
            "Field of view expressed as a full-frame (35 mm) focal length.",
        ),
        0xA020 => named("Encryption key", count(), "Key Samsung uses for its raw data; not needed to view the photo."),
        0xA021 => named("White balance levels (uncorrected)", plain(v), WB_NOTE),
        0xA022 => named("White balance levels (auto)", plain(v), WB_NOTE),
        0xA023 => named("White balance levels (illuminant 1)", plain(v), WB_NOTE),
        0xA024 => named("White balance levels (illuminant 2)", plain(v), WB_NOTE),
        0xA028 => named("Black levels", plain(v), "Sensor black level per color channel (red, green, green, blue)."),
        0xA030 => named("Color matrix", count(), MATRIX_NOTE),
        0xA031 => named("Color matrix (sRGB)", count(), MATRIX_NOTE),
        0xA032 => named("Color matrix (Adobe RGB)", count(), MATRIX_NOTE),
        0xA033 => named("CbCr matrix (default)", count(), MATRIX_NOTE),
        0xA034 => named("CbCr matrix", count(), MATRIX_NOTE),
        0xA035 => named("CbCr gain (default)", count(), MATRIX_NOTE),
        0xA036 => named("CbCr gain", count(), MATRIX_NOTE),
        0xA040 => named("Tone curve (sRGB, default)", count(), CURVE_NOTE),
        0xA041 => named("Tone curve (Adobe RGB, default)", count(), CURVE_NOTE),
        0xA042 => named("Tone curve (sRGB)", count(), CURVE_NOTE),
        0xA043 => named("Tone curve (Adobe RGB)", count(), CURVE_NOTE),
        _ => (format!("Tag 0x{t:04X}"), plain(v), "Not described in public documentation; shown as stored."),
    }
}

/// The decoded tags as display rows, in directory order.
pub(crate) fn samsung_fields(n: &SamsungNote) -> Vec<Field> {
    n.tags
        .iter()
        .map(|(t, v)| {
            let (name, value, note) = describe(*t, v);
            Field::new(name, value, note)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_decode_both_byte_orders() {
        let le = Order(true);
        let be = Order(false);
        assert_eq!(decode(le, 3, 2, &[1, 0, 2, 0]), Val::Int(vec![1, 2]));
        assert_eq!(decode(be, 3, 2, &[0, 1, 0, 2]), Val::Int(vec![1, 2]));
        assert_eq!(decode(le, 9, 1, &(-5i32).to_le_bytes()), Val::Int(vec![-5]));
        assert_eq!(decode(le, 10, 1, &[(-1i32).to_le_bytes(), 3i32.to_le_bytes()].concat()).real().map(|x| (x * 1000.0).round()), Some(-333.0));
        assert_eq!(decode(le, 5, 1, &[1u32.to_le_bytes(), 0u32.to_le_bytes()].concat()).real(), None);
        assert_eq!(decode(le, 11, 1, &1.5f32.to_le_bytes()).real(), Some(1.5));
        assert_eq!(decode(le, 12, 1, &2.25f64.to_le_bytes()).real(), Some(2.25));
        assert_eq!(decode(be, 12, 1, &2.25f64.to_be_bytes()).real(), Some(2.25));
        assert_eq!(decode(le, 2, 4, b"ab\0\0"), Val::Text("ab".into()));
        assert_eq!(plain(&Val::Bytes(vec![1, 2, 255])), "01 02 FF");
        assert_eq!(plain(&Val::Int((0..20).collect())), "(20 values)");
        assert_eq!(trim_num(0.25), "0.25");
        assert_eq!(google_hdrp(b"HDRP\x03abc"), Some(3));
        assert_eq!(google_hdrp(b"Apple iOS\0"), None);
    }

    #[test]
    fn describe_known_and_unknown_tags() {
        assert_eq!(describe(0x0002, &Val::Int(vec![0x12000])).1, "Phone");
        assert_eq!(describe(0x0100, &Val::Int(vec![1])).1, "On");
        assert_eq!(describe(0xA01A, &Val::Int(vec![230])).1, "23 mm");
        assert_eq!(describe(0xA018, &Val::Real(vec![0.01])).1, "1/100 s");
        assert_eq!(describe(0xA010, &Val::Int(vec![0, 0, 4080, 3060, 8, 6, 4072, 3054])).1, "Full 4080 x 3060, used 4064 x 3048 (from 8, 6)");
        let (name, value, note) = describe(0x000C, &Val::Int(vec![7]));
        assert_eq!((name.as_str(), value.as_str()), ("Tag 0x000C", "7"));
        assert!(!note.is_empty());
    }
}
