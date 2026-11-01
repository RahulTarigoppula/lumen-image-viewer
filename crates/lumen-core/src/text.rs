//! Text and document checks on OCR output (see docs/TEXT-ANALYSIS.md):
//! text size, per-word clarity and contrast, words cut by the image border,
//! overlapping words (double print, ghosting), page tilt, a 0-100 document
//! score with a plain sentence, and a word diff between two captures.
//! Pure Rust; the words come from [`crate::ocr`].

use crate::ocr::OcrResult;
use image::RgbaImage;

/// Clarity under this is a blurry word.
pub const BLURRY: u8 = 40;
/// Clarity under this (and at least [`BLURRY`]) is a soft word.
pub const SOFT: u8 = 70;
/// Stroke contrast (0-255) under this is faint text.
pub const LOW_CONTRAST: f32 = 100.0;
/// Word boxes overlapping by this share of the smaller one overlap.
pub const OVERLAP: f32 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Level {
    Good,
    Fair,
    Poor,
    Info,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];
    pub fn name(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }
}

/// One word with its measurements.
#[derive(Clone, Debug, PartialEq)]
pub struct WordStat {
    pub text: String,
    /// x0, y0, x1, y1 in image pixels.
    pub rect: [f32; 4],
    /// Corners in image pixels (a rotated rectangle on a tilted page).
    pub quad: [[f32; 2]; 4],
    /// Index of the line it is on.
    pub line: usize,
    /// Box height in pixels.
    pub height: f32,
    /// Ink to paper difference, 0-255.
    pub contrast: f32,
    /// Width of the stroke edges in pixels (about 1 when sharp).
    pub edge_px: f32,
    /// 0-100: sharp, contrasty strokes score high.
    pub clarity: u8,
    /// Mostly letters and digits, not a jumble of symbols.
    pub readable: bool,
    pub cut: Option<Edge>,
    pub overlaps: bool,
}

impl WordStat {
    pub fn blurry(&self) -> bool {
        self.clarity < BLURRY
    }
    pub fn faint(&self) -> bool {
        self.contrast < LOW_CONTRAST
    }
    pub fn level(&self) -> Level {
        match self.clarity {
            c if c >= SOFT => Level::Good,
            c if c >= BLURRY => Level::Fair,
            _ => Level::Poor,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkewSource {
    /// The OCR engine's text angle.
    Engine,
    /// Slope of the word bottoms along the lines.
    Baselines,
}

/// One check of the page, for the summary rows and the report.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub value: String,
    /// What it means, in plain words.
    pub tip: String,
    /// The words it is about (indices into `TextReport::words`), worst first.
    pub words: Vec<usize>,
    /// The area it is about (x0, y0, x1, y1 in image pixels), when not words.
    pub region: Option<[f32; 4]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextReport {
    pub size: (u32, u32),
    pub words: Vec<WordStat>,
    pub lines: usize,
    pub chars: usize,
    /// The recognized text, one line per row.
    pub text: String,
    pub language: String,
    /// Median word height in pixels.
    pub text_height: Option<f32>,
    pub median_clarity: Option<u8>,
    pub median_contrast: Option<f32>,
    /// Pairs of overlapping words.
    pub overlaps: Vec<(usize, usize)>,
    /// Degrees; positive is clockwise.
    pub skew: Option<f32>,
    pub skew_source: SkewSource,
    /// Share (0-1) of the dark marks of the page that lie outside every
    /// recognized word: text the recognizer could not read (or pictures).
    pub unread: Option<f32>,
    /// Where most of those marks are (x0, y0, x1, y1 in image pixels).
    pub unread_spot: Option<[f32; 4]>,
    /// 0-100; None when no text was found.
    pub score: Option<u8>,
    pub summary: String,
}

/// Luminance 0-255 (Rec. 709 weights) composited over white.
fn luma(p: [u8; 4]) -> f32 {
    let y = 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;
    let a = p[3] as f32 / 255.0;
    y * a + 255.0 * (1.0 - a)
}

/// Word height in pixels at which body text scans at about `dpi` for 11 pt
/// type (a word box spans about 0.85 of the font size).
pub fn dpi_for_height(h: f32) -> f32 {
    h / (0.85 * 11.0 / 72.0)
}

/// Stroke contrast and edge width inside `rect` (clamped to the image).
/// Edge width = contrast / strongest gradients: a clean step from ink to
/// paper takes about 1 px, blur widens it.
pub fn stroke_stats(img: &RgbaImage, rect: [f32; 4]) -> (f32, f32) {
    let (w, h) = img.dimensions();
    let x0 = (rect[0].floor().max(0.0) as u32).min(w.saturating_sub(1));
    let y0 = (rect[1].floor().max(0.0) as u32).min(h.saturating_sub(1));
    let x1 = (rect[2].ceil().max(0.0) as u32).min(w);
    let y1 = (rect[3].ceil().max(0.0) as u32).min(h);
    if x1 <= x0 + 1 || y1 <= y0 + 1 {
        return (0.0, 0.0);
    }
    // At most ~40k samples per word (big headlines).
    let area = (x1 - x0) as u64 * (y1 - y0) as u64;
    let step = ((area as f64 / 40_000.0).sqrt().ceil() as u32).max(1);
    let px = |x: u32, y: u32| luma(img.get_pixel(x, y).0);
    let mut lum = Vec::new();
    let mut grad = Vec::new();
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            let v = px(x, y);
            lum.push(v);
            if x + 1 < x1 && y + 1 < y1 {
                let dx = px(x + 1, y) - v;
                let dy = px(x, y + 1) - v;
                grad.push((dx * dx + dy * dy).sqrt());
            }
            x += step;
        }
        y += step;
    }
    if lum.len() < 4 || grad.is_empty() {
        return (0.0, 0.0);
    }
    lum.sort_by(f32::total_cmp);
    let pct = |v: &[f32], q: f32| v[((v.len() - 1) as f32 * q).round() as usize];
    let contrast = pct(&lum, 0.97) - pct(&lum, 0.03);
    grad.sort_by(|a, b| b.total_cmp(a));
    let n = (grad.len() / 20).max(3).min(grad.len());
    let strong = grad[..n].iter().sum::<f32>() / n as f32;
    let edge = if strong > 0.5 { (contrast / strong).clamp(0.5, 30.0) } else { 30.0 };
    (contrast, edge)
}

/// 0-100 clarity from the edge width, the text height and the contrast.
/// About 1.3 px of edge is what anti-aliasing gives; what is beyond that is
/// blur, judged against the stroke width (a tenth of the text height).
pub fn clarity(edge_px: f32, height: f32, contrast: f32) -> u8 {
    if contrast < 8.0 {
        return 0;
    }
    let stroke = (height * 0.1).max(1.0);
    let q = (edge_px - 1.3).max(0.0) / stroke;
    let sharp = (1.0 - (q - 0.3) / 1.2).clamp(0.0, 1.0);
    let cf = ((contrast - 20.0) / 60.0).clamp(0.0, 1.0);
    (100.0 * sharp * (0.4 + 0.6 * cf)).round() as u8
}

/// Mostly letters and digits (OCR garbage is mostly symbols).
pub fn readable(word: &str) -> bool {
    let n = word.chars().count();
    let alnum = word.chars().filter(|c| c.is_alphanumeric()).count();
    alnum > 0 && alnum * 2 >= n
}

/// Which border the box touches, within a few pixels.
pub fn cut_edge(rect: [f32; 4], size: (u32, u32)) -> Option<Edge> {
    let (w, h) = (size.0 as f32, size.1 as f32);
    let m = (w.min(h) * 0.003).max(2.0);
    let d = [rect[0], w - rect[2], rect[1], h - rect[3]];
    let (i, v) = d.iter().enumerate().min_by(|a, b| a.1.total_cmp(b.1))?;
    (*v <= m).then(|| Edge::ALL[i])
}

/// Pairs of boxes overlapping by at least [`OVERLAP`] of the smaller one.
pub fn overlapping(rects: &[[f32; 4]]) -> Vec<(usize, usize)> {
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by(|&a, &b| rects[a][1].total_cmp(&rects[b][1]));
    let area = |r: &[f32; 4]| (r[2] - r[0]).max(0.0) * (r[3] - r[1]).max(0.0);
    let mut out = Vec::new();
    for (k, &i) in order.iter().enumerate() {
        let a = rects[i];
        for &j in &order[k + 1..] {
            let b = rects[j];
            if b[1] >= a[3] {
                break;
            }
            let ix = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
            let iy = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
            let small = area(&a).min(area(&b));
            if small > 0.0 && ix * iy >= OVERLAP * small {
                out.push((i.min(j), i.max(j)));
            }
        }
    }
    out.sort_unstable();
    out
}

/// Median slope of the word bottoms along lines of 3 or more words, in
/// degrees (positive: the text runs down to the right, clockwise).
pub fn baseline_skew(lines: &[Vec<[f32; 4]>]) -> Option<f32> {
    let mut angles = Vec::new();
    for l in lines.iter().filter(|l| l.len() >= 3) {
        let pts: Vec<(f32, f32)> = l.iter().map(|r| ((r[0] + r[2]) / 2.0, r[3])).collect();
        let n = pts.len() as f32;
        let mx = pts.iter().map(|p| p.0).sum::<f32>() / n;
        let my = pts.iter().map(|p| p.1).sum::<f32>() / n;
        let sxx: f32 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        let sxy: f32 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        if sxx > 1.0 {
            angles.push((sxy / sxx).atan().to_degrees());
        }
    }
    median(&mut angles)
}

fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let m = v.len() / 2;
    Some(if v.len() % 2 == 0 { (v[m - 1] + v[m]) / 2.0 } else { v[m] })
}

