//! The Analysis panel's Text tab: reads the text of the image (every pane in
//! compare mode) with the OCR engine built into Windows and checks how well
//! the capture reads: text size, sharpness, contrast, words cut at the
//! edges, overlapping text and tilt (`lumen_core::text`). Compare mode adds a
//! table of the images and a word diff of each one against A. Word boxes are
//! drawn over the image, colored by clarity. Everything runs offline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver};
use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use image::RgbaImage;
use lumen_core::ocr::{self, OcrLanguage};
use lumen_core::text::{self, DiffOp, TextDiff, TextReport};

use super::canvas::{self, Slot};
use super::look;
use super::panel::PanelAction;
use super::summary::Level;
use super::{Pixels, Subject};
use crate::app::Core;
use crate::ui::icons as I;
use crate::ui::theme::{self, Palette};
use crate::ui::widgets;

/// An image as read: its file and its pixel size.
type Key = (PathBuf, u32, u32);

const LETTERS: [&str; 4] = ["A", "B", "C", "D"];
/// Issue words listed before "Show all".
const ISSUES_SHOWN: usize = 12;

/// One image the tab can read.
struct Image {
    path: PathBuf,
    pixels: Option<Arc<RgbaImage>>,
    /// Still a video frame (not supported) rather than loading.
    video: bool,
}

impl Image {
    fn key(&self) -> Option<Key> {
        self.pixels.as_ref().map(|p| (self.path.clone(), p.width(), p.height()))
    }
}

