//! JPEG: segment walk, encoding facts (coding, subsampling, quality
//! estimate), APP payloads (EXIF, XMP, ICC chunks, MPF), comments, the end
//! of the primary image, multi-picture (MPF) entries and embedded videos.

use super::{be16, be32, find, Ctx, Field, Group, Parts};
use crate::catalog::human_size;

/// IJG / Annex K luminance quantization table, natural (row-major) order.
pub(crate) const STD_LUMA: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, //
    12, 12, 14, 19, 26, 58, 60, 55, //
    14, 13, 16, 24, 40, 57, 69, 56, //
    14, 17, 22, 29, 51, 87, 80, 62, //
    18, 22, 37, 56, 68, 109, 103, 77, //
    24, 35, 55, 64, 81, 104, 113, 92, //
    49, 64, 78, 87, 103, 121, 120, 101, //
    72, 92, 95, 98, 112, 100, 103, 99,
];

/// IJG / Annex K chrominance quantization table, natural order.
pub(crate) const STD_CHROMA: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, //
    18, 21, 26, 66, 99, 99, 99, 99, //
    24, 26, 56, 99, 99, 99, 99, 99, //
    47, 66, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99,
];

/// Zigzag index -> natural index (jpeg_natural_order).
pub(crate) const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, //
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, //
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, //
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Standard (Annex K.3) Huffman code-length counts: DC luma, DC chroma, AC luma, AC chroma.
const STD_BITS: [[u8; 16]; 4] = [
    [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
    [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
];

pub(crate) const XMP_PREFIX: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
pub(crate) const XMP_EXT_PREFIX: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
const ICC_PREFIX: &[u8] = b"ICC_PROFILE\0";
const ISO_GAINMAP_PREFIX: &[u8] = b"urn:iso:std:iso:ts:21496:-1";

/// IJG table scaled to quality `q` (jcparam.c `jpeg_quality_scaling` + `jpeg_add_quant_table`).
pub(crate) fn ijg_table(std: &[u16; 64], q: u32, baseline: bool) -> [u16; 64] {
    let q = q.clamp(1, 100);
    let scale = if q < 50 { 5000 / q } else { 200 - q * 2 };
    let max = if baseline { 255 } else { 32767 };
    let mut t = [0u16; 64];
    for i in 0..64 {
        let v = (std[i] as u32 * scale + 50) / 100;
        t[i] = v.clamp(1, max) as u16;
    }
    t
}

/// Best-matching IJG quality (1-100) for a luminance and optional chroma table
/// (natural order), and the mean relative mismatch at that quality (0 = exact).
pub(crate) fn estimate_quality(luma: &[u16; 64], chroma: Option<&[u16; 64]>) -> (u32, f64) {
    let baseline = luma.iter().chain(chroma.into_iter().flatten()).all(|&v| v <= 255);
    let mut best = (50u32, f64::MAX);
    for q in 1..=100u32 {
        let mut err = 0.0;
        let mut n = 0.0;
        let mut add = |actual: &[u16; 64], std: &[u16; 64]| {
            let exp = ijg_table(std, q, baseline);
            for i in 0..64 {
                let a = actual[i].max(1) as f64;
                let e = exp[i] as f64;
                err += (a - e).abs() / e;
                n += 1.0;
            }
        };
        add(luma, &STD_LUMA);
        if let Some(c) = chroma {
            add(c, &STD_CHROMA);
        }
        let mean = err / n;
        if mean < best.1 - 1e-12 {
            best = (q, mean);
        }
    }
    best
}

/// Chroma subsampling name from the luma and chroma sampling factors.
pub(crate) fn subsampling_name(y: (u8, u8), c: (u8, u8)) -> Option<&'static str> {
    if c.0 == 0 || c.1 == 0 || y.0 % c.0 != 0 || y.1 % c.1 != 0 {
        return None;
    }
    Some(match (y.0 / c.0, y.1 / c.1) {
        (1, 1) => "4:4:4",
        (2, 1) => "4:2:2",
        (2, 2) => "4:2:0",
        (1, 2) => "4:4:0",
        (4, 1) => "4:1:1",
        (4, 2) => "4:1:0",
        _ => return None,
    })
}

struct Sof {
    marker: u8,
    precision: u8,
    width: u16,
    height: u16,
    /// (id, h, v, quant table)
    comps: Vec<(u8, u8, u8, u8)>,
}

fn sof_name(m: u8) -> &'static str {
    match m {
        0xC0 => "Baseline (sequential, Huffman)",
        0xC1 => "Extended sequential (Huffman)",
        0xC2 => "Progressive (Huffman)",
        0xC3 => "Lossless (Huffman)",
        0xC5 => "Differential sequential (hierarchical, Huffman)",
        0xC6 => "Differential progressive (hierarchical, Huffman)",
        0xC7 => "Differential lossless (hierarchical, Huffman)",
        0xC9 => "Extended sequential (arithmetic)",
        0xCA => "Progressive (arithmetic)",
        0xCB => "Lossless (arithmetic)",
        0xCD => "Differential sequential (arithmetic)",
        0xCE => "Differential progressive (arithmetic)",
        _ => "Differential lossless (arithmetic)",
    }
}