/// Measures every word of `ocr` on `img` (the image it was read from) and
/// sums the page up.
pub fn analyze(ocr: &OcrResult, img: &RgbaImage) -> TextReport {
    let size = img.dimensions();
    let mut words = Vec::new();
    for (li, line) in ocr.lines.iter().enumerate() {
        for w in &line.words {
            let height = (w.upright[3] - w.upright[1]).max(0.0);
            // A pixel of paper around the box so the edges are inside.
            let r = [w.rect[0] - 1.0, w.rect[1] - 1.0, w.rect[2] + 1.0, w.rect[3] + 1.0];
            let (contrast, edge_px) = stroke_stats(img, r);
            words.push(WordStat {
                text: w.text.clone(),
                rect: w.rect,
                quad: w.quad,
                line: li,
                height,
                contrast,
                edge_px,
                clarity: clarity(edge_px, height, contrast),
                readable: readable(&w.text),
                cut: cut_edge(w.rect, size),
                overlaps: false,
            });
        }
    }
    // On the levelled page, so the boxes of a tilted line do not overlap.
    let rects: Vec<[f32; 4]> = ocr.words().map(|w| w.upright).collect();
    let overlaps = overlapping(&rects);
    for &(a, b) in &overlaps {
        words[a].overlaps = true;
        words[b].overlaps = true;
    }
    let lines_rects: Vec<Vec<[f32; 4]>> = ocr.lines.iter().map(|l| l.words.iter().map(|w| w.rect).collect()).collect();
    let (skew, skew_source) = match ocr.angle {
        Some(a) => (Some(a), SkewSource::Engine),
        None => (baseline_skew(&lines_rects), SkewSource::Baselines),
    };
    let text_height = median(&mut words.iter().map(|w| w.height).collect::<Vec<_>>());
    let median_clarity = median(&mut words.iter().map(|w| w.clarity as f32).collect::<Vec<_>>()).map(|c| c.round() as u8);
    let median_contrast = median(&mut words.iter().map(|w| w.contrast).collect::<Vec<_>>());
    let boxes: Vec<[f32; 4]> = words.iter().map(|w| w.rect).collect();
    let (unread, unread_spot) = match unread_ink(img, &boxes) {
        Some((u, spot)) => (Some(u), spot),
        None => (None, None),
    };
    let mut r = TextReport {
        size,
        chars: words.iter().map(|w| w.text.chars().count()).sum(),
        words,
        lines: ocr.lines.len(),
        text: ocr.text(),
        language: ocr.language.clone(),
        text_height,
        median_clarity,
        median_contrast,
        overlaps,
        skew,
        skew_source,
        unread,
        unread_spot,
        score: None,
        summary: String::new(),
    };
    r.score = r.compute_score();
    r.summary = r.make_summary();
    r
}

