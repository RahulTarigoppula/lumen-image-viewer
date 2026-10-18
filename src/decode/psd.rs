//! Photoshop PSD / PSB: decodes the flattened composite ("merged image data")
//! stored at the end of the file. Layers are not composited; files saved with
//! "Maximize compatibility" turned off may carry a blank composite.
//!
//! Supports bitmap, grayscale, duotone, indexed, RGB, CMYK, multichannel and Lab
//! documents at 1/8/16/32 bits per channel, raw or PackBits (RLE) compressed.

use super::{check_alloc, Decoded};
use anyhow::{anyhow, bail, Result};
use image::RgbaImage;
use std::sync::Arc;

struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| anyhow!("PSD file is truncated"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        Ok(u64::from_be_bytes(s.try_into().unwrap()))
    }
    fn skip(&mut self, n: u64) -> Result<()> {
        let n = usize::try_from(n).map_err(|_| anyhow!("PSD file is truncated"))?;
        self.take(n).map(|_| ())
    }
}

pub fn is_psd(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && &bytes[0..4] == b"8BPS" && matches!(u16::from_be_bytes([bytes[4], bytes[5]]), 1 | 2)
}

/// Header-only dimension probe (width, height).
pub fn probe(bytes: &[u8]) -> Option<(u32, u32)> {
    if !is_psd(bytes) || bytes.len() < 26 {
        return None;
    }
    let h = u32::from_be_bytes(bytes[14..18].try_into().ok()?);
    let w = u32::from_be_bytes(bytes[18..22].try_into().ok()?);
    Some((w, h))
}