fn marker_name(m: u8) -> String {
    match m {
        0xC4 => "DHT".into(),
        0xDA => "SOS".into(),
        0xDB => "DQT".into(),
        0xDD => "DRI".into(),
        0xFE => "COM".into(),
        0xE0..=0xEF => format!("APP{}", m - 0xE0),
        0xC0..=0xCF => "SOF".into(),
        _ => format!("0x{m:02X}"),
    }
}

/// Walk the JPEG segments up to the primary image's EOI.
pub(crate) fn parse(b: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let n = b.len();
    let mut pos = 2usize;
    let mut sof: Option<Sof> = None;
    let mut dqt: [Option<[u16; 64]>; 4] = [None; 4];
    let mut dqt_16bit = false;
    let mut dht: Vec<(u8, u8, [u8; 16])> = Vec::new();
    let mut restart: Option<u16> = None;
    let mut scans = 0u32;
    let mut icc_chunks: Vec<(u8, u8, &[u8])> = Vec::new();
    let mut jfif: Option<String> = None;
    let mut adobe: Option<u8> = None;
    let mut c2pa = false;
    let mut other_app: std::collections::BTreeMap<u8, usize> = Default::default();
    let mut stray = false;
    let mut exif_count = 0;
    loop {
        if pos + 1 >= n {
            warn.push("The JPEG ends without an end-of-image marker (truncated file?).".into());
            break;
        }
        if b[pos] != 0xFF {
            if !stray {
                warn.push(format!("Unexpected bytes between JPEG segments at offset {pos}."));
                stray = true;
            }
            match b[pos..].iter().position(|&c| c == 0xFF) {
                Some(k) => {
                    pos += k;
                    continue;
                }
                None => {
                    warn.push("The JPEG ends without an end-of-image marker (truncated file?).".into());
                    break;
                }
            }
        }
        let m = b[pos + 1];
        if m == 0xFF {
            pos += 1;
            continue;
        }
        if m == 0xD9 {
            p.image_end = Some(pos + 2);
            break;
        }
        if m == 0xD8 || (0xD0..=0xD7).contains(&m) || m == 0x01 || m == 0x00 {
            pos += 2;
            continue;
        }
        let Some(len) = be16(b, pos + 2).map(|l| l as usize) else {
            warn.push(format!("The {} segment is truncated.", marker_name(m)));
            break;
        };
        if len < 2 || pos + 2 + len > n {
            warn.push(format!("The {} segment is truncated (it claims {len} bytes).", marker_name(m)));
            break;
        }
        let d = &b[pos + 4..pos + 2 + len];
        match m {
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                if sof.is_none() && d.len() >= 6 {
                    let nf = d[5] as usize;
                    let comps = (0..nf)
                        .filter_map(|i| d.get(6 + 3 * i..9 + 3 * i))
                        .map(|c| (c[0], c[1] >> 4, c[1] & 15, c[2]))
                        .collect();
                    sof = Some(Sof {
                        marker: m,
                        precision: d[0],
                        height: be16(d, 1).unwrap_or(0),
                        width: be16(d, 3).unwrap_or(0),
                        comps,
                    });
                }
            }
            0xC4 => {
                let mut i = 0;
                while i + 17 <= d.len() {
                    let mut bits = [0u8; 16];
                    bits.copy_from_slice(&d[i + 1..i + 17]);
                    dht.push((d[i] >> 4, d[i] & 15, bits));
                    i += 17 + bits.iter().map(|&x| x as usize).sum::<usize>();
                }
            }
            0xDB => {
                let mut i = 0;
                while i < d.len() {
                    let pq = d[i] >> 4;
                    let tq = (d[i] & 15) as usize;
                    let size = if pq == 0 { 64 } else { 128 };
                    let Some(vals) = d.get(i + 1..i + 1 + size) else {
                        warn.push("A JPEG quantization table (DQT) is truncated.".into());
                        break;
                    };
                    let mut t = [0u16; 64];
                    for k in 0..64 {
                        let v = if pq == 0 { vals[k] as u16 } else { u16::from_be_bytes([vals[2 * k], vals[2 * k + 1]]) };
                        t[ZIGZAG[k]] = v;
                    }
                    if pq != 0 {
                        dqt_16bit = true;
                    }
                    if tq < 4 {
                        dqt[tq] = Some(t);
                    }
                    i += 1 + size;
                }
            }
            0xDD => restart = be16(d, 0),
            0xE0 => {
                if d.starts_with(b"JFIF\0") && d.len() >= 7 {
                    jfif = Some(format!("{}.{:02}", d[5], d[6]));
                }
            }
            0xE1 => {
                if d.starts_with(b"Exif\0\0") {
                    exif_count += 1;
                    if p.exif_raw.is_none() {
                        p.exif_raw = Some(d[6..].to_vec());
                    }
                } else if d.starts_with(XMP_PREFIX) {
                    if p.xmp.is_none() {
                        p.xmp = Some(String::from_utf8_lossy(&d[XMP_PREFIX.len()..]).into_owned());
                    }
                } else if d.starts_with(XMP_EXT_PREFIX) {
                    let full = be32(d, XMP_EXT_PREFIX.len() + 32).unwrap_or(0) as u64;
                    let (k, len) = p.xmp_extended.unwrap_or((0, 0));
                    p.xmp_extended = Some((k + 1, len.max(full)));
                } else {
                    *other_app.entry(1).or_default() += 1;
                }
            }
            0xE2 => {
                if d.starts_with(ICC_PREFIX) && d.len() >= 14 {
                    icc_chunks.push((d[12], d[13], &d[14..]));
                } else if d.starts_with(b"MPF\0") {
                    if p.mpf.is_none() {
                        p.mpf = Some(d[4..].to_vec());
                    }
                } else if d.starts_with(ISO_GAINMAP_PREFIX) {
                    p.iso_gain_map = true;
                } else {
                    *other_app.entry(2).or_default() += 1;
                }
            }
            0xEB => {
                if find(&d[..d.len().min(64)], b"c2pa").is_some() || find(&d[..d.len().min(64)], b"jumb").is_some() {
                    c2pa = true;
                } else {
                    *other_app.entry(11).or_default() += 1;
                }
            }
            0xEE => {
                if d.starts_with(b"Adobe") && d.len() >= 12 {
                    adobe = Some(d[11]);
                }
            }
            0xFE => {
                let t = String::from_utf8_lossy(d);
                let t = t.trim_matches(|c: char| c == '\0' || c.is_whitespace());
                if !t.is_empty() {
                    p.comments.push(super::clip(t, 2000));
                }
            }
            0xE3..=0xEF => *other_app.entry(m - 0xE0).or_default() += 1,
            _ => {}
        }
        pos += 2 + len;
        if m == 0xDA {
            scans += 1;
            // Skip the entropy-coded data up to the next real marker.
            let mut q = pos;
            let next = loop {
                let Some(k) = b.get(q..).and_then(|s| s.iter().position(|&c| c == 0xFF)) else { break None };
                let at = q + k;
                let Some(&nx) = b.get(at + 1) else { break None };
                if nx == 0xFF {
                    q = at + 1;
                } else if nx == 0x00 || (0xD0..=0xD7).contains(&nx) {
                    q = at + 2;
                } else {
                    break Some(at);
                }
            };
            match next {
                Some(at) => pos = at,
                None => {
                    warn.push("The JPEG image data is truncated (no end-of-image marker).".into());
                    break;
                }
            }
        }
    }
    if exif_count > 1 {
        warn.push(format!("The JPEG has {exif_count} EXIF segments; only the first is shown."));
    }

    // ICC profile: chunks in sequence order.
    if !icc_chunks.is_empty() {
        icc_chunks.sort_by_key(|c| c.0);
        let count = icc_chunks[0].1 as usize;
        let seqs: Vec<usize> = icc_chunks.iter().map(|c| c.0 as usize).collect();
        if count != icc_chunks.len() || seqs.iter().enumerate().any(|(i, &s)| s != i + 1) {
            warn.push(format!(
                "The ICC profile is split over {} chunks but {count} were announced (chunks {seqs:?}).",
                icc_chunks.len()
            ));
        }
        p.icc = Some(icc_chunks.iter().flat_map(|c| c.2.iter().copied()).collect());
    }

    // Encoding group.
    let e = &mut p.encoding;
    if let Some(s) = &sof {
        if s.width > 0 && s.height > 0 {
            p.dims = Some((s.width as u32, s.height as u32));
            p.dims_note = "Stored size from the JPEG frame header (before EXIF rotation).".into();
        }
        let progressive = matches!(s.marker, 0xC2 | 0xC6 | 0xCA | 0xCE);
        let mut note = String::new();
        if progressive {
            note = "The image is stored in several passes of increasing detail (it can show blurry first on slow connections). Usually slightly smaller than baseline.".into();
        } else if s.marker == 0xC0 {
            note = "The most common JPEG kind: one top-to-bottom pass with Huffman coding.".into();
        }
        e.push(Field::new("Coding", sof_name(s.marker), note));
        e.push(Field::new("Bit depth", format!("{} bits per sample", s.precision), ""));
        let nf = s.comps.len();
        let ids: Vec<u8> = s.comps.iter().map(|c| c.0).collect();
        let comps = match nf {
            1 => "1 (grayscale)".to_string(),
            3 if adobe == Some(0) || ids == b"RGB" => "3 (RGB, no YCbCr conversion)".to_string(),
            3 => "3 (YCbCr)".to_string(),
            4 if adobe == Some(2) => "4 (YCCK)".to_string(),
            4 => "4 (CMYK)".to_string(),
            k => k.to_string(),
        };
        e.push(Field::new("Components", comps, ""));
        if nf >= 3 {
            let y = (s.comps[0].1, s.comps[0].2);
            let c1 = (s.comps[1].1, s.comps[1].2);
            let c2 = (s.comps[2].1, s.comps[2].2);
            let factors = s.comps.iter().map(|c| format!("{}x{}", c.1, c.2)).collect::<Vec<_>>().join(", ");
            let name = if c1 == c2 { subsampling_name(y, c1) } else { None };
            let value = match name {
                Some(n) => format!("{n} ({factors})"),
                None => format!("Unusual ({factors})"),
            };
            e.push(Field::new(
                "Chroma subsampling",
                value,
                "How much color resolution is kept. 4:4:4 keeps full color detail; 4:2:2 halves it horizontally; 4:2:0 halves it in both directions (normal for photos, can blur fine colored edges and text).",
            ));
        } else if nf == 1 {
            e.push(Field::new("Chroma subsampling", "None (grayscale)", ""));
        }
        if matches!(s.marker, 0xC2 | 0xC6 | 0xCA | 0xCE) || scans > 1 {
            e.push(Field::new("Scans", scans.to_string(), "Number of passes over the image data."));
        }

        // Quality estimate: the luma table is the one component 1 uses, chroma the one component 2 uses.
        let luma_id = s.comps.first().map(|c| c.3 as usize).unwrap_or(0).min(3);
        let chroma_id = s.comps.get(1).map(|c| c.3 as usize).unwrap_or(1).min(3);
        if let Some(luma) = dqt[luma_id] {
            let chroma = if nf >= 3 && chroma_id != luma_id { dqt[chroma_id] } else { None };
            let (q, mismatch) = estimate_quality(&luma, chroma.as_ref());
            let mut note = "Estimated from the quantization tables; non-IJG encoders (phones) use their own tables, so read it as equivalent quality.".to_string();
            if mismatch < 1e-9 {
                note.push_str(" The tables match the standard IJG (libjpeg) tables exactly.");
            } else if mismatch > 0.25 {
                note.push_str(" The tables differ a lot from the standard ones, so the estimate is rough.");
            }
            e.push(Field::new("JPEG quality", format!("about {q}"), note));
            let stats = |t: &[u16; 64]| {
                let mean = t.iter().map(|&v| v as f64).sum::<f64>() / 64.0;
                format!("DC {}, average {:.1}", t[0], mean)
            };
            e.push(Field::new(
                "Quantization (luma)",
                stats(&luma),
                "Divisors applied to brightness detail before rounding: smaller numbers keep more detail (1 is lossless-like). Comparable across encoders.",
            ));
            if let Some(c) = chroma {
                e.push(Field::new("Quantization (chroma)", stats(&c), "The same for color detail."));
            }
            if dqt_16bit {
                e.push(Field::new("Quantization precision", "16-bit tables", ""));
            }
        }
        if s.marker <= 0xC7 && s.marker != 0xC3 {
            let value = if dht.is_empty() {
                "Standard (none stored; Motion JPEG style)".to_string()
            } else if dht.iter().all(|(class, _, bits)| STD_BITS.iter().skip(if *class == 0 { 0 } else { 2 }).take(2).any(|s| s == bits)) {
                "Standard (Annex K tables)".to_string()
            } else {
                "Optimized for this image".to_string()
            };
            e.push(Field::new(
                "Huffman tables",
                value,
                "Optimized tables are computed per image and save a few percent of file size; standard tables are the generic ones from the JPEG specification.",
            ));
        }
    } else if p.image_end.is_some() || !warn.is_empty() {
        warn.push("No JPEG frame header (SOF) was found.".into());
    }
    e.push(Field::new(
        "Restart interval",
        match restart {
            Some(r) if r > 0 => format!("Every {r} MCUs"),
            _ => "None".into(),
        },
        "Restart markers let a decoder resynchronize after damaged data and allow parallel decoding.",
    ));
    if let Some(v) = jfif {
        e.push(Field::new("JFIF", format!("Version {v}"), ""));
    }
    if let Some(t) = adobe {
        let v = match t {
            0 => "Adobe APP14, no color transform",
            1 => "Adobe APP14, YCbCr",
            2 => "Adobe APP14, YCCK",
            _ => "Adobe APP14",
        };
        e.push(Field::new("Adobe marker", v, ""));
    }
    if c2pa {
        e.push(Field::new(
            "Content credentials (C2PA)",
            "Present (APP11 JUMBF)",
            "Signed provenance data that records which device or software made or edited the image.",
        ));
    }
    if !other_app.is_empty() {
        let v = other_app.iter().map(|(k, c)| if *c > 1 { format!("APP{k} x{c}") } else { format!("APP{k}") }).collect::<Vec<_>>().join(", ");
        e.push(Field::new("Other APP segments", v, "Vendor-specific segments that are not decoded here."));
    }
    if let Some(end) = p.image_end {
        if end < n {
            e.push(Field::new(
                "Data after the image",
                format!("{} after the JPEG end marker", human_size((n - end) as u64)),
                "Extra images (multi-picture, gain map, depth), a motion photo video or a vendor trailer.",
            ));
        }
    }
}

