//! Metadata kept in step with a losslessly edited picture: EXIF pixel size,
//! orientation and thumbnail, XMP pixel size, and the HDR gain map.

use super::{Component, Jpeg};

const EXIF_HEADER: &[u8] = b"Exif\0";
pub(super) const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// Largest APPn payload.
const MAX_SEGMENT: usize = 65533;

/// The TIFF data of an APP1 EXIF payload.
pub(super) fn exif_tiff(app1: &[u8]) -> Option<&[u8]> {
    (app1.len() > 6 && app1.starts_with(EXIF_HEADER)).then(|| &app1[6..])
}

/// The XML of an APP1 XMP payload.
fn xmp_text(app1: &[u8]) -> Option<&str> {
    std::str::from_utf8(app1.strip_prefix(XMP_HEADER)?).ok()
}

fn is_mpf(m: u8, b: &[u8]) -> bool {
    m == 0xE2 && b.starts_with(b"MPF\0")
}

/// Byte order of a TIFF block and integer access in it.
#[derive(Clone, Copy)]
struct Tiff {
    le: bool,
}

/// One IFD entry: tag, type, count and the position of its value field.
#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    typ: u16,
    pos: usize,
}

impl Tiff {
    fn of(t: &[u8]) -> Option<Tiff> {
        match t.get(0..4)? {
            [b'I', b'I', 42, 0] => Some(Tiff { le: true }),
            [b'M', b'M', 0, 42] => Some(Tiff { le: false }),
            _ => None,
        }
    }

