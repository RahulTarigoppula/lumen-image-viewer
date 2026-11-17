//! EXIF tag groups, maker note summary (with a few robust Apple tags) and
//! typed accessors used by the Capture section.

use super::{Field, Group};
use exif::{Context, In, Tag, Value};

/// Parse raw TIFF-structured EXIF, keeping partial results.
pub(crate) fn read_raw(raw: Vec<u8>, whole_tiff: bool, warn: &mut Vec<String>) -> Option<exif::Exif> {
    let mut reader = exif::Reader::new();
    reader.continue_on_error(true);
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.read_raw(raw)));
    let Ok(read) = read else {
        warn.push("The EXIF reader failed on this file (it may be corrupt).".into());
        return None;
    };
    match read {
        Ok(e) => Some(e),
        Err(e) => {
            let mut first: Option<String> = None;
            let res = e.distill_partial_result(|errs| first = errs.first().map(|x| x.to_string()));
            match res {
                Ok(exif) => {
                    if let Some(f) = first {
                        warn.push(format!("Part of the EXIF data could not be read: {f}"));
                    }
                    Some(exif)
                }
                Err(e) => {
                    let what = if whole_tiff { "TIFF directory" } else { "EXIF data" };
                    warn.push(format!("The {what} could not be read: {e}"));
                    None
                }
            }
        }
    }
}

/// Typed access to primary-image EXIF fields.
pub(crate) struct Ex<'a>(pub &'a exif::Exif);

impl<'a> Ex<'a> {
    pub fn field(&self, tag: Tag) -> Option<&'a exif::Field> {
        self.0.get_field(tag, In::PRIMARY)
    }

    pub fn uint(&self, tag: Tag) -> Option<u32> {
        self.field(tag).and_then(|f| f.value.get_uint(0))
    }

    /// Unsigned ints at every index (Byte/Short/Long values).
    pub fn uints(&self, tag: Tag) -> Vec<u32> {
        self.field(tag).and_then(|f| f.value.iter_uint().map(|it| it.collect())).unwrap_or_default()
    }

    pub fn f64(&self, tag: Tag) -> Option<f64> {
        let f = self.field(tag)?;
        let v = match &f.value {
            Value::Rational(v) => v.first().map(|r| r.to_f64()),
            Value::SRational(v) => v.first().map(|r| r.to_f64()),
            Value::Float(v) => v.first().map(|&x| x as f64),
            Value::Double(v) => v.first().copied(),
            Value::SShort(v) => v.first().map(|&x| x as f64),
            Value::SLong(v) => v.first().map(|&x| x as f64),
            other => other.get_uint(0).map(|x| x as f64),
        }?;
        v.is_finite().then_some(v)
    }

    pub fn text(&self, tag: Tag) -> Option<String> {
        let f = self.field(tag)?;
        let s = match &f.value {
            Value::Ascii(v) => v
                .iter()
                .map(|x| String::from_utf8_lossy(x).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string())
                .find(|s| !s.is_empty())?,
            _ => f.display_value().to_string(),
        };
        let s = s.trim().trim_matches('"').trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

fn group_of(f: &exif::Field) -> &'static str {
    if f.ifd_num == In::PRIMARY {
        match f.tag.context() {
            Context::Gps => "GPS",
            Context::Exif => "EXIF",
            Context::Interop => "Interop",
            _ => "Image",
        }
    } else {
        "Thumbnail"
    }
}

/// Every EXIF tag grouped as "Image", "EXIF", "GPS", "Interop", "Thumbnail".
pub(crate) fn groups(e: &exif::Exif) -> Vec<Group> {
    let order = ["Image", "EXIF", "GPS", "Interop", "Thumbnail"];
    let mut groups: Vec<Group> = order.iter().map(|n| Group { name: n.to_string(), fields: Vec::new() }).collect();
    for f in e.fields() {
        let g = group_of(f);
        let name = if f.tag.description().is_some() {
            f.tag.to_string()
        } else {
            format!("Tag 0x{:04X}", f.tag.number())
        };
        let value = field_value(e, f);
        let idx = order.iter().position(|n| *n == g).unwrap_or(0);
        groups[idx].fields.push(Field::new(name, value, tag_note(f.tag)));
    }
    groups.retain(|g| !g.fields.is_empty());
    groups
}

