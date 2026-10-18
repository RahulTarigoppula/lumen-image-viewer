//! Printing through the classic Windows GDI print path: page layout math
//! (fit / fill / actual size / custom size, 1-9 images per page), the system
//! print dialog and a background job that renders and spools the pages.

use image::RgbaImage;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Where the image goes on the printable area of the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Fit an `iw` × `ih` image inside a `pw` × `ph` printable area with `margin`
/// device pixels on each side, preserving aspect ratio and centering it.
pub fn fit_on_page(iw: u32, ih: u32, pw: i32, ph: i32, margin: i32) -> Placement {
    let aw = (pw - 2 * margin).max(1) as f64;
    let ah = (ph - 2 * margin).max(1) as f64;
    let s = (aw / iw.max(1) as f64).min(ah / ih.max(1) as f64);
    let w = ((iw as f64 * s).round() as i32).max(1);
    let h = ((ih as f64 * s).round() as i32).max(1);
    Placement { x: (pw - w) / 2, y: (ph - h) / 2, w, h }
}

/// Top-down 32-bit BGRA pixels for a DIB (alpha flattened onto white paper).
pub fn to_bgra(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.as_raw().len());
    for p in img.as_raw().chunks_exact(4) {
        let a = p[3] as u32;
        let c = |v: u8| ((v as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        out.extend_from_slice(&[c(p[2]), c(p[1]), c(p[0]), 0]);
    }
    out
}

// ------------------------------------------------------------------ options

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Sizing {
    /// As large as possible, whole image visible.
    Fit,
    /// Cover the whole cell, cropping the overflow.
    Fill,
    /// The image's own DPI (or the assumed DPI when it has none).
    Actual,
    /// A box of the given size; the image is fitted inside it.
    Custom,
}

impl Sizing {
    pub const ALL: [Sizing; 4] = [Sizing::Fit, Sizing::Fill, Sizing::Actual, Sizing::Custom];
    pub fn label(self) -> &'static str {
        match self {
            Sizing::Fit => "Fit to page",
            Sizing::Fill => "Fill page (crop)",
            Sizing::Actual => "Actual size (image DPI)",
            Sizing::Custom => "Custom size",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Unit {
    Cm,
    Inch,
}

impl Unit {
    pub fn to_inches(self, v: f32) -> f32 {
        match self {
            Unit::Cm => v / 2.54,
            Unit::Inch => v,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Unit::Cm => "cm",
            Unit::Inch => "in",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Orientation {
    Auto,
    Portrait,
    Landscape,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PrintOptions {
    pub sizing: Sizing,
    pub custom_w: f32,
    pub custom_h: f32,
    pub unit: Unit,
    /// Center in the cell (otherwise top-left).
    pub center: bool,
    /// Page margin in mm, measured from the paper edge.
    pub margin_mm: f32,
    /// Gap between images on multi-image pages, mm.
    pub gap_mm: f32,
    pub copies: u16,
    /// 1, 2, 4, 6 or 9 images per page.
    pub per_page: u8,
    pub orientation: Orientation,
    /// DPI used for "actual size" when a file stores none.
    pub assume_dpi: f32,
}

impl Default for PrintOptions {
    fn default() -> Self {
        PrintOptions {
            sizing: Sizing::Fit,
            custom_w: 15.0,
            custom_h: 10.0,
            unit: Unit::Cm,
            center: true,
            margin_mm: 5.0,
            gap_mm: 4.0,
            copies: 1,
            per_page: 1,
            orientation: Orientation::Auto,
            assume_dpi: 96.0,
        }
    }
}

pub const PER_PAGE_CHOICES: [u8; 5] = [1, 2, 4, 6, 9];

/// Grid (cols, rows) for `per_page` images on a page of the given orientation.
pub fn grid_for(per_page: u8, landscape: bool) -> (u32, u32) {
    let (a, b) = match per_page {
        0 | 1 => (1, 1),
        2 => (1, 2),
        3 | 4 => (2, 2),
        5 | 6 => (2, 3),
        _ => (3, 3),
    };
    if landscape {
        (b, a)
    } else {
        (a, b)
    }
}

/// Pick the page orientation: explicit, or from the images' shapes.
pub fn choose_landscape(opts: &PrintOptions, dims: &[(u32, u32)]) -> bool {
    match opts.orientation {
        Orientation::Portrait => false,
        Orientation::Landscape => true,
        Orientation::Auto => {
            let wide = dims.iter().filter(|(w, h)| w > h).count();
            let tall = dims.iter().filter(|(w, h)| h > w).count();
            let mostly_wide = wide > tall;
            // Two per page: two wide images stack on a portrait page.
            if opts.per_page == 2 {
                !mostly_wide && tall > 0
            } else {
                mostly_wide
            }
        }
    }
}

/// The printer page in device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageInfo {
    /// Physical paper size.
    pub paper_w: i32,
    pub paper_h: i32,
    /// Printable area origin on the paper and its size.
    pub offset_x: i32,
    pub offset_y: i32,
    pub printable_w: i32,
    pub printable_h: i32,
    pub dpi_x: f32,
    pub dpi_y: f32,
}

impl PageInfo {
    /// A page of `w_in` x `h_in` inches at `dpi`, all printable.
    pub fn paper(w_in: f32, h_in: f32, dpi: f32) -> PageInfo {
        let (w, h) = ((w_in * dpi).round() as i32, (h_in * dpi).round() as i32);
        PageInfo { paper_w: w, paper_h: h, offset_x: 0, offset_y: 0, printable_w: w, printable_h: h, dpi_x: dpi, dpi_y: dpi }
    }
}

/// One image placed on a page: destination rectangle in printable-area
/// coordinates and the source rectangle (in image pixels) that fills it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub dst: Placement,
    pub src: (u32, u32, u32, u32),
}

/// Lay out up to `per_page` images (`dims`, each with its own dpi or None) on
/// one page. Destinations are clipped to the printable area.
pub fn layout_page(page: &PageInfo, opts: &PrintOptions, dims: &[((u32, u32), Option<f32>)], landscape: bool) -> Vec<Placed> {
    let (cols, rows) = grid_for(opts.per_page, landscape);
    let mm = |v: f32, dpi: f32| (v.max(0.0) / 25.4 * dpi).round() as i32;
    // Margin from the paper edge; the unprintable border counts towards it.
    let (mx, my) = (mm(opts.margin_mm, page.dpi_x), mm(opts.margin_mm, page.dpi_y));
    let left = mx.max(page.offset_x) - page.offset_x;
    let top = my.max(page.offset_y) - page.offset_y;
    let right_edge = (page.paper_w - mx).min(page.offset_x + page.printable_w) - page.offset_x;
    let bottom_edge = (page.paper_h - my).min(page.offset_y + page.printable_h) - page.offset_y;
    let (area_w, area_h) = ((right_edge - left).max(1), (bottom_edge - top).max(1));
    let (gx, gy) = if cols * rows > 1 { (mm(opts.gap_mm, page.dpi_x), mm(opts.gap_mm, page.dpi_y)) } else { (0, 0) };
    let cell_w = ((area_w - gx * (cols as i32 - 1)) / cols as i32).max(1);
    let cell_h = ((area_h - gy * (rows as i32 - 1)) / rows as i32).max(1);
    let mut out = Vec::new();
    for (k, &((iw, ih), img_dpi)) in dims.iter().enumerate().take((cols * rows) as usize) {
        if iw == 0 || ih == 0 {
            continue;
        }
        let (c, r) = (k as i32 % cols as i32, k as i32 / cols as i32);
        let (cx, cy) = (left + c * (cell_w + gx), top + r * (cell_h + gy));
        let (iwf, ihf) = (iw as f64, ih as f64);
        // Destination size before clipping to the cell (device px).
        let (dw, dh) = match opts.sizing {
            Sizing::Fit => {
                let s = (cell_w as f64 / iwf).min(cell_h as f64 / ihf);
                (iwf * s, ihf * s)
            }
            Sizing::Fill => {
                let s = (cell_w as f64 / iwf).max(cell_h as f64 / ihf);
                (iwf * s, ihf * s)
            }
            Sizing::Actual => {
                let d = img_dpi.filter(|d| *d >= 1.0).unwrap_or(opts.assume_dpi.max(1.0)) as f64;
                (iwf / d * page.dpi_x as f64, ihf / d * page.dpi_y as f64)
            }
            Sizing::Custom => {
                let bw = opts.unit.to_inches(opts.custom_w) as f64 * page.dpi_x as f64;
                let bh = opts.unit.to_inches(opts.custom_h) as f64 * page.dpi_y as f64;
                let s = (bw / iwf).min(bh / ihf);
                (iwf * s, ihf * s)
            }
        };
        let (dw, dh) = (dw.max(1.0), dh.max(1.0));
        // Clip to the cell, cropping the matching part of the source.
        let vw = dw.min(cell_w as f64);
        let vh = dh.min(cell_h as f64);
        let (sx_scale, sy_scale) = (iwf / dw, ihf / dh);
        let sw = (vw * sx_scale).round().clamp(1.0, iwf) as u32;
        let sh = (vh * sy_scale).round().clamp(1.0, ihf) as u32;
        // Overflowing images show their centre (or top-left when not centred).
        let (sx, sy) = if opts.center || opts.sizing == Sizing::Fill { ((iw - sw) / 2, (ih - sh) / 2) } else { (0, 0) };
        let (vw, vh) = (vw.round() as i32, vh.round() as i32);
        let (x, y) = if opts.center { (cx + (cell_w - vw) / 2, cy + (cell_h - vh) / 2) } else { (cx, cy) };
        out.push(Placed { dst: Placement { x, y, w: vw.max(1), h: vh.max(1) }, src: (sx, sy, sw, sh) });
    }
    out
}

/// Number of pages for `n` images.
pub fn page_count(n: usize, per_page: u8) -> usize {
    n.div_ceil(per_page.max(1) as usize).max(1)
}

/// Highest resolution we send to the printer; the driver scales up the rest.
pub const MAX_SEND_DPI: f32 = 300.0;

/// Crop and resample `img` for a placement, at no more than `MAX_SEND_DPI`.
/// Returns the pixels to send (never larger than the placed size).
pub fn prepare_for_placement(img: &RgbaImage, p: &Placed, printer_dpi: f32) -> RgbaImage {
    let (sx, sy, sw, sh) = p.src;
    let cropped = if (sx, sy, sw, sh) == (0, 0, img.width(), img.height()) { img.clone() } else { image::imageops::crop_imm(img, sx, sy, sw, sh).to_image() };
    let f = (MAX_SEND_DPI / printer_dpi.max(1.0)).min(1.0);
    let tw = ((p.dst.w as f32 * f).round() as u32).clamp(1, cropped.width().max(1));
    let th = ((p.dst.h as f32 * f).round() as u32).clamp(1, cropped.height().max(1));
    if (tw, th) == cropped.dimensions() {
        cropped
    } else {
        crate::imaging::resize_exact(&cropped, tw, th, crate::imaging::Filter::Lanczos3)
    }
}

// ---------------------------------------------------------------- sources

/// Something to print: a file, or an image already in memory.
#[derive(Clone)]
pub enum PrintSource {
    File(PathBuf),
    Image { img: Arc<RgbaImage>, name: String },
}

impl PrintSource {
    pub fn name(&self) -> String {
        match self {
            PrintSource::File(p) => crate::ui::batch::file_name(p),
            PrintSource::Image { name, .. } => name.clone(),
        }
    }

    /// Pixel size without decoding (when possible).
    pub fn dims(&self) -> Option<(u32, u32)> {
        match self {
            PrintSource::File(p) => crate::decode::probe_dimensions(p),
            PrintSource::Image { img, .. } => Some(img.dimensions()),
        }
    }

    pub fn dpi(&self) -> Option<f32> {
        match self {
            PrintSource::File(p) => crate::ui::batch::pipeline::read_dpi_file(p),
            PrintSource::Image { .. } => None,
        }
    }

    pub fn load(&self) -> Result<Arc<RgbaImage>, String> {
        match self {
            PrintSource::File(p) => crate::decode::decode_file(p, Default::default()).map(|d| d.image).map_err(|e| format!("{}: {e:#}", crate::ui::batch::file_name(p))),
            PrintSource::Image { img, .. } => Ok(img.clone()),
        }
    }
}

/// Shared progress of a print job.
#[derive(Default)]
pub struct PrintProgress {
    pub pages_done: AtomicUsize,
    pub pages_total: AtomicUsize,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
}

// ---------------------------------------------------------- Windows printing

#[cfg(windows)]
mod win {
    use super::*;
    use anyhow::anyhow;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::UI::Controls::Dialogs::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    /// A printer device context chosen in the print dialog. It is used by one
    /// thread at a time (handed from the UI thread to the print job).
    pub struct Printer {
        hdc: HDC,
        pub copies: u32,
        pub page: PageInfo,
    }

    // SAFETY: GDI device contexts are process-wide handles; the job thread is
    // the only user once the dialog has returned.
    unsafe impl Send for Printer {}

    impl Drop for Printer {
        fn drop(&mut self) {
            unsafe {
                let _ = DeleteDC(self.hdc);
            }
        }
    }

    /// Show the system print dialog (modal, on the calling thread). Ok(None)
    /// when the user cancels.
    pub fn choose_printer(landscape: bool, copies: u16) -> anyhow::Result<Option<Printer>> {
        unsafe {
            let owner = GetActiveWindow();
            let mut pd = PRINTDLGW { lStructSize: std::mem::size_of::<PRINTDLGW>() as u32, hwndOwner: owner, Flags: PD_RETURNDEFAULT, ..Default::default() };
            if PrintDlgW(&mut pd).as_bool() && !pd.hDevMode.is_invalid() {
                let dm = GlobalLock(pd.hDevMode) as *mut DEVMODEW;
                if !dm.is_null() {
                    (*dm).dmFields |= DM_ORIENTATION | DM_COPIES;
                    (*dm).Anonymous1.Anonymous1.dmOrientation = if landscape { DMORIENT_LANDSCAPE as i16 } else { DMORIENT_PORTRAIT as i16 };
                    (*dm).Anonymous1.Anonymous1.dmCopies = copies.clamp(1, 999) as i16;
                    let _ = GlobalUnlock(pd.hDevMode);
                }
            }
            pd.Flags = PD_RETURNDC | PD_NOSELECTION | PD_NOPAGENUMS | PD_USEDEVMODECOPIESANDCOLLATE;
            pd.nCopies = copies.clamp(1, 999);
            let ok = PrintDlgW(&mut pd).as_bool();
            let free = |h: HGLOBAL| {
                if !h.is_invalid() {
                    let _ = GlobalFree(Some(h));
                }
            };
            free(pd.hDevMode);
            free(pd.hDevNames);
            if !ok {
                let err = CommDlgExtendedError();
                return match err.0 {
                    0 => Ok(None),
                    0x1008 => Err(anyhow!("No printer is installed")),
                    e => Err(anyhow!("The print dialog failed (error {e:#x})")),
                };
            }
            let hdc = pd.hDC;
            if hdc.is_invalid() {
                return Err(anyhow!("No printer device context was returned"));
            }
            let cap = |i| GetDeviceCaps(Some(hdc), i);
            let page = PageInfo {
                paper_w: cap(PHYSICALWIDTH),
                paper_h: cap(PHYSICALHEIGHT),
                offset_x: cap(PHYSICALOFFSETX),
                offset_y: cap(PHYSICALOFFSETY),
                printable_w: cap(HORZRES),
                printable_h: cap(VERTRES),
                dpi_x: cap(LOGPIXELSX).max(72) as f32,
                dpi_y: cap(LOGPIXELSY).max(72) as f32,
            };
            let printer = Printer { hdc, copies: (pd.nCopies as u32).max(1), page };
            if page.printable_w <= 0 || page.printable_h <= 0 {
                return Err(anyhow!("The printer reported an empty page"));
            }
            // A page reported without physical size (some virtual printers).
            let mut printer = printer;
            if printer.page.paper_w <= 0 || printer.page.paper_h <= 0 {
                printer.page.paper_w = page.printable_w;
                printer.page.paper_h = page.printable_h;
                printer.page.offset_x = 0;
                printer.page.offset_y = 0;
            }
            Ok(Some(printer))
        }
    }

    fn draw(hdc: HDC, img: &RgbaImage, dst: Placement) -> bool {
        let (iw, ih) = img.dimensions();
        let bits = to_bgra(img);
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: iw as i32,
                biHeight: -(ih as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
            StretchDIBits(hdc, dst.x, dst.y, dst.w, dst.h, 0, 0, iw as i32, ih as i32, Some(bits.as_ptr() as *const _), &bmi, DIB_RGB_COLORS, SRCCOPY) != 0
        }
    }

    /// Render and spool every page. Images are decoded one at a time and
    /// resampled to their placed size first, so memory stays bounded.
    pub fn run_job(printer: Printer, sources: &[PrintSource], opts: &PrintOptions, title: &str, prog: &PrintProgress) -> anyhow::Result<()> {
        let hdc = printer.hdc;
        let dims: Vec<((u32, u32), Option<f32>)> = sources.iter().map(|s| (s.dims().unwrap_or((1, 1)), if opts.sizing == Sizing::Actual { s.dpi() } else { None })).collect();
        let shapes: Vec<(u32, u32)> = dims.iter().map(|d| d.0).collect();
        let landscape = printer.page.paper_w > printer.page.paper_h || (printer.page.paper_w == printer.page.paper_h && choose_landscape(opts, &shapes));
        let per = opts.per_page.max(1) as usize;
        let pages = page_count(sources.len(), opts.per_page);
        let copies = printer.copies.clamp(1, 999) as usize;
        prog.pages_total.store(pages * copies, Ordering::SeqCst);
        let doc_name = HSTRING::from(if title.is_empty() { "Lumen" } else { title });
        let di = DOCINFOW { cbSize: std::mem::size_of::<DOCINFOW>() as i32, lpszDocName: PCWSTR(doc_name.as_ptr()), ..Default::default() };
        unsafe {
            if StartDocW(hdc, &di) <= 0 {
                return Err(anyhow!("Could not start the print job"));
            }
            let abort = |e: anyhow::Error| -> anyhow::Result<()> {
                AbortDoc(hdc);
                Err(e)
            };
            for _copy in 0..copies {
                for page in 0..pages {
                    if prog.cancel.load(Ordering::Relaxed) {
                        AbortDoc(hdc);
                        return Ok(());
                    }
                    let range = page * per..((page + 1) * per).min(sources.len());
                    let placed = layout_page(&printer.page, opts, &dims[range.clone()], landscape);
                    if StartPage(hdc) <= 0 {
                        return abort(anyhow!("Could not start a page"));
                    }
                    for (k, p) in placed.iter().enumerate() {
                        let src = &sources[range.start + k];
                        let img = match src.load() {
                            Ok(i) => i,
                            Err(e) => return abort(anyhow!(e)),
                        };
                        // Layout used probed sizes; re-place if decoding disagrees (EXIF rotation).
                        let p = if img.dimensions() != dims[range.start + k].0 {
                            let mut d = dims[range.clone()].to_vec();
                            d[k].0 = img.dimensions();
                            layout_page(&printer.page, opts, &d, landscape)[k]
                        } else {
                            *p
                        };
                        let pixels = prepare_for_placement(&img, &p, printer.page.dpi_x);
                        drop(img);
                        if !draw(hdc, &pixels, p.dst) {
                            return abort(anyhow!("The printer driver rejected the image"));
                        }
                    }
                    if EndPage(hdc) <= 0 {
                        return abort(anyhow!("Could not finish the page"));
                    }
                    prog.pages_done.fetch_add(1, Ordering::Relaxed);
                }
            }
            if EndDoc(hdc) <= 0 {
                return Err(anyhow!("Could not finish the print job"));
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
#[allow(unused_imports)]
pub use win::{choose_printer, run_job, Printer};

#[cfg(not(windows))]
pub struct Printer {
    pub copies: u32,
    pub page: PageInfo,
}

#[cfg(not(windows))]
pub fn choose_printer(_landscape: bool, _copies: u16) -> anyhow::Result<Option<Printer>> {
    Err(anyhow::anyhow!("Printing is only supported on Windows"))
}

#[cfg(not(windows))]
pub fn run_job(_printer: Printer, _sources: &[PrintSource], _opts: &PrintOptions, _title: &str, _prog: &PrintProgress) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("Printing is only supported on Windows"))
}

/// Print one image with default options (shows the Windows print dialog).
/// Returns Ok(()) when the user cancels. Convenience wrapper; the Print
/// options window offers layouts and runs the job off the UI thread.
pub fn print_image(img: &RgbaImage, title: &str) -> anyhow::Result<()> {
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 {
        return Err(anyhow::anyhow!("Nothing to print"));
    }
    let opts = PrintOptions::default();
    let Some(printer) = choose_printer(iw > ih, 1)? else { return Ok(()) };
    let src = [PrintSource::Image { img: Arc::new(img.clone()), name: title.to_string() }];
    run_job(printer, &src, &opts, title, &PrintProgress::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_centers_and_preserves_aspect() {
        let p = fit_on_page(4000, 2000, 1000, 1000, 0);
        assert_eq!((p.w, p.h), (1000, 500));
        assert_eq!((p.x, p.y), (0, 250));
        let p = fit_on_page(100, 200, 1000, 1000, 100);
        assert_eq!((p.w, p.h), (400, 800));
        assert_eq!((p.x, p.y), (300, 100));
    }

    #[test]
    fn bgra_conversion() {
        let img = RgbaImage::from_raw(2, 1, vec![10, 20, 30, 255, 0, 0, 0, 0]).unwrap();
        assert_eq!(to_bgra(&img), vec![30, 20, 10, 0, 255, 255, 255, 0]);
    }

    fn opts(sizing: Sizing) -> PrintOptions {
        PrintOptions { sizing, margin_mm: 0.0, gap_mm: 0.0, ..Default::default() }
    }

    #[test]
    fn page_layouts() {
        // 10 x 10 inch page at 100 dpi.
        let page = PageInfo::paper(10.0, 10.0, 100.0);
        let fit = layout_page(&page, &opts(Sizing::Fit), &[((400, 200), None)], false);
        assert_eq!(fit[0].dst, Placement { x: 0, y: 250, w: 1000, h: 500 });
        assert_eq!(fit[0].src, (0, 0, 400, 200));
        let fill = layout_page(&page, &opts(Sizing::Fill), &[((400, 200), None)], false);
        assert_eq!(fill[0].dst, Placement { x: 0, y: 0, w: 1000, h: 1000 });
        assert_eq!(fill[0].src, (100, 0, 200, 200), "centre square of the source");
        // 300 px at 100 dpi = 3 inches = 300 device px.
        let actual = layout_page(&page, &opts(Sizing::Actual), &[((300, 150), Some(100.0))], false);
        assert_eq!((actual[0].dst.w, actual[0].dst.h), (300, 150));
        assert_eq!((actual[0].dst.x, actual[0].dst.y), (350, 425));
        let custom = PrintOptions { custom_w: 2.0, custom_h: 2.0, unit: Unit::Inch, ..opts(Sizing::Custom) };
        let c = layout_page(&page, &custom, &[((400, 200), None)], false);
        assert_eq!((c[0].dst.w, c[0].dst.h), (200, 100));
        // Margins: 25.4 mm = 1 inch on every side.
        let m = PrintOptions { margin_mm: 25.4, ..opts(Sizing::Fit) };
        let r = layout_page(&page, &m, &[((100, 100), None)], false);
        assert_eq!(r[0].dst, Placement { x: 100, y: 100, w: 800, h: 800 });
        // An unprintable border counts towards the margin.
        let mut p2 = page;
        p2.offset_x = 50;
        p2.offset_y = 50;
        p2.printable_w = 900;
        p2.printable_h = 900;
        let r = layout_page(&p2, &m, &[((100, 100), None)], false);
        assert_eq!(r[0].dst, Placement { x: 50, y: 50, w: 800, h: 800 });
    }

    #[test]
    fn grids_and_orientation() {
        assert_eq!(grid_for(2, false), (1, 2));
        assert_eq!(grid_for(2, true), (2, 1));
        assert_eq!(grid_for(6, true), (3, 2));
        assert_eq!(grid_for(9, false), (3, 3));
        let page = PageInfo::paper(10.0, 10.0, 100.0);
        let o = PrintOptions { per_page: 4, ..opts(Sizing::Fit) };
        let r = layout_page(&page, &o, &[((100, 100), None); 5], false);
        assert_eq!(r.len(), 4);
        assert_eq!(r[3].dst, Placement { x: 500, y: 500, w: 500, h: 500 });
        assert_eq!(page_count(5, 4), 2);
        assert_eq!(page_count(0, 4), 1);
        let auto = PrintOptions::default();
        assert!(choose_landscape(&auto, &[(300, 200)]));
        assert!(!choose_landscape(&auto, &[(200, 300)]));
        assert!(choose_landscape(&PrintOptions { per_page: 2, ..auto.clone() }, &[(200, 300), (200, 300)]));
    }

    #[test]
    fn sends_at_most_placed_size() {
        let img = RgbaImage::new(4000, 3000);
        let page = PageInfo::paper(8.0, 6.0, 600.0);
        let p = layout_page(&page, &opts(Sizing::Fit), &[((4000, 3000), None)], true)[0];
        let px = prepare_for_placement(&img, &p, 600.0);
        assert_eq!(px.dimensions(), (2400, 1800), "capped at 300 dpi");
        let small = RgbaImage::new(100, 75);
        let p = layout_page(&page, &opts(Sizing::Fit), &[((100, 75), None)], true)[0];
        assert_eq!(prepare_for_placement(&small, &p, 600.0).dimensions(), (100, 75), "never upscaled");
    }
}
