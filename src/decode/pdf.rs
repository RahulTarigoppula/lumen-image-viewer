//! PDF pages rendered with the built-in Windows PDF renderer (WinRT
//! Windows.Data.Pdf). Pages are exposed as frames with `delay_ms == 0`.

use super::{AnimFrame, Decoded};
use anyhow::{anyhow, bail, Result};
use image::RgbaImage;
use std::sync::Arc;
use windows::Data::Pdf::{PdfDocument, PdfPage, PdfPageRenderOptions};
use windows::Storage::Streams::{DataReader, DataWriter, IRandomAccessStream, InMemoryRandomAccessStream};

/// Long side of a rendered page.
pub const PAGE_SIDE: u32 = 2000;
/// Pages beyond this are not rendered (memory: ~16 MB per page at 2000 px).
pub const MAX_PAGES: usize = 30;

pub fn is_pdf(b: &[u8]) -> bool {
    b.len() >= 5 && b.windows(5).take(1024).any(|w| w == b"%PDF-")
}


/// WinRT PDF work runs on its own short-lived STA thread that uninitializes
/// COM when done. Rendering from
/// MTA threads that exit without that leaves the PDF runtime in a state that
/// crashes the process at exit (observed with the test harness).
fn on_worker<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    std::thread::Builder::new()
        .name("lumen-pdf".into())
        .spawn(move || {
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
            let init = unsafe { CoInitializeEx(None, if true { COINIT_APARTMENTTHREADED } else { COINIT_MULTITHREADED }) };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err(anyhow!("The PDF renderer crashed on this file")));
            if init.is_ok() {
                unsafe { CoUninitialize() };
            }
            r
        })
        .map_err(|_| anyhow!("PDF renderer is unavailable"))?
        .join()
        .map_err(|_| anyhow!("PDF renderer is unavailable"))?
}

fn open(bytes: &[u8]) -> Result<PdfDocument> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    writer.WriteBytes(bytes)?;
    writer.StoreAsync()?.join()?;
    writer.FlushAsync()?.join()?;
    writer.DetachStream()?;
    stream.Seek(0)?;
    let s: IRandomAccessStream = stream.into();
    PdfDocument::LoadFromStreamAsync(&s)?.join().map_err(|e| {
        if e.code().0 as u32 == 0x8007_052B {
            anyhow!("This PDF is password protected")
        } else {
            anyhow!("Cannot open PDF: {}", e.message())
        }
    })
}

fn render_page(page: &PdfPage, max_side: u32) -> Result<RgbaImage> {
    let size = page.Size()?;
    let (pw, ph) = (size.Width.max(1.0), size.Height.max(1.0));
    let s = max_side as f32 / pw.max(ph);
    let (w, h) = (((pw * s).round() as u32).max(1), ((ph * s).round() as u32).max(1));
    // The renderer scales the destination (in DIPs) by the display scale
    // factor, so ask for proportionally less on high-DPI systems.
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() }.max(96) as f32;
    let k = 96.0 / dpi;
    let opts = PdfPageRenderOptions::new()?;
    opts.SetDestinationWidth(((w as f32 * k).round() as u32).max(1))?;
    opts.SetDestinationHeight(((h as f32 * k).round() as u32).max(1))?;
    let out = InMemoryRandomAccessStream::new()?;
    page.RenderWithOptionsToStreamAsync(&out, &opts)?.join()?;
    let len = out.Size()? as usize;
    if len == 0 {
        bail!("PDF page rendered to an empty stream");
    }
    let input = out.GetInputStreamAt(0)?;
    let reader = DataReader::CreateDataReader(&input)?;
    reader.LoadAsync(len as u32)?.join()?;
    let mut buf = vec![0u8; len];
    reader.ReadBytes(&mut buf)?;
    // The default encoder is PNG.
    let img = image::load_from_memory(&buf)?.into_rgba8();
    if img.width().max(img.height()) > max_side + 2 {
        // Process DPI awareness differs from the system DPI: fix up the size.
        return Ok(crate::imaging::resize_to_fit(&img, w, h));
    }
    Ok(img)
}

/// Size of the first page as rendered (long side = PAGE_SIDE).
pub fn probe(bytes: &[u8]) -> Option<(u32, u32)> {
    let b = bytes.to_vec();
    on_worker(move || probe_inner(&b).ok_or_else(|| anyhow!("no page"))).ok()
}