fn field_value(e: &exif::Exif, f: &exif::Field) -> String {
    let binary_len = match &f.value {
        Value::Undefined(v, _) => Some(v.len()),
        Value::Byte(v) => Some(v.len()),
        _ => None,
    };
    if f.tag == Tag::MakerNote {
        return format!("({} bytes, see Maker note)", super::thousands(binary_len.unwrap_or(0) as u64));
    }
    if let (Some(n), Value::Undefined(v, _)) = (binary_len, &f.value) {
        // Text stored as UNDEFINED (UserComment) reads better as text.
        if f.tag == Tag::UserComment && v.len() > 8 {
            if let Some(t) = super::printable(&v[8..]) {
                return super::clip(&t, 300);
            }
        }
        if n > 64 {
            return format!("(binary data, {} bytes)", super::thousands(n as u64));
        }
    }
    if let (Some(n), Value::Byte(_)) = (binary_len, &f.value) {
        if n > 64 {
            return format!("(binary data, {} bytes)", super::thousands(n as u64));
        }
    }
    let count = match &f.value {
        Value::Short(v) => v.len(),
        Value::Long(v) => v.len(),
        Value::Rational(v) => v.len(),
        Value::SRational(v) => v.len(),
        Value::Ascii(v) => v.iter().map(|x| x.len()).sum::<usize>() / 16,
        _ => 0,
    };
    if count > 256 {
        return format!("({} values)", super::thousands(count as u64));
    }
    let s = f.display_value().with_unit(e).to_string();
    let s = s.trim().trim_matches('"').trim();
    super::clip(s, 300)
}

fn tag_note(t: Tag) -> &'static str {
    match t {
        Tag::ExposureTime => "How long the shutter was open.",
        Tag::FNumber => "Lens aperture: smaller numbers let in more light and blur the background more.",
        Tag::PhotographicSensitivity => "ISO sensitivity: higher values brighten dark scenes but add noise.",
        Tag::ExposureBiasValue => "Exposure compensation dialed in by the user or the camera.",
        Tag::FocalLength => "Real focal length of the lens (phone lenses are a few mm).",
        Tag::FocalLengthIn35mmFilm => "Field of view expressed as a full-frame (35 mm) focal length: about 13 mm ultra-wide, 24-28 mm main camera, 50 mm and up telephoto.",
        Tag::DigitalZoomRatio => "Digital zoom: 0 or 1 means none.",
        Tag::ColorSpace => "sRGB, or Uncalibrated when a wider gamut (Display P3, Adobe RGB) is described by the ICC profile.",
        Tag::Orientation => "How the viewer must rotate or flip the stored pixels.",
        Tag::CompositeImage => "Whether the image was merged from several frames (EXIF 2.32).",
        Tag::SourceImageNumberOfCompositeImage => "Number of frames captured and used for a composite image.",
        Tag::SourceExposureTimesOfCompositeImage => "Exposure times of the frames merged into a composite image.",
        Tag::OffsetTimeOriginal | Tag::OffsetTime | Tag::OffsetTimeDigitized => "Time zone offset from UTC.",
        Tag::SubSecTimeOriginal | Tag::SubSecTime | Tag::SubSecTimeDigitized => "Fractions of a second for the matching date.",
        Tag::BrightnessValue => "Scene brightness measured by the camera (APEX units; higher is brighter).",
        Tag::ShutterSpeedValue => "Shutter speed in APEX units.",
        Tag::ApertureValue => "Aperture in APEX units.",
        Tag::MaxApertureValue => "Widest aperture of the lens in APEX units.",
        Tag::SubjectDistance => "Distance to the focused subject.",
        Tag::SubjectArea => "Location of the main subject (focus/face area) in the frame.",
        Tag::ImageUniqueID => "Identifier the camera assigned to this capture.",
        Tag::SensingMethod => "Sensor type.",
        Tag::SceneType => "1 = directly photographed.",
        Tag::ComponentsConfiguration => "Order of the color components.",
        Tag::YCbCrPositioning => "Position of chroma samples relative to luma.",
        Tag::GPSHPositioningError => "Estimated horizontal GPS error.",
        Tag::GPSImgDirection => "Compass direction the camera was pointing.",
        Tag::GPSDOP => "GPS dilution of precision (lower is more precise).",
        Tag::LensSpecification => "Focal length and aperture range of the lens.",
        Tag::ExifVersion => "EXIF standard version the camera follows.",
        _ => "",
    }
}

/// The few Apple maker note tags decoded robustly.
#[derive(Clone, Debug, Default)]
pub(crate) struct AppleNote {
    /// 0x000a HDRImageType: 3 = HDR image, 4 = original image.
    pub hdr_type: Option<i64>,
    /// 0x000b BurstUUID.
    pub burst: Option<String>,
    /// 0x0011 ContentIdentifier (pairs the photo with its Live Photo video).
    pub content_id: Option<String>,
    /// 0x0014 ImageCaptureType.
    pub capture_type: Option<i64>,
}

impl AppleNote {
    fn is_empty(&self) -> bool {
        self.hdr_type.is_none() && self.burst.is_none() && self.content_id.is_none() && self.capture_type.is_none()
    }
}

fn maker_note_bytes(e: &exif::Exif) -> Option<&[u8]> {
    match &e.get_field(Tag::MakerNote, In::PRIMARY)?.value {
        Value::Undefined(v, _) => Some(v),
        Value::Byte(v) => Some(v),
        _ => None,
    }
}

