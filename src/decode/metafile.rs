//! Windows metafiles (EMF / WMF) rendered with GDI into a 32-bit DIB section.

use super::Decoded;
use anyhow::{anyhow, bail, Result};
use image::RgbaImage;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::{SetWinMetaFileBits, METAFILEPICT};

const PLACEABLE_KEY: u32 = 0x9AC6_CDD7;
const EMF_SIGNATURE: u32 = 0x464D_4520; // " EMF"

pub fn is_emf(b: &[u8]) -> bool {
    b.len() >= 44 && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == 1 && u32::from_le_bytes([b[40], b[41], b[42], b[43]]) == EMF_SIGNATURE
}

pub fn is_wmf(b: &[u8]) -> bool {
    if b.len() >= 22 && u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == PLACEABLE_KEY {
        return true;
    }
    // Plain METAHEADER: type 1 (memory) or 2 (disk), header size 9 words.
    b.len() >= 18 && matches!(u16::from_le_bytes([b[0], b[1]]), 1 | 2) && u16::from_le_bytes([b[2], b[3]]) == 9
}

struct Emf(HENHMETAFILE);
impl Drop for Emf {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteEnhMetaFile(Some(self.0));
        }
    }
}

/// Load either format as an enhanced metafile handle plus its natural size in
/// pixels at 96 DPI.
fn load(b: &[u8]) -> Result<(Emf, f32, f32, &'static str)> {
    unsafe {
        if is_emf(b) {
            let h = SetEnhMetaFileBits(b);
            if h.is_invalid() {
                bail!("Invalid or corrupt EMF file");
            }
            let emf = Emf(h);
            let (w, hh) = emf_size(&emf);
            return Ok((emf, w, hh, "EMF"));
        }
        if !is_wmf(b) {
            bail!("Not a Windows metafile");
        }
        let (data, pict) = if u32::from_le_bytes([b[0], b[1], b[2], b[3]]) == PLACEABLE_KEY {
            let rd = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]) as i32;
            let (l, t, r, btm) = (rd(6), rd(8), rd(10), rd(12));
            let inch = u16::from_le_bytes([b[14], b[15]]).max(1) as i32;
            let (w, h) = ((r - l).abs(), (btm - t).abs());
            // Extents in HIMETRIC (0.01 mm).
            let pict = METAFILEPICT { mm: MM_ANISOTROPIC.0, xExt: w * 2540 / inch, yExt: h * 2540 / inch, hMF: HMETAFILE::default() };
            (&b[22..], Some(pict))
        } else {
            // Without a placeable header GDI would stretch the picture to the
            // reference device (screen) aspect: use the metafile's own window
            // extent instead, or a 4:3 default when it sets none.
            let (w, h) = wmf_window_ext(b).unwrap_or((1024, 768));
            let (w, h) = sane_wmf_size(w, h);
            // Extents in HIMETRIC at 96 DPI.
            let pict = METAFILEPICT { mm: MM_ANISOTROPIC.0, xExt: w * 2540 / 96, yExt: h * 2540 / 96, hMF: HMETAFILE::default() };
            (b, Some(pict))
        };
        let h = SetWinMetaFileBits(data, None, pict.as_ref().map(|p| p as *const METAFILEPICT));
        if h.is_invalid() {
            bail!("Invalid or corrupt WMF file");
        }
        let emf = Emf(h);
        let (w, hh) = emf_size(&emf);
        Ok((emf, w, hh, "WMF"))
    }
}

/// The first META_SETWINDOWEXT record of a plain (non-placeable) WMF, as
/// (width, height) in logical units.
fn wmf_window_ext(b: &[u8]) -> Option<(i32, i32)> {
    let header_words = u16::from_le_bytes([*b.get(2)?, *b.get(3)?]) as usize;
    let mut pos = header_words * 2;
    // Bounded scan: the extent is set near the start of real files.
    for _ in 0..4096 {
        let rec = b.get(pos..pos + 6)?;
        let size_words = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) as usize;
        let func = u16::from_le_bytes([rec[4], rec[5]]);
        if func == 0 || size_words < 3 {
            return None; // META_EOF or corrupt
        }
        if func == 0x020C {
            let p = b.get(pos + 6..pos + 10)?;
            let (h, w) = (i16::from_le_bytes([p[0], p[1]]) as i32, i16::from_le_bytes([p[2], p[3]]) as i32);
            return (w != 0 && h != 0).then_some((w.abs(), h.abs()));
        }
        pos = pos.checked_add(size_words.checked_mul(2)?)?;
    }
    None
}