fn probe_inner(bytes: &[u8]) -> Option<(u32, u32)> {
    let doc = open(bytes).ok()?;
    let size = doc.GetPage(0).ok()?.Size().ok()?;
    let (pw, ph) = (size.Width.max(1.0), size.Height.max(1.0));
    let s = PAGE_SIDE as f32 / pw.max(ph);
    Some(((pw * s).round() as u32, (ph * s).round() as u32))
}

pub fn page_count(bytes: &[u8]) -> Result<u32> {
    let b = bytes.to_vec();
    on_worker(move || Ok(open(&b)?.PageCount()?))
}

/// Render only the first page with its long side at `max_side` (thumbnails).
pub fn first_page(bytes: &[u8], max_side: u32) -> Result<RgbaImage> {
    let b = bytes.to_vec();
    on_worker(move || first_page_inner(&b, max_side))
}

fn first_page_inner(bytes: &[u8], max_side: u32) -> Result<RgbaImage> {
    let doc = open(bytes)?;
    if doc.PageCount()? == 0 {
        bail!("PDF has no pages");
    }
    render_page(&doc.GetPage(0)?, max_side)
}

pub fn decode(bytes: &[u8], all_pages: bool) -> Result<Decoded> {
    let b = bytes.to_vec();
    on_worker(move || decode_inner(&b, all_pages))
}

fn decode_inner(bytes: &[u8], all_pages: bool) -> Result<Decoded> {
    let doc = open(bytes)?;
    let count = doc.PageCount()? as usize;
    if count == 0 {
        bail!("PDF has no pages");
    }
    let n = if all_pages { count.min(MAX_PAGES) } else { 1 };
    let mut pages = Vec::with_capacity(n);
    for i in 0..n {
        let img = render_page(&doc.GetPage(i as u32)?, PAGE_SIDE)?;
        pages.push(AnimFrame { image: Arc::new(img), delay_ms: 0 });
    }
    let first = pages[0].image.clone();
    let size = first.dimensions();
    let format = if count > 1 { format!("PDF ({count} pages)") } else { "PDF".into() };
    Ok(Decoded {
        image: first,
        frames: if pages.len() > 1 { Some(Arc::new(pages)) } else { None },
        has_alpha: false,
        format,
        decoder: "Windows (PDF)",
        color_info: if count > n && all_pages { format!("Rendered · first {n} of {count} pages") } else { "Rendered".into() },
        icc_profile: None,
        exif: None,
        orientation: 1,
        source_size: size,
        frame_count: count,
        bit_depth: 8,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a tiny valid PDF with `pages` pages (612x792 pt), each with a
    /// filled black rectangle in the top-left quarter.
    pub fn sample_pdf(pages: usize) -> Vec<u8> {
        let mut objs: Vec<String> = Vec::new();
        let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 3 + i * 2)).collect();
        objs.push("<< /Type /Catalog /Pages 2 0 R >>".into());
        objs.push(format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), pages));
        for i in 0..pages {
            let content = "0 0 0 rg 0 396 306 396 re f";
            objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {} 0 R >>", 4 + i * 2));
            objs.push(format!("<< /Length {} >>\nstream\n{}\nendstream", content.len(), content));
        }
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, o).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", objs.len() + 1, xref).as_bytes());
        out
    }

    #[test]
    fn renders_pages_as_frames() {
        let f = sample_pdf(3);
        assert!(is_pdf(&f));
        let d = decode(&f, true).expect("pdf decode");
        assert_eq!(d.frame_count, 3);
        assert_eq!(d.format, "PDF (3 pages)");
        let frames = d.frames.as_ref().unwrap();
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|f| f.delay_ms == 0));
        let (w, h) = d.image.dimensions();
        assert!((h as i32 - PAGE_SIDE as i32).abs() <= 3, "height {h}");
        assert!((w as i32 - 1545).abs() <= 2, "width {w}");
        assert!(d.image.get_pixel(w / 4, h / 4).0[0] < 40);
        assert!(d.image.get_pixel(w * 3 / 4, h * 3 / 4).0[0] > 220);
        let t = first_page(&f, 256).unwrap();
        assert_eq!(t.height(), 256);
    }

    #[test]
    fn garbage_fails_cleanly() {
        assert!(decode(b"%PDF-1.4 garbage", false).is_err());
        assert!(decode(b"", false).is_err());
    }
}