impl TextReport {
    pub fn blurry(&self) -> usize {
        self.words.iter().filter(|w| w.blurry()).count()
    }
    pub fn faint(&self) -> usize {
        self.words.iter().filter(|w| w.faint()).count()
    }
    pub fn readable(&self) -> usize {
        self.words.iter().filter(|w| w.readable).count()
    }
    pub fn cut(&self) -> usize {
        self.words.iter().filter(|w| w.cut.is_some()).count()
    }
    pub fn overlapping_words(&self) -> usize {
        self.words.iter().filter(|w| w.overlaps).count()
    }
    /// Words cut at each edge, in [`Edge::ALL`] order.
    pub fn cut_by_edge(&self) -> [usize; 4] {
        let mut n = [0; 4];
        for w in &self.words {
            if let Some(e) = w.cut {
                n[Edge::ALL.iter().position(|x| *x == e).unwrap_or(0)] += 1;
            }
        }
        n
    }
    /// The words as normalized tokens, for [`diff_words`].
    pub fn tokens(&self) -> Vec<&str> {
        self.words.iter().map(|w| w.text.as_str()).collect()
    }

    fn size_level(&self) -> Level {
        match self.text_height {
            Some(h) if h >= 20.0 => Level::Good,
            Some(h) if h >= 12.0 => Level::Fair,
            Some(_) => Level::Poor,
            None => Level::Info,
        }
    }

    fn skew_level(&self) -> Level {
        match self.skew.map(f32::abs) {
            Some(a) if a < 0.5 => Level::Good,
            Some(a) if a < 2.0 => Level::Fair,
            Some(_) => Level::Poor,
            None => Level::Info,
        }
    }

    fn compute_score(&self) -> Option<u8> {
        let n = self.words.len();
        if n == 0 {
            return None;
        }
        let share = |k: usize| k as f32 / n as f32;
        let mut s = 100.0;
        let soft = self.words.iter().filter(|w| !w.blurry() && w.clarity < SOFT).count();
        s -= 60.0 * share(self.blurry()) + 15.0 * share(soft);
        s -= 30.0 * share(self.faint());
        s -= 20.0 * share(n - self.readable());
        s -= match self.size_level() {
            Level::Poor => 20.0,
            Level::Fair => 8.0,
            _ => 0.0,
        };
        s -= (3.0 * self.cut() as f32).min(15.0);
        s -= (4.0 * self.overlaps.len() as f32).min(20.0);
        if let Some(a) = self.skew {
            s -= ((a.abs() - 0.5).max(0.0) * 4.0).min(15.0);
        }
        if let Some(u) = self.unread {
            s -= ((u - UNREAD_OK).max(0.0) * 90.0).min(70.0);
        }
        Some(s.clamp(0.0, 100.0).round() as u8)
    }

    /// "312 words read, text sharp, 2 words cut at the right edge, page
    /// tilted 1.8°."
    fn make_summary(&self) -> String {
        let n = self.words.len();
        if n == 0 {
            if self.unread.is_some_and(|u| u > 0.5) {
                return "No words could be read, although the picture has marks that may be text: it may be too small, too blurry, doubled, or in a language that is not installed.".into();
            }
            return "No text found. If the picture has text, it may be too small, too blurry, or in a language that is not installed.".into();
        }
        let plural = |k: usize, one: &str, many: &str| if k == 1 { format!("1 {one}") } else { format!("{k} {many}") };
        let mut parts = vec![format!("{} read", plural(n, "word", "words"))];
        let blurry = self.blurry();
        let soft = self.median_clarity.is_some_and(|c| c < SOFT);
        parts.push(match blurry {
            0 if !soft => "text sharp".into(),
            0 => "text a little soft".into(),
            b if b * 2 > n => "text blurry".into(),
            b => format!("{} blurry", plural(b, "word", "words")),
        });
        if let (Level::Poor | Level::Fair, Some(h)) = (self.size_level(), self.text_height) {
            parts.push(format!("text {} ({h:.0} px tall)", if self.size_level() == Level::Poor { "too small" } else { "small" }));
        }
        let faint = self.faint();
        if faint > 0 {
            parts.push(if faint * 2 > n { "text faint".into() } else { format!("{} faint", plural(faint, "word", "words")) });
        }
        let edges = self.cut_by_edge();
        for (i, &k) in edges.iter().enumerate().filter(|(_, k)| **k > 0) {
            parts.push(format!("{} cut at the {} edge", plural(k, "word", "words"), Edge::ALL[i].name()));
        }
        let ov = self.overlapping_words();
        if ov > 0 {
            parts.push(format!("{} overlapping", plural(ov, "word", "words")));
        }
        let unreadable = n - self.readable();
        if unreadable * 10 > n {
            parts.push(format!("{} look garbled", plural(unreadable, "word", "words")));
        }
        if let Some(a) = self.skew.filter(|a| a.abs() >= 0.5) {
            parts.push(format!("page tilted {:.1}°", a.abs()));
        }
        if let Some(u) = self.unread.filter(|u| *u > UNREAD_OK) {
            parts.push(format!("{}% of the dark marks were not read as words", (u * 100.0).round()));
        }
        let mut s = parts.join(", ");
        s.push('.');
        s
    }