    fn u16(self, t: &[u8], o: usize) -> Option<u16> {
        let b = t.get(o..o + 2)?;
        Some(if self.le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    }

    fn u32(self, t: &[u8], o: usize) -> Option<u32> {
        let b = t.get(o..o + 4)?;
        let b = [b[0], b[1], b[2], b[3]];
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn put16(self, t: &mut [u8], o: usize, v: u16) {
        if let Some(b) = t.get_mut(o..o + 2) {
            b.copy_from_slice(&if self.le { v.to_le_bytes() } else { v.to_be_bytes() });
        }
    }

    fn put32(self, t: &mut [u8], o: usize, v: u32) {
        if let Some(b) = t.get_mut(o..o + 4) {
            b.copy_from_slice(&if self.le { v.to_le_bytes() } else { v.to_be_bytes() });
        }
    }

    fn ifd0(self, t: &[u8]) -> Option<usize> {
        Some(self.u32(t, 4)? as usize)
    }

    fn entries(self, t: &[u8], ifd: usize) -> Option<Vec<Entry>> {
        let n = self.u16(t, ifd)? as usize;
        // The whole table must be there (this also bounds the allocation).
        t.get(ifd + 2..ifd + 2 + 12 * n)?;
        (0..n).map(|i| ifd + 2 + 12 * i).map(|e| Some(Entry { tag: self.u16(t, e)?, typ: self.u16(t, e + 2)?, pos: e + 8 })).collect()
    }

    /// Position of the IFD's link to the next IFD.
    fn next_link(self, t: &[u8], ifd: usize) -> Option<usize> {
        let p = ifd + 2 + 12 * self.u16(t, ifd)? as usize;
        t.get(p..p + 4)?;
        Some(p)
    }

    /// An integer entry's (first) value.
    fn int(self, t: &[u8], e: Entry) -> Option<u32> {
        match e.typ {
            3 => self.u16(t, e.pos).map(u32::from),
            4 => self.u32(t, e.pos),
            _ => None,
        }
    }

    fn set_int(self, t: &mut [u8], e: Entry, v: u32) {
        match e.typ {
            3 => self.put16(t, e.pos, v.min(65535) as u16),
            4 => self.put32(t, e.pos, v),
            _ => {}
        }
    }
}

/// The EXIF orientation tag's value, if the IFD0 of the TIFF block `t` has one.
pub(super) fn orientation(t: &[u8]) -> Option<u16> {
    let tf = Tiff::of(t)?;
    let e = tf.entries(t, tf.ifd0(t)?)?.into_iter().find(|e| e.tag == 0x0112)?;
    tf.int(t, e).and_then(|v| u16::try_from(v).ok())
}

fn has_thumbnail(t: &[u8]) -> bool {
    let link = Tiff::of(t).and_then(|tf| Some((tf, tf.next_link(t, tf.ifd0(t)?)?)));
    link.is_some_and(|(tf, l)| tf.u32(t, l).is_some_and(|v| v != 0))
}

/// Set the pixel size (IFD0 and EXIF sub-IFD tags), reset the orientation
/// when asked, and put `thumb` in as the thumbnail (none: removed).
fn update_exif(t: &mut Vec<u8>, size: (u32, u32), reset_orientation: bool, thumb: Option<&[u8]>) {
    let Some(tf) = Tiff::of(t) else { return };
    let Some(ifd0) = tf.ifd0(t) else { return };
    let Some(entries) = tf.entries(t, ifd0) else { return };
    let mut sub = None;
    for e in entries {
        match e.tag {
            0x0100 => tf.set_int(t, e, size.0),
            0x0101 => tf.set_int(t, e, size.1),
            0x0112 if reset_orientation => tf.set_int(t, e, 1),
            0x8769 => sub = tf.int(t, e),
            _ => {}
        }
    }
    if let Some(entries) = sub.and_then(|s| tf.entries(t, s as usize)) {
        for e in entries {
            match e.tag {
                0xA002 => tf.set_int(t, e, size.0),
                0xA003 => tf.set_int(t, e, size.1),
                _ => {}
            }
        }
    }
    set_thumbnail(t, tf, ifd0, thumb);
}

/// Replace the JPEG thumbnail of IFD1 with `thumb`, or unlink IFD1 when there
/// is no new one (or it would not fit in the segment).
fn set_thumbnail(t: &mut Vec<u8>, tf: Tiff, ifd0: usize, thumb: Option<&[u8]>) {
    let Some(link) = tf.next_link(t, ifd0) else { return };
    let ifd1 = tf.u32(t, link).unwrap_or(0) as usize;
    if ifd1 == 0 {
        return;
    }
    let entries = tf.entries(t, ifd1).unwrap_or_default();
    let find = |tag: u16| entries.iter().find(|e| e.tag == tag).copied();
    let (Some(off_e), Some(len_e)) = (find(0x0201), find(0x0202)) else {
        // Not a JPEG thumbnail (or unreadable): drop it.
        tf.put32(t, link, 0);
        return;
    };
    let old = tf.int(t, off_e).zip(tf.int(t, len_e)).map(|(o, l)| (o as usize, l as usize));
    let ifd1_end = ifd1 + 2 + 12 * entries.len() + 4;
    // An old thumbnail at the very end of the block is cut off; anywhere
    // else it stays as unused bytes.
    if let Some((o, l)) = old {
        if o >= ifd1_end && o <= t.len() && o + l >= t.len() {
            t.truncate(o);
        }
    }
    match thumb {
        Some(b) if EXIF_HEADER.len() + 1 + t.len() + 1 + b.len() <= MAX_SEGMENT => {
            if t.len() % 2 == 1 {
                t.push(0);
            }
            let off = t.len() as u32;
            t.extend_from_slice(b);
            tf.put32(t, off_e.pos, off);
            tf.set_int(t, len_e, b.len() as u32);
        }
        _ => tf.put32(t, link, 0),
    }
}

/// Bring the metadata in line with the changed picture `j`: EXIF and XMP
/// pixel size, the orientation reset after turning the picture itself, a
/// new EXIF thumbnail when there was one, and no MPF images (an HDR gain map
/// or large preview would no longer match). Returns whether an HDR gain map
/// was removed.
pub(super) fn update(j: &mut Jpeg, reset_orientation: bool) -> bool {
    let mpf = j.segments.iter().any(|(m, b)| is_mpf(*m, b));
    let hdr = j.segments.iter().any(|(m, b)| *m == 0xE1 && xmp_text(b).is_some_and(|x| x.contains("hdrgm:")));
    let thumb = if j.segments.iter().any(|(m, b)| *m == 0xE1 && exif_tiff(b).is_some_and(has_thumbnail)) { thumbnail(j) } else { None };
    let size = (j.width, j.height);
    let drop_extra = mpf || hdr;
    if drop_extra {
        // The extra images live after the end of the picture.
        j.segments.retain(|(m, b)| !is_mpf(*m, b));
        j.trailer.clear();
    }
    for (m, b) in &mut j.segments {
        if *m != 0xE1 {
            continue;
        }
        if exif_tiff(b).is_some() {
            let mut t = b[6..].to_vec();
            update_exif(&mut t, size, reset_orientation, thumb.as_deref());
            b.truncate(6);
            b.extend_from_slice(&t);
        } else if let Some(x) = xmp_text(b) {
            let mut x = set_xmp_size(x, size);
            if drop_extra {
                x = strip_extra_images(&x);
            }
            if XMP_HEADER.len() + x.len() <= MAX_SEGMENT {
                *b = [XMP_HEADER, x.as_bytes()].concat();
            }
        }
    }
    hdr
}

/// XMP without the HDR gain map description and the container directory and
/// motion photo offsets that pointed at the removed trailing data.
fn strip_extra_images(x: &str) -> String {
    let mut s = remove_attrs(x, "hdrgm:");
    s = remove_attrs(&s, "GCamera:MotionPhoto");
    s = remove_attrs(&s, "GCamera:MicroVideo");
    s = remove_elements(&s, "hdrgm:");
    remove_elements(&s, "Container:Directory")
}

fn set_xmp_size(x: &str, (w, h): (u32, u32)) -> String {
    let mut s = x.to_string();
    for (name, v) in [("exif:PixelXDimension", w), ("exif:PixelYDimension", h), ("tiff:ImageWidth", w), ("tiff:ImageLength", h)] {
        s = set_xmp_value(&s, name, &v.to_string());
    }
    s
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, ':' | '_' | '-' | '.')
}

/// Replace the value of `name` written as an attribute (`name="..."`) or as
/// a simple element (`<name>...</name>`).
fn set_xmp_value(s: &str, name: &str, v: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s;
    while let Some(i) = rest.find(name) {
        let before = &rest[..i];
        let after = &rest[i + name.len()..];
        out.push_str(&rest[..i + name.len()]);
        rest = after;
        if after.starts_with(is_name_char) {
            continue;
        }
        if before.ends_with('<') {
            if let Some(t) = after.strip_prefix('>') {
                if let Some(end) = t.find('<') {
                    out.push('>');
                    out.push_str(v);
                    rest = &t[end..];
                }
            }
        } else if before.ends_with(char::is_whitespace) {
            if let Some(t) = after.trim_start().strip_prefix('=') {
                let t = t.trim_start();
                if let Some(q) = t.chars().next().filter(|c| *c == '"' || *c == '\'') {
                    if let Some(end) = t[1..].find(q) {
                        out.push('=');
                        out.push(q);
                        out.push_str(v);
                        out.push(q);
                        rest = &t[1 + end + 1..];
                    }
                }
            }
        }
    }
    out.push_str(rest);
    out
}

/// Remove every attribute whose name starts with `prefix` (with the white
/// space before it).
fn remove_attrs(s: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(prefix) {
        let ws = rest[..i].trim_end_matches(char::is_whitespace).len();
        let after = &rest[i..];
        let name_len = after.find(|c: char| !is_name_char(c)).unwrap_or(after.len());
        let value = after[name_len..].trim_start().strip_prefix('=').map(str::trim_start);
        let quoted = value.and_then(|t| {
            let q = t.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let end = t[1..].find(q)?;
            Some(&t[1 + end + 1..])
        });
        match quoted {
            Some(tail) if ws < i => {
                out.push_str(&rest[..ws]);
                rest = tail;
            }
            _ => {
                out.push_str(&rest[..i + prefix.len()]);
                rest = &rest[i + prefix.len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Remove every element whose name starts with `prefix`, with its content.
fn remove_elements(s: &str, prefix: &str) -> String {
    let open = format!("<{prefix}");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + 1..];
        let name_len = after.find(|c: char| !is_name_char(c)).unwrap_or(after.len());
        let name = &after[..name_len];
        let Some(gt) = after.find('>') else { break };
        let end = if after[..gt].ends_with('/') {
            Some(i + 1 + gt + 1)
        } else {
            let close = format!("</{name}>");
            after.find(&close).map(|k| i + 1 + k + close.len())
        };
        let Some(end) = end else { break };
        out.push_str(&rest[..i]);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Adobe APP14 color transform: 0 = none (RGB or CMYK), 1 = YCbCr, 2 = YCCK.
fn adobe_transform(j: &Jpeg) -> Option<u8> {
    j.segments.iter().find(|(m, b)| *m == 0xEE && b.len() >= 12 && b.starts_with(b"Adobe")).map(|(_, b)| b[11])
}

/// A small JPEG of the picture for the EXIF thumbnail, made from the DC
/// coefficients (each block's average color) without decoding anything else.
/// `None` for CMYK pictures.
fn thumbnail(j: &Jpeg) -> Option<Vec<u8>> {
    let n = j.comps.len();
    if n != 1 && n != 3 {
        return None;
    }
    // One pixel per 8 x 8 block of the full picture.
    let (w, h) = (j.width.div_ceil(8) as usize, j.height.div_ceil(8) as usize);
    let (hmax, vmax) = (j.hmax(), j.vmax());
    let plane = |c: &Component| -> Option<Vec<f32>> {
        let q0 = c.q?[0] as f32;
        if c.blocks.is_empty() {
            return None;
        }
        let mut p = Vec::with_capacity(w * h);
        for y in 0..h {
            let by = (y * c.v / vmax).min(c.bh - 1);
            for x in 0..w {
                let bx = (x * c.h / hmax).min(c.bw - 1);
                p.push(c.blocks[by * c.bw + bx][0] as f32 * q0 / 8.0 + 128.0);
            }
        }
        Some(p)
    };
    let planes = j.comps.iter().map(plane).collect::<Option<Vec<_>>>()?;
    let rgb = n == 3 && (adobe_transform(j) == Some(0) || j.comps.iter().map(|c| c.id).eq(*b"RGB"));
    let px = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    let mut img = image::RgbImage::new(w as u32, h as u32);
    for (i, p) in img.pixels_mut().enumerate() {
        p.0 = if n == 1 {
            [px(planes[0][i]); 3]
        } else if rgb {
            [px(planes[0][i]), px(planes[1][i]), px(planes[2][i])]
        } else {
            let (y, cb, cr) = (planes[0][i], planes[1][i] - 128.0, planes[2][i] - 128.0);
            [px(y + 1.402 * cr), px(y - 0.344_136 * cb - 0.714_136 * cr), px(y + 1.772 * cb)]
        };
    }
    // At most 160 px on the long side, like camera thumbnails.
    let k = (160.0 / w.max(h) as f32).min(1.0);
    let (tw, th) = (((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1));
    let small = if (tw, th) == img.dimensions() { img } else { image::imageops::thumbnail(&img, tw, th) };
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, 80);
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
    enc.encode(small.as_raw(), tw as u16, th as u16, jpeg_encoder::ColorType::Rgb).ok()?;
    Some(out)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn xmp_values() {
        let x = r#"<rdf:Description exif:PixelXDimension="4000" exif:PixelYDimension = '3000' tiff:ImageWidthX="1"><tiff:ImageWidth>4000</tiff:ImageWidth></rdf:Description>"#;
        let s = set_xmp_size(x, (123, 45));
        assert_eq!(s, r#"<rdf:Description exif:PixelXDimension="123" exif:PixelYDimension='45' tiff:ImageWidthX="1"><tiff:ImageWidth>123</tiff:ImageWidth></rdf:Description>"#);
        // Nothing to change: the same text.
        assert_eq!(set_xmp_size("<a b=\"1\"/>", (1, 2)), "<a b=\"1\"/>");
    }

    #[test]
    fn xmp_gain_map_is_removed() {
        let x = "<x:xmpmeta><rdf:Description xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\" hdrgm:Version=\"1.0\" xmp:Rating=\"4\"\n   GCamera:MotionPhoto=\"1\">\
                 <Container:Directory><rdf:Seq><rdf:li/></rdf:Seq></Container:Directory><hdrgm:Gamma>1</hdrgm:Gamma><dc:title>Keep</dc:title></rdf:Description></x:xmpmeta>";
        let s = strip_extra_images(x);
        assert_eq!(s, "<x:xmpmeta><rdf:Description xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\" xmp:Rating=\"4\"><dc:title>Keep</dc:title></rdf:Description></x:xmpmeta>");
        // Text that only looks like a prefix stays.
        assert_eq!(remove_attrs("<a>hdrgm:x</a>", "hdrgm:"), "<a>hdrgm:x</a>");
        assert_eq!(remove_elements("<a><b/></a>", "c:"), "<a><b/></a>");
    }

    /// A little-endian TIFF block: IFD0 (orientation, EXIF pointer), the
    /// EXIF sub-IFD (pixel size as SHORT and LONG) and IFD1 with a thumbnail.
    pub(crate) fn sample_tiff(orientation: u16, thumb: &[u8]) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(b"II*\0");
        t.extend_from_slice(&8u32.to_le_bytes());
        let entry = |t: &mut Vec<u8>, tag: u16, typ: u16, v: u32| {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&typ.to_le_bytes());
            t.extend_from_slice(&1u32.to_le_bytes());
            t.extend_from_slice(&v.to_le_bytes());
        };
        // IFD0 at 8: 2 entries -> ends at 8 + 2 + 24 + 4 = 38.
        t.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut t, 0x0112, 3, orientation as u32);
        entry(&mut t, 0x8769, 4, 38);
        t.extend_from_slice(&68u32.to_le_bytes());
        // EXIF IFD at 38: 2 entries -> ends at 68.
        t.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut t, 0xA002, 3, 4000);
        entry(&mut t, 0xA003, 4, 3000);
        t.extend_from_slice(&0u32.to_le_bytes());
        // IFD1 at 68: 3 entries -> ends at 68 + 2 + 36 + 4 = 110.
        t.extend_from_slice(&3u16.to_le_bytes());
        entry(&mut t, 0x0103, 3, 6);
        entry(&mut t, 0x0201, 4, 110);
        entry(&mut t, 0x0202, 4, thumb.len() as u32);
        t.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(t.len(), 110);
        t.extend_from_slice(thumb);
        t
    }

    #[test]
    fn exif_size_orientation_and_thumbnail() {
        let mut t = sample_tiff(6, &[0xAB; 50]);
        assert_eq!(orientation(&t), Some(6));
        assert!(has_thumbnail(&t));
        update_exif(&mut t, (640, 480), true, Some(&[0xCD; 20]));
        let ex = exif::Reader::new().read_raw(t.clone()).unwrap();
        let get = |tag, ifd| ex.get_field(tag, ifd).and_then(|f| f.value.get_uint(0));
        assert_eq!(get(exif::Tag::PixelXDimension, exif::In::PRIMARY), Some(640));
        assert_eq!(get(exif::Tag::PixelYDimension, exif::In::PRIMARY), Some(480));
        assert_eq!(get(exif::Tag::Orientation, exif::In::PRIMARY), Some(1));
        // The old thumbnail (at the end) was replaced in place.
        assert_eq!(t.len(), 110 + 20);
        assert_eq!(get(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL), Some(20));
        assert_eq!(&t[110..], &[0xCD; 20]);
        // No new thumbnail: IFD1 is unlinked.
        update_exif(&mut t, (64, 48), false, None);
        assert!(!has_thumbnail(&t));
        assert_eq!(t.len(), 110);
        // Broken blocks are left alone without panicking.
        for n in 0..t.len() {
            let mut cut = t[..n].to_vec();
            update_exif(&mut cut, (1, 1), true, Some(&[1, 2, 3]));
            let _ = orientation(&cut);
        }
    }
}
