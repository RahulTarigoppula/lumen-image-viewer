//! Samsung "SEF" trailer appended to JPEGs by Samsung phones.
//!
//! Layout (as read by ExifTool's Samsung.pm `ProcessSamsung`): the file ends
//! with a 4-byte little-endian length L and "SEFT". The directory starts at
//! (file end - 8 - L) with "SEFH", u32 version, u32 count, then `count`
//! 12-byte entries: u16 padding, u16 type, u32 offset (distance from the
//! directory start backwards to the entry's data), u32 size. Each data block
//! starts with u16 padding, u16 type, u32 name length, the ASCII name, then
//! the payload.

use super::{le16, le32, printable, Field, Group};

/// One trailer entry.
#[derive(Clone, Debug)]
pub(crate) struct SefEntry {
    pub name: String,
    pub kind: u16,
    /// Absolute offset of the payload (after the name).
    pub start: usize,
    /// Payload length.
    pub len: usize,
}

/// A parsed Samsung trailer.
#[derive(Clone, Debug)]
pub(crate) struct Sef {
    pub version: u32,
    pub entries: Vec<SefEntry>,
    /// Absolute offset of the first entry's data block (where the trailer region starts).
    pub data_start: usize,
}

/// Parse the trailer when the file ends with "SEFT". Inconsistencies add a
/// warning and stop (entries read so far are kept).
pub(crate) fn parse(b: &[u8], warn: &mut Vec<String>) -> Option<Sef> {
    let n = b.len();
    if n < 8 || &b[n - 4..] != b"SEFT" {
        return None;
    }
    let l = le32(b, n - 8)? as usize;
    let Some(dir) = (n - 8).checked_sub(l) else {
        warn.push("Samsung trailer: its length points before the start of the file.".into());
        return None;
    };
    if l < 12 || b.get(dir..dir + 4) != Some(&b"SEFH"[..]) {
        warn.push("Samsung trailer: the directory header (SEFH) is missing.".into());
        return None;
    }
    let version = le32(b, dir + 4)?;
    let count = le32(b, dir + 8)? as usize;
    if count > 4096 || 12 + count * 12 > l {
        warn.push(format!("Samsung trailer: {count} entries do not fit in its {l}-byte directory."));
        return None;
    }
    let mut entries = Vec::new();
    let mut data_start = dir;
    for i in 0..count {
        let e = dir + 12 + 12 * i;
        let (Some(kind), Some(off), Some(size)) = (le16(b, e + 2), le32(b, e + 4), le32(b, e + 8)) else { break };
        let (off, size) = (off as usize, size as usize);
        let Some(start) = dir.checked_sub(off) else {
            warn.push(format!("Samsung trailer: entry {} points before the start of the file.", i + 1));
            break;
        };
        let end = start.checked_add(size).unwrap_or(usize::MAX);
        if end > dir || size < 8 {
            warn.push(format!("Samsung trailer: entry {} overlaps the directory or is too small.", i + 1));
            break;
        }
        let blk = &b[start..end];
        let Some(name_len) = le32(blk, 4).map(|v| v as usize) else { break };
        if name_len > size - 8 {
            warn.push(format!("Samsung trailer: entry {} has a name longer than its data.", i + 1));
            break;
        }
        let name = String::from_utf8_lossy(&blk[8..8 + name_len]).trim_matches('\0').trim().to_string();
        data_start = data_start.min(start);
        entries.push(SefEntry { name, kind, start: start + 8 + name_len, len: size - 8 - name_len });
    }
    Some(Sef { version, entries, data_start })
}

/// Plain-language explanation of a known entry name.
pub(crate) fn note_for(name: &str) -> &'static str {
    let l = name.to_ascii_lowercase();
    let has = |s: &str| l.contains(s);
    if has("image_utc_data") {
        "Capture time in milliseconds since 1970 (UTC)."
    } else if has("mcc_data") {
        "Mobile country code of the network when the photo was taken (for example 310 = USA, 404 = India, 450 = South Korea)."
    } else if has("camera_capture_mode_info") {
        "Shooting mode code of the Samsung camera app."
    } else if has("hdr") {
        "HDR (multi-frame high dynamic range) processing information."
    } else if has("dual_shot") || has("live_focus") || has("dualshot") {
        "Portrait (Live focus) data: background blur settings and depth map."
    } else if has("motion_photo") || has("motionphoto") {
        "Motion photo: a short video stored with the photo."
    } else if has("burst") {
        "The photo is part of a burst sequence."
    } else if has("panorama") {
        "Panorama capture data."
    } else if has("food") {
        "Shot in Food mode."
    } else if has("pro_mode") {
        "Shot in Pro (manual) mode."
    } else if has("color_display_p3") {
        "The image is tagged for the Display P3 wide color gamut."
    } else if has("original_path_hash") {
        "Gallery bookkeeping: hash of the original file path."
    } else if has("single_take") || has("singletake") {
        "Captured with Single Take (several photos and clips from one press)."
    } else if has("night") {
        "Night mode information."
    } else if has("remaster") {
        "The photo was enhanced with the Gallery remaster feature."
    } else {
        ""
    }
}

/// "Samsung trailer" group: every entry with its payload as text or size.
pub(crate) fn group(s: &Sef, b: &[u8]) -> Group {
    let mut f = vec![Field::new(
        "Trailer",
        format!("SEF version {}, {} entr{}", s.version, s.entries.len(), if s.entries.len() == 1 { "y" } else { "ies" }),
        "Samsung phones append this block after the JPEG with camera mode and feature data.",
    )];
    for e in &s.entries {
        let payload = b.get(e.start..e.start + e.len).unwrap_or(&[]);
        let mut value = match printable(payload) {
            Some(t) if !t.is_empty() => super::clip(&t, 300),
            Some(_) => "(empty)".to_string(),
            None => format!("(binary, {} bytes)", super::thousands(e.len as u64)),
        };
        if e.name.eq_ignore_ascii_case("Image_UTC_Data") {
            if let Ok(ms) = value.trim().parse::<i64>() {
                if let Some(t) = chrono::DateTime::from_timestamp_millis(ms) {
                    value = format!("{value} ({} UTC)", t.format("%Y-%m-%d %H:%M:%S"));
                }
            }
        }
        let name = if e.name.is_empty() { format!("Type 0x{:04X}", e.kind) } else { e.name.clone() };
        f.push(Field::new(name, value, note_for(&e.name)));
    }
    Group { name: "Samsung trailer".into(), fields: f }
}