/// Decode a few well-known tags of an Apple maker note ("Apple iOS\0",
/// u16 version, "MM", big-endian IFD at offset 14; offsets relative to the
/// maker note start). Anything inconsistent yields an empty result.
pub(crate) fn apple_note(e: &exif::Exif) -> AppleNote {
    let mut out = AppleNote::default();
    let Some(m) = maker_note_bytes(e) else { return out };
    if !m.starts_with(b"Apple iOS\0") || m.get(12..14) != Some(&b"MM"[..]) {
        return out;
    }
    let be16 = |o: usize| super::be16(m, o);
    let be32 = |o: usize| super::be32(m, o);
    let Some(count) = be16(14) else { return out };
    for i in 0..count.min(512) as usize {
        let o = 16 + 12 * i;
        let (Some(tag), Some(typ), Some(cnt)) = (be16(o), be16(o + 2), be32(o + 4)) else { break };
        let cnt = cnt as usize;
        let int = || -> Option<i64> {
            match (typ, cnt) {
                (3, 1) => be16(o + 8).map(|v| v as i64),
                (8, 1) => be16(o + 8).map(|v| v as i16 as i64),
                (4, 1) => be32(o + 8).map(|v| v as i64),
                (9, 1) => be32(o + 8).map(|v| v as i32 as i64),
                _ => None,
            }
        };
        let ascii = || -> Option<String> {
            if typ != 2 || cnt == 0 || cnt > 256 {
                return None;
            }
            let data = if cnt <= 4 { m.get(o + 8..o + 8 + cnt)? } else { m.get(be32(o + 8)? as usize..)?.get(..cnt)? };
            let s = String::from_utf8_lossy(data).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string();
            (!s.is_empty() && s.chars().all(|c| c.is_ascii_graphic() || c == ' ')).then_some(s)
        };
        match tag {
            0x000a => out.hdr_type = int(),
            0x000b => out.burst = ascii(),
            0x0011 => out.content_id = ascii(),
            0x0014 => out.capture_type = int(),
            _ => {}
        }
    }
    out
}

pub(crate) fn apple_capture_type(v: i64) -> Option<&'static str> {
    Some(match v {
        1 => "ProRAW",
        2 => "Portrait",
        10 => "Photo",
        11 => "Manual focus",
        12 => "Scene",
        _ => return None,
    })
}

/// "Maker note" group: size, maker, recognizable format and Apple tags.
pub(crate) fn maker_group(e: &exif::Exif, apple: &AppleNote) -> Option<Group> {
    let m = maker_note_bytes(e)?;
    let ex = Ex(e);
    let mut f = Vec::new();
    if let Some(make) = ex.text(Tag::Make) {
        f.push(Field::new("Maker", make, "From the EXIF Make tag."));
    }
    f.push(Field::new(
        "Size",
        format!("{} bytes", super::thousands(m.len() as u64)),
        "Maker notes hold the manufacturer's private data (processing settings, focus, face detection...). Their layout is proprietary and is not decoded here.",
    ));
    let format = [
        (&b"Apple iOS\0"[..], "Apple"),
        (b"Nikon\0", "Nikon"),
        (b"OLYMPUS\0", "Olympus"),
        (b"OM SYSTEM", "OM System"),
        (b"FUJIFILM", "Fujifilm"),
        (b"SONY", "Sony"),
        (b"Panasonic", "Panasonic"),
        (b"LEICA", "Leica"),
        (b"PENTAX", "Pentax"),
        (b"AOC\0", "Pentax"),
        (b"HUAWEI", "Huawei"),
        (b"HDRP", "Google HDR+"),
    ];
    let format = sigs
        .iter()
        .find(|(sig, _)| m.starts_with(sig))
        .map(|(_, n)| *n);
    if let Some(fm) = format {
        f.push(Field::new("Format", format!("{fm} maker note"), "Recognized from the maker note header."));
    }
    if !apple.is_empty() {
        let note = "Decoded from the Apple maker note.";
        if let Some(v) = apple.hdr_type {
            let t = match v {
                3 => "HDR image".to_string(),
                4 => "Original (non-HDR) image".to_string(),
                v => format!("Type {v}"),
            };
            f.push(Field::new("HDR image type", t, note));
        }
        if let Some(v) = apple.capture_type {
            f.push(Field::new("Capture type", apple_capture_type(v).map_or(format!("Type {v}"), str::to_string), note));
        }
        if let Some(v) = &apple.burst {
            f.push(Field::new("Burst ID", v.clone(), "Shared by all photos of one burst."));
        }
        if let Some(v) = &apple.content_id {
            f.push(Field::new("Live Photo ID", v.clone(), "Pairs the photo with its Live Photo video (a separate .MOV file)."));
        }
    }
    Some(Group { name: "Maker note".into(), fields: f })
}