    /// One row per check, for the summary and the report.
    pub fn checks(&self) -> Vec<Check> {
        let n = self.words.len();
        let mut out = Vec::new();
        let by = |f: &dyn Fn(&WordStat) -> bool, key: &dyn Fn(&WordStat) -> f32| {
            let mut v: Vec<usize> = (0..n).filter(|&i| f(&self.words[i])).collect();
            v.sort_by(|&a, &b| key(&self.words[a]).total_cmp(&key(&self.words[b])));
            v
        };
        let readable = self.readable();
        let pct = if n > 0 { readable * 100 / n } else { 0 };
        out.push(Check {
            name: "Text found",
            level: match pct {
                _ if n == 0 => Level::Poor,
                p if p >= 90 => Level::Good,
                p if p >= 70 => Level::Fair,
                _ => Level::Poor,
            },
            value: if n == 0 { "none".into() } else { format!("{n} words, {pct}% readable") },
            tip: "Words the text recognizer found on the page. A word counts as readable when it is mostly letters and digits; a low share means the recognizer is guessing (blur, noise, unusual font or a language that is not installed).".into(),
            words: by(&|w| !w.readable, &|w| w.clarity as f32),
            region: None,
        });
        out.push(Check {
            name: "Text size",
            level: self.size_level(),
            value: self.text_height.map(|h| format!("{h:.0} px  (about {:.0} dpi at 11 pt)", dpi_for_height(h))).unwrap_or_else(|| "-".into()),
            tip: "Typical height of a word in pixels. 20 px or more reads reliably; under 12 px letters start to merge. The dpi is what a page of 11 pt text would have been scanned at (300 dpi is the usual archive target).".into(),
            words: by(&|w| w.height < 12.0, &|w| w.height),
            region: None,
        });
        let blurry = self.blurry();
        out.push(Check {
            name: "Sharpness",
            level: match (self.median_clarity, blurry) {
                (None, _) => Level::Info,
                (_, b) if b * 4 > n => Level::Poor,
                (Some(c), b) if c >= SOFT && b == 0 => Level::Good,
                _ => Level::Fair,
            },
            value: self.median_clarity.map(|c| if blurry > 0 { format!("{c} / 100, {blurry} blurry") } else { format!("{c} / 100") }).unwrap_or_else(|| "-".into()),
            tip: "How crisp the letter edges are (0-100), judged against the size of the text: a clean edge goes from ink to paper in about one pixel, focus or motion blur widens it. Words under 40 are counted as blurry.".into(),
            words: by(&|w| w.clarity < SOFT, &|w| w.clarity as f32),
            region: None,
        });
        let faint = self.faint();
        out.push(Check {
            name: "Contrast",
            level: match (self.median_contrast, faint) {
                (None, _) => Level::Info,
                (_, f) if f * 4 > n => Level::Poor,
                (_, 0) => Level::Good,
                _ => Level::Fair,
            },
            value: self.median_contrast.map(|c| if faint > 0 { format!("{c:.0} / 255, {faint} faint") } else { format!("{c:.0} / 255") }).unwrap_or_else(|| "-".into()),
            tip: "Difference in brightness between the ink and the paper. Under 100 the text looks grey or washed out (light print, over-exposure, a strong 'whiten' filter).".into(),
            words: by(&|w| w.faint(), &|w| w.contrast),
            region: None,
        });
        let cut = self.cut();
        let edges = self.cut_by_edge();
        let sides: Vec<&str> = Edge::ALL.iter().zip(edges).filter(|(_, k)| *k > 0).map(|(e, _)| e.name()).collect();
        out.push(Check {
            name: "Cut at the edges",
            level: if cut == 0 { Level::Good } else if cut <= 2 { Level::Fair } else { Level::Poor },
            value: if cut == 0 { "none".into() } else { format!("{cut} at the {}", sides.join(", ")) },
            tip: "Words that touch the border of the picture, so part of them is probably missing: the crop or the page edge cut through the text.".into(),
            words: by(&|w| w.cut.is_some(), &|w| w.rect[1]),
            region: None,
        });
        let ov = self.overlapping_words();
        out.push(Check {
            name: "Overlapping text",
            level: if ov == 0 { Level::Good } else if self.overlaps.len() <= 1 { Level::Fair } else { Level::Poor },
            value: if ov == 0 { "none".into() } else { format!("{ov} words") },
            tip: "Words drawn on top of each other: a double exposure, a shifted second copy (ghosting), text showing through from the back of the page, or a bad stitch between two captures.".into(),
            words: by(&|w| w.overlaps, &|w| w.rect[1]),
            region: None,
        });
        out.push(Check {
            name: "Tilt",
            level: self.skew_level(),
            value: self.skew.map(|a| format!("{:.1}°", a.abs())).unwrap_or_else(|| "-".into()),
            tip: match self.skew_source {
                SkewSource::Engine => "How far the lines of text are rotated from level, as found by the text recognizer. Under 0.5° looks straight; over 2° the page was clearly placed or photographed at a slant.",
                SkewSource::Baselines => "How far the lines of text are rotated from level, from the slope of the words along each line. Under 0.5° looks straight; over 2° the page was clearly placed or photographed at a slant.",
            }
            .into(),
            words: Vec::new(),
            region: None,
        });
        out.push(Check {
            name: "Unread marks",
            level: match self.unread {
                None => Level::Info,
                Some(u) if u <= UNREAD_OK => Level::Good,
                Some(u) if u <= 0.4 => Level::Fair,
                Some(_) => Level::Poor,
            },
            value: self.unread.map(|u| format!("{:.0}%", u * 100.0)).unwrap_or_else(|| "-".into()),
            tip: "Share of the dark marks on the page that are not inside any recognized word. A high share means text the recognizer could not read (blurred, doubled or smeared text), but pictures, logos, stamps, table lines and dark borders count too.".into(),
            words: Vec::new(),
            region: self.unread_spot.filter(|_| self.unread.is_some_and(|u| u > UNREAD_OK)),
        });
        out
    }
}

