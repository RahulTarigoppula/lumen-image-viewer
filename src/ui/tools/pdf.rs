//! A minimal PDF writer: one JPEG image (DCTDecode) per page, streamed to
//! any `Write`, with a correct cross-reference table.

use std::io::{self, Write};

/// Counts bytes so object offsets can be recorded.
struct Counter<W: Write> {
    inner: W,
    pos: u64,
}

impl<W: Write> Write for Counter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Points (1/72 inch) for common paper sizes, portrait.
pub const A4_PT: (f32, f32) = (595.276, 841.89);
pub const LETTER_PT: (f32, f32) = (612.0, 792.0);

pub struct PdfWriter<W: Write> {
    out: Counter<W>,
    /// Byte offset of every object, indexed by object number - 1.
    offsets: Vec<u64>,
    pages: Vec<usize>,
}

/// Object numbers 1 and 2 are reserved for the catalog and the page tree,
/// which are written last (when all pages are known).
const CATALOG: usize = 1;
const PAGES: usize = 2;

impl<W: Write> PdfWriter<W> {
    pub fn new(w: W) -> io::Result<Self> {
        let mut out = Counter { inner: w, pos: 0 };
        // Header plus a binary comment so tools treat the file as binary.
        out.write_all(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n")?;
        Ok(PdfWriter { out, offsets: vec![0, 0], pages: Vec::new() })
    }

    fn reserve(&mut self) -> usize {
        self.offsets.push(0);
        self.offsets.len()
    }

    fn begin(&mut self, id: usize) -> io::Result<()> {
        self.offsets[id - 1] = self.out.pos;
        write!(self.out, "{id} 0 obj\n")
    }

    /// Add a page of `page_w` x `page_h` points showing a baseline JPEG of
    /// `px_w` x `px_h` pixels (RGB or grayscale) in the rectangle (x, y, w, h),
    /// in points from the bottom-left corner.
    #[allow(clippy::too_many_arguments)]
    pub fn add_jpeg_page(&mut self, jpeg: &[u8], px_w: u32, px_h: u32, gray: bool, page_w: f32, page_h: f32, rect: (f32, f32, f32, f32)) -> io::Result<()> {
        let img = self.reserve();
        let content = self.reserve();
        let page = self.reserve();
        self.begin(img)?;
        write!(
            self.out,
            "<< /Type /XObject /Subtype /Image /Width {px_w} /Height {px_h} /ColorSpace /{} /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            if gray { "DeviceGray" } else { "DeviceRGB" },
            jpeg.len()
        )?;
        self.out.write_all(jpeg)?;
        self.out.write_all(b"\nendstream\nendobj\n")?;
        let (x, y, w, h) = rect;
        let ops = format!("q\n{w:.3} 0 0 {h:.3} {x:.3} {y:.3} cm\n/Im0 Do\nQ\n");
        self.begin(content)?;
        write!(self.out, "<< /Length {} >>\nstream\n{ops}endstream\nendobj\n", ops.len())?;
        self.begin(page)?;
        write!(
            self.out,
            "<< /Type /Page /Parent {PAGES} 0 R /MediaBox [0 0 {page_w:.3} {page_h:.3}] /Resources << /XObject << /Im0 {img} 0 R >> /ProcSet [/PDF /ImageB /ImageC] >> /Contents {content} 0 R >>\nendobj\n"
        )?;
        self.pages.push(page);
        Ok(())
    }

    /// Write the page tree, catalog, cross-reference table and trailer.
    pub fn finish(mut self, title: &str) -> io::Result<W> {
        let kids: Vec<String> = self.pages.iter().map(|p| format!("{p} 0 R")).collect();
        self.begin(PAGES)?;
        write!(self.out, "<< /Type /Pages /Kids [{}] /Count {} >>\nendobj\n", kids.join(" "), self.pages.len())?;
        self.begin(CATALOG)?;
        write!(self.out, "<< /Type /Catalog /Pages {PAGES} 0 R >>\nendobj\n")?;
        let info = self.reserve();
        self.begin(info)?;
        write!(self.out, "<< /Title ({}) /Producer (Lumen) >>\nendobj\n", pdf_string(title))?;
        let xref = self.out.pos;
        let n = self.offsets.len() + 1;
        write!(self.out, "xref\n0 {n}\n0000000000 65535 f\r\n")?;
        for off in &self.offsets {
            write!(self.out, "{off:010} 00000 n\r\n")?;
        }
        write!(self.out, "trailer\n<< /Size {n} /Root {CATALOG} 0 R /Info {info} 0 R >>\nstartxref\n{xref}\n%%EOF\n")?;
        self.out.flush()?;
        Ok(self.out.inner)
    }
}

/// Escape a string for a PDF literal; non-ASCII characters become '?'.
fn pdf_string(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

/// Where an image goes on a page: (x, y, w, h) in points from the bottom-left,
/// fitted inside the margins and centered.
pub fn fit_rect(page_w: f32, page_h: f32, margin: f32, img_w: u32, img_h: u32) -> (f32, f32, f32, f32) {
    let aw = (page_w - 2.0 * margin).max(1.0);
    let ah = (page_h - 2.0 * margin).max(1.0);
    let s = (aw / img_w.max(1) as f32).min(ah / img_h.max(1) as f32);
    let (w, h) = (img_w as f32 * s, img_h as f32 * s);
    ((page_w - w) / 2.0, (page_h - h) / 2.0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).rposition(|w| w == needle)
    }

    /// Parse the trailer and xref and check every offset points at its object.
    pub fn check_structure(pdf: &[u8]) -> usize {
        assert!(pdf.starts_with(b"%PDF-1."), "header");
        assert!(pdf.ends_with(b"%%EOF\n"), "eof marker");
        let sx = rfind(pdf, b"startxref\n").expect("startxref");
        let tail = std::str::from_utf8(&pdf[sx + 10..]).unwrap();
        let xref_off: usize = tail.lines().next().unwrap().trim().parse().unwrap();
        assert_eq!(&pdf[xref_off..xref_off + 5], b"xref\n", "startxref points at the xref table");
        let table = std::str::from_utf8(&pdf[xref_off..sx]).unwrap();
        let mut lines = table.split('\n');
        assert_eq!(lines.next(), Some("xref"));
        let head: Vec<usize> = lines.next().unwrap().split(' ').map(|v| v.parse().unwrap()).collect();
        assert_eq!(head[0], 0);
        let n = head[1];
        // Entries are exactly 20 bytes each.
        let entries_start = xref_off + 5 + format!("0 {n}\n").len();
        for i in 0..n {
            let e = &pdf[entries_start + i * 20..entries_start + (i + 1) * 20];
            let e = std::str::from_utf8(e).unwrap();
            assert!(e.ends_with("\r\n"), "entry {i} is 20 bytes: {e:?}");
            if i == 0 {
                assert_eq!(e, "0000000000 65535 f\r\n");
                continue;
            }
            let off: usize = e[..10].parse().unwrap();
            let want = format!("{i} 0 obj\n");
            assert_eq!(&pdf[off..off + want.len()], want.as_bytes(), "object {i} offset");
        }
        let trailer = std::str::from_utf8(&pdf[entries_start + n * 20..sx]).unwrap();
        assert!(trailer.starts_with("trailer\n"), "{trailer}");
        assert!(trailer.contains(&format!("/Size {n}")), "{trailer}");
        assert!(trailer.contains("/Root 1 0 R"));
        n
    }

    #[test]
    fn pdf_is_structurally_valid() {
        let img = image::RgbImage::from_fn(40, 30, |x, y| image::Rgb([(x * 6) as u8, (y * 8) as u8, 99]));
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 80).encode(img.as_raw(), 40, 30, jpeg_encoder::ColorType::Rgb).unwrap();
        let mut w = PdfWriter::new(Vec::new()).unwrap();
        let r = fit_rect(A4_PT.0, A4_PT.1, 36.0, 40, 30);
        w.add_jpeg_page(&jpeg, 40, 30, false, A4_PT.0, A4_PT.1, r).unwrap();
        w.add_jpeg_page(&jpeg, 40, 30, false, 40.0, 30.0, (0.0, 0.0, 40.0, 30.0)).unwrap();
        let pdf = w.finish("Test (1) \\ ok").unwrap();
        // 2 reserved + 3 per page + info, plus the free entry.
        assert_eq!(check_structure(&pdf), 2 + 6 + 1 + 1);
        assert!(find(&pdf, b"/Count 2").is_some());
        assert!(find(&pdf, b"/Kids [5 0 R 8 0 R]").is_some());
        assert!(find(&pdf, b"(Test \\(1\\) \\\\ ok)").is_some());
        // The JPEG is embedded verbatim.
        assert!(find(&pdf, &jpeg[..64]).is_some());
    }

    #[test]
    fn empty_pdf_is_valid() {
        let pdf = PdfWriter::new(Vec::new()).unwrap().finish("").unwrap();
        check_structure(&pdf);
    }

    #[test]
    fn fit_rect_centers() {
        let (x, y, w, h) = fit_rect(200.0, 100.0, 0.0, 50, 50);
        assert_eq!((x, y, w, h), (50.0, 0.0, 100.0, 100.0));
        let (x, y, w, h) = fit_rect(200.0, 100.0, 10.0, 400, 100);
        assert_eq!((x, w), (10.0, 180.0));
        assert!((y - 27.5).abs() < 1e-3 && (h - 45.0).abs() < 1e-3);
    }
}
