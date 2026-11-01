//! Text recognition with the OCR engine built into Windows (WinRT
//! Windows.Media.Ocr). Runs offline; the languages are the ones installed in
//! Windows with "Optical character recognition". Boxes are in the pixels of
//! the image passed in, whatever size the engine actually read.

use anyhow::{anyhow, bail, Result};
use image::RgbaImage;

/// Shown when Windows has no OCR language at all.
pub const NO_LANGUAGE: &str = "No text recognition language is installed in Windows. Add one in Settings > Time & language > Language & region (pick a language and include \"Optical character recognition\").";

/// One recognized word.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    /// x0, y0, x1, y1 in image pixels: the box around the word as it lies
    /// in the image (tilted words get the box around their tilted outline).
    pub rect: [f32; 4],
    /// The word's corners in image pixels (top left, top right, bottom
    /// right, bottom left); a rotated rectangle when the page is tilted.
    pub quad: [[f32; 2]; 4],
    /// x0, y0, x1, y1 with the page turned level (the engine's frame):
    /// true word height, and boxes of a line stay side by side.
    pub upright: [f32; 4],
}

impl Word {
    /// A word on a level page.
    pub fn level(text: impl Into<String>, rect: [f32; 4]) -> Word {
        let [x0, y0, x1, y1] = rect;
        Word { text: text.into(), rect, quad: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]], upright: rect }
    }
}

/// Places a box of the levelled page (the engine deskews the image about
/// its center before reading) back on the image tilted by `angle` degrees
/// (positive: clockwise).
pub fn tilt_box(upright: [f32; 4], angle: f32, size: (u32, u32)) -> ([f32; 4], [[f32; 2]; 4]) {
    let [x0, y0, x1, y1] = upright;
    if angle.abs() < 0.01 {
        return (upright, [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]);
    }
    let (cx, cy) = (size.0 as f32 / 2.0, size.1 as f32 / 2.0);
    let (s, c) = angle.to_radians().sin_cos();
    let turn = |x: f32, y: f32| [cx + (x - cx) * c - (y - cy) * s, cy + (x - cx) * s + (y - cy) * c];
    let quad = [turn(x0, y0), turn(x1, y0), turn(x1, y1), turn(x0, y1)];
    let mut r = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for [x, y] in quad {
        r = [r[0].min(x), r[1].min(y), r[2].max(x), r[3].max(y)];
    }
    (r, quad)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub words: Vec<Word>,
}

impl Line {
    pub fn text(&self) -> String {
        self.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OcrResult {
    /// Lines in reading order.
    pub lines: Vec<Line>,
    /// Text angle the engine found, in degrees (None: not detected).
    pub angle: Option<f32>,
    /// Language tag used, e.g. "en-US".
    pub language: String,
    /// Size of the image the boxes refer to.
    pub size: (u32, u32),
    /// The engine read the image scaled by this (1 unless it was too big).
    pub scale: f32,
}

impl OcrResult {
    /// The text, one line per row.
    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.text()).collect::<Vec<_>>().join("\n")
    }

    pub fn words(&self) -> impl Iterator<Item = &Word> {
        self.lines.iter().flat_map(|l| l.words.iter())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OcrLanguage {
    /// BCP-47 tag ("en-US").
    pub tag: String,
    /// Display name ("English (United States)").
    pub name: String,
}

/// The OCR languages installed in Windows.
pub fn available_languages() -> Result<Vec<OcrLanguage>> {
    on_worker(|| {
        use windows::Media::Ocr::OcrEngine;
        let langs = OcrEngine::AvailableRecognizerLanguages().map_err(|_| unavailable())?;
        let mut out = Vec::new();
        for l in &langs {
            out.push(OcrLanguage { tag: l.LanguageTag()?.to_string_lossy(), name: l.DisplayName()?.to_string_lossy() });
        }
        Ok(out)
    })
}

fn unavailable() -> anyhow::Error {
    anyhow!("Text recognition is not available on this version of Windows")
}

/// Reads the text of `img`. `lang` is a language tag ("en-US"); None uses
/// the user's Windows languages. Blocks until done (run it off the UI
/// thread); WinRT runs on its own thread, so any caller thread works.
pub fn recognize(img: &RgbaImage, lang: Option<&str>) -> Result<OcrResult> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("The image is empty");
    }
    // BGRA over white: transparent areas read as paper, not as black ink.
    let mut bgra = Vec::with_capacity(img.as_raw().len());
    for p in img.pixels() {
        let [r, g, b, a] = p.0;
        let over = |c: u8| ((c as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255) as u8;
        bgra.extend_from_slice(&[over(b), over(g), over(r), 255]);
    }
    let lang = lang.map(str::to_string);
    on_worker(move || read(bgra, w, h, lang.as_deref()))
}

fn read(bgra: Vec<u8>, w: u32, h: u32, lang: Option<&str>) -> Result<OcrResult> {
    use windows::core::HSTRING;
    use windows::Globalization::Language;
    use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;

    let engine = match lang {
        Some(tag) => {
            let l = Language::CreateLanguage(&HSTRING::from(tag)).map_err(|_| anyhow!("Unknown language \"{tag}\""))?;
            if !OcrEngine::IsLanguageSupported(&l).map_err(|_| unavailable())? {
                bail!("The text recognition language \"{tag}\" is not installed in Windows");
            }
            OcrEngine::TryCreateFromLanguage(&l).ok()
        }
        None => OcrEngine::TryCreateFromUserProfileLanguages().ok(),
    };
    // None of the profile languages has OCR: use any installed one.
    let engine = match engine {
        Some(e) => e,
        None => {
            let langs = OcrEngine::AvailableRecognizerLanguages().map_err(|_| unavailable())?;
            let first = langs.into_iter().next().ok_or_else(|| anyhow!(NO_LANGUAGE))?;
            OcrEngine::TryCreateFromLanguage(&first).map_err(|_| anyhow!(NO_LANGUAGE))?
        }
    };
    let language = engine.RecognizerLanguage().and_then(|l| l.LanguageTag()).map(|t| t.to_string_lossy()).unwrap_or_default();

    // Too big for the engine: read a scaled copy, map the boxes back.
    let max = OcrEngine::MaxImageDimension().unwrap_or(10_000).max(64);
    let (mut buf, mut sw, mut sh, mut scale) = (bgra, w, h, 1.0f32);
    if w.max(h) > max {
        scale = max as f32 / w.max(h) as f32;
        sw = ((w as f32 * scale).round() as u32).clamp(1, max);
        sh = ((h as f32 * scale).round() as u32).clamp(1, max);
        let src = image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| anyhow!("Bad image buffer"))?;
        buf = image::imageops::resize(&src, sw, sh, image::imageops::FilterType::Triangle).into_raw();
        scale = sw as f32 / w as f32;
    }

