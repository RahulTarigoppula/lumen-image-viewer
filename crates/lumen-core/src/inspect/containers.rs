//! PNG, GIF, WebP, HEIF/AVIF, BMP, JPEG XL and TIFF container parsing.

use super::{be16, be32, cicp_text, le16, le32, Field, Parts};
use crate::catalog::human_size;

/// Largest decompressed size accepted for one zlib stream (zip-bomb guard).
const MAX_INFLATE: u64 = 64 << 20;

/// zlib-decompress, at most `MAX_INFLATE` bytes.
pub(crate) fn inflate(d: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(d).take(MAX_INFLATE).read_to_end(&mut out).ok()?;
    Some(out)
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// Keys AI image generators use for their settings.
fn is_generator_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "parameters" | "prompt" | "workflow" | "negative_prompt" | "dream" | "sd-metadata" | "invokeai_metadata" | "invokeai_graph" | "generation_data" | "comfyui" | "novelai" | "aesthetic_score"
    )
}

fn push_text(p: &mut Parts, key: &str, text: &str, how: &str) {
    let gen = is_generator_key(key);
    let note = if gen {
        format!("Image generator settings (prompt, model, sampler, seed...), stored in a PNG {how} chunk.")
    } else {
        format!("PNG {how} text chunk.")
    };
    let value = super::clip(text.trim(), 2000);
    p.png_text.push(Field::new(if key.is_empty() { "(no key)".to_string() } else { key.to_string() }, value, note));
}

