//! The round deck: one report for a whole workbench project. A title page,
//! an executive summary (Win / Par / Lose per device, mean scores), the
//! scorecard by attribute, the flagged artifacts, the blind test ranking,
//! then every scene with its captures side by side, scores, verdicts and
//! issues, and its detail crops.
//!
//! It is laid out into the same [`Slide`]s as a single comparison, so the
//! PowerPoint and PDF writers are shared; the HTML version shows the slides
//! as scalable pages.

use super::{best_grid, contain, fitted, header, leading, text, text_width, wrap, Align, Box2, Detail, El, Format, Picture, Slide, Table, TextEl, ACCENT, INK, M, MUTED, PAGE_H, PAGE_W, PANEL, RULE};
use anyhow::Result;
use std::path::Path;

/// One capture of a scene in the deck.
#[derive(Clone, Debug)]
pub struct DeckShot {
    pub device: String,
    pub color: [u8; 3],
    /// The capture with its marks drawn in.
    pub picture: Picture,
    /// ("Win", color) against the reference.
    pub verdict: Option<(String, [u8; 3])>,
    /// (attribute, score) in project order.
    pub scores: Vec<(String, i32)>,
    pub note: String,
    /// "4000 x 3000, 3.2 MB".
    pub details: String,
}

/// A flagged mark.
#[derive(Clone, Debug, PartialEq)]
pub struct DeckIssue {
    pub device: String,
    /// The mark's number on its capture.
    pub number: usize,
    pub tag: String,
    pub severity: String,
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct DeckScene {
    pub name: String,
    pub comment: String,
    pub shots: Vec<DeckShot>,
    pub issues: Vec<DeckIssue>,
    /// Detail crops (the same region from every capture), labeled by device.
    pub details: Vec<Detail>,
}

#[derive(Clone, Debug, Default)]
pub struct DeckTally {
    pub device: String,
    pub color: [u8; 3],
    pub win: usize,
    pub par: usize,
    pub lose: usize,
    pub scenes: usize,
    /// Mean score per attribute (project order; missing = not scored).
    pub means: Vec<(String, f64)>,
}

/// A blind test result row.
#[derive(Clone, Debug)]
pub struct DeckRank {
    pub device: String,
    pub color: [u8; 3],
    pub score: f64,
    pub low: f64,
    pub high: f64,
    pub votes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Deck {
    pub title: String,
    pub author: String,
    pub date: String,
    pub reference: Option<String>,
    /// Plain-language headline lines.
    pub headline: Vec<String>,
    pub summary: String,
    pub tally: Vec<DeckTally>,
    pub attributes: Vec<String>,
    /// Score scale (min, max).
    pub scale: (i32, i32),
    pub ranking: Vec<DeckRank>,
    /// (tag, count per device in `tally` order).
    pub tag_counts: Vec<(String, Vec<usize>)>,
    pub scenes: Vec<DeckScene>,
}

const BODY_TOP: f32 = M + 44.0 + 12.0;

fn body() -> Box2 {
    Box2::new(M, BODY_TOP, PAGE_W - 2.0 * M, PAGE_H - BODY_TOP - M - 4.0)
}

fn footer(s: &mut Slide, title: &str, page: usize) {
    let y = PAGE_H - M + 6.0;
    s.els.push(text(Box2::new(M, y, PAGE_W / 2.0, 20.0), title.to_string(), 8.0, false, MUTED));
    let num = page.to_string();
    let w = text_width(&num, 8.0, false);
    s.els.push(text(Box2::new(PAGE_W - M - w - 2.0, y, w + 4.0, 20.0), num, 8.0, false, MUTED));
}

fn centered(bx: Box2, t: &str, size: f32, bold: bool, color: [u8; 3]) -> El {
    El::Text(TextEl { bx, text: t.to_string(), size, bold, color, align: Align::Center })
}

/// A colored chip with the device name; returns its width.
fn chip(s: &mut Slide, x: f32, y: f32, name: &str, color: [u8; 3], size: f32, max_w: f32) -> f32 {
    let h = leading(size) + 4.0;
    let w = (text_width(name, size, true) + 14.0).min(max_w).max(h);
    s.els.push(El::Fill { bx: Box2::new(x, y, w, h), color });
    s.els.push(fitted(Box2::new(x + 7.0, y + 2.0, w - 10.0, leading(size)), name, &[size, size * 0.85], true, [255, 255, 255]));
    w
}

fn short_attr(a: &str) -> String {
    match a {
        "Dynamic range" => "DR".into(),
        "Exposure" => "Exp".into(),
        "Artifacts" => "Artif.".into(),
        _ if a.chars().count() > 9 => a.chars().take(8).collect::<String>() + ".",
        _ => a.to_string(),
    }
}

fn fmt_mean(v: f64) -> String {
    format!("{v:.1}")
}

fn title_page(d: &Deck) -> Slide {
    let mut s = Slide::default();
    s.els.push(El::Fill { bx: Box2::new(0.0, 0.0, PAGE_W, PAGE_H), color: [24, 30, 42] });
    s.els.push(El::Fill { bx: Box2::new(72.0, 200.0, 64.0, 4.0), color: ACCENT });
    let title = if d.title.trim().is_empty() { "Camera comparison".to_string() } else { d.title.clone() };
    s.els.push(fitted(Box2::new(72.0, 216.0, PAGE_W - 144.0, 92.0), &title, &[40.0, 34.0, 28.0], true, [255, 255, 255]));
    let sub = format!("{} device{} · {} scene{}", d.tally.len(), if d.tally.len() == 1 { "" } else { "s" }, d.scenes.len(), if d.scenes.len() == 1 { "" } else { "s" });
    s.els.push(text(Box2::new(72.0, 314.0, PAGE_W - 144.0, 22.0), sub, 16.0, false, [200, 208, 220]));
    let mut x = 72.0;
    for t in &d.tally {
        let name = if d.reference.as_deref() == Some(t.device.as_str()) { format!("{} (reference)", t.device) } else { t.device.clone() };
        if x > PAGE_W - 200.0 {
            break;
        }
        x += chip(&mut s, x, 346.0, &name, t.color, 11.0, (PAGE_W - 72.0 - x).min(220.0)) + 8.0;
    }
    let by = [d.date.as_str(), d.author.as_str()].iter().filter(|t| !t.is_empty()).cloned().collect::<Vec<_>>().join("  ·  ");
    if !by.is_empty() {
        s.els.push(text(Box2::new(72.0, 440.0, PAGE_W - 144.0, 18.0), by, 12.0, false, [200, 208, 220]));
    }
    s.els.push(text(Box2::new(72.0, PAGE_H - 48.0, 300.0, 14.0), "Made with Lumen", 9.0, false, [120, 130, 146]));
    s
}

fn summary_page(d: &Deck, title: &str) -> Slide {
    let mut s = Slide::default();
    header(&mut s, title, "Executive summary");
    let b = body();
    let mut y = b.y;
    let mut lines: Vec<String> = d.headline.clone();
    if !d.summary.trim().is_empty() {
        lines.insert(0, d.summary.trim().to_string());
    }
    if !lines.is_empty() {
        let txt = lines.iter().map(|l| format!("•  {l}")).collect::<Vec<_>>().join("\n");
        let h = (wrap(&txt, 13.0, false, b.w - 28.0).len() as f32 * leading(13.0) + 20.0).min(b.h * 0.42);
        s.els.push(El::Fill { bx: Box2::new(b.x, y, b.w, h), color: PANEL });
        s.els.push(El::Fill { bx: Box2::new(b.x, y, 3.0, h), color: ACCENT });
        s.els.push(fitted(Box2::new(b.x + 14.0, y + 10.0, b.w - 28.0, h - 16.0), &txt, &[13.0, 11.0, 9.0], false, INK));
        y += h + 16.0;
    }
    // Win / Par / Lose bars (devices with verdicts).
    let judged: Vec<&DeckTally> = d.tally.iter().filter(|t| t.win + t.par + t.lose > 0).collect();
    if !judged.is_empty() {
        let against = d.reference.as_deref().map(|r| format!("Against {r}")).unwrap_or_else(|| "Verdicts".into());
        s.els.push(text(Box2::new(b.x, y, b.w, 14.0), against.to_uppercase(), 9.0, true, ACCENT));
        y += 18.0;
        let label_w = 150.0;
        let bar_w = b.w - label_w - 120.0;
        let row = 22.0;
        for t in judged.iter().take(((b.y + b.h - y) / row).floor().max(0.0) as usize) {
            s.els.push(El::Fill { bx: Box2::new(b.x, y + 4.0, 4.0, row - 8.0), color: t.color });
            s.els.push(fitted(Box2::new(b.x + 10.0, y + 3.0, label_w - 14.0, row - 4.0), &t.device, &[11.0, 9.0], true, INK));
            let n = (t.win + t.par + t.lose) as f32;
            let mut x = b.x + label_w;
            for (count, color) in [(t.win, crate::workbench::Verdict::Win.color()), (t.par, crate::workbench::Verdict::Par.color()), (t.lose, crate::workbench::Verdict::Lose.color())] {
                if count == 0 {
                    continue;
                }
                let w = bar_w * count as f32 / n;
                s.els.push(El::Fill { bx: Box2::new(x, y + 3.0, w, row - 6.0), color });
                if w > 18.0 {
                    s.els.push(centered(Box2::new(x, y + 3.0 + (row - 6.0 - leading(9.0)) / 2.0, w, leading(9.0)), &count.to_string(), 9.0, true, [255, 255, 255]));
                }
                x += w;
            }
            let label = format!("{} W · {} P · {} L", t.win, t.par, t.lose);
            s.els.push(text(Box2::new(b.x + label_w + bar_w + 10.0, y + 4.0, 110.0, row), label, 9.0, false, MUTED));
            y += row;
        }
        y += 12.0;
    }
    // Mean scores table.
    let scored: Vec<&DeckTally> = d.tally.iter().filter(|t| !t.means.is_empty()).collect();
    if !scored.is_empty() && y < b.y + b.h - 60.0 {
        let attrs: Vec<&String> = d.attributes.iter().filter(|a| scored.iter().any(|t| t.means.iter().any(|(k, _)| k == *a))).take(8).collect();
        let mut header_row = vec!["Mean score".to_string()];
        header_row.extend(attrs.iter().map(|a| short_attr(a)));
        let rows: Vec<Vec<String>> = scored
            .iter()
            .map(|t| {
                let mut r = vec![t.device.clone()];
                r.extend(attrs.iter().map(|a| t.means.iter().find(|(k, _)| k == *a).map(|(_, v)| fmt_mean(*v)).unwrap_or_else(|| "-".into())));
                r
            })
            .collect();
        let row_h = 22.0;
        let h = (row_h * (rows.len() + 1) as f32).min(b.y + b.h - y);
        s.els.push(El::Table { bx: Box2::new(b.x, y, b.w, h), table: Table { header: header_row, rows, note: String::new() } });
    }
    s
}

/// Grouped bars per attribute: one bar per device.
fn attribute_page(d: &Deck, title: &str) -> Option<Slide> {
    let scored: Vec<&DeckTally> = d.tally.iter().filter(|t| !t.means.is_empty()).collect();
    let attrs: Vec<&String> = d.attributes.iter().filter(|a| scored.iter().any(|t| t.means.iter().any(|(k, _)| k == *a))).collect();
    if scored.is_empty() || attrs.is_empty() {
        return None;
    }
    let mut s = Slide::default();
    header(&mut s, title, "Scorecard by attribute");
    let b = body();
    let (lo, hi) = d.scale;
    let span = (hi - lo).max(1) as f32;
    // Legend.
    let mut x = b.x;
    for t in &scored {
        x += chip(&mut s, x, b.y, &t.device, t.color, 9.0, 180.0) + 6.0;
        if x > b.x + b.w - 120.0 {
            break;
        }
    }
    let top = b.y + 28.0;
    let label_h = 18.0;
    let chart_h = b.y + b.h - top - label_h - 4.0;
    let group_w = b.w / attrs.len() as f32;
    let bar_w = ((group_w - 16.0) / scored.len() as f32).min(28.0);
    // Zero line (relative scale) or baseline.
    let zero = if lo < 0 { top + chart_h * (hi as f32 / span) } else { top + chart_h };
    s.els.push(El::Fill { bx: Box2::new(b.x, zero, b.w, 1.0), color: RULE });
    for (k, a) in attrs.iter().enumerate() {
        let gx = b.x + k as f32 * group_w;
        let used = bar_w * scored.len() as f32;
        let mut bx = gx + (group_w - used) / 2.0;
        for t in &scored {
            if let Some((_, v)) = t.means.iter().find(|(name, _)| name == *a) {
                let v = *v as f32;
                let (y0, y1) = if lo < 0 {
                    let y = top + chart_h * ((hi as f32 - v) / span);
                    (y.min(zero), y.max(zero))
                } else {
                    (top + chart_h * ((hi as f32 - v) / span), zero)
                };
                let h = (y1 - y0).max(1.0);
                s.els.push(El::Fill { bx: Box2::new(bx + 2.0, y0, bar_w - 4.0, h), color: t.color });
                let ty = if lo < 0 && v < 0.0 { y1 + 2.0 } else { (y0 - leading(8.0) - 1.0).max(top - 10.0) };
                s.els.push(centered(Box2::new(bx, ty, bar_w, leading(8.0)), &fmt_mean(v as f64), 8.0, false, MUTED));
            }
            bx += bar_w;
        }
        s.els.push(centered(Box2::new(gx, top + chart_h + 4.0, group_w, label_h), &short_attr(a), 10.0, true, INK));
    }
    Some(s)
}

fn issues_page(d: &Deck, title: &str) -> Option<Slide> {
    if d.tag_counts.is_empty() {
        return None;
    }
    let mut s = Slide::default();
    header(&mut s, title, "Flagged artifacts");
    let b = body();
    let mut head = vec!["Artifact".to_string()];
    head.extend(d.tally.iter().map(|t| t.device.clone()));
    let rows: Vec<Vec<String>> = d
        .tag_counts
        .iter()
        .take(14)
        .map(|(tag, counts)| {
            let mut r = vec![tag.clone()];
            r.extend(counts.iter().map(|c| if *c == 0 { "-".to_string() } else { c.to_string() }));
            r
        })
        .collect();
    let row_h = 24.0;
    let h = (row_h * (rows.len() + 1) as f32).min(b.h - 30.0);
    s.els.push(El::Table { bx: Box2::new(b.x, b.y, b.w, h), table: Table { header: head, rows, note: String::new() } });
    s.els.push(fitted(Box2::new(b.x, b.y + h + 8.0, b.w, 24.0), "Number of marked areas per artifact and device, over all scenes. Fewer is better.", &[10.0], false, MUTED));
    Some(s)
}

fn ranking_page(d: &Deck, title: &str) -> Option<Slide> {
    if d.ranking.is_empty() {
        return None;
    }
    let mut s = Slide::default();
    header(&mut s, title, "Blind test");
    let b = body();
    let label_w = 170.0;
    let chart_w = b.w - label_w - 150.0;
    let row = (b.h / (d.ranking.len() as f32 + 2.0)).min(40.0);
    let mut y = b.y + 10.0;
    // 50% line.
    let mid = b.x + label_w + chart_w * 0.5;
    let rows_h = row * d.ranking.len() as f32;
    s.els.push(El::Fill { bx: Box2::new(mid, y, 1.0, rows_h), color: RULE });
    for r in &d.ranking {
        s.els.push(El::Fill { bx: Box2::new(b.x, y + 6.0, 4.0, row - 12.0), color: r.color });
        s.els.push(fitted(Box2::new(b.x + 10.0, y + (row - leading(12.0)) / 2.0, label_w - 14.0, leading(12.0)), &r.device, &[12.0, 10.0], true, INK));
        let x0 = b.x + label_w;
        let w = chart_w * (r.score / 100.0) as f32;
        s.els.push(El::Fill { bx: Box2::new(x0, y + row * 0.28, w.max(1.0), row * 0.44), color: r.color });
        // Interval whisker.
        let lx = x0 + chart_w * (r.low / 100.0) as f32;
        let hx = x0 + chart_w * (r.high / 100.0) as f32;
        s.els.push(El::Fill { bx: Box2::new(lx, y + row / 2.0 - 0.75, (hx - lx).max(1.0), 1.5), color: INK });
        s.els.push(El::Fill { bx: Box2::new(lx, y + row * 0.3, 1.5, row * 0.4), color: INK });
        s.els.push(El::Fill { bx: Box2::new(hx - 1.5, y + row * 0.3, 1.5, row * 0.4), color: INK });
        let label = format!("{:.0}  ({:.0}-{:.0}), {} votes", r.score, r.low, r.high, r.votes);
        s.els.push(text(Box2::new(x0 + chart_w + 10.0, y + (row - leading(10.0)) / 2.0, 140.0, leading(10.0)), label, 10.0, false, MUTED));
        y += row;
    }
    let note = "Score: chance (0-100) that the device is preferred over an average device in a blind side-by-side choice (Bradley-Terry model). The line shows the 90% interval; overlapping intervals mean the difference is not certain.";
    s.els.push(fitted(Box2::new(b.x, y + 14.0, b.w, 40.0), note, &[10.0, 9.0], false, MUTED));
    Some(s)
}

fn scene_page(d: &Deck, k: usize, sc: &DeckScene) -> Slide {
    let mut s = Slide::default();
    header(&mut s, &sc.name, &format!("Scene {} of {}", k + 1, d.scenes.len()));
    let b = body();
    let mut notes_txt = String::new();
    if !sc.comment.trim().is_empty() {
        notes_txt.push_str(sc.comment.trim());
    }
    for sh in &sc.shots {
        if !sh.note.trim().is_empty() {
            if !notes_txt.is_empty() {
                notes_txt.push('\n');
            }
            notes_txt.push_str(&format!("{}: {}", sh.device, sh.note.trim()));
        }
    }
    for is in &sc.issues {
        if !notes_txt.is_empty() {
            notes_txt.push('\n');
        }
        let note = if is.note.trim().is_empty() { String::new() } else { format!(": {}", is.note.trim()) };
        notes_txt.push_str(&format!("{} #{}  {} ({}){}", is.device, is.number, is.tag, is.severity.to_lowercase(), note));
    }
    let bottom_h = if notes_txt.is_empty() { 0.0 } else { (wrap(&notes_txt, 10.0, false, b.w - 28.0).len() as f32 * leading(10.0) + 16.0).clamp(36.0, b.h * 0.3) };
    let has_scores = sc.shots.iter().any(|x| !x.scores.is_empty());
    let score_h = if has_scores { 30.0 } else { 0.0 };
    let cap = 22.0;
    let area = Box2::new(b.x, b.y, b.w, b.h - bottom_h - if bottom_h > 0.0 { 10.0 } else { 0.0 });
    let aspects: Vec<f32> = sc.shots.iter().map(|x| x.picture.w.max(1) as f32 / x.picture.h.max(1) as f32).collect();
    let gap = 12.0;
    let (cols, rows) = best_grid(&aspects, area.w, area.h - (cap + score_h) * 1.0, gap);
    let cw = (area.w - gap * (cols as f32 - 1.0)) / cols.max(1) as f32;
    let ch = (area.h - gap * (rows as f32 - 1.0)) / rows.max(1) as f32;
    for (i, sh) in sc.shots.iter().enumerate() {
        let (cx, cy) = (i % cols.max(1), i / cols.max(1));
        let cell = Box2::new(area.x + cx as f32 * (cw + gap), area.y + cy as f32 * (ch + gap), cw, ch);
        let ib = contain(Box2::new(cell.x, cell.y + cap + 2.0, cell.w, cell.h - cap - 2.0 - score_h), aspects[i]);
        let cy0 = ib.y - cap - 2.0;
        let w = chip(&mut s, ib.x, cy0 + 2.0, &sh.device, sh.color, 10.0, (ib.w * 0.7).max(80.0));
        if let Some((v, c)) = &sh.verdict {
            let vw = text_width(v, 9.0, true) + 12.0;
            let vx = (ib.x + ib.w - vw).max(ib.x + w + 4.0);
            s.els.push(El::Fill { bx: Box2::new(vx, cy0 + 3.0, vw, leading(9.0) + 4.0), color: *c });
            s.els.push(centered(Box2::new(vx, cy0 + 5.0, vw, leading(9.0)), v, 9.0, true, [255, 255, 255]));
        }
        s.els.push(El::Image { bx: ib, pic: sh.picture.clone() });
        s.els.push(El::Frame { bx: ib, color: sh.color, width: 1.25 });
        if has_scores && !sh.scores.is_empty() {
            let line = sh.scores.iter().map(|(a, v)| format!("{} {}", short_attr(a), v)).collect::<Vec<_>>().join(" · ");
            s.els.push(fitted(Box2::new(ib.x, ib.y + ib.h + 4.0, ib.w.max(120.0).min(cell.w), score_h - 4.0), &line, &[9.0, 8.0, 7.0], false, MUTED));
        }
    }
    if bottom_h > 0.0 {
        let sb = Box2::new(b.x, b.y + b.h - bottom_h, b.w, bottom_h);
        s.els.push(El::Fill { bx: sb, color: PANEL });
        s.els.push(El::Fill { bx: Box2::new(sb.x, sb.y, 3.0, sb.h), color: ACCENT });
        s.els.push(fitted(Box2::new(sb.x + 14.0, sb.y + 8.0, sb.w - 28.0, sb.h - 12.0), &notes_txt, &[10.0, 9.0, 8.0], false, INK));
    }
    s.notes = notes_txt;
    s
}

fn detail_page(d: &Deck, sc: &DeckScene, k: usize, det: &Detail) -> Slide {
    let mut s = Slide::default();
    let t = if det.title.trim().is_empty() { format!("{}: detail {}", sc.name, k + 1) } else { format!("{}: {}", sc.name, det.title.trim()) };
    header(&mut s, &t, "Detail");
    let b = body();
    let ch_comment = if det.comment.trim().is_empty() { 0.0 } else { 54.0 };
    let area = Box2::new(b.x, b.y, b.w, b.h - ch_comment - if ch_comment > 0.0 { 10.0 } else { 0.0 });
    let aspects: Vec<f32> = det.crops.iter().map(|(_, p)| p.w.max(1) as f32 / p.h.max(1) as f32).collect();
    let gap = 12.0;
    let cap = 22.0;
    let (cols, rows) = best_grid(&aspects, area.w, area.h - cap, gap);
    let cw = (area.w - gap * (cols as f32 - 1.0)) / cols.max(1) as f32;
    let chh = (area.h - gap * (rows as f32 - 1.0)) / rows.max(1) as f32;
    for (i, (label, pic)) in det.crops.iter().enumerate() {
        let (cx, cy) = (i % cols.max(1), i / cols.max(1));
        let cell = Box2::new(area.x + cx as f32 * (cw + gap), area.y + cy as f32 * (chh + gap), cw, chh);
        let color = d.tally.iter().find(|t| &t.device == label).map(|t| t.color).unwrap_or(ACCENT);
        let ib = contain(Box2::new(cell.x, cell.y + cap + 2.0, cell.w, cell.h - cap - 2.0), aspects[i]);
        chip(&mut s, ib.x, ib.y - cap, label, color, 10.0, (ib.w * 0.8).max(80.0));
        s.els.push(El::Image { bx: ib, pic: pic.clone() });
        s.els.push(El::Frame { bx: ib, color, width: 1.25 });
    }
    if ch_comment > 0.0 {
        let sb = Box2::new(b.x, b.y + b.h - ch_comment, b.w, ch_comment);
        s.els.push(El::Fill { bx: sb, color: PANEL });
        s.els.push(El::Fill { bx: Box2::new(sb.x, sb.y, 3.0, sb.h), color: ACCENT });
        s.els.push(fitted(Box2::new(sb.x + 14.0, sb.y + 8.0, sb.w - 28.0, sb.h - 12.0), &det.comment, &[11.0, 9.0], false, INK));
    }
    s.notes = det.comment.clone();
    s
}

/// Lay the deck out as pages.
pub fn layout(d: &Deck) -> Vec<Slide> {
    let title = if d.title.trim().is_empty() { "Camera comparison".to_string() } else { d.title.clone() };
    let mut slides = vec![title_page(d)];
    slides.push(summary_page(d, &title));
    slides.extend(attribute_page(d, &title));
    slides.extend(issues_page(d, &title));
    slides.extend(ranking_page(d, &title));
    for (k, sc) in d.scenes.iter().enumerate() {
        if sc.shots.is_empty() {
            continue;
        }
        slides.push(scene_page(d, k, sc));
        for (j, det) in sc.details.iter().enumerate() {
            slides.push(detail_page(d, sc, j, det));
        }
    }
    for (k, s) in slides.iter_mut().enumerate().skip(1) {
        footer(s, &title, k + 1);
    }
    slides
}

/// Write the deck to `path` (atomically).
pub fn write(d: &Deck, fmt: Format, path: &Path) -> Result<()> {
    let slides = layout(d);
    let bytes = match fmt {
        Format::Pptx => super::pptx::render_slides(&slides, &d.title, &d.author)?,
        Format::Pdf => super::pdf::render_slides(&slides, &d.title)?,
        Format::Html => super::html::render_slides(&slides, &d.title).into_bytes(),
    };
    crate::encode::write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;

    fn pic(w: u32, h: u32) -> Picture {
        Picture::from_rgba(&RgbaImage::from_pixel(w, h, image::Rgba([90, 120, 160, 255])), 800).unwrap()
    }

    pub fn sample() -> Deck {
        let devices = [("A56", [47u8, 111, 235]), ("PhoneX", [234, 120, 32]), ("PhoneY", [34, 160, 92])];
        let attrs: Vec<String> = ["Exposure", "Color", "Noise", "Overall"].iter().map(|s| s.to_string()).collect();
        let tally = devices
            .iter()
            .enumerate()
            .map(|(i, (n, c))| DeckTally { device: n.to_string(), color: *c, win: if i == 0 { 0 } else { 2 }, par: 1, lose: i, scenes: 3, means: attrs.iter().map(|a| (a.clone(), 3.0 + i as f64 * 0.5)).collect() })
            .collect();
        let scenes = (0..3)
            .map(|k| DeckScene {
                name: format!("Night {k}"),
                comment: "Street lights flare on X".into(),
                shots: devices
                    .iter()
                    .map(|(n, c)| DeckShot { device: n.to_string(), color: *c, picture: pic(400, 300), verdict: Some(("Win".into(), [34, 160, 92])), scores: vec![("Noise".into(), 4), ("Overall".into(), 3)], note: "soft corners".into(), details: String::new() })
                    .collect(),
                issues: vec![DeckIssue { device: "PhoneX".into(), number: 1, tag: "Flare".into(), severity: "Severe".into(), note: "lamp".into() }],
                details: vec![Detail { title: "Lamp".into(), comment: "flare".into(), crops: devices.iter().map(|(n, _)| (n.to_string(), pic(200, 200))).collect() }],
            })
            .collect();
        Deck {
            title: "Round 3 night".into(),
            author: "Rahul".into(),
            date: "17 November 2026".into(),
            reference: Some("A56".into()),
            headline: vec!["PhoneX: 2 wins, 1 par, 1 loss against A56 over 4 scenes".into()],
            summary: "PhoneY leads on noise.".into(),
            tally,
            attributes: attrs,
            scale: (1, 5),
            ranking: vec![DeckRank { device: "PhoneY".into(), color: [34, 160, 92], score: 64.0, low: 52.0, high: 75.0, votes: 20 }, DeckRank { device: "A56".into(), color: [47, 111, 235], score: 40.0, low: 30.0, high: 51.0, votes: 20 }],
            tag_counts: vec![("Flare".into(), vec![0, 3, 1]), ("Noise".into(), vec![2, 1, 0])],
            scenes,
        }
    }

    #[test]
    fn deck_pages_stay_on_page() {
        let d = sample();
        let slides = layout(&d);
        // title, summary, attributes, issues, ranking, 3 x (scene + detail)
        assert_eq!(slides.len(), 5 + 6);
        for s in &slides {
            for e in &s.els {
                let b = match e {
                    El::Fill { bx, .. } | El::Frame { bx, .. } | El::Image { bx, .. } | El::Table { bx, .. } => *bx,
                    El::Text(t) => t.bx,
                };
                assert!(b.x >= -0.01 && b.y >= -0.01 && b.x + b.w <= PAGE_W + 0.01 && b.y + b.h <= PAGE_H + 0.01, "{b:?}");
            }
        }
    }

    #[test]
    fn relative_scale_and_empty_parts() {
        let mut d = sample();
        d.scale = (-3, 3);
        for t in &mut d.tally {
            for m in &mut t.means {
                m.1 -= 3.5;
            }
        }
        d.ranking.clear();
        d.tag_counts.clear();
        let slides = layout(&d);
        assert_eq!(slides.len(), 3 + 6);
    }

    #[test]
    fn deck_writers_produce_files() {
        let d = sample();
        let dir = std::env::temp_dir().join(format!("lumen-deck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in Format::ALL {
            let p = dir.join(format!("deck.{}", f.ext()));
            write(&d, f, &p).unwrap();
            assert!(std::fs::metadata(&p).unwrap().len() > 2000);
        }
        if let Some(out) = std::env::var_os("LUMEN_REPORT_OUT") {
            let out = std::path::PathBuf::from(out);
            std::fs::create_dir_all(&out).unwrap();
            for f in Format::ALL {
                write(&d, f, &out.join(format!("deck.{}", f.ext()))).unwrap();
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