/// Map logical window extents to a natural pixel size: kept as is when it
/// looks like pixels, otherwise scaled so the long side is 1024 (aspect kept,
/// extreme aspects limited to 16:1).
fn sane_wmf_size(w: i32, h: i32) -> (i32, i32) {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let (w, h) = (w.min(h * 16.0), h.min(w * 16.0));
    let long = w.max(h);
    let k = if (64.0..=4096.0).contains(&long) { 1.0 } else { 1024.0 / long };
    (((w * k).round() as i32).max(1), ((h * k).round() as i32).max(1))
}

fn emf_size(emf: &Emf) -> (f32, f32) {
    unsafe {
        let mut hdr = ENHMETAHEADER::default();
        GetEnhMetaFileHeader(emf.0, std::mem::size_of::<ENHMETAHEADER>() as u32, Some(&mut hdr));
        let f = hdr.rclFrame;
        let (fw, fh) = ((f.right - f.left) as f32 * 96.0 / 2540.0, (f.bottom - f.top) as f32 * 96.0 / 2540.0);
        if fw >= 1.0 && fh >= 1.0 {
            return (fw, fh);
        }
        let r = hdr.rclBounds;
        ((r.right - r.left + 1).max(1) as f32, (r.bottom - r.top + 1).max(1) as f32)
    }
}

pub fn probe(b: &[u8]) -> Option<(u32, u32)> {
    let (_e, w, h, _) = load(b).ok()?;
    Some((w.round() as u32, h.round() as u32))
}

pub fn render(b: &[u8], max_side: Option<u32>) -> Result<(RgbaImage, &'static str, (u32, u32))> {
    let (emf, nw, nh, kind) = load(b)?;
    let long = nw.max(nh).max(1.0);
    let mut side = long.clamp(super::svg::MIN_SIDE as f32, super::svg::MAX_SIDE as f32);
    if let Some(m) = max_side {
        side = side.min(m as f32);
    }
    let s = side / long;
    let (w, h) = (((nw * s).round() as i32).max(1), ((nh * s).round() as i32).max(1));
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            bail!("Cannot create a GDI device context");
        }
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let bmp = match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(b) if !bits.is_null() => b,
            _ => {
                let _ = DeleteDC(dc);
                bail!("Cannot allocate a {w}x{h} bitmap for the metafile");
            }
        };
        let old = SelectObject(dc, bmp.into());
        let len = w as usize * h as usize * 4;
        // White background (GDI metafiles have no alpha).
        std::ptr::write_bytes(bits as *mut u8, 0xFF, len);
        SetStretchBltMode(dc, HALFTONE);
        let rect = RECT { left: 0, top: 0, right: w, bottom: h };
        let ok = PlayEnhMetaFile(dc, emf.0, &rect).as_bool();
        let _ = GdiFlush();
        let src = std::slice::from_raw_parts(bits as *const u8, len);
        let mut out = Vec::with_capacity(len);
        for p in src.chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        SelectObject(dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        if !ok {
            log::warn!("PlayEnhMetaFile reported an error; showing partial output");
        }
        let img = RgbaImage::from_raw(w as u32, h as u32, out).ok_or_else(|| anyhow!("Bad buffer"))?;
        Ok((img, kind, (nw.round() as u32, nh.round() as u32)))
    }
}

