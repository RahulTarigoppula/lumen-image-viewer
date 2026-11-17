//! Building the round deck of a workbench project on a worker thread: every
//! scene's captures are decoded, their review marks drawn in, and the scores,
//! verdicts, artifact tags and blind test ranking collected into a
//! [`lumen_core::report::deck::Deck`].

use super::review;
use anyhow::Result;
use lumen_core::report::{self, deck};
use lumen_core::workbench::{measure, rank, Workspace};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Long side of a capture in the deck (pixels).
const SHOT_SIDE: u32 = 1600;
const CROP_SIDE: u32 = 1000;

#[derive(Clone)]
pub struct DeckJob {
    pub ws: Workspace,
    pub title: String,
    pub author: String,
    pub summary: String,
    pub format: report::Format,
    pub out: PathBuf,
    /// Include each scene's detail crops.
    pub details: bool,
    /// Only scenes marked reviewed.
    pub reviewed_only: bool,
}

/// One scene's captures decoded and annotated, with its tagged marks.
fn scene(job: &DeckJob, k: usize) -> Option<(deck::DeckScene, Vec<(String, String)>)> {
    let ws = &job.ws;
    let p = &ws.project;
    let sc = p.scenes.get(k)?;
    // One capture per device (the first when there are several).
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for (dev, path) in ws.scene_files(k) {
        if !files.iter().any(|(d, _)| *d == dev) {
            files.push((dev, path));
        }
    }
    if files.is_empty() {
        return None;
    }
    let paths: Vec<PathBuf> = files.iter().map(|(_, f)| f.clone()).collect();
    let rv = review::load_in(&review::project_reviews_dir(&ws.root), Some(&ws.root), &paths);
    let decoded: Vec<Option<std::sync::Arc<image::RgbaImage>>> = paths
        .par_iter()
        .map(|f| match lumen_core::decode::decode_file(f, lumen_core::decode::DecodeOptions { color_manage: true, animations: false }) {
            Ok(d) => Some(d.image),
            Err(e) => {
                log::warn!("round deck: could not read {}: {e:#}", f.display());
                None
            }
        })
        .collect();
    let mut shots = Vec::new();
    let mut issues = Vec::new();
    let mut tags = Vec::new();
    for (i, ((dev, path), img)) in files.iter().zip(&decoded).enumerate() {
        let Some(img) = img else { continue };
        let marks = rv.images.get(i).map(|im| im.marks.clone()).unwrap_or_default();
        let pic = match report::Picture::from_rgba(&review::annotated(img, &marks), SHOT_SIDE) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("round deck: could not encode {}: {e:#}", path.display());
                continue;
            }
        };
        let score = sc.score(dev);
        let name = p.display(dev).to_string();
        for (n, m) in marks.iter().filter(|m| m.is_valid()).enumerate() {
            if let Some(t) = &m.tag {
                issues.push(deck::DeckIssue { device: name.clone(), number: n + 1, tag: t.clone(), severity: m.severity.label().to_string(), note: m.note.trim().to_string() });
                tags.push((dev.clone(), t.clone()));
            }
        }
        let mut note = score.map(|s| s.note.trim().to_string()).unwrap_or_default();
        if let Some(c) = rv.images.get(i).map(|im| im.comment.trim().to_string()).filter(|c| !c.is_empty()) {
            note = if note.is_empty() { c } else { format!("{note}. {c}") };
        }
        shots.push(deck::DeckShot {
            device: name,
            color: p.device_color(dev),
            picture: pic,
            verdict: score.and_then(|s| s.verdict).filter(|_| p.reference.as_deref() != Some(dev.as_str())).map(|v| (v.label().to_string(), v.color())),
            scores: p.attributes.iter().filter_map(|a| score.and_then(|s| s.attrs.get(a)).map(|v| (a.clone(), *v))).collect(),
            note,
            details: format!("{} x {}", img.width(), img.height()),
        });
    }
    let mut details = Vec::new();
    if job.details {
        for (n, d) in rv.details.iter().enumerate() {
            let crops: Vec<(String, report::Picture)> = files
                .iter()
                .zip(&decoded)
                .enumerate()
                .filter_map(|(i, ((dev, _), img))| {
                    let img = img.as_ref()?;
                    let marks = rv.images.get(i).map(|im| im.marks.clone()).unwrap_or_default();
                    let pic = report::Picture::from_rgba(&review::detail_crop(img, d.rect, &marks), CROP_SIDE).ok()?;
                    Some((p.display(dev).to_string(), pic))
                })
                .collect();
            if !crops.is_empty() {
                let title = if d.title.trim().is_empty() { format!("Detail {}", n + 1) } else { d.title.trim().to_string() };
                details.push(report::Detail { title, comment: d.comment.trim().to_string(), crops });
            }
        }
    }
    let mut comment = sc.comment.trim().to_string();
    if !rv.summary.trim().is_empty() {
        comment = if comment.is_empty() { rv.summary.trim().to_string() } else { format!("{comment}\n{}", rv.summary.trim()) };
    }
    Some((deck::DeckScene { name: sc.name.clone(), comment, shots, issues, details }, tags))
}