    let writer = DataWriter::new()?;
    writer.WriteBytes(&buf)?;
    drop(buf);
    let ibuf = writer.DetachBuffer()?;
    let bmp = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(&ibuf, BitmapPixelFormat::Bgra8, sw as i32, sh as i32, BitmapAlphaMode::Premultiplied)?;
    let r = engine.RecognizeAsync(&bmp)?.join().map_err(|e| anyhow!("Text recognition failed: {}", e.message()))?;

    let inv = 1.0 / scale;
    let angle = r.TextAngle().ok().and_then(|a| a.Value().ok()).map(|a| a as f32);
    let mut lines = Vec::new();
    for line in &r.Lines()? {
        let mut words = Vec::new();
        for word in &line.Words()? {
            let b = word.BoundingRect()?;
            let upright = [b.X * inv, b.Y * inv, (b.X + b.Width) * inv, (b.Y + b.Height) * inv];
            let (rect, quad) = tilt_box(upright, angle.unwrap_or(0.0), (w, h));
            words.push(Word { text: word.Text()?.to_string_lossy(), rect, quad, upright });
        }
        if !words.is_empty() {
            lines.push(Line { words });
        }
    }
    Ok(OcrResult { lines, angle, language, size: (w, h), scale })
}

type Task = Box<dyn FnOnce() + Send>;

/// WinRT work on one long-lived MTA thread. The windows crate caches the
/// OCR activation factories process-wide, so the apartment they were made
/// in must stay alive (a thread that uninitializes COM leaves them dangling).
fn on_worker<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    static WORKER: std::sync::OnceLock<Option<std::sync::Mutex<std::sync::mpsc::Sender<Task>>>> = std::sync::OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Task>();
        std::thread::Builder::new()
            .name("lumen-ocr".into())
            .spawn(move || {
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                for task in rx {
                    task();
                }
            })
            .ok()
            .map(|_| std::sync::Mutex::new(tx))
    });
    let tx = worker.as_ref().ok_or_else(unavailable)?.lock().map_err(|_| unavailable())?.clone();
    let (rtx, rrx) = std::sync::mpsc::channel();
    tx.send(Box::new(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err(anyhow!("Text recognition crashed on this image")));
        let _ = rtx.send(r);
    }))
    .map_err(|_| unavailable())?;
    rrx.recv().map_err(|_| unavailable())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilted_boxes_turn_about_the_center() {
        let (r, q) = tilt_box([10.0, 20.0, 30.0, 40.0], 0.0, (100, 100));
        assert_eq!(r, [10.0, 20.0, 30.0, 40.0]);
        assert_eq!(q[2], [30.0, 40.0]);
        // A quarter turn clockwise: the top left corner goes to the top right.
        let (r, q) = tilt_box([0.0, 0.0, 10.0, 10.0], 90.0, (100, 100));
        assert!((q[0][0] - 100.0).abs() < 1e-3 && q[0][1].abs() < 1e-3, "{q:?}");
        assert!((r[0] - 90.0).abs() < 1e-3 && (r[3] - 10.0).abs() < 1e-3, "{r:?}");
        // A small tilt: the right end of a long word goes down.
        let (_, q) = tilt_box([0.0, 50.0, 100.0, 60.0], 3.0, (100, 100));
        assert!(q[1][1] > q[0][1] + 5.0);
    }

    /// Real OCR on a rendered line of text (needs an OCR language in Windows).
    #[test]
    #[ignore]
    fn reads_rendered_text() {
        let langs = available_languages().unwrap();
        assert!(!langs.is_empty(), "{NO_LANGUAGE}");
        let font = ab_glyph::FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).unwrap();
        let mut img = RgbaImage::from_pixel(900, 160, image::Rgba([255, 255, 255, 255]));
        crate::text::tests::draw_text(&mut img, &font, "Quick brown foxes jump", 40.0, 30.0, 56.0, 0);
        let r = recognize(&img, None).unwrap();
        let text = r.text().to_lowercase();
        assert!(text.contains("quick") && text.contains("brown") && text.contains("jump"), "{text:?}");
        let w = r.words().next().unwrap();
        assert!(w.rect[0] >= 20.0 && w.rect[0] < 80.0 && w.rect[1] > 10.0 && w.rect[3] < 120.0, "{w:?}");
        assert_eq!(r.size, (900, 160));
    }
}