/// One multi-picture (MPF) index entry.
#[derive(Clone, Debug)]
pub(crate) struct MpImage {
    /// Attribute: flags in the top byte, image type code in the low 24 bits.
    pub attr: u32,
    pub size: u32,
    pub offset: u32,
}

impl MpImage {
    pub fn type_code(&self) -> u32 {
        self.attr & 0x00FF_FFFF
    }
}

/// Parse the MP index IFD of an MPF APP2 payload (after "MPF\0").
pub(crate) fn parse_mpf(d: &[u8], warn: &mut Vec<String>) -> Vec<MpImage> {
    let le = match d.get(0..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => {
            warn.push("The multi-picture (MPF) segment has no valid header.".into());
            return Vec::new();
        }
    };
    let u16_at = |o: usize| if le { super::le16(d, o) } else { be16(d, o) };
    let u32_at = |o: usize| if le { super::le32(d, o) } else { be32(d, o) };
    let Some(ifd) = u32_at(4).map(|v| v as usize) else { return Vec::new() };
    let Some(count) = u16_at(ifd) else {
        warn.push("The multi-picture (MPF) index is truncated.".into());
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..count as usize {
        let e = ifd + 2 + 12 * i;
        let (Some(tag), Some(cnt), Some(val)) = (u16_at(e), u32_at(e + 4), u32_at(e + 8)) else {
            warn.push("The multi-picture (MPF) index is truncated.".into());
            break;
        };
        if tag != 0xB002 {
            continue;
        }
        let cnt = cnt as usize;
        let base = if cnt <= 4 { e + 8 } else { val as usize };
        let n = (cnt / 16).min(64);
        for k in 0..n {
            let o = base + 16 * k;
            match (u32_at(o), u32_at(o + 4), u32_at(o + 8)) {
                (Some(attr), Some(size), Some(offset)) => out.push(MpImage { attr, size, offset }),
                _ => {
                    warn.push("A multi-picture (MPF) entry is truncated.".into());
                    break;
                }
            }
        }
    }
    out
}

fn mp_type_name(code: u32) -> &'static str {
    match code {
        0x030000 => "Baseline primary image",
        0x010001 => "Large thumbnail (VGA)",
        0x010002 => "Large thumbnail (full HD)",
        0x020001 => "Multi-frame panorama",
        0x020002 => "Multi-frame disparity (depth)",
        0x020003 => "Multi-angle",
        0x000000 => "Undefined",
        _ => "Other",
    }
}

