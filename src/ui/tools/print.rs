//! Printing through the classic Windows GDI print path.

use image::RgbaImage;

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

#[cfg(windows)]
pub fn print_image(img: &RgbaImage, title: &str) -> anyhow::Result<()> {
    use anyhow::anyhow;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::Storage::Xps::{AbortDoc, EndDoc, EndPage, StartDocW, StartPage, DOCINFOW};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::UI::Controls::Dialogs::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 {
        return Err(anyhow!("Nothing to print"));
    }
    if iw > 32767 || ih > 32767 {
        return Err(anyhow!("The image is too large to print ({iw} × {ih})"));
    }

    unsafe {
        let owner = GetActiveWindow();
        // 1. Get the default printer's settings and pre-select the orientation
        //    that suits the image (the user can still change it in the dialog).
        let mut pd = PRINTDLGW { lStructSize: std::mem::size_of::<PRINTDLGW>() as u32, hwndOwner: owner, Flags: PD_RETURNDEFAULT, ..Default::default() };
        if PrintDlgW(&mut pd).as_bool() && !pd.hDevMode.is_invalid() {
            let dm = GlobalLock(pd.hDevMode) as *mut DEVMODEW;
            if !dm.is_null() {
                (*dm).dmFields |= DM_ORIENTATION;
                (*dm).Anonymous1.Anonymous1.dmOrientation = if iw > ih { DMORIENT_LANDSCAPE as i16 } else { DMORIENT_PORTRAIT as i16 };
                let _ = GlobalUnlock(pd.hDevMode);
            }
        }
        // 2. The regular print dialog, returning a printer DC.
        pd.Flags = PD_RETURNDC | PD_NOSELECTION | PD_NOPAGENUMS | PD_USEDEVMODECOPIESANDCOLLATE;
        pd.nCopies = 1;
        let ok = PrintDlgW(&mut pd).as_bool();
        let free = |h: HGLOBAL| {
            if !h.is_invalid() {
                let _ = GlobalFree(Some(h));
            }
        };
        if !ok {
            free(pd.hDevMode);
            free(pd.hDevNames);
            let err = CommDlgExtendedError();
            return match err.0 {
                0 => Ok(()), // cancelled by the user
                0x1008 => Err(anyhow!("No printer is installed")),
                e => Err(anyhow!("The print dialog failed (error {e:#x})")),
            };
        }
        free(pd.hDevMode);
        free(pd.hDevNames);
        let hdc = pd.hDC;
        if hdc.is_invalid() {
            return Err(anyhow!("No printer device context was returned"));
        }
        let result = (|| -> anyhow::Result<()> {
            let pw = GetDeviceCaps(Some(hdc), HORZRES);
            let ph = GetDeviceCaps(Some(hdc), VERTRES);
            let dpi = GetDeviceCaps(Some(hdc), LOGPIXELSX).max(72);
            if pw <= 0 || ph <= 0 {
                return Err(anyhow!("The printer reported an empty page"));
            }
            // A small extra margin (~5 mm) inside the printable area looks nicer.
            let margin = (dpi as f64 * 0.2) as i32;
            let place = fit_on_page(iw, ih, pw, ph, margin);

            let doc_name = HSTRING::from(if title.is_empty() { "Lumen" } else { title });
            let di = DOCINFOW { cbSize: std::mem::size_of::<DOCINFOW>() as i32, lpszDocName: PCWSTR(doc_name.as_ptr()), ..Default::default() };
            if StartDocW(hdc, &di) <= 0 {
                return Err(anyhow!("Could not start the print job"));
            }
            if StartPage(hdc) <= 0 {
                AbortDoc(hdc);
                return Err(anyhow!("Could not start a page"));
            }
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
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
            let lines = StretchDIBits(hdc, place.x, place.y, place.w, place.h, 0, 0, iw as i32, ih as i32, Some(bits.as_ptr() as *const _), &bmi, DIB_RGB_COLORS, SRCCOPY);
            if lines == 0 {
                AbortDoc(hdc);
                return Err(anyhow!("The printer driver rejected the image"));
            }
            if EndPage(hdc) <= 0 {
                AbortDoc(hdc);
                return Err(anyhow!("Could not finish the page"));
            }
            if EndDoc(hdc) <= 0 {
                return Err(anyhow!("Could not finish the print job"));
            }
            Ok(())
        })();
        let _ = DeleteDC(hdc);
        result
    }
}

#[cfg(not(windows))]
pub fn print_image(_img: &RgbaImage, _title: &str) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("Printing is only supported on Windows"))
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
}