struct Job {
    rx: Receiver<(Key, Result<TextReport, String>)>,
    /// Images still to be read, in order.
    pending: Vec<Key>,
    total: usize,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

/// Where a word is: image `img` (0 = A) of the shown list, word index.
#[derive(Clone, Copy, PartialEq)]
struct Flash {
    img: usize,
    word: usize,
    until: f64,
}

pub struct TextView {
    results: HashMap<Key, Result<Arc<TextReport>, String>>,
    job: Option<Job>,
    /// Language tag; None reads with the user's Windows languages.
    lang: Option<String>,
    langs: Option<Result<Vec<OcrLanguage>, String>>,
    langs_rx: Option<Receiver<Result<Vec<OcrLanguage>, String>>>,
    /// Word boxes on the image.
    pub show_boxes: bool,
    /// The image whose details are shown (0 = A).
    pick: usize,
    /// The image diffed against A (1 = B).
    diff_pick: usize,
    /// Only the differences (with a few words around them) in the diff.
    diff_only: bool,
    show_all_issues: bool,
    /// Run once the tab has the subject (dev hook).
    run_requested: bool,
    flash: Option<Flash>,
    /// Next word to show per check, so repeated clicks walk through them.
    cursor: HashMap<&'static str, usize>,
    /// Diffs by (A, other).
    diffs: HashMap<(Key, Key), Arc<TextDiff>>,
    /// The images of the subject last shown (for overlays and the report).
    shown: Vec<(PathBuf, Option<Key>)>,
}

impl Default for TextView {
    fn default() -> Self {
        TextView {
            results: HashMap::new(),
            job: None,
            lang: None,
            langs: None,
            langs_rx: None,
            show_boxes: true,
            pick: 0,
            diff_pick: 1,
            diff_only: true,
            show_all_issues: false,
            run_requested: false,
            flash: None,
            cursor: HashMap::new(),
            diffs: HashMap::new(),
            shown: Vec::new(),
        }
    }
}

fn level(l: text::Level) -> Level {
    match l {
        text::Level::Good => Level::Good,
        text::Level::Fair => Level::Fair,
        text::Level::Poor => Level::Poor,
        text::Level::Info => Level::Info,
    }
}

fn score_level(s: u8) -> Level {
    match s {
        80.. => Level::Good,
        55.. => Level::Fair,
        _ => Level::Poor,
    }
}

/// Box colors on the image.
fn box_color(p: &Palette, w: &text::WordStat) -> Color32 {
    if w.cut.is_some() {
        p.danger
    } else if w.overlaps {
        Color32::from_rgb(170, 90, 230)
    } else {
        look::level_color(p, level(w.level()))
    }
}

/// The images of the subject in pane order: every compare pane, else A (and B).
fn images(core: &Core, subject: &Subject) -> Vec<Image> {
    let ready = |s: &Option<super::SubjectImage>, path: &PathBuf| match s.as_ref().filter(|s| &s.path == path).map(|s| &s.pixels) {
        Some(Pixels::Ready(img)) => Some(img.clone()),
        _ => None,
    };
    let panes = core.metrics.compare_panes();
    if !panes.is_empty() {
        return panes
            .iter()
            .map(|p| Image { path: p.clone(), pixels: core.loader.get_ready(p).map(|l| l.decoded.image.clone()).or_else(|| ready(&subject.a, p).or_else(|| ready(&subject.b, p))), video: false })
            .collect();
    }
    [&subject.a, &subject.b]
        .into_iter()
        .flatten()
        .map(|s| Image {
            path: s.path.clone(),
            pixels: match &s.pixels {
                Pixels::Ready(i) => Some(i.clone()),
                _ => None,
            },
            video: matches!(s.pixels, Pixels::VideoFrame { .. }),
        })
        .collect()
}

impl TextView {
    fn report(&self, key: &Option<Key>) -> Option<&Arc<TextReport>> {
        key.as_ref().and_then(|k| self.results.get(k)).and_then(|r| r.as_ref().ok())
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Reads every image that has pixels, in the background.
    fn run(&mut self, ctx: &egui::Context, imgs: &[Image]) {
        let todo: Vec<(Key, Arc<RgbaImage>)> = imgs.iter().filter_map(|i| Some((i.key()?, i.pixels.clone()?))).collect();
        if todo.is_empty() {
            return;
        }
        if let Some(j) = self.job.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
        for (k, _) in &todo {
            self.results.remove(k);
        }
        self.diffs.retain(|(a, b), _| !todo.iter().any(|(k, _)| k == a || k == b));
        self.cursor.clear();
        self.flash = None;
        let (tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let lang = self.lang.clone();
        let ctx = ctx.clone();
        let pending: Vec<Key> = todo.iter().map(|t| t.0.clone()).collect();
        let total = pending.len();
        let spawned = std::thread::Builder::new().name("lumen-text".into()).spawn(move || {
            for (key, img) in todo {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let o = ocr::recognize(&img, lang.as_deref()).map_err(|e| e.to_string())?;
                    Ok(text::analyze(&o, &img))
                }))
                .unwrap_or_else(|_| Err("Text analysis crashed on this image".to_string()));
                if tx.send((key, r)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        if spawned.is_ok() {
            self.job = Some(Job { rx, pending, total, cancel, started: Instant::now() });
        }
    }

    fn cancel(&mut self) {
        if let Some(j) = self.job.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn poll(&mut self) {
        let Some(job) = &mut self.job else { return };
        // The worker ends (and drops its sender) when done or stopped.
        let mut ended = false;
        loop {
            match job.rx.try_recv() {
                Ok((key, r)) => {
                    job.pending.retain(|k| k != &key);
                    self.results.insert(key, r.map(Arc::new));
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    ended = true;
                    break;
                }
            }
        }
        if job.pending.is_empty() || ended {
            self.job = None;
        }
    }

    /// Installed OCR languages, listed once in the background.
    fn poll_langs(&mut self, ctx: &egui::Context) {
        if self.langs.is_none() && self.langs_rx.is_none() {
            let (tx, rx) = unbounded();
            let ctx = ctx.clone();
            let _ = std::thread::Builder::new().name("lumen-ocr-langs".into()).spawn(move || {
                let _ = tx.send(ocr::available_languages().map_err(|e| e.to_string()));
                ctx.request_repaint();
            });
            self.langs_rx = Some(rx);
        }
        if let Some(r) = self.langs_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.langs = Some(r);
            self.langs_rx = None;
        }
    }

    fn no_language(&self) -> bool {
        matches!(&self.langs, Some(Ok(l)) if l.is_empty())
    }

    /// The tab. `out` gets zoom requests.
    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core, subject: &Subject, out: &mut Vec<PanelAction>) {
        let p = core.pal;
        self.poll();
        self.poll_langs(ui.ctx());
        let imgs = images(core, subject);
        self.shown = imgs.iter().map(|i| (i.path.clone(), i.key())).collect();
        if self.pick >= imgs.len() {
            self.pick = 0;
        }
        if self.diff_pick >= imgs.len() || self.diff_pick == 0 {
            self.diff_pick = 1;
        }
        let compare = imgs.len() > 1;
        let readable = imgs.iter().filter(|i| i.pixels.is_some()).count();
        if std::mem::take(&mut self.run_requested) && readable > 0 && !self.no_language() {
            self.run(ui.ctx(), &imgs);
        }

        self.controls(ui, &p, &imgs, compare, readable);
        if imgs.iter().any(|i| i.video) {
            look::note(ui, &p, "Text reading works on still images. Save the video frame as an image and open it to read its text.");
        }
        if self.no_language() {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(format!("{}  {}", I::WARNING, ocr::NO_LANGUAGE)).size(theme::text::SMALL).color(p.warn)).wrap());
            return;
        }
        if let Some(Err(e)) = &self.langs {
            ui.add(egui::Label::new(RichText::new(format!("{}  {e}", I::WARNING)).size(theme::text::SMALL).color(p.warn)).wrap());
        }

        let reports: Vec<Option<Arc<TextReport>>> = imgs.iter().map(|i| self.report(&i.key()).cloned()).collect();
        let errors: Vec<Option<String>> = imgs.iter().map(|i| i.key().and_then(|k| self.results.get(&k)).and_then(|r| r.as_ref().err().cloned())).collect();
        if reports.iter().all(Option::is_none) && errors.iter().all(Option::is_none) {
            if !self.busy() {
                ui.add_space(6.0);
                look::note(ui, &p, "Reads the text in the picture and checks how well it reads: text size, sharpness, contrast, words cut off at the edges, overlapping (double) text and page tilt. Good for scans and photos of documents, receipts, slides and screenshots.");
            }
            return;
        }

        ui.add_space(8.0);
        // Compare: the images side by side and what each read differently
        // from A, then the picked image's details.
        if compare {
            self.ensure_diffs(&imgs);
            self.table(ui, &p, &imgs, &reports, &errors);
            ui.add_space(10.0);
            self.diff_section(ui, &p, &imgs, &reports);
            ui.add_space(10.0);
        }
        if let Some(e) = &errors[self.pick] {
            ui.add(egui::Label::new(RichText::new(format!("{}  {e}", I::WARNING)).size(theme::text::SMALL).color(p.danger)).wrap());
        }
        if let Some(r) = reports[self.pick].clone() {
            self.details(ui, &p, self.pick, &imgs[self.pick], &r, compare, out);
            ui.add_space(10.0);
            self.boxes_toggle(ui, &p);
            ui.add_space(10.0);
            self.text_section(ui, &p, &r, out, core);
        }
        if self.flash.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn controls(&mut self, ui: &mut Ui, p: &Palette, imgs: &[Image], compare: bool, readable: usize) {
        ui.horizontal(|ui| {
            // Language on the right, the button takes the rest.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                self.language_combo(ui);
                let bw = ui.available_width().max(80.0);
                if let Some(job) = &self.job {
                    let done = job.total - job.pending.len();
                    let resp = ui.allocate_ui_with_layout(Vec2::new(bw, theme::size::CONTROL), Layout::left_to_right(Align::Center), |ui| {
                        ui.spinner();
                        let what = match job.pending.first().and_then(|k| imgs.iter().position(|i| i.key().as_ref() == Some(k))) {
                            Some(i) if compare => format!("Reading {} ({} of {})", LETTERS[i.min(3)], done + 1, job.total),
                            _ => "Reading text".to_string(),
                        };
                        ui.label(RichText::new(what).size(theme::text::SMALL).color(p.muted));
                        ui.label(RichText::new(format!("{:.0} s", job.started.elapsed().as_secs_f32())).size(theme::text::SMALL).color(p.faint));
                        ui.add(egui::Button::new(RichText::new("Cancel").size(theme::text::SMALL).color(p.accent)).frame(false)).on_hover_text("Stop reading").clicked()
                    });
                    if resp.inner {
                        self.cancel();
                    }
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
                } else {
                    let enabled = readable > 0 && !self.no_language();
                    let label = if compare { "Read text in all" } else { "Read text" };
                    let tip = if compare {
                        "Find and read the text in every image with the text recognizer built into Windows, then check how well each one reads and what each one read differently from A. Nothing leaves this computer."
                    } else {
                        "Find and read the text in the picture with the text recognizer built into Windows, then check its size, sharpness, contrast, cut-off words, overlaps and tilt. Nothing leaves this computer."
                    };
                    let b = ui.allocate_ui_with_layout(Vec2::new(bw, theme::size::CONTROL), Layout::top_down_justified(Align::Center), |ui| widgets::primary_button_enabled(ui, p, enabled, label)).inner;
                    if b.on_hover_text(tip).on_disabled_hover_text("Wait for the image to load (or install a text recognition language in Windows)").clicked() {
                        self.run(ui.ctx(), imgs);
                    }
                }
            });
        });
    }