/// Unread marks up to this share are normal (punctuation, rules, specks).
pub const UNREAD_OK: f32 = 0.15;

/// Share of the dark marks of `img` outside every box (each grown by a fifth
/// of its height), and the area where most of them are. None on a page with
/// no marks. Sampled on a grid of at most ~1.5 M points.
pub fn unread_ink(img: &RgbaImage, boxes: &[[f32; 4]]) -> Option<(f32, Option<[f32; 4]>)> {
    let (w, h) = img.dimensions();
    if w < 8 || h < 8 {
        return None;
    }
    let s = ((w as f64 * h as f64 / 1.5e6).sqrt().ceil() as u32).max(1);
    let (gw, gh) = (w.div_ceil(s) as usize, h.div_ceil(s) as usize);
    let mut lum = Vec::with_capacity(gw * gh);
    for gy in 0..gh {
        for gx in 0..gw {
            lum.push(luma(img.get_pixel(gx as u32 * s, gy as u32 * s).0));
        }
    }
    let mut sorted = lum.clone();
    sorted.sort_by(f32::total_cmp);
    let pct = |q: f32| sorted[((sorted.len() - 1) as f32 * q) as usize];
    // Paper is most of a document; the darkest few are ink.
    let (paper, dark) = (pct(0.7), pct(0.005));
    if paper - dark < 40.0 {
        return None;
    }
    let thr = paper - 0.4 * (paper - dark);
    let mut covered = vec![false; gw * gh];
    for b in boxes {
        let g = (b[3] - b[1]).max(0.0) * 0.2;
        let cx0 = ((b[0] - g).max(0.0) / s as f32) as usize;
        let cy0 = ((b[1] - g).max(0.0) / s as f32) as usize;
        let cx1 = (((b[2] + g) / s as f32).ceil() as usize).min(gw);
        let cy1 = (((b[3] + g) / s as f32).ceil() as usize).min(gh);
        for y in cy0..cy1 {
            covered[y * gw + cx0.min(cx1)..y * gw + cx1].fill(true);
        }
    }
    const BINS: usize = 10;
    let mut bins = [[0u32; BINS]; BINS];
    let (mut ink, mut outside) = (0u64, 0u64);
    for gy in 0..gh {
        for gx in 0..gw {
            let i = gy * gw + gx;
            if lum[i] < thr {
                ink += 1;
                if !covered[i] {
                    outside += 1;
                    bins[gy * BINS / gh][gx * BINS / gw] += 1;
                }
            }
        }
    }
    // A blank page (a few specks).
    if ink * 2000 < (gw * gh) as u64 {
        return None;
    }
    let (mut best, mut at) = (0, (0, 0));
    for (by, row) in bins.iter().enumerate() {
        for (bx, &n) in row.iter().enumerate() {
            if n > best {
                best = n;
                at = (bx, by);
            }
        }
    }
    let spot = (best > 0).then(|| {
        let (bw, bh) = (w as f32 / BINS as f32, h as f32 / BINS as f32);
        [at.0 as f32 * bw, at.1 as f32 * bh, (at.0 + 1) as f32 * bw, (at.1 + 1) as f32 * bh]
    });
    Some((outside as f32 / ink as f32, spot))
}

/// One step of a word diff of A against another capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOp {
    Same { a: usize, b: usize },
    /// In A, not in the other.
    Missing { a: usize },
    /// Only in the other.
    Extra { b: usize },
    /// A's word read differently.
    Changed { a: usize, b: usize },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextDiff {
    pub ops: Vec<DiffOp>,
    pub same: usize,
    pub missing: usize,
    pub extra: usize,
    pub changed: usize,
    /// 2 × same / (words in A + words in B), 0-100.
    pub similarity: f32,
    /// Too different to align word by word; `similarity` counts shared words.
    pub rough: bool,
}

/// Lower case, without the punctuation around the word.
pub fn normalize(w: &str) -> String {
    let t = w.trim_matches(|c: char| !c.is_alphanumeric());
    if t.is_empty() { w.to_lowercase() } else { t.to_lowercase() }
}

/// Edit budget of the word diff (memory grows with its square).
const MAX_D: usize = 2000;