pub fn decode(bytes: &[u8], color_manage: bool) -> Result<Decoded> {
    let mut r = Rd { b: bytes, pos: 0 };
    if r.take(4)? != b"8BPS" {
        bail!("Not a Photoshop file");
    }
    let version = r.u16()?;
    let psb = match version {
        1 => false,
        2 => true,
        v => bail!("Unsupported PSD version {v}"),
    };
    r.skip(6)?;
    let channels = r.u16()? as usize;
    let height = r.u32()?;
    let width = r.u32()?;
    let depth = r.u16()?;
    let mode = r.u16()?;
    if channels == 0 || channels > 56 {
        bail!("Invalid PSD channel count {channels}");
    }
    if width == 0 || height == 0 {
        bail!("PSD has no pixels");
    }
    let max = if psb { 300_000 } else { 30_000 };
    if width > max || height > max {
        bail!("Invalid PSD dimensions {width}x{height}");
    }
    if !matches!(depth, 1 | 8 | 16 | 32) {
        bail!("Unsupported PSD bit depth {depth}");
    }
    check_alloc(width, height)?;

    // Color mode data (palette for indexed images).
    let cm_len = r.u32()? as usize;
    let color_mode_data = r.take(cm_len)?;

    // Image resources: ICC profile (1039), transparent index (1047).
    let res_len = r.u32()? as usize;
    let res = r.take(res_len)?;
    let (icc, transparent_index) = parse_resources(res);

    // Layer and mask information: we only need to know whether the first
    // extra channel of the composite is its transparency (negative layer count).
    let lm_len = if psb { r.u64()? } else { r.u32()? as u64 };
    let lm_start = r.pos;
    let merged_alpha = if lm_len >= 4 { merged_alpha_flag(bytes, lm_start, lm_len, psb) } else { false };
    r.skip(lm_len)?;

    // Image data.
    let compression = r.u16()?;
    let row_bytes = (width as usize * depth as usize).div_ceil(8);
    let plane = row_bytes.checked_mul(height as usize).ok_or_else(|| anyhow!("PSD too large"))?;
    let color_channels = match mode {
        0 | 1 | 2 | 8 => 1,
        3 | 9 => 3,
        4 => 4,
        7 => channels.min(4),
        m => bail!("Unsupported PSD color mode {m}"),
    };
    if channels < color_channels {
        bail!("PSD has too few channels for its color mode");
    }
    let has_alpha = channels > color_channels && merged_alpha;
    let want = color_channels + has_alpha as usize;
    let total = plane.checked_mul(want).ok_or_else(|| anyhow!("PSD too large"))?;
    if total as u64 > super::MAX_DECODE_BYTES {
        bail!("{}", super::too_large_msg(width, height));
    }
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(want);
    match compression {
        0 => {
            for _ in 0..want {
                planes.push(r.take(plane)?.to_vec());
            }
        }
        1 => {
            let rows = channels * height as usize;
            // The row byte-count table comes straight from the header: make sure
            // the file actually holds it before reserving memory for it.
            let entry = if psb { 4 } else { 2 };
            let remaining = bytes.len().saturating_sub(r.pos);
            if rows.checked_mul(entry).is_none_or(|n| n > remaining) {
                bail!("PSD file is truncated");
            }
            let mut counts: Vec<usize> = Vec::new();
            counts.try_reserve_exact(rows).map_err(|_| anyhow!("PSD too large"))?;
            for _ in 0..rows {
                counts.push(if psb { r.u32()? as usize } else { r.u16()? as usize });
            }
            for c in 0..want {
                let mut out = vec![0u8; plane];
                for y in 0..height as usize {
                    let n = counts[c * height as usize + y];
                    let src = r.take(n)?;
                    unpack_bits(src, &mut out[y * row_bytes..(y + 1) * row_bytes]);
                }
                planes.push(out);
            }
        }
        2 | 3 => bail!("ZIP-compressed PSD composites are not supported"),
        c => bail!("Unknown PSD compression {c}"),
    }

    let n = width as usize * height as usize;
    // Normalise every plane to 8 bits per sample.
    let planes8: Vec<Vec<u8>> = planes
        .into_iter()
        .map(|p| match depth {
            1 => {
                let mut o = Vec::with_capacity(n);
                for y in 0..height as usize {
                    let row = &p[y * row_bytes..(y + 1) * row_bytes];
                    for x in 0..width as usize {
                        // In bitmap mode 1 = black.
                        let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
                        o.push(if bit == 1 { 0 } else { 255 });
                    }
                }
                o
            }
            8 => p,
            16 => p.chunks_exact(2).map(|c| c[0]).collect(),
            _ => p
                .chunks_exact(4)
                .map(|c| {
                    let v = f32::from_be_bytes([c[0], c[1], c[2], c[3]]);
                    (srgb_encode(v) * 255.0 + 0.5) as u8
                })
                .collect(),
        })
        .collect();

    let mut img = RgbaImage::new(width, height);
    let out = img.as_mut();
    let alpha = if has_alpha { Some(&planes8[color_channels]) } else { None };
    let mut info = match mode {
        0 => "Bitmap 1-bit".to_string(),
        1 => format!("Grayscale {depth}-bit"),
        2 => "Indexed 8-bit".to_string(),
        3 => format!("RGB {depth}-bit"),
        4 => format!("CMYK {depth}-bit"),
        7 => format!("Multichannel {depth}-bit"),
        8 => format!("Duotone {depth}-bit"),
        _ => format!("Lab {depth}-bit"),
    };
    for i in 0..n {
        let px = match mode {
            0 | 1 | 8 => {
                let g = planes8[0][i];
                [g, g, g]
            }
            2 => {
                let idx = planes8[0][i] as usize;
                if color_mode_data.len() >= 768 {
                    [color_mode_data[idx], color_mode_data[256 + idx], color_mode_data[512 + idx]]
                } else {
                    [idx as u8; 3]
                }
            }
            3 => [planes8[0][i], planes8[1][i], planes8[2][i]],
            4 => {
                // Stored inverted: 255 = no ink.
                let k = planes8[3][i] as u32;
                let f = |c: u8| ((c as u32 * k + 127) / 255) as u8;
                [f(planes8[0][i]), f(planes8[1][i]), f(planes8[2][i])]
            }
            7 => {
                if color_channels >= 3 {
                    [planes8[0][i], planes8[1][i], planes8[2][i]]
                } else {
                    [planes8[0][i]; 3]
                }
            }
            _ => lab_to_srgb(planes8[0][i], planes8[1][i], planes8[2][i]),
        };
        let mut a = alpha.map(|a| a[i]).unwrap_or(255);
        if mode == 2 && transparent_index == Some(planes8[0][i]) {
            a = 0;
        }
        let o = &mut out[i * 4..i * 4 + 4];
        if a > 0 && a < 255 && alpha.is_some() {
            // The composite is matted against white: undo that.
            let un = |c: u8| -> u8 {
                let v = (c as f32 - (255.0 - a as f32)) * 255.0 / a as f32;
                v.clamp(0.0, 255.0) as u8
            };
            o.copy_from_slice(&[un(px[0]), un(px[1]), un(px[2]), a]);
        } else {
            o.copy_from_slice(&[px[0], px[1], px[2], a]);
        }
    }
    if mode == 3 && color_manage {
        if let Some(profile) = icc.as_deref() {
            match super::color::to_srgb_in_place(&mut img, profile) {
                super::color::Outcome::Converted(name) => info = format!("{info} · {name} → sRGB"),
                super::color::Outcome::AlreadySrgb => info = format!("{info} · sRGB"),
                super::color::Outcome::Unsupported => {}
            }
        }
    }
    let has_alpha = (has_alpha || transparent_index.is_some()) && super::image_has_alpha(&img);
    Ok(Decoded {
        image: Arc::new(img),
        frames: None,
        has_alpha,
        format: if psb { "Photoshop PSB".into() } else { "Photoshop PSD".into() },
        decoder: "Lumen",
        color_info: info,
        icc_profile: icc,
        exif: None,
        orientation: 1,
        source_size: (width, height),
        frame_count: 1,
        bit_depth: depth.max(8) as u8,
    })
}