/// PNG / APNG.
pub(crate) fn png(b: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let n = b.len();
    let mut pos = 8;
    let mut chunks: Vec<(String, usize)> = Vec::new();
    let mut ihdr: Option<(u8, u8, u8)> = None;
    let mut gamma: Option<u32> = None;
    let mut srgb: Option<u8> = None;
    let mut chrm = false;
    let mut iccp_name: Option<String> = None;
    let mut actl: Option<(u32, u32)> = None;
    let mut fctl = 0u32;
    let mut phys: Option<(u32, u32, u8)> = None;
    let mut time: Option<String> = None;
    let mut end: Option<usize> = None;
    loop {
        if pos + 8 > n {
            warn.push("The PNG ends without an IEND chunk (truncated file?).".into());
            break;
        }
        let len = be32(b, pos).unwrap_or(0) as usize;
        let ty = &b[pos + 4..pos + 8];
        let tyname = latin1(ty);
        let Some(data_end) = (pos + 8).checked_add(len).filter(|&e| e <= n) else {
            warn.push(format!("The PNG {tyname} chunk is truncated."));
            break;
        };
        let d = &b[pos + 8..data_end];
        if chunks.last().is_some_and(|(t, _)| *t == tyname) {
            if let Some(last) = chunks.last_mut() {
                last.1 += 1;
            }
        } else {
            chunks.push((tyname.clone(), 1));
        }
        match ty {
            b"IHDR" if d.len() >= 13 => {
                p.dims = Some((be32(d, 0).unwrap_or(0), be32(d, 4).unwrap_or(0)));
                ihdr = Some((d[8], d[9], d[12]));
            }
            b"gAMA" => gamma = be32(d, 0),
            b"sRGB" => srgb = d.first().copied(),
            b"cHRM" => chrm = true,
            b"iCCP" => {
                if let Some(z) = d.iter().position(|&c| c == 0) {
                    iccp_name = Some(latin1(&d[..z]));
                    match d.get(z + 2..).and_then(inflate) {
                        Some(icc) => p.icc = Some(icc),
                        None => warn.push("The PNG ICC profile (iCCP) could not be decompressed.".into()),
                    }
                }
            }
            b"eXIf" => {
                let raw = d.strip_prefix(b"Exif\0\0").unwrap_or(d);
                p.exif_raw = Some(raw.to_vec());
            }
            b"tEXt" => {
                let z = d.iter().position(|&c| c == 0).unwrap_or(d.len());
                let key = latin1(&d[..z]);
                let text = latin1(d.get(z + 1..).unwrap_or(&[]));
                push_text(p, &key, &text, "tEXt");
            }
            b"zTXt" => {
                let z = d.iter().position(|&c| c == 0).unwrap_or(d.len());
                let key = latin1(&d[..z]);
                match d.get(z + 2..).and_then(inflate) {
                    Some(t) => push_text(p, &key, &latin1(&t), "zTXt"),
                    None => {
                        warn.push(format!("The PNG text chunk '{key}' could not be decompressed."));
                        push_text(p, &key, "(compressed, unreadable)", "zTXt");
                    }
                }
            }
            b"iTXt" => itxt(d, p, warn),
            b"acTL" => {
                if let (Some(f), Some(l)) = (be32(d, 0), be32(d, 4)) {
                    actl = Some((f, l));
                }
            }
            b"fcTL" => fctl += 1,
            b"pHYs" if d.len() >= 9 => phys = Some((be32(d, 0).unwrap_or(0), be32(d, 4).unwrap_or(0), d[8])),
            b"cICP" if d.len() >= 4 => p.cicp = Some(cicp_text(d[0] as u16, d[1] as u16, d[2] as u16, d[3] != 0)),
            b"tIME" if d.len() >= 7 => {
                time = Some(format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    be16(d, 0).unwrap_or(0),
                    d[2],
                    d[3],
                    d[4],
                    d[5],
                    d[6]
                ))
            }
            b"IEND" => {
                end = Some((data_end + 4).min(n));
            }
            _ => {}
        }
        pos = data_end + 4;
        if end.is_some() {
            break;
        }
    }

    let e = &mut p.encoding;
    if let Some((depth, color, interlace)) = ihdr {
        e.push(Field::new("Bit depth", format!("{depth} bits per channel"), ""));
        let ct = match color {
            0 => "Grayscale",
            2 => "RGB",
            3 => "Palette (indexed color)",
            4 => "Grayscale + alpha",
            6 => "RGBA (RGB + alpha)",
            _ => "Invalid",
        };
        e.push(Field::new("Color type", ct, ""));
        e.push(Field::new(
            "Interlace",
            if interlace == 1 { "Adam7 (interlaced)" } else { "None" },
            "Interlaced PNGs show a coarse preview first while loading, at the cost of a larger file.",
        ));
    }
    if let Some(g) = gamma {
        let v = if g > 0 { format!("{:.5} (1/{:.2})", g as f64 / 100000.0, 100000.0 / g as f64) } else { "0".into() };
        e.push(Field::new("Gamma (gAMA)", v, "Declared encoding gamma; 0.45455 (1/2.2) is the usual value."));
    }
    if let Some(i) = srgb {
        let v = match i {
            0 => "Perceptual",
            1 => "Relative colorimetric",
            2 => "Saturation",
            3 => "Absolute colorimetric",
            _ => "Unknown intent",
        };
        e.push(Field::new("sRGB chunk", format!("Present ({v})"), "The image declares the sRGB color space."));
    }
    if chrm {
        e.push(Field::new("Chromaticities (cHRM)", "Present", "Primaries and white point of the image's color space."));
    }
    if let Some(name) = iccp_name {
        e.push(Field::new("ICC profile (iCCP)", format!("Present ('{name}')"), ""));
    }
    if let Some(c) = p.cicp.clone() {
        e.push(Field::new("Color code points (cICP)", c, "Color primaries and transfer function signaling (used for HDR PNGs)."));
    }
    if let Some((frames, plays)) = actl {
        p.format = "PNG (animated, APNG)".into();
        let loops = if plays == 0 { "loops forever".to_string() } else { format!("plays {plays} time{}", if plays == 1 { "" } else { "s" }) };
        e.push(Field::new("Frames", format!("{frames} ({loops})"), format!("Animated PNG with {fctl} frame control chunks.")));
    }
    if let Some((x, y, unit)) = phys {
        if unit == 1 && x > 0 {
            let dpi = x as f64 * 0.0254;
            let v = if x == y { format!("{dpi:.0} dpi") } else { format!("{dpi:.0} x {:.0} dpi", y as f64 * 0.0254) };
            e.push(Field::new("Resolution (pHYs)", v, "Print resolution; it does not change the pixels."));
        }
    }
    if let Some(t) = time {
        e.push(Field::new("Modified (tIME)", t, "Last modification time stored in the PNG."));
    }
    let list = chunks
        .iter()
        .map(|(t, c)| if *c > 1 { format!("{t} x{c}") } else { t.clone() })
        .collect::<Vec<_>>()
        .join(", ");
    e.push(Field::new("Chunks", super::clip(&list, 600), "The PNG chunks in file order."));
    if let Some(end) = end {
        if end < n {
            e.push(Field::new("Data after IEND", human_size((n - end) as u64), "Extra bytes after the end of the PNG."));
        }
    }
}