/// Word-level diff of `a` (the reference) against `b` (Myers' O(ND)
/// algorithm). A missing word next to an extra one is a changed word.
pub fn diff_words(a: &[&str], b: &[&str]) -> TextDiff {
    let na: Vec<String> = a.iter().map(|w| normalize(w)).collect();
    let nb: Vec<String> = b.iter().map(|w| normalize(w)).collect();
    let (n, m) = (na.len(), nb.len());
    let mut ops = Vec::new();
    // Common prefix and suffix need no search.
    let mut pre = 0;
    while pre < n && pre < m && na[pre] == nb[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < n - pre && suf < m - pre && na[n - 1 - suf] == nb[m - 1 - suf] {
        suf += 1;
    }
    for i in 0..pre {
        ops.push(DiffOp::Same { a: i, b: i });
    }
    let mid = match myers(&na[pre..n - suf], &nb[pre..m - suf]) {
        Some(m) => m,
        None => {
            // Shared words as a multiset.
            let mut count = std::collections::HashMap::<&str, i32>::new();
            for w in &na {
                *count.entry(w.as_str()).or_default() += 1;
            }
            let mut same = 0;
            for w in &nb {
                if let Some(c) = count.get_mut(w.as_str()).filter(|c| **c > 0) {
                    *c -= 1;
                    same += 1;
                }
            }
            let similarity = if n + m == 0 { 100.0 } else { 200.0 * same as f32 / (n + m) as f32 };
            return TextDiff { ops: Vec::new(), same, missing: n - same, extra: m - same, changed: 0, similarity, rough: true };
        }
    };
    for op in mid {
        ops.push(match op {
            Raw::Same(i, j) => DiffOp::Same { a: i + pre, b: j + pre },
            Raw::Del(i) => DiffOp::Missing { a: i + pre },
            Raw::Ins(j) => DiffOp::Extra { b: j + pre },
        });
    }
    for k in 0..suf {
        ops.push(DiffOp::Same { a: n - suf + k, b: m - suf + k });
    }
    let ops = pair_changes(ops);
    let mut d = TextDiff { ops, ..Default::default() };
    for op in &d.ops {
        match op {
            DiffOp::Same { .. } => d.same += 1,
            DiffOp::Missing { .. } => d.missing += 1,
            DiffOp::Extra { .. } => d.extra += 1,
            DiffOp::Changed { .. } => d.changed += 1,
        }
    }
    d.similarity = if n + m == 0 { 100.0 } else { 200.0 * d.same as f32 / (n + m) as f32 };
    d
}

/// Runs of missing words followed by extra ones become changed words.
fn pair_changes(ops: Vec<DiffOp>) -> Vec<DiffOp> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;
    while i < ops.len() {
        if !matches!(ops[i], DiffOp::Missing { .. } | DiffOp::Extra { .. }) {
            out.push(ops[i]);
            i += 1;
            continue;
        }
        let mut j = i;
        let (mut dels, mut ins) = (Vec::new(), Vec::new());
        while j < ops.len() {
            match ops[j] {
                DiffOp::Missing { a } => dels.push(a),
                DiffOp::Extra { b } => ins.push(b),
                _ => break,
            }
            j += 1;
        }
        let k = dels.len().min(ins.len());
        for t in 0..k {
            out.push(DiffOp::Changed { a: dels[t], b: ins[t] });
        }
        out.extend(dels[k..].iter().map(|&a| DiffOp::Missing { a }));
        out.extend(ins[k..].iter().map(|&b| DiffOp::Extra { b }));
        i = j;
    }
    out
}

enum Raw {
    Same(usize, usize),
    Del(usize),
    Ins(usize),
}