/// Build and write the deck; `done` counts finished scenes.
pub fn run(job: &DeckJob, done: &AtomicUsize) -> Result<PathBuf> {
    let p = &job.ws.project;
    let mut scenes = Vec::new();
    let mut tag_rows: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for k in 0..p.scenes.len() {
        if job.reviewed_only && !p.scenes[k].done {
            done.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if let Some((s, tags)) = scene(job, k) {
            for (dev, tag) in tags {
                let row = tag_rows.entry(tag).or_insert_with(|| vec![0; p.devices.len()]);
                if let Some(i) = p.device_index(&dev) {
                    row[i] += 1;
                }
            }
            scenes.push(s);
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    anyhow::ensure!(!scenes.is_empty(), "no scene had a readable capture");
    let mut tag_counts: Vec<(String, Vec<usize>)> = tag_rows.into_iter().collect();
    tag_counts.sort_by(|a, b| b.1.iter().sum::<usize>().cmp(&a.1.iter().sum::<usize>()).then_with(|| a.0.cmp(&b.0)));
    let tally = p
        .tally()
        .into_iter()
        .map(|t| deck::DeckTally { device: p.display(&t.device).to_string(), color: p.device_color(&t.device), win: t.win, par: t.par, lose: t.lose, scenes: t.scenes, means: t.mean })
        .collect();
    let names: Vec<String> = p.devices.iter().map(|d| d.name.clone()).collect();
    let ranking = if p.votes.is_empty() {
        Vec::new()
    } else {
        rank::rank(&names, &p.votes)
            .into_iter()
            .filter(|r| r.votes > 0)
            .map(|r| deck::DeckRank { device: p.display(&r.device).to_string(), color: p.device_color(&r.device), score: r.score, low: r.low, high: r.high, votes: r.votes })
            .collect()
    };
    let means = p.measure_means();
    let ids = p.measured_ids();
    let measures = if ids.is_empty() {
        None
    } else {
        let mut header = vec!["Device".to_string()];
        header.extend(ids.iter().map(|id| measure::name_of(id).to_string()));
        let rows = means
            .iter()
            .filter(|(_, m)| !m.is_empty())
            .map(|(dev, m)| {
                let mut r = vec![p.display(dev).to_string()];
                r.extend(ids.iter().map(|id| m.iter().find(|(k, _)| k == id).map(|(_, v)| measure::fmt_value(id, *v)).unwrap_or_else(|| "-".into())));
                r
            })
            .collect();
        let better: Vec<String> = ids
            .iter()
            .filter_map(|id| measure::higher_is_better(id).map(|h| format!("{} {}", measure::name_of(id), if h { "higher is better" } else { "lower is better" })))
            .collect();
        let note = format!("Mean over all scenes, every capture scaled to {} px on its long side. {}.", measure::MEASURE_SIDE, better.join("; "));
        Some(report::Table { header, rows, note })
    };
    let d = deck::Deck {
        title: if job.title.trim().is_empty() { p.name.clone() } else { job.title.trim().to_string() },
        author: job.author.trim().to_string(),
        date: review::today(),
        reference: p.reference.as_ref().map(|r| p.display(r).to_string()),
        headline: p.headline(),
        summary: job.summary.trim().to_string(),
        tally,
        attributes: p.attributes.clone(),
        scale: p.scale.range(),
        ranking,
        tag_counts,
        measures,
        scenes,
    };
    deck::write(&d, job.format, &job.out)?;
    Ok(job.out.clone())
}
