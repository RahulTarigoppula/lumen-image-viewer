//! PDF writer: one page per laid-out slide, Helvetica text (WinAnsi), JPEG
//! pictures embedded as-is (DCTDecode). No external dependencies.

use super::{layout, wrap, leading, text_width, Align, Box2, El, Report, Table, INK, MUTED, PAGE_H, RULE};
use anyhow::Result;
use std::fmt::Write as _;
use std::sync::Arc;

/// Map a char to WinAnsi (cp1252); unknown characters become '?'.
fn winansi(c: char) -> u8 {
    let u = c as u32;
    match u {
        0x20..=0x7E => u as u8,
        0xA0..=0xFF => u as u8,
        _ => match c {
            '€' => 0x80,
            '‚' => 0x82,
            '„' => 0x84,
            '\u{2026}' => 0x85,
            '\u{2018}' => 0x91,
            '\u{2019}' => 0x92,
            '\u{201C}' => 0x93,
            '\u{201D}' => 0x94,
            '•' => 0x95,
            '\u{2013}' => 0x96,
            '\u{2014}' => 0x97,
            '™' => 0x99,
            '\t' => b' ',
            _ => b'?',
        },
    }
}

fn pdf_string(s: &str) -> Vec<u8> {
    let mut o = vec![b'('];
    for c in s.chars() {
        let b = winansi(c);
        if matches!(b, b'(' | b')' | b'\\') {
            o.push(b'\\');
        }
        o.push(b);
    }
    o.push(b')');
    o
}

fn rgb(c: [u8; 3]) -> String {
    format!("{:.3} {:.3} {:.3}", c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0)
}

/// Content stream builder with top-left coordinates.
struct Page {
    ops: Vec<u8>,
    /// Image XObject names used on this page.
    images: Vec<(String, usize)>,
}

impl Page {
    fn raw(&mut self, s: &str) {
        self.ops.extend_from_slice(s.as_bytes());
    }
    fn y(&self, top: f32) -> f32 {
        PAGE_H - top
    }
    fn fill(&mut self, b: Box2, c: [u8; 3]) {
        let s = format!("{} rg {:.2} {:.2} {:.2} {:.2} re f\n", rgb(c), b.x, self.y(b.y + b.h), b.w, b.h);
        self.raw(&s);
    }
    fn frame(&mut self, b: Box2, c: [u8; 3], w: f32) {
        let s = format!("{} RG {w:.2} w {:.2} {:.2} {:.2} {:.2} re S\n", rgb(c), b.x, self.y(b.y + b.h), b.w, b.h);
        self.raw(&s);
    }
    fn text_line(&mut self, x: f32, top: f32, s: &str, size: f32, bold: bool, c: [u8; 3]) {
        // Baseline about 80% down the line box.
        let base = self.y(top + size * 0.95);
        let f = if bold { "F2" } else { "F1" };
        let head = format!("BT /{f} {size:.2} Tf {} rg {x:.2} {base:.2} Td ", rgb(c));
        self.raw(&head);
        let s = pdf_string(s);
        self.ops.extend_from_slice(&s);
        self.raw(" Tj ET\n");
    }
    fn text(&mut self, b: Box2, s: &str, size: f32, bold: bool, c: [u8; 3], align: Align) {
        let lh = leading(size);
        let mut top = b.y;
        for line in wrap(s, size, bold, b.w + 0.5) {
            if top + lh > b.y + b.h + lh * 0.3 {
                break;
            }
            let x = match align {
                Align::Left => b.x,
                Align::Center => b.x + (b.w - text_width(&line, size, bold)) / 2.0,
            };
            self.text_line(x, top, &line, size, bold, c);
            top += lh;
        }
    }
    fn image(&mut self, b: Box2, idx: usize) {
        let name = format!("Im{}", idx + 1);
        let s = format!("q {:.2} 0 0 {:.2} {:.2} {:.2} cm /{name} Do Q\n", b.w, b.h, b.x, self.y(b.y + b.h));
        self.raw(&s);
        if !self.images.iter().any(|(n, _)| *n == name) {
            self.images.push((name, idx));
        }
    }
    fn table(&mut self, b: Box2, t: &Table) {
        let cols = t.header.len().max(1);
        let first_w = (b.w * 0.34).min(260.0);
        let other_w = if cols > 1 { (b.w - first_w) / (cols - 1) as f32 } else { b.w };
        let row_h = b.h / (t.rows.len() + 1) as f32;
        let col_x = |c: usize| if c == 0 { b.x } else { b.x + first_w + (c - 1) as f32 * other_w };
        let col_w = |c: usize| if c == 0 && cols > 1 { first_w } else { other_w };
        self.fill(Box2::new(b.x, b.y, b.w, row_h), [47, 111, 235]);
        let size = 11.0;
        let pad = 6.0;
        let ty = |row: usize| b.y + row as f32 * row_h + (row_h - leading(size)) / 2.0;
        for (c, h) in t.header.iter().enumerate() {
            let cb = Box2::new(col_x(c) + pad, ty(0), col_w(c) - 2.0 * pad, leading(size));
            self.text(cb, h, size, true, [255, 255, 255], if c > 0 { Align::Center } else { Align::Left });
        }
        for (k, r) in t.rows.iter().enumerate() {
            let rb = Box2::new(b.x, b.y + (k + 1) as f32 * row_h, b.w, row_h);
            if k % 2 == 1 {
                self.fill(rb, [245, 247, 250]);
            }
            self.fill(Box2::new(b.x, rb.y + rb.h - 0.5, b.w, 0.5), RULE);
            for c in 0..cols {
                let s = r.get(c).map(String::as_str).unwrap_or("");
                let cb = Box2::new(col_x(c) + pad, ty(k + 1), col_w(c) - 2.0 * pad, leading(size));
                self.text(cb, s, size, c == 0, if c == 0 { INK } else { MUTED }, if c > 0 { Align::Center } else { Align::Left });
            }
        }
    }
}