fn itxt(d: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let Some(z) = d.iter().position(|&c| c == 0) else { return };
    let key = latin1(&d[..z]);
    let (Some(&flag), Some(_method)) = (d.get(z + 1), d.get(z + 2)) else { return };
    let mut i = z + 3;
    // Language tag and translated keyword, both null terminated.
    for _ in 0..2 {
        match d.get(i..).and_then(|s| s.iter().position(|&c| c == 0)) {
            Some(k) => i += k + 1,
            None => return,
        }
    }
    let raw = d.get(i..).unwrap_or(&[]);
    let text = if flag == 1 {
        match inflate(raw) {
            Some(t) => String::from_utf8_lossy(&t).into_owned(),
            None => {
                warn.push(format!("The PNG text chunk '{key}' could not be decompressed."));
                return;
            }
        }
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    if key == "XML:com.adobe.xmp" {
        if p.xmp.is_none() {
            p.xmp = Some(text);
        }
    } else {
        push_text(p, &key, &text, "iTXt");
    }
}

/// GIF: frames, loop count, comments.
pub(crate) fn gif(b: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let n = b.len();
    if n < 13 {
        warn.push("The GIF header is truncated.".into());
        return;
    }
    p.dims = Some((le16(b, 6).unwrap_or(0) as u32, le16(b, 8).unwrap_or(0) as u32));
    let flags = b[10];
    let gct = if flags & 0x80 != 0 { 1usize << ((flags & 7) + 1) } else { 0 };
    let mut pos = 13 + 3 * gct;
    let mut frames = 0u32;
    let mut loops: Option<u16> = None;
    // Returns the index after the sub-block chain, collecting the data.
    let sub_blocks = |mut pos: usize, collect: &mut Vec<u8>| -> Option<usize> {
        loop {
            let len = *b.get(pos)? as usize;
            pos += 1;
            if len == 0 {
                return Some(pos);
            }
            let s = b.get(pos..pos + len)?;
            if collect.len() < 1 << 20 {
                collect.extend_from_slice(s);
            }
            pos += len;
        }
    };
    let mut ok = false;
    while pos < n {
        match b[pos] {
            0x21 => {
                let Some(&label) = b.get(pos + 1) else { break };
                let mut data = Vec::new();
                let Some(next) = sub_blocks(pos + 2, &mut data) else { break };
                if label == 0xFE {
                    let t = String::from_utf8_lossy(&data);
                    let t = t.trim_matches(|c: char| c == '\0' || c.is_whitespace());
                    if !t.is_empty() {
                        p.comments.push(super::clip(t, 2000));
                    }
                } else if label == 0xFF && data.starts_with(b"NETSCAPE2.0") && data.len() >= 14 {
                    loops = le16(&data, 12);
                }
                pos = next;
            }
            0x2C => {
                frames += 1;
                let Some(&lflags) = b.get(pos + 9) else { break };
                pos += 10;
                if lflags & 0x80 != 0 {
                    pos += 3 * (1usize << ((lflags & 7) + 1));
                }
                pos += 1; // LZW minimum code size
                let mut sink = Vec::new();
                let Some(next) = sub_blocks(pos, &mut sink) else { break };
                pos = next;
            }
            0x3B => {
                ok = true;
                break;
            }
            _ => {
                warn.push(format!("Unexpected byte 0x{:02X} in the GIF structure at offset {pos}.", b[pos]));
                ok = true;
                break;
            }
        }
    }
    if !ok {
        warn.push("The GIF is truncated (no trailer).".into());
    }
    let e = &mut p.encoding;
    e.push(Field::new("Version", latin1(&b[0..6]), ""));
    e.push(Field::new("Frames", frames.to_string(), if frames > 1 { "Animated GIF." } else { "" }));
    if let Some(l) = loops {
        e.push(Field::new("Loop", if l == 0 { "Forever".to_string() } else { format!("{l} times") }, ""));
    }
    if gct > 0 {
        e.push(Field::new("Global color table", format!("{gct} colors"), "GIF images have at most 256 colors per frame."));
    }
}

/// WebP: lossy / lossless / animated, alpha, metadata chunks.
pub(crate) fn webp(b: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let n = b.len();
    let mut pos = 12;
    let (mut lossy, mut lossless, mut alpha) = (false, false, false);
    let mut frames = 0u32;
    let mut loops: Option<u16> = None;
    let mut chunks: Vec<String> = Vec::new();
    let mut canvas: Option<(u32, u32)> = None;
    while pos + 8 <= n {
        let fourcc = &b[pos..pos + 4];
        let size = le32(b, pos + 4).unwrap_or(0) as usize;
        let name = latin1(fourcc).trim().to_string();
        let Some(end) = (pos + 8).checked_add(size).filter(|&e| e <= n) else {
            warn.push(format!("The WebP {name} chunk is truncated."));
            break;
        };
        let d = &b[pos + 8..end];
        if chunks.len() < 64 && !(fourcc == b"ANMF" && chunks.last().map(String::as_str) == Some("ANMF")) {
            chunks.push(name);
        }
        match fourcc {
            b"VP8 " => {
                lossy = true;
                if p.dims.is_none() && d.len() >= 10 && d[3..6] == [0x9d, 0x01, 0x2a] {
                    p.dims = Some(((le16(d, 6).unwrap_or(0) & 0x3fff) as u32, (le16(d, 8).unwrap_or(0) & 0x3fff) as u32));
                }
            }
            b"VP8L" => {
                lossless = true;
                if d.len() >= 5 && d[0] == 0x2f {
                    let bits = le32(d, 1).unwrap_or(0);
                    if p.dims.is_none() {
                        p.dims = Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
                    }
                    if (bits >> 28) & 1 == 1 {
                        alpha = true;
                    }
                }
            }
            b"VP8X" if d.len() >= 10 => {
                if d[0] & 0x10 != 0 {
                    alpha = true;
                }
                let w = (d[4] as u32 | (d[5] as u32) << 8 | (d[6] as u32) << 16) + 1;
                let h = (d[7] as u32 | (d[8] as u32) << 8 | (d[9] as u32) << 16) + 1;
                canvas = Some((w, h));
            }
            b"ICCP" => p.icc = Some(d.to_vec()),
            b"EXIF" => p.exif_raw = Some(d.strip_prefix(b"Exif\0\0").unwrap_or(d).to_vec()),
            b"XMP " => p.xmp = Some(String::from_utf8_lossy(d).into_owned()),
            b"ANIM" => loops = le16(d, 4),
            b"ANMF" => {
                frames += 1;
                match d.get(16..20) {
                    Some(b"VP8 ") => lossy = true,
                    Some(b"VP8L") => lossless = true,
                    _ => {}
                }
            }
            b"ALPH" => alpha = true,
            _ => {}
        }
        pos = end + (size & 1);
    }
    if canvas.is_some() {
        p.dims = canvas;
    }
    let kind = match (lossy, lossless) {
        (true, true) => "lossy and lossless",
        (false, true) => "lossless",
        _ => "lossy",
    };
    let e = &mut p.encoding;
    if frames > 0 {
        p.format = "WebP (animated)".into();
        e.push(Field::new("Compression", format!("Animated, {frames} frames ({kind})"), ""));
        if let Some(l) = loops {
            e.push(Field::new("Loop", if l == 0 { "Forever".to_string() } else { format!("{l} times") }, ""));
        }
    } else {
        let v = if lossless && !lossy { "Lossless (VP8L)" } else { "Lossy (VP8)" };
        e.push(Field::new("Compression", v, "Lossy WebP is based on VP8 video coding (4:2:0 color); lossless WebP keeps every pixel exactly."));
    }
    e.push(Field::new("Alpha", if alpha { "Yes" } else { "No" }, "Whether the image has transparency."));
    e.push(Field::new("Chunks", chunks.join(", "), "The RIFF chunks in file order."));
}

/// ISO BMFF box header: (type, payload start, box end).
fn bmff_box(b: &[u8], pos: usize, limit: usize) -> Option<([u8; 4], usize, usize)> {
    let size = be32(b, pos)? as u64;
    let ty: [u8; 4] = b.get(pos + 4..pos + 8)?.try_into().ok()?;
    let (start, end) = match size {
        0 => (pos + 8, limit as u64),
        1 => {
            let hi = be32(b, pos + 8)? as u64;
            let lo = be32(b, pos + 12)? as u64;
            (pos + 16, (pos as u64).checked_add((hi << 32) | lo)?)
        }
        s => (pos + 8, pos as u64 + s),
    };
    if end < start as u64 || end > limit as u64 {
        return None;
    }
    Some((ty, start, end as usize))
}

#[derive(Default)]
struct HeifScan {
    ispe: Vec<(u32, u32)>,
    codecs: Vec<&'static str>,
    pixi: Option<String>,
    items: Option<u32>,
    truncated: bool,
}

fn heif_walk(b: &[u8], start: usize, end: usize, depth: usize, s: &mut HeifScan, p: &mut Parts) {
    if depth > 8 {
        return;
    }
    let mut pos = start;
    while pos + 8 <= end {
        let Some((ty, ds, de)) = bmff_box(b, pos, end) else {
            s.truncated = true;
            return;
        };
        let d = &b[ds..de];
        match &ty {
            b"meta" => heif_walk(b, ds + 4, de, depth + 1, s, p),
            b"iprp" | b"ipco" => heif_walk(b, ds, de, depth + 1, s, p),
            b"ispe" if d.len() >= 12 => s.ispe.push((be32(d, 4).unwrap_or(0), be32(d, 8).unwrap_or(0))),
            b"colr" if d.len() >= 4 => match &d[0..4] {
                b"prof" | b"rICC" => {
                    if p.icc.is_none() {
                        p.icc = Some(d[4..].to_vec());
                    }
                }
                b"nclx" if d.len() >= 11 => {
                    p.cicp = Some(cicp_text(be16(d, 4).unwrap_or(0), be16(d, 6).unwrap_or(0), be16(d, 8).unwrap_or(0), d[10] & 0x80 != 0));
                }
                _ => {}
            },
            b"pixi" if d.len() >= 5 => {
                let nch = d[4] as usize;
                let bits: Vec<String> = d.get(5..5 + nch).unwrap_or(&[]).iter().map(|v| v.to_string()).collect();
                s.pixi = Some(format!("{} bits per channel ({} channels)", bits.first().cloned().unwrap_or_default(), nch));
            }
            b"hvcC" => s.codecs.push("HEVC (H.265)"),
            b"av1C" => s.codecs.push("AV1"),
            b"vvcC" => s.codecs.push("VVC (H.266)"),
            b"jpgC" => s.codecs.push("JPEG"),
            b"auxC" if d.len() > 4 => {
                let z = d[4..].iter().position(|&c| c == 0).map_or(d.len(), |k| 4 + k);
                let urn = String::from_utf8_lossy(&d[4..z]).to_string();
                if !urn.is_empty() && !p.aux.contains(&urn) {
                    p.aux.push(urn);
                }
            }
            b"iinf" if d.len() >= 6 => {
                s.items = if d[0] == 0 { be16(d, 4).map(u32::from) } else { be32(d, 4) };
            }
            _ => {}
        }
        if de <= pos {
            return;
        }
        pos = de;
    }
}

/// HEIF / HEIC / AVIF (and other ISO BMFF files identified by brand).
pub(crate) fn heif(b: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let size = (be32(b, 0).unwrap_or(0) as usize).max(8).min(b.len());
    let major = latin1(&b[8..12]);
    let compat: Vec<String> = b.get(16..size).unwrap_or(&[]).chunks_exact(4).map(latin1).collect();
    let all: Vec<&str> = std::iter::once(major.as_str()).chain(compat.iter().map(String::as_str)).collect();
    let any = |names: &[&str]| all.iter().any(|b| names.contains(b));
    p.format = if any(&["avif", "avis"]) {
        "AVIF".into()
    } else if any(&["heic", "heix", "heim", "heis", "hevc", "hevx", "hevm", "hevs"]) {
        "HEIC".into()
    } else if any(&["crx "]) {
        "CR3 (Canon raw)".into()
    } else if any(&["mif1", "msf1", "mif2"]) {
        "HEIF".into()
    } else if any(&["isom", "iso2", "mp41", "mp42", "qt  ", "M4V ", "3gp4", "3gp5", "avc1"]) {
        "MP4 / QuickTime (video)".into()
    } else {
        format!("ISO media (brand {})", major.trim())
    };
    p.format_note = "Identified from the brands in the file type (ftyp) box.".into();
    let mut s = HeifScan::default();
    heif_walk(b, 0, b.len(), 0, &mut s, p);
    if s.truncated {
        warn.push("A box in the file is truncated or has an invalid size.".into());
    }
    if let Some(&(w, h)) = s.ispe.iter().max_by_key(|(w, h)| *w as u64 * *h as u64) {
        if w > 0 && h > 0 {
            p.dims = Some((w, h));
            p.dims_note = "Largest image spatial extent (ispe) in the file: the full image, before rotation.".into();
        }
    }
    let e = &mut p.encoding;
    e.push(Field::new("Major brand", major.trim(), "Main file type brand: heic = HEVC still image, avif = AV1 still image, mif1 = generic HEIF."));
    if !compat.is_empty() {
        e.push(Field::new("Compatible brands", compat.iter().map(|c| c.trim()).collect::<Vec<_>>().join(", "), ""));
    }
    s.codecs.dedup();
    if !s.codecs.is_empty() {
        e.push(Field::new("Codec", s.codecs.join(", "), ""));
    }
    if let Some(px) = s.pixi {
        e.push(Field::new("Bit depth", px, ""));
    }
    if let Some(c) = p.cicp.clone() {
        e.push(Field::new("Color (nclx)", c, "Color primaries and transfer function; PQ or HLG transfer means an HDR image."));
    }
    if let Some(n) = s.items {
        e.push(Field::new("Items", n.to_string(), "Images and metadata blocks in the file (grid tiles, thumbnails, depth, gain map, EXIF, XMP)."));
    }
    if !p.aux.is_empty() {
        e.push(Field::new("Auxiliary images", p.aux.join(", "), "Extra images such as alpha, depth, portrait mattes or an HDR gain map."));
    }
    // EXIF via kamadak-exif's HEIF reader.
    let mut reader = exif::Reader::new();
    reader.continue_on_error(true);
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.read_from_container(&mut std::io::Cursor::new(b))));
    let Ok(read) = read else {
        warn.push("The EXIF reader failed on this file (it may be corrupt).".into());
        return;
    };
    match read {
        Ok(ex) => p.exif_parsed = Some(ex),
        Err(exif::Error::NotFound(_)) => {}
        Err(err) => {
            let mut first = None;
            match err.distill_partial_result(|errs| first = errs.first().map(|x| x.to_string())) {
                Ok(ex) => {
                    p.exif_parsed = Some(ex);
                    if let Some(f) = first {
                        warn.push(format!("Part of the EXIF data could not be read: {f}"));
                    }
                }
                Err(exif::Error::NotSupported(_)) => {}
                Err(err) => warn.push(format!("The EXIF data could not be read: {err}")),
            }
        }
    }
}