/// Whether the first extra channel of the merged composite is its
/// transparency: signalled by a negative layer count in the layer info, which
/// 16/32-bit documents store in an `Lr16` / `Lr32` tagged block after the
/// (empty) layer info and global mask sections.
fn merged_alpha_flag(bytes: &[u8], start: usize, len: u64, psb: bool) -> bool {
    let end = usize::try_from(len).ok().and_then(|l| start.checked_add(l)).map_or(bytes.len(), |e| e.min(bytes.len()));
    let section = &bytes[..end];
    let mut lr = Rd { b: section, pos: start };
    let li_len = if psb { lr.u64() } else { lr.u32().map(|v| v as u64) };
    let Ok(li_len) = li_len else { return false };
    if li_len >= 2 {
        return lr.u16().map(|c| (c as i16) < 0).unwrap_or(false);
    }
    // Empty layer info: skip it and the global layer mask info, then scan the
    // additional layer information blocks.
    if lr.skip(li_len).is_err() {
        return false;
    }
    let Ok(gm_len) = lr.u32() else { return false };
    if lr.skip(gm_len as u64).is_err() {
        return false;
    }
    while lr.pos + 12 <= section.len() {
        let Ok(sig) = lr.take(4) else { return false };
        if sig != b"8BIM" && sig != b"8B64" {
            return false;
        }
        let Ok(key) = lr.take(4) else { return false };
        // PSB uses 8-byte lengths for a handful of keys, including Lr16/Lr32.
        let long = psb && matches!(key, b"LMsk" | b"Lr16" | b"Lr32" | b"Layr" | b"Mt16" | b"Mt32" | b"Mtrn" | b"Alph" | b"FMsk" | b"lnk2" | b"FEid" | b"FXid" | b"PxSD");
        let blen = if long { lr.u64() } else { lr.u32().map(|v| v as u64) };
        let Ok(blen) = blen else { return false };
        if matches!(key, b"Lr16" | b"Lr32" | b"Layr") {
            if blen < 2 {
                return false;
            }
            return lr.u16().map(|c| (c as i16) < 0).unwrap_or(false);
        }
        // Blocks are padded to an even length (4 in some writers; tolerate both
        // by trusting the stated length rounded up to 2).
        if lr.skip(blen + (blen & 1)).is_err() {
            return false;
        }
    }
    false
}

fn parse_resources(mut res: &[u8]) -> (Option<Vec<u8>>, Option<u8>) {
    let mut icc = None;
    let mut tidx = None;
    while res.len() >= 12 && &res[0..4] == b"8BIM" {
        let id = u16::from_be_bytes([res[4], res[5]]);
        let name_len = res[6] as usize;
        // Pascal string (length byte + chars) padded to an even size.
        let name_total = (1 + name_len + 1) & !1;
        let p = 6 + name_total;
        if p + 4 > res.len() {
            break;
        }
        let size = u32::from_be_bytes([res[p], res[p + 1], res[p + 2], res[p + 3]]) as usize;
        let start = p + 4;
        let Some(end) = start.checked_add(size).filter(|&e| e <= res.len()) else { break };
        let data = &res[start..end];
        match id {
            1039 => icc = Some(data.to_vec()),
            1047 if data.len() >= 2 => tidx = Some(u16::from_be_bytes([data[0], data[1]]).min(255) as u8),
            _ => {}
        }
        let next = (end + 1) & !1;
        if next > res.len() {
            break;
        }
        res = &res[next..];
    }
    (icc, tidx)
}