/// Shortest edit script; None past [`MAX_D`] edits.
fn myers(a: &[String], b: &[String]) -> Option<Vec<Raw>> {
    let (n, m) = (a.len() as i32, b.len() as i32);
    let max = (n + m) as usize;
    if max == 0 {
        return Some(Vec::new());
    }
    let off = max as i32 + 1;
    let mut v = vec![0i32; 2 * max + 3];
    let at = |k: i32| (k + off) as usize;
    // V after each round d, for k in -d..=d only.
    let mut trace: Vec<Vec<i32>> = Vec::new();
    let mut done = None;
    'search: for d in 0..=max.min(MAX_D) as i32 {
        let mut k = -d;
        while k <= d {
            let down = k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]);
            let mut x = if down { v[at(k + 1)] } else { v[at(k - 1)] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                trace.push(v[at(-d)..=at(d)].to_vec());
                done = Some(d);
                break 'search;
            }
            k += 2;
        }
        trace.push(v[at(-d)..=at(d)].to_vec());
    }
    let dmax = done?;
    // Walk back from (n, m): each round is one edit then a run of matches.
    let mut out = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=dmax).rev() {
        let prev = &trace[(d - 1) as usize];
        let get = |k: i32| prev[(k + d - 1) as usize];
        let k = x - y;
        let down = k == -d || (k != d && get(k - 1) < get(k + 1));
        let pk = if down { k + 1 } else { k - 1 };
        let px = get(pk);
        let py = px - pk;
        let (sx, sy) = if down { (px, py + 1) } else { (px + 1, py) };
        while x > sx && y > sy {
            out.push(Raw::Same((x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        out.push(if down { Raw::Ins(py as usize) } else { Raw::Del(px as usize) });
        x = px;
        y = py;
    }
    while x > 0 && y > 0 {
        out.push(Raw::Same((x - 1) as usize, (y - 1) as usize));
        x -= 1;
        y -= 1;
    }
    out.reverse();
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ocr::{Line, Word};
    use ab_glyph::{Font, PxScale, ScaleFont};

    /// Dark text on `img` with its top at `y`, ink gray level `ink`.
    pub(crate) fn draw_text(img: &mut RgbaImage, font: &impl Font, text: &str, x: f32, y: f32, px: f32, ink: u8) -> [f32; 4] {
        let sf = font.as_scaled(PxScale::from(px));
        let mut caret = x;
        let base = y + sf.ascent();
        let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for c in text.chars() {
            let g = sf.scaled_glyph(c);
            let adv = sf.h_advance(g.id);
            let g = ab_glyph::Glyph { position: ab_glyph::point(caret, base), ..g };
            if let Some(o) = font.outline_glyph(g) {
                let b = o.px_bounds();
                bounds = [bounds[0].min(b.min.x), bounds[1].min(b.min.y), bounds[2].max(b.max.x), bounds[3].max(b.max.y)];
                o.draw(|gx, gy, cov| {
                    let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                    if px >= 0 && py >= 0 && (px as u32) < img.width() && (py as u32) < img.height() {
                        let p = img.get_pixel_mut(px as u32, py as u32);
                        for ch in 0..3 {
                            let v = p.0[ch] as f32 * (1.0 - cov) + ink as f32 * cov;
                            p.0[ch] = v.round() as u8;
                        }
                    }
                });
            }
            caret += adv;
        }
        bounds
    }

    fn font() -> ab_glyph::FontRef<'static> {
        ab_glyph::FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).unwrap()
    }

    fn blur(img: &RgbaImage, sigma: f32) -> RgbaImage {
        image::imageops::blur(img, sigma)
    }

    fn page(text: &str, px: f32, ink: u8) -> (RgbaImage, [f32; 4]) {
        let mut img = RgbaImage::from_pixel(600, 120, image::Rgba([255, 255, 255, 255]));
        let r = draw_text(&mut img, &font(), text, 20.0, 20.0, px, ink);
        (img, r)
    }

    #[test]
    fn sharp_text_scores_high_and_blur_lowers_it() {
        let (img, r) = page("Halloween", 36.0, 0);
        let (c, e) = stroke_stats(&img, r);
        let h = r[3] - r[1];
        let sharp = clarity(e, h, c);
        assert!(c > 200.0 && e < 2.0 && sharp >= 85, "contrast {c} edge {e} clarity {sharp}");
        let (c2, e2) = stroke_stats(&blur(&img, 2.0), r);
        let soft = clarity(e2, h, c2);
        assert!(e2 > e + 1.5 && soft < sharp, "edge {e2} clarity {soft}");
        let (c3, e3) = stroke_stats(&blur(&img, 4.0), r);
        assert!(clarity(e3, h, c3) < BLURRY, "edge {e3} clarity {}", clarity(e3, h, c3));
    }

    #[test]
    fn faint_text_has_low_contrast() {
        let (img, r) = page("Halloween", 36.0, 215);
        let (c, _) = stroke_stats(&img, r);
        assert!(c < LOW_CONTRAST, "{c}");
        let (img, r) = page("Halloween", 36.0, 0);
        assert!(stroke_stats(&img, r).0 > 200.0);
    }

    #[test]
    fn blur_matters_less_on_big_text() {
        // The same 2 px of blur on small and on headline text.
        assert!(clarity(4.0, 16.0, 200.0) < BLURRY);
        assert!(clarity(4.0, 120.0, 200.0) >= SOFT);
        assert_eq!(clarity(1.2, 16.0, 200.0), 100);
    }

    #[test]
    fn readable_words() {
        assert!(readable("Hello,"));
        assert!(readable("2026"));
        assert!(readable("e.g."));
        assert!(!readable("|"));
        assert!(!readable("~=-:"));
        assert!(!readable("']|;"));
    }

    #[test]
    fn words_at_the_border_are_cut() {
        let size = (1000, 800);
        assert_eq!(cut_edge([0.0, 100.0, 50.0, 120.0], size), Some(Edge::Left));
        assert_eq!(cut_edge([900.0, 100.0, 999.0, 120.0], size), Some(Edge::Right));
        assert_eq!(cut_edge([400.0, 1.0, 450.0, 20.0], size), Some(Edge::Top));
        assert_eq!(cut_edge([400.0, 780.0, 450.0, 800.0], size), Some(Edge::Bottom));
        assert_eq!(cut_edge([10.0, 100.0, 50.0, 120.0], size), None);
    }

    #[test]
    fn overlapping_boxes() {
        let rects = [
            [0.0, 0.0, 100.0, 20.0],
            [110.0, 0.0, 200.0, 20.0],
            // A ghost of the first word shifted by a few pixels.
            [5.0, 4.0, 105.0, 24.0],
            // Touching the second only slightly.
            [195.0, 18.0, 260.0, 38.0],
            [0.0, 100.0, 50.0, 120.0],
        ];
        assert_eq!(overlapping(&rects), vec![(0, 2)]);
        assert!(overlapping(&[]).is_empty());
    }

    #[test]
    fn skew_from_baselines() {
        let tilt = 2.0f32.to_radians().tan();
        let line = |y: f32| (0..6).map(|i| {
            let x = i as f32 * 100.0;
            [x, y + x * tilt - 20.0, x + 80.0, y + (x + 40.0) * tilt]
        }).collect::<Vec<_>>();
        let a = baseline_skew(&[line(100.0), line(200.0), vec![[0.0, 0.0, 10.0, 10.0]]]).unwrap();
        assert!((a - 2.0).abs() < 0.05, "{a}");
        assert_eq!(baseline_skew(&[]), None);
    }

    fn ocr(lines: &[&[(&str, [f32; 4])]], size: (u32, u32)) -> OcrResult {
        OcrResult {
            lines: lines.iter().map(|l| Line { words: l.iter().map(|(t, r)| Word::level(*t, *r)).collect() }).collect(),
            angle: None,
            language: "en-US".into(),
            size,
            scale: 1.0,
        }
    }

    #[test]
    fn report_sums_up_the_page() {
        let mut img = RgbaImage::from_pixel(600, 200, image::Rgba([255, 255, 255, 255]));
        let f = font();
        let a = draw_text(&mut img, &f, "Invoice", 20.0, 20.0, 40.0, 0);
        let b = draw_text(&mut img, &f, "total", 250.0, 20.0, 40.0, 0);
        // Cut at the right edge.
        let c = draw_text(&mut img, &f, "amount", 520.0, 100.0, 40.0, 0);
        let c = [c[0], c[1], 600.0, c[3]];
        let o = ocr(&[&[("Invoice", a), ("total", b)], &[("amount", c)]], (600, 200));
        let r = analyze(&o, &img);
        assert_eq!(r.words.len(), 3);
        assert_eq!(r.lines, 2);
        assert_eq!(r.chars, 18);
        assert_eq!(r.cut(), 1);
        assert_eq!(r.words[2].cut, Some(Edge::Right));
        assert!(r.words[0].clarity >= SOFT, "{:?}", r.words[0]);
        assert!(r.summary.starts_with("3 words read, text sharp"), "{}", r.summary);
        assert!(r.summary.contains("1 word cut at the right edge"), "{}", r.summary);
        let s = r.score.unwrap();
        assert!((80..100).contains(&s), "{s}");
        let checks = r.checks();
        assert!(r.unread.unwrap() < UNREAD_OK, "{:?}", r.unread);
        let cut = checks.iter().find(|c| c.name == "Cut at the edges").unwrap();
        assert_eq!(cut.words, vec![2]);
        assert_eq!(cut.level, Level::Fair);
        assert_eq!(r.text, "Invoice total\namount");
    }

    #[test]
    fn marks_outside_the_words_are_unread() {
        let mut img = RgbaImage::from_pixel(600, 200, image::Rgba([255, 255, 255, 255]));
        let f = font();
        let a = draw_text(&mut img, &f, "Read", 20.0, 20.0, 40.0, 0);
        draw_text(&mut img, &f, "Smudged unreadable text", 20.0, 120.0, 40.0, 0);
        let (u, spot) = unread_ink(&img, &[a]).unwrap();
        assert!(u > 0.6, "{u}");
        let spot = spot.unwrap();
        assert!(spot[1] >= 100.0, "{spot:?}");
        let r = analyze(&ocr(&[&[("Read", a)]], (600, 200)), &img);
        assert!(r.score.unwrap() < 70 && r.summary.contains("not read as words"), "{:?} {}", r.score, r.summary);
        // A blank page has no marks to read.
        assert_eq!(unread_ink(&RgbaImage::from_pixel(100, 100, image::Rgba([255; 4])), &[]), None);
    }

    #[test]
    fn empty_page() {
        let img = RgbaImage::from_pixel(50, 50, image::Rgba([255, 255, 255, 255]));
        let r = analyze(&ocr(&[], (50, 50)), &img);
        assert_eq!(r.score, None);
        assert!(r.summary.starts_with("No text found"));
        assert_eq!(r.checks()[0].level, Level::Poor);
    }

    #[test]
    fn blurry_overlapping_tilted_page() {
        let mut img = RgbaImage::from_pixel(600, 200, image::Rgba([255, 255, 255, 255]));
        let f = font();
        let a = draw_text(&mut img, &f, "Ghost", 20.0, 20.0, 40.0, 0);
        let b = draw_text(&mut img, &f, "Ghost", 24.0, 26.0, 40.0, 0);
        let img = blur(&img, 3.0);
        let mut o = ocr(&[&[("Ghost", a)], &[("Ghost", b)]], (600, 200));
        o.angle = Some(-3.0);
        let r = analyze(&o, &img);
        assert_eq!(r.overlaps, vec![(0, 1)]);
        assert_eq!(r.blurry(), 2);
        assert!(r.summary.contains("text blurry") && r.summary.contains("2 words overlapping") && r.summary.contains("page tilted 3.0°"), "{}", r.summary);
        assert!(r.score.unwrap() < 50);
    }

    fn words(s: &str) -> Vec<&str> {
        s.split_whitespace().collect()
    }

    #[test]
    fn diff_finds_missing_extra_and_changed_words() {
        let a = words("The quick brown fox jumps over the lazy dog.");
        let b = words("the quick brwn fox jumps over lazy dog today");
        let d = diff_words(&a, &b);
        assert_eq!((d.same, d.changed, d.missing, d.extra), (7, 1, 1, 1), "{:?}", d.ops);
        assert!(d.ops.contains(&DiffOp::Changed { a: 2, b: 2 }));
        assert!(d.ops.contains(&DiffOp::Missing { a: 6 }));
        assert!(d.ops.contains(&DiffOp::Extra { b: 8 }));
        assert!((d.similarity - 200.0 * 7.0 / 18.0).abs() < 0.01);
        // Every word of both sides appears once, in order.
        let mut ia = Vec::new();
        let mut ib = Vec::new();
        for op in &d.ops {
            match *op {
                DiffOp::Same { a, b } | DiffOp::Changed { a, b } => {
                    ia.push(a);
                    ib.push(b);
                }
                DiffOp::Missing { a } => ia.push(a),
                DiffOp::Extra { b } => ib.push(b),
            }
        }
        assert_eq!(ia, (0..a.len()).collect::<Vec<_>>());
        assert_eq!(ib, (0..b.len()).collect::<Vec<_>>());
    }

    #[test]
    fn diff_edge_cases() {
        let same = diff_words(&words("a b c"), &words("A b, c"));
        assert_eq!((same.same, same.similarity), (3, 100.0));
        let empty = diff_words(&[], &[]);
        assert_eq!(empty.similarity, 100.0);
        let gone = diff_words(&words("one two"), &[]);
        assert_eq!((gone.missing, gone.similarity), (2, 0.0));
        let new = diff_words(&[], &words("x y z"));
        assert_eq!(new.extra, 3);
        // Words moved around.
        let d = diff_words(&words("a b c d e f"), &words("d e f a b c"));
        assert_eq!(d.same, 3);
        assert_eq!(d.missing + d.changed, 3);
    }

    #[test]
    fn diff_of_long_texts_stays_consistent() {
        let a: Vec<String> = (0..3000).map(|i| format!("w{}", i % 97)).collect();
        let mut b = a.clone();
        for i in (0..3000).step_by(50) {
            b[i] = "typo".into();
        }
        b.insert(1000, "inserted".into());
        b.remove(2000);
        let (ra, rb): (Vec<&str>, Vec<&str>) = (a.iter().map(|s| s.as_str()).collect(), b.iter().map(|s| s.as_str()).collect());
        let d = diff_words(&ra, &rb);
        assert!(!d.rough);
        assert_eq!(d.same + d.changed + d.missing, a.len());
        assert_eq!(d.same + d.changed + d.extra, b.len());
        assert!(d.similarity > 95.0, "{}", d.similarity);
        // Nothing in common at all: no alignment needed.
        let c: Vec<String> = (0..3000).map(|i| format!("z{i}")).collect();
        let rc: Vec<&str> = c.iter().map(|s| s.as_str()).collect();
        let far = diff_words(&ra, &rc);
        assert!(far.rough && far.similarity == 0.0);
    }
}