/// BMP header facts.
pub(crate) fn bmp(b: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let hdr = le32(b, 14).unwrap_or(0);
    let (w, h, bpp, comp) = if hdr == 12 {
        (le16(b, 18).unwrap_or(0) as i64, le16(b, 20).unwrap_or(0) as i64, le16(b, 24).unwrap_or(0), 0)
    } else {
        (
            le32(b, 18).unwrap_or(0) as i32 as i64,
            le32(b, 22).unwrap_or(0) as i32 as i64,
            le16(b, 28).unwrap_or(0),
            le32(b, 30).unwrap_or(0),
        )
    };
    if w == 0 || h == 0 {
        warn.push("The BMP header has no image size.".into());
    } else {
        p.dims = Some((w.unsigned_abs() as u32, h.unsigned_abs() as u32));
    }
    let e = &mut p.encoding;
    e.push(Field::new("Bits per pixel", bpp.to_string(), ""));
    let c = match comp {
        0 => "None (BI_RGB)",
        1 => "RLE 8-bit",
        2 => "RLE 4-bit",
        3 => "Bit fields",
        4 => "JPEG",
        5 => "PNG",
        6 => "Alpha bit fields",
        _ => "Unknown",
    };
    e.push(Field::new("Compression", c, ""));
    e.push(Field::new("Row order", if h < 0 { "Top-down" } else { "Bottom-up" }, ""));
    e.push(Field::new("Header", format!("{hdr} bytes"), "40 = BITMAPINFOHEADER, 108 = V4, 124 = V5 (with color space)."));
}