/// "Multi-picture (MPF)" group: MPF entries and GContainer items from XMP.
pub(crate) fn mpf_group(ctx: &Ctx) -> Group {
    let mut f = Vec::new();
    let gain_hint = ctx.xhas_ns("hdrgm:") || !ctx.items.is_empty() || ctx.iso_gain_map;
    if !ctx.mpf.is_empty() {
        f.push(Field::new(
            "Images",
            ctx.mpf.len().to_string(),
            "Number of images in the file according to the multi-picture (MPF) index.",
        ));
        for (i, im) in ctx.mpf.iter().enumerate() {
            let code = im.type_code();
            let mut name = mp_type_name(code).to_string();
            if code == 0 && i > 0 && gain_hint {
                name.push_str(" (probably a gain map)");
            } else if name == "Other" {
                name = format!("Type 0x{code:06X}");
            }
            let mut flags = Vec::new();
            if im.attr & 0x2000_0000 != 0 {
                flags.push("representative image");
            }
            if im.attr & 0x8000_0000 != 0 {
                flags.push("has dependent images");
            }
            if im.attr & 0x4000_0000 != 0 {
                flags.push("dependent image");
            }
            let mut note = if code == 0 && i > 0 && gain_hint {
                "Ultra HDR and similar files store their HDR gain map as an extra image with an undefined MPF type.".to_string()
            } else {
                flags.join(", ")
            };
            if im.offset > 0 {
                let at = format!("Stored {} bytes after the MPF header.", super::thousands(im.offset as u64));
                note = if note.is_empty() { at } else { format!("{note} {at}") };
            }
            f.push(Field::new(format!("Image {}", i + 1), format!("{name}, {}", kb(im.size as u64)), note));
        }
    }
    for (i, item) in ctx.items.iter().enumerate() {
        let get = |suffix: &str| item.iter().find(|(k, _)| k.ends_with(suffix)).map(|(_, v)| v.as_str());
        let sem = get(":Semantic").unwrap_or("?");
        let mut v = sem.to_string();
        if let Some(m) = get(":Mime") {
            v.push_str(&format!(", {m}"));
        }
        if let Some(len) = get(":Length").and_then(|l| l.trim().parse::<u64>().ok()) {
            if len > 0 {
                v.push_str(&format!(", {}", kb(len)));
            }
        }
        let note = match sem {
            "Primary" => "The main image.",
            "GainMap" => "HDR gain map: lets HDR screens show brighter highlights (Ultra HDR).",
            "Depth" => "Depth map used for portrait background blur.",
            "MotionPhoto" => "The video of a motion photo, stored after the image.",
            "Confidence" => "Confidence map for the depth map.",
            _ => "",
        };
        f.push(Field::new(format!("Container item {}", i + 1), v, note));
    }
    Group { name: "Multi-picture (MPF)".into(), fields: f }
}

