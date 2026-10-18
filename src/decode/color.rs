//! ICC color management: convert embedded profiles (Adobe RGB, Display P3,
//! ProPhoto, …) to sRGB so colors look right on a standard display.

use image::RgbaImage;
use rayon::prelude::*;

pub enum Outcome {
    Converted(String),
    AlreadySrgb,
    Unsupported,
}

pub fn to_srgb_in_place(img: &mut RgbaImage, icc: &[u8]) -> Outcome {
    let Some(input) = qcms::Profile::new_from_slice(icc, false) else {
        return Outcome::Unsupported;
    };
    let name = profile_description(icc).unwrap_or_else(|| "Embedded ICC".into());
    if input.is_sRGB() || name.to_ascii_lowercase().contains("srgb") {
        return Outcome::AlreadySrgb;
    }
    let mut output = qcms::Profile::new_sRGB();
    output.precache_output_transform();
    let Some(transform) =
        qcms::Transform::new(&input, &output, qcms::DataType::RGBA8, qcms::Intent::Perceptual)
    else {
        return Outcome::Unsupported;
    };
    // Rows are independent, so transform in parallel chunks.
    let row_bytes = img.width() as usize * 4;
    let chunk = row_bytes * 64;
    img.as_mut()
        .par_chunks_mut(chunk.max(4))
        .for_each(|c| transform.apply(c));
    Outcome::Converted(name)
}

/// Extract the human-readable profile description ('desc' tag) from ICC bytes.
pub fn profile_description(icc: &[u8]) -> Option<String> {
    let be32 = |o: usize| -> Option<u32> {
        icc.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let count = be32(128)? as usize;
    for i in 0..count.min(256) {
        let base = 132 + i * 12;
        let sig = icc.get(base..base + 4)?;
        if sig != b"desc" {
            continue;
        }
        let off = be32(base + 4)? as usize;
        let len = be32(base + 8)? as usize;
        let tag = icc.get(off..off + len)?;
        let ty = tag.get(0..4)?;
        if ty == b"desc" {
            let n = u32::from_be_bytes(tag.get(8..12)?.try_into().ok()?) as usize;
            let s = tag.get(12..12 + n.saturating_sub(1))?;
            return Some(String::from_utf8_lossy(s).trim().to_string());
        } else if ty == b"mluc" {
            let rec_count = u32::from_be_bytes(tag.get(8..12)?.try_into().ok()?) as usize;
            if rec_count == 0 {
                return None;
            }
            let slen = u32::from_be_bytes(tag.get(20..24)?.try_into().ok()?) as usize;
            let soff = u32::from_be_bytes(tag.get(24..28)?.try_into().ok()?) as usize;
            let s = tag.get(soff..soff + slen)?;
            let utf16: Vec<u16> = s.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            return Some(String::from_utf16_lossy(&utf16).trim().to_string());
        }
    }
    None
}