pub fn decode(b: &[u8]) -> Result<Decoded> {
    let (img, kind, natural) = render(b, None)?;
    let mut d = Decoded::from_rgba(img, kind);
    d.decoder = "Windows (GDI)";
    d.color_info = format!("Vector · {}x{} natural", natural.0, natural.1);
    d.source_size = natural;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A placeable WMF (1 inch = 1440 units, 2x1 inch) with a red filled rectangle.
    pub fn sample_wmf() -> Vec<u8> {
        let mut recs: Vec<u8> = Vec::new();
        let mut rec = |func: u16, params: &[i16]| {
            let size = (3 + params.len()) as u32;
            recs.extend_from_slice(&size.to_le_bytes());
            recs.extend_from_slice(&func.to_le_bytes());
            for p in params {
                recs.extend_from_slice(&p.to_le_bytes());
            }
        };
        rec(0x020B, &[0, 0]); // SetWindowOrg(y, x)
        rec(0x020C, &[1440, 2880]); // SetWindowExt(y, x)
        // CreateBrushIndirect: style BS_SOLID, color 0x000000FF (red), hatch 0.
        rec(0x02FC, &[0, 0x00FF, 0x0000, 0]);
        rec(0x012D, &[0]); // SelectObject(0)
        rec(0x041B, &[1440, 1440, 0, 0]); // Rectangle(bottom, right, top, left)
        rec(0x0000, &[]); // EOF
        let words = (recs.len() / 2 + 9) as u32;
        let mut v = Vec::new();
        v.extend_from_slice(&PLACEABLE_KEY.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        for x in [0i16, 0, 2880, 1440] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(&1440u16.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        let mut ck = 0u16;
        for i in (0..20).step_by(2) {
            ck ^= u16::from_le_bytes([v[i], v[i + 1]]);
        }
        v.extend_from_slice(&ck.to_le_bytes());
        // METAHEADER
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&9u16.to_le_bytes());
        v.extend_from_slice(&0x0300u16.to_le_bytes());
        v.extend_from_slice(&words.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // objects
        v.extend_from_slice(&6u32.to_le_bytes()); // max record
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&recs);
        v
    }

    #[test]
    fn wmf_renders() {
        let f = sample_wmf();
        assert!(is_wmf(&f));
        let d = decode(&f).expect("wmf decode");
        assert_eq!(d.source_size, (192, 96));
        let (w, h) = d.image.dimensions();
        assert_eq!((w, h), (512, 256));
        // Left half red, right half white background.
        let l = d.image.get_pixel(w / 4, h / 2).0;
        let r = d.image.get_pixel(w * 3 / 4, h / 2).0;
        assert!(l[0] > 200 && l[1] < 60 && l[2] < 60, "left {l:?}");
        assert_eq!(r, [255, 255, 255, 255]);
    }

    #[test]
    fn emf_renders() {
        // Convert the WMF to EMF bytes with GDI, then decode those.
        let (emf, _, _, _) = load(&sample_wmf()).unwrap();
        let bytes = unsafe {
            let n = GetEnhMetaFileBits(emf.0, None);
            let mut buf = vec![0u8; n as usize];
            GetEnhMetaFileBits(emf.0, Some(&mut buf));
            buf
        };
        assert!(is_emf(&bytes));
        let d = decode(&bytes).expect("emf decode");
        assert_eq!(d.format, "EMF");
        let (w, h) = d.image.dimensions();
        assert!(d.image.get_pixel(w / 4, h / 2).0[1] < 60);
    }

    #[test]
    fn plain_wmf_uses_its_own_aspect() {
        // Strip the placeable header: the window extent (2880 x 1440) sets a 2:1 aspect.
        let f = sample_wmf();
        let plain = f[22..].to_vec();
        assert!(is_wmf(&plain));
        assert_eq!(wmf_window_ext(&plain), Some((2880, 1440)));
        let d = decode(&plain).expect("plain wmf decode");
        let (w, h) = d.image.dimensions();
        assert!((w as f32 / h as f32 - 2.0).abs() < 0.05, "{w}x{h}");
        assert_eq!(sane_wmf_size(100, 50), (100, 50));
        assert_eq!(sane_wmf_size(20000, 10000), (1024, 512));
        assert_eq!(sane_wmf_size(1, 100000), (64, 1024));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode(b"").is_err());
        assert!(decode(&[1, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        let mut f = sample_wmf();
        f.truncate(30);
        let _ = decode(&f); // must not crash
    }
}