    fn language_combo(&mut self, ui: &mut Ui) {
        let langs = match &self.langs {
            Some(Ok(l)) => l.clone(),
            _ => Vec::new(),
        };
        let name = |tag: &Option<String>| match tag {
            None => "Automatic".to_string(),
            Some(t) => langs.iter().find(|l| &l.tag == t).map(|l| l.name.clone()).unwrap_or_else(|| t.clone()),
        };
        let mut pick = self.lang.clone();
        widgets::combo("text-lang")
            .selected_text(RichText::new(name(&pick)).size(theme::text::SMALL))
            .width(110.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut pick, None, "Automatic").on_hover_text("Use the languages of your Windows profile");
                for l in &langs {
                    ui.selectable_value(&mut pick, Some(l.tag.clone()), &l.name);
                }
            })
            .response
            .on_hover_text("Language of the text. Only the languages installed in Windows with text recognition are listed (add more in Settings > Time & language > Language & region).");
        self.lang = pick;
    }

    /// One row per image: score, words, the main problems and the match with A.
    fn table(&mut self, ui: &mut Ui, p: &Palette, imgs: &[Image], reports: &[Option<Arc<TextReport>>], errors: &[Option<String>]) {
        ui.label(RichText::new("Each image").font(theme::semibold(theme::text::CAPTION)).color(p.muted));
        let a_key = imgs.first().and_then(|i| i.key());
        look::card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, img) in imgs.iter().enumerate() {
                let w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
                let painter = ui.painter();
                if i == self.pick {
                    painter.rect_filled(rect.expand2(Vec2::new(6.0, 0.0)), CornerRadius::same(6), p.selected_bg);
                } else if resp.hovered() {
                    painter.rect_filled(rect.expand2(Vec2::new(6.0, 0.0)), CornerRadius::same(6), p.hover.gamma_multiply(0.6));
                }
                let c = rect.left_center();
                let badge = Rect::from_center_size(c + Vec2::new(9.0, 0.0), Vec2::splat(18.0));
                painter.rect_filled(badge, CornerRadius::same(4), look::pane_color(p, i));
                painter.text(badge.center(), Align2::CENTER_CENTER, LETTERS[i.min(3)], FontId::proportional(11.0), Color32::WHITE);
                let r = &reports[i];
                let (score, score_c) = match r.as_ref().and_then(|r| r.score) {
                    Some(s) => (s.to_string(), look::level_color(p, score_level(s))),
                    None => ("-".into(), p.faint),
                };
                painter.text(c + Vec2::new(26.0, 0.0), Align2::LEFT_CENTER, score, theme::semibold(theme::text::BODY), score_c);
                // Match with A on the right.
                let matched = match (i, &a_key, img.key(), r) {
                    (0, ..) => "reference".to_string(),
                    (_, Some(a), Some(b), Some(_)) => self.diff(a, &b).map(|d| format!("{:.0}% match", d.similarity)).unwrap_or_default(),
                    _ => String::new(),
                };
                let mg = painter.layout_no_wrap(matched, theme::text::small(), p.muted);
                let mw = mg.size().x;
                painter.galley(rect.right_center() - Vec2::new(mw, mg.size().y / 2.0), mg, p.muted);
                let short = match (r, &errors[i]) {
                    (Some(r), _) => short_issues(r),
                    (None, Some(_)) => "could not read".into(),
                    (None, None) if self.job.as_ref().is_some_and(|j| img.key().is_some_and(|k| j.pending.contains(&k))) => "reading...".into(),
                    (None, None) if img.pixels.is_none() => "not loaded".into(),
                    _ => "not read yet".into(),
                };
                let x0 = c.x + 56.0;
                let room = (rect.right() - mw - 10.0 - x0).max(20.0);
                // One line, cut at the end with an ellipsis when narrow.
                let mut job = LayoutJob::simple_singleline(short, theme::text::small(), p.text);
                job.wrap = egui::text::TextWrapping::truncate_at_width(room);
                let g = painter.layout_job(job);
                painter.galley(Pos2::new(x0, c.y - g.size().y / 2.0), g, p.text);
                let tip = match (r, &errors[i]) {
                    (Some(r), _) => format!("{}: {}\nScore {} / 100. Click for the details.", img_name(img), r.summary, r.score.map(|s| s.to_string()).unwrap_or("-".into())),
                    (None, Some(e)) => format!("{}: {e}", img_name(img)),
                    _ => img_name(img),
                };
                if resp.on_hover_text(tip).clicked() {
                    self.pick = i;
                }
            }
        });
    }

    fn diff(&self, a: &Key, b: &Key) -> Option<Arc<TextDiff>> {
        self.diffs.get(&(a.clone(), b.clone())).cloned()
    }

    /// Computes the diffs of A against the others once their text is read.
    fn ensure_diffs(&mut self, imgs: &[Image]) {
        let Some(a) = imgs.first().and_then(|i| i.key()) else { return };
        let Some(ra) = self.report(&Some(a.clone())).cloned() else { return };
        for img in &imgs[1..] {
            let Some(b) = img.key() else { continue };
            if self.diffs.contains_key(&(a.clone(), b.clone())) {
                continue;
            }
            if let Some(rb) = self.report(&Some(b.clone())).cloned() {
                self.diffs.insert((a.clone(), b), Arc::new(text::diff_words(&ra.tokens(), &rb.tokens())));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn details(&mut self, ui: &mut Ui, p: &Palette, idx: usize, img: &Image, r: &TextReport, compare: bool, out: &mut Vec<PanelAction>) {
        if compare {
            ui.label(RichText::new(format!("{}  {}", LETTERS[idx.min(3)], img_name(img))).font(theme::semibold(theme::text::CAPTION)).color(p.muted));
        }
        look::card(ui, p, |ui| {
            ui.horizontal(|ui| {
                match r.score {
                    Some(s) => {
                        ui.label(RichText::new(s.to_string()).size(26.0).strong().color(look::level_color(p, score_level(s))))
                            .on_hover_text("Document score, 0-100: how well the text in this capture reads. 80 and up is good; under 55 has clear problems (blur, faint or tiny text, cut-off words, overlaps, tilt).");
                        ui.label(RichText::new("/ 100").size(theme::text::SMALL).color(p.faint));
                    }
                    None => {
                        ui.label(RichText::new("-").size(26.0).color(p.faint));
                    }
                }
            });
            ui.add(egui::Label::new(RichText::new(&r.summary).size(theme::text::BODY).color(p.text)).wrap());
            ui.add_space(6.0);
            let zoomable = idx < 2;
            for c in r.checks() {
                let link = ((!c.words.is_empty() || c.region.is_some()) && zoomable).then_some("Show");
                let (_, clicked) = look::status_row(ui, p, level(c.level), c.name, &c.value, &c.tip, false, link);
                if clicked && c.words.is_empty() {
                    if let Some(rc) = c.region {
                        let (iw, ih) = (r.size.0.max(1) as f32, r.size.1.max(1) as f32);
                        out.push(PanelAction::ZoomTo { slot: if idx == 1 { Slot::B } else { Slot::A }, rect: [rc[0] / iw, rc[1] / ih, rc[2] / iw, rc[3] / ih] });
                    }
                } else if clicked {
                    let n = self.cursor.entry(c.name).or_insert(0);
                    let w = c.words[*n % c.words.len()];
                    *n += 1;
                    self.show_word(ui.ctx(), idx, r, w, out);
                }
            }
            if !zoomable {
                look::note(ui, p, "Zooming to a word works on A and B.");
            }
        });
        self.issues(ui, p, idx, r, out);
    }

    /// The words with a problem; a click zooms to it.
    fn issues(&mut self, ui: &mut Ui, p: &Palette, idx: usize, r: &TextReport, out: &mut Vec<PanelAction>) {
        let mut list: Vec<(usize, String, Level)> = Vec::new();
        for (i, w) in r.words.iter().enumerate() {
            let mut why = Vec::new();
            let mut lv = Level::Fair;
            if let Some(e) = w.cut {
                why.push(format!("cut at the {} edge", e.name()));
                lv = Level::Poor;
            }
            if w.overlaps {
                let other = r.overlaps.iter().find_map(|&(a, b)| if a == i { Some(b) } else if b == i { Some(a) } else { None });
                why.push(match other {
                    Some(o) => format!("overlaps \"{}\"", r.words[o].text),
                    None => "overlaps another word".into(),
                });
                lv = Level::Poor;
            }
            if w.blurry() {
                why.push(format!("blurry ({} / 100)", w.clarity));
                lv = Level::Poor;
            } else if w.clarity < text::SOFT {
                why.push(format!("soft ({} / 100)", w.clarity));
            }
            if w.faint() {
                why.push(format!("faint ({:.0} / 255)", w.contrast));
            }
            if !w.readable {
                why.push("looks garbled".into());
            }
            if !why.is_empty() {
                list.push((i, why.join(", "), lv));
            }
        }
        if list.is_empty() {
            return;
        }
        // Worst first, then in reading order.
        list.sort_by_key(|(i, _, lv)| (*lv != Level::Poor, *i));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Words to check ({})", list.len())).font(theme::semibold(theme::text::CAPTION)).color(p.muted))
                .on_hover_text("Every word with a problem, worst first. Click one to zoom the image to it.");
        });
        let zoomable = idx < 2;
        look::card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let n = if self.show_all_issues { list.len() } else { list.len().min(ISSUES_SHOWN) };
            for (i, why, lv) in &list[..n] {
                let w = &r.words[*i];
                let label = format!("\"{}\"", w.text);
                let (resp, _) = look::status_row(ui, p, *lv, &label, why, "Click to zoom to this word.", false, None);
                if zoomable && resp.interact(Sense::click()).clicked() {
                    self.show_word(ui.ctx(), idx, r, *i, out);
                }
            }
            if list.len() > ISSUES_SHOWN {
                let t = if self.show_all_issues { "Show fewer".to_string() } else { format!("Show all {}", list.len()) };
                if ui.add(egui::Button::new(RichText::new(t).size(theme::text::SMALL).color(p.accent_text)).frame(false)).clicked() {
                    self.show_all_issues = !self.show_all_issues;
                }
            }
        });
    }

    /// Zooms pane `idx` to word `w` and flashes its box.
    fn show_word(&mut self, ctx: &egui::Context, idx: usize, r: &TextReport, w: usize, out: &mut Vec<PanelAction>) {
        let Some(word) = r.words.get(w) else { return };
        let (iw, ih) = (r.size.0.max(1) as f32, r.size.1.max(1) as f32);
        let [x0, y0, x1, y1] = word.rect;
        let h = (y1 - y0).max(4.0);
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let half_w = ((x1 - x0) * 1.5).max(h * 6.0);
        let half_h = h * 2.5;
        let rect = [((cx - half_w) / iw).max(0.0), ((cy - half_h) / ih).max(0.0), ((cx + half_w) / iw).min(1.0), ((cy + half_h) / ih).min(1.0)];
        out.push(PanelAction::ZoomTo { slot: if idx == 1 { Slot::B } else { Slot::A }, rect });
        self.show_boxes = true;
        self.flash = Some(Flash { img: idx, word: w, until: ctx.input(|i| i.time) + 1.6 });
    }

    fn boxes_toggle(&mut self, ui: &mut Ui, p: &Palette) {
        widgets::checkbox(ui, &mut self.show_boxes, "Show word boxes on the image").on_hover_text("Outline every word the recognizer found: green is sharp, amber soft, red blurry. Cut-off words have a thick red outline, overlapping words are purple.");
        if self.show_boxes {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let purple = Color32::from_rgb(170, 90, 230);
                for (c, t, tip) in [
                    (p.ok, "Sharp", "Clarity 70 or more"),
                    (p.warn, "Soft", "Clarity 40 to 70"),
                    (p.danger, "Blurry or cut", "Clarity under 40, or the word touches the image border"),
                    (purple, "Overlapping", "Drawn over another word"),
                ] {
                    look::dot(ui, c, tip);
                    ui.label(RichText::new(t).size(theme::text::CAPTION).color(p.muted)).on_hover_text(tip);
                    ui.add_space(8.0);
                }
            });
        }
    }

    fn text_section(&mut self, ui: &mut Ui, p: &Palette, r: &TextReport, out: &mut Vec<PanelAction>, core: &Core) {
        let _ = core;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Recognized text").font(theme::semibold(theme::text::CAPTION)).color(p.muted))
                .on_hover_text(format!("What the recognizer read ({} words, {} characters, {} lines, language {}). Select and copy any part.", r.words.len(), r.chars, r.lines, r.language));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::icon_button_sized(ui, I::COPY, "Copy all the recognized text", false, 22.0, 13.0).clicked() {
                    crate::fileops::copy_text(&r.text);
                    out.push(PanelAction::Toast("Copied the recognized text".into()));
                }
            });
        });
        let mut shown = if r.text.is_empty() { "(no text)" } else { r.text.as_str() };
        egui::ScrollArea::vertical().id_salt("text-recognized").max_height(220.0).show(ui, |ui| {
            ui.add(egui::TextEdit::multiline(&mut shown).font(FontId::proportional(theme::text::BODY)).desired_width(f32::INFINITY).desired_rows(4).frame(egui::Frame::NONE));
        });
    }

    fn diff_section(&mut self, ui: &mut Ui, p: &Palette, imgs: &[Image], reports: &[Option<Arc<TextReport>>]) {
        self.ensure_diffs(imgs);
        let Some(a) = imgs.first().and_then(|i| i.key()) else { return };
        let (Some(ra), true) = (reports[0].clone(), imgs.len() > 1) else { return };
        ui.label(RichText::new("Text differences from A").font(theme::semibold(theme::text::CAPTION)).color(p.muted))
            .on_hover_text("Word by word, what another capture read differently from A (case and punctuation around words are ignored). Missing: A has the word, this image does not. Extra: only this image has it. Changed: the same word read differently.");
        if imgs.len() > 2 {
            let labels: Vec<String> = (1..imgs.len()).map(|i| format!("{} vs A", LETTERS[i.min(3)])).collect();
            let labels: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
            let tips: Vec<String> = (1..imgs.len()).map(|i| format!("Compare the text of {} with A", LETTERS[i.min(3)])).collect();
            let tips: Vec<&str> = tips.iter().map(|s| s.as_str()).collect();
            let mut idx = self.diff_pick - 1;
            if widgets::segmented_with_tips(ui, &mut idx, &labels, &tips) {
                self.diff_pick = idx + 1;
            }
            ui.add_space(4.0);
        }
        let b_idx = self.diff_pick;
        let (Some(b), Some(rb)) = (imgs[b_idx].key(), reports[b_idx].clone()) else {
            look::note(ui, p, &format!("Read the text of {} to compare it.", LETTERS[b_idx.min(3)]));
            return;
        };
        let Some(d) = self.diff(&a, &b) else { return };
        look::card(ui, p, |ui| {
            ui.horizontal_wrapped(|ui| {
                let lv = match d.similarity {
                    s if s >= 95.0 => Level::Good,
                    s if s >= 80.0 => Level::Fair,
                    _ => Level::Poor,
                };
                ui.label(RichText::new(format!("{:.0}% the same", d.similarity)).strong().color(look::level_color(p, lv)))
                    .on_hover_text("Share of words both images read the same: 2 × matching words / (words in A + words in this image).");
                let purple = Color32::from_rgb(170, 90, 230);
                for (n, t, c, tip) in [
                    (d.missing, "missing", p.danger, "Words A has that this image lost (cut off, blurred away, not recognized)"),
                    (d.extra, "extra", p.ok, "Words only this image has (A lost them, or this image picked up noise or a ghost)"),
                    (d.changed, "read differently", purple, "The same word read with different letters, usually blur, compression or a broken stroke"),
                ] {
                    ui.label(RichText::new(format!("{n} {t}")).size(theme::text::SMALL).color(if n > 0 { c } else { p.faint })).on_hover_text(tip);
                }
            });
            if d.rough {
                look::note(ui, p, "The two texts are too different to line up word by word; the share counts the words they have in common.");
                return;
            }
            if d.missing + d.extra + d.changed == 0 {
                look::note(ui, p, "Both images read exactly the same words.");
                return;
            }
            widgets::checkbox(ui, &mut self.diff_only, "Only the differences").on_hover_text("Show just the words that differ, with a few words around each, instead of the whole text");
            ui.add_space(4.0);
            let job = diff_job(p, &ra, &rb, &d, self.diff_only, ui.available_width());
            egui::ScrollArea::vertical().id_salt("text-diff").max_height(260.0).show(ui, |ui| {
                ui.add(egui::Label::new(job).wrap());
            });
        });
    }

    /// Draws the word boxes over the panes on screen.
    pub fn overlay(&mut self, ui: &mut Ui, core: &Core, panes: &[canvas::Pane]) {
        let now = ui.input(|i| i.time);
        if self.flash.is_some_and(|f| f.until < now) {
            self.flash = None;
        }
        if !self.show_boxes && self.flash.is_none() {
            return;
        }
        let p = core.pal;
        let hover = ui.input(|i| i.pointer.hover_pos()).filter(|_| !crate::ui::pointer_over_overlay(ui.ctx()) && !egui::Popup::is_any_open(ui.ctx()));
        for pane in panes {
            let idx = if pane.slot == Slot::B { 1 } else { 0 };
            let Some((_, key)) = self.shown.get(idx) else { continue };
            let Some(r) = self.report(key).cloned() else { continue };
            // The pane may show the image at another size (preview, turned).
            let sx = pane.xf.size.x / r.size.0.max(1) as f32;
            let sy = pane.xf.size.y / r.size.1.max(1) as f32;
            let to_screen = |rc: [f32; 4]| pane.xf.rect_to_screen([rc[0] * sx, rc[1] * sy, rc[2] * sx, rc[3] * sy]);
            for clip in canvas::uncovered(ui.ctx(), pane.clip) {
                let painter = ui.painter().with_clip_rect(clip);
                let mut hovered = None;
                if self.show_boxes {
                    let level = r.skew_source != text::SkewSource::Engine || r.skew.is_none_or(|a| a.abs() < 0.01);
                    for (i, w) in r.words.iter().enumerate() {
                        let s = to_screen(w.rect);
                        if !s.intersects(clip) {
                            continue;
                        }
                        let c = box_color(&p, w);
                        let stroke = Stroke::new(if w.cut.is_some() { 2.5 } else { 1.2 }, c.gamma_multiply(0.9));
                        let quad: Vec<Pos2> = w.quad.iter().map(|q| pane.xf.to_screen(Pos2::new(q[0] * sx, q[1] * sy))).collect();
                        if level {
                            painter.rect_filled(s, CornerRadius::ZERO, c.gamma_multiply(0.10));
                            painter.rect_stroke(s, CornerRadius::ZERO, stroke, egui::StrokeKind::Outside);
                        } else {
                            // A tilted page: the word's own rotated outline.
                            painter.add(egui::Shape::convex_polygon(quad.clone(), c.gamma_multiply(0.10), stroke));
                        }
                        if hover.is_some_and(|h| clip.contains(h) && in_quad(&quad, h)) {
                            hovered = Some((i, quad));
                        }
                    }
                }
                if let Some(f) = self.flash.filter(|f| f.img == idx) {
                    if let Some(w) = r.words.get(f.word) {
                        let s = to_screen(w.rect);
                        let t = ((f.until - now) as f32 * 6.0).sin().abs();
                        painter.rect_stroke(s.expand(3.0 + 3.0 * t), CornerRadius::same(3), Stroke::new(2.5, p.accent), egui::StrokeKind::Outside);
                    }
                }
                if let (Some((i, quad)), Some(h)) = (hovered, hover) {
                    let w = &r.words[i];
                    painter.add(egui::Shape::closed_line(quad, Stroke::new(2.0, Color32::WHITE)));
                    word_readout(ui, &painter, h, w, &r);
                }
            }
        }
    }

    /// The per-image table for the review report.
    #[allow(dead_code)] // For the review report.
    pub fn report_table(&self) -> Option<lumen_core::report::Table> {
        let reports: Vec<(usize, &PathBuf, &Arc<TextReport>, &Option<Key>)> =
            self.shown.iter().enumerate().filter_map(|(i, (path, key))| self.report(key).map(|r| (i, path, r, key))).collect();
        if reports.is_empty() {
            return None;
        }
        let a_key = self.shown.first().and_then(|s| s.1.clone());
        let compare = self.shown.len() > 1;
        let mut header: Vec<String> = ["Image", "Score", "Words", "Text size", "Blurry", "Faint", "Cut off", "Overlapping", "Tilt", "Unread marks"].iter().map(|s| s.to_string()).collect();
        if compare {
            header.push("Same text as A".into());
        }
        let rows = reports
            .iter()
            .map(|(i, path, r, key)| {
                let mut row = vec![
                    format!("{}  {}", LETTERS[(*i).min(3)], super::file_name(path)),
                    r.score.map(|s| s.to_string()).unwrap_or("-".into()),
                    r.words.len().to_string(),
                    r.text_height.map(|h| format!("{h:.0} px")).unwrap_or("-".into()),
                    r.blurry().to_string(),
                    r.faint().to_string(),
                    r.cut().to_string(),
                    r.overlapping_words().to_string(),
                    r.skew.map(|a| format!("{:.1}°", a.abs())).unwrap_or("-".into()),
                    r.unread.map(|u| format!("{:.0}%", u * 100.0)).unwrap_or("-".into()),
                ];
                if compare {
                    row.push(match (i, &a_key, key) {
                        (0, ..) => "reference".into(),
                        (_, Some(a), Some(k)) => self.diff(a, k).map(|d| format!("{:.0}%", d.similarity)).unwrap_or("-".into()),
                        _ => "-".into(),
                    });
                }
                row
            })
            .collect();
        Some(lumen_core::report::Table {
            header,
            rows,
            note: "Text read on this computer with the Windows text recognizer. Score 0-100 (80+ good). Text size is the typical word height; blurry, faint, cut-off and overlapping are word counts.".into(),
        })
    }

    /// Dev hook `ai text <verb>`: run, boxes on|off, pick <n>, diff <n>,
    /// show <check name>, lang <tag|auto>; returns a state line.
    pub fn dev(&mut self, ctx: &egui::Context, rest: &str, out: &mut Vec<PanelAction>) -> String {
        let (verb, arg) = rest.split_once(' ').map(|(v, r)| (v, r.trim())).unwrap_or((rest, ""));
        match verb {
            "run" => self.run_requested = true,
            "boxes" => self.show_boxes = arg != "off",
            "pick" => self.pick = arg.parse().unwrap_or(0),
            "diff" => self.diff_pick = arg.parse().unwrap_or(1),
            "lang" => self.lang = (arg != "auto" && !arg.is_empty()).then(|| arg.to_string()),
            "all" => self.diff_only = arg == "off",
            "show" => {
                if let Some((_, key)) = self.shown.get(self.pick).cloned() {
                    if let Some(r) = self.report(&key).cloned() {
                        if let Some(c) = r.checks().into_iter().find(|c| c.name.eq_ignore_ascii_case(arg)) {
                            if let Some(&w) = c.words.first() {
                                self.show_word(ctx, self.pick, &r, w, out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let mut s = format!("text: busy={} lang={:?} boxes={}", self.busy(), self.lang, self.show_boxes);
        for (i, (path, key)) in self.shown.iter().enumerate() {
            match key.as_ref().and_then(|k| self.results.get(k)) {
                Some(Ok(r)) => s += &format!(
                    " | {} {}: score={:?} words={} h={:?} blurry={} faint={} cut={:?} overlaps={} skew={:?} unread={:?} summary={:?}",
                    LETTERS[i.min(3)],
                    super::file_name(path),
                    r.score,
                    r.words.len(),
                    r.text_height.map(|h| h.round()),
                    r.blurry(),
                    r.faint(),
                    r.cut_by_edge(),
                    r.overlaps.len(),
                    r.skew.map(|a| (a * 10.0).round() / 10.0),
                    r.unread.map(|u| (u * 100.0).round()),
                    r.summary
                ),
                Some(Err(e)) => s += &format!(" | {} error={e:?}", LETTERS[i.min(3)]),
                None => s += &format!(" | {} not read", LETTERS[i.min(3)]),
            }
        }
        if let Some(a) = self.shown.first().and_then(|s| s.1.clone()) {
            for (i, (_, key)) in self.shown.iter().enumerate().skip(1) {
                if let Some(d) = key.as_ref().and_then(|k| self.diff(&a, k)) {
                    s += &format!(" | diff A-{}: {:.1}% missing={} extra={} changed={}", LETTERS[i.min(3)], d.similarity, d.missing, d.extra, d.changed);
                }
            }
        }
        s
    }
}

fn img_name(img: &Image) -> String {
    super::file_name(&img.path)
}

/// "Sharp, 2 cut" for the table.
fn short_issues(r: &TextReport) -> String {
    if r.words.is_empty() {
        return "no text found".into();
    }
    let mut v = vec![format!("{} words", r.words.len())];
    let b = r.blurry();
    if b > 0 {
        v.push(format!("{b} blurry"));
    }
    if r.faint() > 0 {
        v.push(format!("{} faint", r.faint()));
    }
    if r.cut() > 0 {
        v.push(format!("{} cut", r.cut()));
    }
    if r.overlapping_words() > 0 {
        v.push(format!("{} overlap", r.overlapping_words()));
    }
    if let Some(a) = r.skew.filter(|a| a.abs() >= 0.5) {
        v.push(format!("{:.1}° tilt", a.abs()));
    }
    if let Some(u) = r.unread.filter(|u| *u > text::UNREAD_OK) {
        v.push(format!("{:.0}% unread", u * 100.0));
    }
    if v.len() == 1 {
        v.push("no problems".into());
    }
    v.join(", ")
}


/// The word under the pointer: text, clarity, contrast, problems.
fn word_readout(ui: &Ui, painter: &egui::Painter, pos: Pos2, w: &text::WordStat, r: &TextReport) {
    let mut lines = vec![format!("\"{}\"", w.text), format!("clarity {} / 100   contrast {:.0} / 255   {:.0} px tall", w.clarity, w.contrast, w.height)];
    if let Some(e) = w.cut {
        lines.push(format!("cut at the {} edge", e.name()));
    }
    if w.overlaps {
        lines.push("overlaps another word".into());
    }
    let _ = r;
    let text = lines.join("\n");
    let g = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
    let size = g.size() + Vec2::new(12.0, 8.0);
    let screen = ui.ctx().content_rect();
    let mut min = pos + Vec2::new(14.0, 16.0);
    if min.x + size.x > screen.max.x {
        min.x = pos.x - size.x - 10.0;
    }
    if min.y + size.y > screen.max.y {
        min.y = pos.y - size.y - 10.0;
    }
    let rect = Rect::from_min_size(min, size);
    let top = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("text-word-readout")));
    top.rect_filled(rect, CornerRadius::same(6), Color32::from_black_alpha(215));
    top.galley(rect.min + Vec2::new(6.0, 4.0), g, Color32::WHITE);
}

/// The word diff as flowing text: missing words struck through in red,
/// extra ones underlined green, changed ones "old → new" in purple.
fn diff_job(p: &Palette, a: &TextReport, b: &TextReport, d: &TextDiff, only: bool, width: f32) -> LayoutJob {
    let font = FontId::proportional(theme::text::BODY);
    let mut job = LayoutJob { wrap: egui::text::TextWrapping { max_width: width, ..Default::default() }, ..Default::default() };
    let purple = Color32::from_rgb(170, 90, 230);
    let plain = TextFormat { font_id: font.clone(), color: p.muted, ..Default::default() };
    let missing = TextFormat { font_id: font.clone(), color: p.danger, strikethrough: Stroke::new(1.0, p.danger), ..Default::default() };
    let extra = TextFormat { font_id: font.clone(), color: p.ok, underline: Stroke::new(1.0, p.ok), ..Default::default() };
    let changed = TextFormat { font_id: font.clone(), color: purple, ..Default::default() };
    // With `only`, keep 3 words of context around each difference.
    let diff_at: Vec<bool> = d.ops.iter().map(|o| !matches!(o, DiffOp::Same { .. })).collect();
    let near = |i: usize| (i.saturating_sub(3)..(i + 4).min(d.ops.len())).any(|j| diff_at[j]);
    let line_of = |op: &DiffOp| match *op {
        DiffOp::Same { a: i, .. } | DiffOp::Missing { a: i } | DiffOp::Changed { a: i, .. } => a.words.get(i).map(|w| w.line),
        DiffOp::Extra { .. } => None,
    };
    let mut gap = false;
    let mut last_line = None;
    let mut shown = 0usize;
    for (i, op) in d.ops.iter().enumerate() {
        if only && !near(i) {
            gap = true;
            continue;
        }
        if shown > 4000 {
            job.append(" …", 0.0, plain.clone());
            break;
        }
        if gap && shown > 0 {
            job.append("  …  ", 0.0, plain.clone());
        } else if !only && line_of(op).is_some() && last_line.is_some() && line_of(op) != last_line {
            job.append("\n", 0.0, plain.clone());
        } else if shown > 0 {
            job.append(" ", 0.0, plain.clone());
        }
        gap = false;
        if let Some(l) = line_of(op) {
            last_line = Some(l);
        }
        fn word(r: &TextReport, i: usize) -> &str {
            r.words.get(i).map(|w| w.text.as_str()).unwrap_or("")
        }
        match *op {
            DiffOp::Same { a: i, .. } => job.append(word(a, i), 0.0, plain.clone()),
            DiffOp::Missing { a: i } => job.append(word(a, i), 0.0, missing.clone()),
            DiffOp::Extra { b: j } => job.append(word(b, j), 0.0, extra.clone()),
            DiffOp::Changed { a: i, b: j } => job.append(&format!("{} → {}", word(a, i), word(b, j)), 0.0, changed.clone()),
        }
        shown += 1;
    }
    job
}

/// Inside a convex outline (either winding).
fn in_quad(q: &[Pos2], p: Pos2) -> bool {
    let mut sign = 0.0f32;
    for i in 0..q.len() {
        let (a, b) = (q[i], q[(i + 1) % q.len()]);
        let c = (b - a).x * (p - a).y - (b - a).y * (p - a).x;
        if c != 0.0 {
            if sign != 0.0 && c.signum() != sign {
                return false;
            }
            sign = c.signum();
        }
    }
    !q.is_empty()
}