/// PackBits decompression of one row; tolerant of malformed input.
pub(crate) fn unpack_bits(src: &[u8], dst: &mut [u8]) {
    let (mut i, mut o) = (0usize, 0usize);
    while i < src.len() && o < dst.len() {
        let n = src[i] as i8;
        i += 1;
        if n >= 0 {
            let count = n as usize + 1;
            let end = (i + count).min(src.len());
            let take = (end - i).min(dst.len() - o);
            dst[o..o + take].copy_from_slice(&src[i..i + take]);
            o += take;
            i = end;
        } else if n != -128 {
            let count = (1 - n as isize) as usize;
            let Some(&v) = src.get(i) else { break };
            i += 1;
            let take = count.min(dst.len() - o);
            dst[o..o + take].fill(v);
            o += take;
        }
    }
}

fn srgb_encode(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// CIE L*a*b* (D50, PSD 8-bit encoding) to sRGB.
fn lab_to_srgb(l: u8, a: u8, b: u8) -> [u8; 3] {
    let l = l as f32 * 100.0 / 255.0;
    let a = a as f32 - 128.0;
    let b = b as f32 - 128.0;
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let finv = |t: f32| if t > 6.0 / 29.0 { t * t * t } else { 3.0 * (6.0f32 / 29.0).powi(2) * (t - 4.0 / 29.0) };
    // D50 white point.
    let (x, y, z) = (0.9642 * finv(fx), finv(fy), 0.8249 * finv(fz));
    // Bradford-adapted XYZ(D50) -> linear sRGB.
    let r = 3.1338561 * x - 1.6168667 * y - 0.4906146 * z;
    let g = -0.9787684 * x + 1.9161415 * y + 0.0334540 * z;
    let bl = 0.0719453 * x - 0.2289914 * y + 1.4052427 * z;
    let e = |v: f32| (srgb_encode(v) * 255.0 + 0.5) as u8;
    [e(r), e(g), e(bl)]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a minimal PSD/PSB with the given planar 8- or 16-bit channel data.
    pub fn build(psb: bool, mode: u16, depth: u16, w: u32, h: u32, chans: &[Vec<u8>], rle: bool, negative_layers: bool) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"8BPS");
        v.extend_from_slice(&(if psb { 2u16 } else { 1 }).to_be_bytes());
        v.extend_from_slice(&[0; 6]);
        v.extend_from_slice(&(chans.len() as u16).to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&depth.to_be_bytes());
        v.extend_from_slice(&mode.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // color mode data
        v.extend_from_slice(&0u32.to_be_bytes()); // resources
        // Layer & mask section: layer info with a layer count.
        let mut lm = Vec::new();
        if psb { lm.extend_from_slice(&2u64.to_be_bytes()) } else { lm.extend_from_slice(&2u32.to_be_bytes()) }
        lm.extend_from_slice(&(if negative_layers { -1i16 } else { 0 }).to_be_bytes());
        if psb { v.extend_from_slice(&(lm.len() as u64).to_be_bytes()) } else { v.extend_from_slice(&(lm.len() as u32).to_be_bytes()) }
        v.extend_from_slice(&lm);
        let row = (w as usize * depth as usize).div_ceil(8);
        if rle {
            v.extend_from_slice(&1u16.to_be_bytes());
            let mut packed_rows = Vec::new();
            for c in chans {
                for y in 0..h as usize {
                    let r = &c[y * row..(y + 1) * row];
                    // Literal runs of up to 128 bytes.
                    let mut p = Vec::new();
                    for chunk in r.chunks(128) {
                        p.push((chunk.len() - 1) as u8);
                        p.extend_from_slice(chunk);
                    }
                    packed_rows.push(p);
                }
            }
            for p in &packed_rows {
                if psb { v.extend_from_slice(&(p.len() as u32).to_be_bytes()) } else { v.extend_from_slice(&(p.len() as u16).to_be_bytes()) }
            }
            for p in packed_rows {
                v.extend_from_slice(&p);
            }
        } else {
            v.extend_from_slice(&0u16.to_be_bytes());
            for c in chans {
                v.extend_from_slice(c);
            }
        }
        v
    }

    #[test]
    fn rgb8_rle_and_raw() {
        let (w, h) = (5u32, 3u32);
        let r: Vec<u8> = (0..15).map(|i| i * 10).collect();
        let g = vec![100u8; 15];
        let b = vec![200u8; 15];
        for rle in [false, true] {
            let f = build(false, 3, 8, w, h, &[r.clone(), g.clone(), b.clone()], rle, false);
            let d = decode(&f, false).unwrap();
            assert_eq!(d.image.dimensions(), (5, 3));
            assert_eq!(d.image.get_pixel(4, 2).0, [140, 100, 200, 255]);
            assert!(!d.has_alpha);
        }
    }

    #[test]
    fn rgba16_psb_with_merged_alpha() {
        let (w, h) = (2u32, 2u32);
        let p16 = |v: u16| -> Vec<u8> { (0..4).flat_map(|_| v.to_be_bytes()).collect() };
        let chans = vec![p16(0xFFFF), p16(0x0000), p16(0x0000), p16(0xFFFF)];
        let f = build(true, 3, 16, w, h, &chans, true, true);
        let d = decode(&f, false).unwrap();
        assert_eq!(d.format, "Photoshop PSB");
        assert_eq!(d.bit_depth, 16);
        assert_eq!(d.image.get_pixel(1, 1).0, [255, 0, 0, 255]);
    }

    #[test]
    fn rgba16_alpha_from_lr16_block() {
        // 16-bit documents: empty layer info, alpha flag in an Lr16 block.
        let (w, h) = (2u32, 1u32);
        let p16 = |v: u16| -> Vec<u8> { (0..2).flat_map(|_| v.to_be_bytes()).collect() };
        let chans = vec![p16(0xFFFF), p16(0), p16(0), p16(0)];
        let mut f = build(false, 3, 16, w, h, &chans, false, false);
        // Replace the layer & mask section (offset 34: 4 + 2 + 6 + 2 + 4 + 4 + 2 + 2 + 4 + 4).
        let lm_off = 34;
        let old_len = u32::from_be_bytes(f[lm_off..lm_off + 4].try_into().unwrap()) as usize;
        let mut lm = Vec::new();
        lm.extend_from_slice(&0u32.to_be_bytes()); // layer info (empty)
        lm.extend_from_slice(&0u32.to_be_bytes()); // global mask info
        lm.extend_from_slice(b"8BIMLr16");
        lm.extend_from_slice(&2u32.to_be_bytes());
        lm.extend_from_slice(&(-1i16).to_be_bytes());
        let mut sec = (lm.len() as u32).to_be_bytes().to_vec();
        sec.extend_from_slice(&lm);
        f.splice(lm_off..lm_off + 4 + old_len, sec);
        let d = decode(&f, false).unwrap();
        assert!(d.has_alpha);
        assert_eq!(d.image.get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn crafted_rle_row_table_does_not_allocate() {
        // PSB 300000 x 300000 fails check_alloc; use a size that passes it
        // with 56 channels so the RLE table claims ~ 56 * 20000 entries in a
        // tiny file.
        let mut f = build(true, 3, 8, 1, 1, &[vec![1], vec![2], vec![3]], true, false);
        f[12..14].copy_from_slice(&56u16.to_be_bytes());
        f[14..18].copy_from_slice(&20_000u32.to_be_bytes());
        f[18..22].copy_from_slice(&1u32.to_be_bytes());
        let t = std::time::Instant::now();
        assert!(decode(&f, false).is_err());
        assert!(t.elapsed().as_secs() < 2);
    }

    #[test]
    fn cmyk_and_gray() {
        // CMYK stored inverted: 255 everywhere = white; K=0 = black.
        let white = vec![255u8; 4];
        let black_k = vec![0u8; 4];
        let f = build(false, 4, 8, 2, 2, &[white.clone(), white.clone(), white.clone(), black_k], false, false);
        assert_eq!(decode(&f, false).unwrap().image.get_pixel(0, 0).0, [0, 0, 0, 255]);
        let f = build(false, 4, 8, 2, 2, &[vec![0; 4], white.clone(), white.clone(), white.clone()], false, false);
        assert_eq!(decode(&f, false).unwrap().image.get_pixel(0, 0).0, [0, 255, 255, 255]); // full cyan
        let f = build(false, 1, 8, 2, 2, &[vec![77; 4]], true, false);
        assert_eq!(decode(&f, false).unwrap().image.get_pixel(1, 0).0, [77, 77, 77, 255]);
    }

    #[test]
    fn corrupt_inputs_error() {
        let f = build(false, 3, 8, 4, 4, &[vec![1; 16], vec![2; 16], vec![3; 16]], true, false);
        for cut in [0, 5, 20, 30, f.len() - 3] {
            assert!(decode(&f[..cut], false).is_err(), "cut at {cut}");
        }
        // Absurd claimed size must fail cleanly instead of allocating.
        let mut big = build(true, 3, 8, 1, 1, &[vec![1], vec![2], vec![3]], false, false);
        big[14..18].copy_from_slice(&200_000u32.to_be_bytes());
        big[18..22].copy_from_slice(&200_000u32.to_be_bytes());
        assert!(decode(&big, false).is_err());
    }
}