/// LSB-first bit reader for the JPEG XL size header.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self.d.get(self.pos / 8)?;
            v |= (((byte >> (self.pos % 8)) & 1) as u32) << i;
            self.pos += 1;
        }
        Some(v)
    }
    fn size(&mut self) -> Option<u32> {
        let bits = [9, 13, 18, 30][self.u(2)? as usize];
        Some(self.u(bits)? + 1)
    }
}

/// Image size from a JPEG XL codestream (FF 0A + SizeHeader).
pub(crate) fn jxl_dims(cs: &[u8]) -> Option<(u32, u32)> {
    if cs.get(0..2)? != [0xFF, 0x0A] {
        return None;
    }
    let mut br = Bits { d: &cs[2..], pos: 0 };
    let small = br.u(1)? == 1;
    let h = if small { (br.u(5)? + 1) * 8 } else { br.size()? };
    let ratio = br.u(3)?;
    let w = if ratio == 0 {
        if small { (br.u(5)? + 1) * 8 } else { br.size()? }
    } else {
        let (num, den) = [(1u64, 1u64), (12, 10), (4, 3), (3, 2), (16, 9), (5, 4), (2, 1)][ratio as usize - 1];
        (h as u64 * num / den) as u32
    };
    Some((w, h))
}

/// JPEG XL: bare codestream or ISO BMFF container with Exif / xml boxes.
pub(crate) fn jxl(b: &[u8], p: &mut Parts, warn: &mut Vec<String>) {
    let e_container = b.starts_with(&[0, 0, 0, 0x0C]);
    let mut jbrd = false;
    let mut brob: Vec<String> = Vec::new();
    if e_container {
        let mut pos = 0;
        let mut first_jxlp = true;
        while pos + 8 <= b.len() {
            let Some((ty, ds, de)) = bmff_box(b, pos, b.len()) else {
                warn.push("A JPEG XL box is truncated or has an invalid size.".into());
                break;
            };
            let d = &b[ds..de];
            match &ty {
                b"jxlc" => p.dims = p.dims.or_else(|| jxl_dims(d)),
                b"jxlp" if first_jxlp && d.len() > 4 => {
                    first_jxlp = false;
                    p.dims = p.dims.or_else(|| jxl_dims(&d[4..]));
                }
                b"Exif" if d.len() > 4 => {
                    let off = be32(d, 0).unwrap_or(0) as usize;
                    if let Some(raw) = d.get(4 + off..) {
                        p.exif_raw = Some(raw.to_vec());
                    }
                }
                b"xml " => p.xmp = Some(String::from_utf8_lossy(d).into_owned()),
                b"jbrd" => jbrd = true,
                b"brob" if d.len() >= 4 => brob.push(latin1(&d[0..4]).trim().to_string()),
                _ => {}
            }
            if de <= pos {
                break;
            }
            pos = de;
        }
    } else {
        p.dims = jxl_dims(b);
    }
    let e = &mut p.encoding;
    e.push(Field::new("Container", if e_container { "ISO BMFF container" } else { "Bare codestream" }, ""));
    if jbrd {
        e.push(Field::new(
            "JPEG reconstruction",
            "Yes",
            "This file is a losslessly recompressed JPEG: the original JPEG can be restored bit for bit.",
        ));
    }
    if !brob.is_empty() {
        e.push(Field::new("Compressed metadata", brob.join(", "), "Brotli-compressed metadata boxes (not decoded here)."));
    }
}