/// Render the report as PDF bytes.
pub fn render(r: &Report) -> Result<Vec<u8>> {
    let slides = layout(r);
    let mut pics: Vec<(Arc<Vec<u8>>, u32, u32)> = Vec::new();
    let mut pages = Vec::new();
    for s in &slides {
        let mut p = Page { ops: Vec::new(), images: Vec::new() };
        for e in &s.els {
            match e {
                El::Fill { bx, color } => p.fill(*bx, *color),
                El::Frame { bx, color, width } => p.frame(*bx, *color, *width),
                El::Image { bx, pic } => {
                    let idx = match pics.iter().position(|(j, _, _)| Arc::ptr_eq(j, &pic.jpeg)) {
                        Some(i) => i,
                        None => {
                            pics.push((pic.jpeg.clone(), pic.w, pic.h));
                            pics.len() - 1
                        }
                    };
                    p.image(*bx, idx);
                }
                El::Text(t) => p.text(t.bx, &t.text, t.size, t.bold, t.color, t.align),
                El::Table { bx, table } => p.table(*bx, table),
            }
        }
        pages.push(p);
    }

    // Objects: 1 catalog, 2 pages, 3 F1, 4 F2, 5.. images, then page/content pairs.
    let mut out: Vec<u8> = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets: Vec<usize> = Vec::new();
    let img_base = 5;
    let page_base = img_base + pics.len();
    let total = page_base + pages.len() * 2 - 1;
    let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]| {
        offsets.push(out.len());
        let n = offsets.len();
        out.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };
    obj(&mut out, &mut offsets, b"<< /Type /Catalog /Pages 2 0 R >>");
    let kids: String = (0..pages.len()).map(|i| format!("{} 0 R ", page_base + i * 2)).collect();
    obj(&mut out, &mut offsets, format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", pages.len()).as_bytes());
    obj(&mut out, &mut offsets, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>");
    obj(&mut out, &mut offsets, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>");
    for (j, w, h) in &pics {
        let mut body = format!("<< /Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n", j.len()).into_bytes();
        body.extend_from_slice(j);
        body.extend_from_slice(b"\nendstream");
        obj(&mut out, &mut offsets, &body);
    }
    for (i, p) in pages.iter().enumerate() {
        let xobj: String = p.images.iter().map(|(n, idx)| format!("/{n} {} 0 R ", img_base + idx)).collect();
        let content_id = page_base + i * 2 + 1;
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << /F1 3 0 R /F2 4 0 R >> /XObject << {xobj}>> >> /Contents {content_id} 0 R >>",
            super::PAGE_W,
            PAGE_H
        );
        obj(&mut out, &mut offsets, page.as_bytes());
        let mut c = format!("<< /Length {} >>\nstream\n", p.ops.len()).into_bytes();
        c.extend_from_slice(&p.ops);
        c.extend_from_slice(b"\nendstream");
        obj(&mut out, &mut offsets, &c);
    }
    debug_assert_eq!(offsets.len(), total);
    let xref_at = out.len();
    let mut x = format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1);
    for o in &offsets {
        let _ = writeln!(x, "{o:010} 00000 n ");
    }
    let info_title = String::from_utf8_lossy(&pdf_string(&r.title)).to_string();
    let _ = write!(x, "trailer\n<< /Size {} /Root 1 0 R /Info << /Title {info_title} /Producer (Lumen) >> >>\nstartxref\n{xref_at}\n%%EOF\n", offsets.len() + 1);
    out.extend_from_slice(x.as_bytes());
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn pdf_is_well_formed() {
        let r = super::super::tests::sample(2);
        let b = super::render(&r).unwrap();
        assert!(b.starts_with(b"%PDF-1.4"));
        let tail = String::from_utf8_lossy(&b[b.len() - 64..]).to_string();
        assert!(tail.contains("%%EOF"));
        // Each xref offset points at "N 0 obj".
        let s = String::from_utf8_lossy(&b).to_string();
        let xref = s.rfind("xref\n").unwrap();
        for (k, line) in s[xref..].lines().skip(3).take_while(|l| l.ends_with(" n ")).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(b[off..].starts_with(format!("{} 0 obj", k + 1).as_bytes()), "object {}", k + 1);
        }
        assert_eq!(super::winansi('×'), 0xD7);
        assert_eq!(super::winansi('·'), 0xB7);
        assert_eq!(super::pdf_string("a(b)"), b"(a\\(b\\))".to_vec());
    }
}