fn kb(n: u64) -> String {
    format!("{} KB", super::thousands(n.div_ceil(1024)))
}

/// An embedded video after the primary JPEG (Samsung/Google motion photos):
/// returns its size in bytes when an "ftyp" box follows the end of the image.
pub(crate) fn embedded_video(b: &[u8], end: usize, sef: Option<&super::samsung::Sef>) -> Option<usize> {
    let tail = b.get(end..)?;
    let mut from = 0;
    let start = loop {
        let k = find(&tail[from..], b"ftyp")?;
        let at = end + from + k;
        from += k + 4;
        if at < end + 4 {
            continue;
        }
        let size = be32(b, at - 4)? as usize;
        let brand_ok = b.get(at + 4..at + 8).is_some_and(|s| s.iter().all(|c| c.is_ascii_alphanumeric() || *c == b' '));
        if (8..=512).contains(&size) && brand_ok {
            break at - 4;
        }
    };
    // Inside a Samsung trailer entry: the video ends with that entry's payload.
    if let Some(s) = sef {
        if let Some(e) = s.entries.iter().find(|e| start >= e.start && start < e.start + e.len) {
            return Some(e.start + e.len - start);
        }
        if s.data_start > start {
            return Some(s.data_start - start);
        }
    }
    Some(b.len() - start)
}