/// TIFF-based files: format refinement and the embedded ICC profile.
pub(crate) fn tiff_after_exif(exif: Option<&exif::Exif>, p: &mut Parts) {
    let Some(e) = exif else { return };
    let ex = super::exifview::Ex(e);
    if ex.field(exif::Tag(exif::Context::Tiff, 0xC612)).is_some() {
        p.format = "DNG (raw)".into();
    }
    if let Some(f) = ex.field(exif::Tag(exif::Context::Tiff, 0x8773)) {
        match &f.value {
            exif::Value::Undefined(v, _) | exif::Value::Byte(v) => p.icc = Some(v.clone()),
            _ => {}
        }
    }
    if let Some(f) = ex.field(exif::Tag(exif::Context::Tiff, 0x02BC)) {
        if let exif::Value::Byte(v) | exif::Value::Undefined(v, _) = &f.value {
            p.xmp = Some(String::from_utf8_lossy(v).into_owned());
        }
    }
    let enc = &mut p.encoding;
    for (tag, name) in [
        (exif::Tag::Compression, "Compression"),
        (exif::Tag::BitsPerSample, "Bits per sample"),
        (exif::Tag::SamplesPerPixel, "Samples per pixel"),
        (exif::Tag::PhotometricInterpretation, "Photometric"),
        (exif::Tag::PlanarConfiguration, "Planar configuration"),
    ] {
        if let Some(f) = ex.field(tag) {
            enc.push(Field::new(name, super::clip(&f.display_value().to_string(), 200), ""));
        }
    }
}
