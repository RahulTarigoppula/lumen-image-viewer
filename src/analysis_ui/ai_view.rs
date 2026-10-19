//! The Analysis panel's AI tab: server status and model, preset, Run and
//! Cancel, the live output while the answer streams, result cards, the
//! per-file history, and the numbered finding boxes drawn on the image.

use std::collections::HashMap;

use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use lumen_ai::{all_presets, AiReport, ImageLabel, OutputKind, Preference, Preset, PresetId, RunResult, Severity};

use super::ai::{Outcome, Phase, RunRecord, RunRequest};
use super::canvas::{Pane, Slot};
use super::{PanelAction, Subject, SubjectKind};
use crate::app::Core;
use crate::ui::icons as I;
use crate::ui::theme::{semibold, Palette};

/// Per-panel UI state of the AI tab.
#[derive(Default)]
pub struct AiView {
    /// Chosen preset id per kind of view.
    presets: HashMap<SubjectKind, String>,
    /// Free-form question (and extra instructions for the other presets).
    pub question: String,
    /// Finding under the pointer in the list (drawn highlighted on the image).
    hovered: Option<usize>,
    md: egui_commonmark::CommonMarkCache,
}

/// Built-in presets offered for a kind of view, then matching custom ones.
pub fn presets_for(kind: SubjectKind, custom: &[lumen_ai::CustomPreset]) -> Vec<Preset> {
    use PresetId::*;
    let ids: &[PresetId] = match kind {
        SubjectKind::Image => &[Quality, Photo, FreeForm, QuickScore],
        SubjectKind::Pair => &[Compare, Encode, FreeForm],
        SubjectKind::Video => &[VideoFrame, FreeForm, QuickScore],
        SubjectKind::VideoPair => &[VideoCompare, VideoFrame, FreeForm],
    };
    let pair = kind.is_pair();
    let mut v: Vec<Preset> = ids.iter().map(|&id| Preset::builtin(id)).collect();
    v.extend(all_presets(custom).into_iter().filter(|p| p.builtin.is_none() && (p.arity == lumen_ai::Arity::Two) == pair));
    v
}

pub fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Minor => Color32::from_rgb(235, 200, 70),
        Severity::Moderate => Color32::from_rgb(245, 140, 50),
        Severity::Severe => Color32::from_rgb(235, 70, 70),
    }
}

fn score_color(p: &Palette, score: u8) -> Color32 {
    match score {
        8..=10 => p.ok,
        5..=7 => p.warn,
        _ => p.danger,
    }
}

impl AiView {
    /// The preset the tab would run for `kind`.
    pub fn preset(&self, kind: SubjectKind, core: &Core) -> Preset {
        let list = presets_for(kind, &core.ai.settings.custom_presets);
        let want = self.presets.get(&kind).cloned().unwrap_or_else(|| core.ai.settings.preset.clone());
        list.iter().find(|p| p.id == want).cloned().unwrap_or_else(|| list[0].clone())
    }

    pub fn set_preset(&mut self, kind: SubjectKind, id: &str) {
        self.presets.insert(kind, id.to_string());
    }

    /// Run (button, Ctrl+Shift+Q, dev hook).
    pub fn run(&mut self, core: &mut Core, subject: &Subject, roi: Option<(u32, u32, u32, u32)>, metrics: Vec<(String, f64, String)>) {
        let preset = self.preset(subject.kind, core);
        // Quick score asks a fixed question; nothing typed is sent with it.
        let question = if preset.output == OutputKind::QuickScore { String::new() } else { self.question.clone() };
        let scope = match (subject.kind.is_pair(), roi.is_some()) {
            (true, true) => "B vs A, ROI",
            (true, false) => "B vs A",
            (false, true) => "ROI",
            (false, false) => "",
        };
        core.ai.start_run(RunRequest { subject: subject.clone(), preset, roi, question, metrics, metric_scope: scope.to_string() });
    }

    /// The tab's contents.
    pub fn ui(&mut self, ui: &mut Ui, core: &mut Core, subject: &Subject, roi: Option<(u32, u32, u32, u32)>, metrics: Vec<(String, f64, String)>, out: &mut Vec<PanelAction>) {
        let p = core.pal;
        if !core.ai.settings.enabled {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{}  Remote AI analysis is off", I::SPARKLE)).font(semibold(14.0)).color(p.text));
            ui.add_space(2.0);
            ui.add(egui::Label::new(RichText::new("Send the image (or a region of it) to your own vision model server, for example Qwen on vLLM, and get a scored critique with marked problem areas. Nothing is sent until you turn it on and press Run.").size(12.0).color(p.muted)).wrap());
            ui.add_space(8.0);
            if ui.add(egui::Button::new(RichText::new(format!("{}  Set up in Settings > AI", I::GEAR_SIX)).color(Color32::WHITE)).fill(p.accent)).clicked() {
                out.push(PanelAction::OpenSettings("Ai"));
            }
            return;
        }
        self.server_row(ui, core, out);
        ui.add_space(4.0);
        self.preset_row(ui, core, subject.kind);
        let preset = self.preset(subject.kind, core);
        // Free-form needs a question; the other presets take optional extra
        // instructions (sent too, so they always stay visible).
        if preset.output == OutputKind::Markdown {
            ui.add(egui::TextEdit::multiline(&mut self.question).hint_text("Your question, for example: Is the sky banded?").desired_rows(2).desired_width(f32::INFINITY));
        } else if preset.output != OutputKind::QuickScore {
            ui.add(egui::TextEdit::singleline(&mut self.question).hint_text("Extra instructions (optional)").desired_width(f32::INFINITY));
        }
        ui.add_space(6.0);
        self.run_row(ui, core, subject, roi, metrics);
        if let Some(e) = core.ai.error.clone() {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(format!("{}  {e}", I::WARNING)).size(12.0).color(p.danger)).wrap());
        }
        if matches!(core.ai.phase, Phase::Running { .. }) {
            self.live(ui, core);
        }
        let Some(key) = subject.file_key() else { return };
        let Some(shown) = core.ai.shown_index(&key) else { return };
        ui.add_space(8.0);
        self.history_row(ui, core, &key, shown);
        if let Some(rec) = core.ai.shown_record(&key).cloned() {
            self.card(ui, &p, &rec, subject, out);
        }
    }

    fn server_row(&mut self, ui: &mut Ui, core: &mut Core, out: &mut Vec<PanelAction>) {
        let p = core.pal;
        let ai = &mut core.ai;
        let server = ai.endpoint().map(|e| e.display());
        let (dot, tip) = match (ai.testing(), ai.connected(), &server) {
            (_, _, None) => (p.faint, "No server set".to_string()),
            (true, _, _) => (p.warn, "Testing the connection".to_string()),
            (false, Some((true, m)), _) => (p.ok, m.to_string()),
            (false, Some((false, m)), _) => (p.danger, m.to_string()),
            (false, None, _) => (p.faint, "Not tested yet: press Test".to_string()),
        };
        ui.horizontal(|ui| {
            let (r, resp) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 5.0, dot);
            resp.on_hover_text(tip.clone());
            ui.label(RichText::new(server.clone().unwrap_or_else(|| "No server".into())).size(12.5).color(p.text)).on_hover_text(tip);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if crate::ui::widgets::icon_button_sized(ui, I::GEAR_SIX, "Settings > AI", false, 24.0, 14.0).clicked() {
                    out.push(PanelAction::OpenSettings("Ai"));
                }
                if server.is_some() && ui.add_enabled(!ai.testing(), egui::Button::new("Test").small()).on_hover_text("Test connection and list the models").clicked() {
                    ai.test_connection();
                }
            });
        });
        // Model picker: the server's list after a test, or the stored id.
        let current = ai.settings.model.clone().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Model").size(12.0).color(p.muted));
            if ai.models.is_empty() {
                let resp = ui.add(egui::TextEdit::singleline(&mut ai.model_edit).hint_text("Test lists the models").desired_width(ui.available_width()));
                if resp.lost_focus() {
                    let m = ai.model_edit.clone();
                    ai.set_model(&m);
                }
            } else {
                let mut pick = None;
                egui::ComboBox::from_id_salt("ai-model")
                    .selected_text(if current.is_empty() { "Pick a model".to_string() } else { current.clone() })
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for m in &ai.models {
                            let label = format!("{}  ({})", m.id, ai.vision_label(&m.id));
                            if ui.selectable_label(m.id == current, label).clicked() {
                                pick = Some(m.id.clone());
                            }
                        }
                    });
                if let Some(m) = pick {
                    ai.set_model(&m);
                }
            }
        });
        if !current.is_empty() {
            ui.label(RichText::new(ai.vision_label(&current)).size(11.0).color(p.faint));
        }
    }

    fn preset_row(&mut self, ui: &mut Ui, core: &mut Core, kind: SubjectKind) {
        let p = core.pal;
        let list = presets_for(kind, &core.ai.settings.custom_presets);
        let cur = self.preset(kind, core);
        let mut pick = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Preset").size(12.0).color(p.muted));
            egui::ComboBox::from_id_salt(("ai-preset", kind)).selected_text(cur.name.clone()).width(ui.available_width()).show_ui(ui, |ui| {
                for pr in &list {
                    if ui.selectable_label(pr.id == cur.id, &pr.name).on_hover_text(&pr.description).clicked() {
                        pick = Some(pr.id.clone());
                    }
                }
            });
        });
        ui.label(RichText::new(&cur.description).size(11.0).color(p.faint));
        if let Some(id) = pick {
            self.set_preset(kind, &id);
            if core.ai.settings.preset != id {
                core.ai.settings.preset = id;
                core.ai.dirty = true;
            }
        }
    }

    fn run_row(&mut self, ui: &mut Ui, core: &mut Core, subject: &Subject, roi: Option<(u32, u32, u32, u32)>, metrics: Vec<(String, f64, String)>) {
        let p = core.pal;
        let busy = core.ai.busy();
        ui.horizontal(|ui| {
            if busy {
                if ui.add(egui::Button::new(RichText::new(format!("{}  Cancel", I::STOP)).color(Color32::WHITE)).fill(p.danger).min_size(Vec2::new(96.0, 28.0))).clicked() {
                    core.ai.cancel();
                }
            } else {
                let ready = subject.a.is_some();
                let resp = ui
                    .add_enabled(ready, egui::Button::new(RichText::new(format!("{}  Run", I::SPARKLE)).color(Color32::WHITE)).fill(p.accent).min_size(Vec2::new(96.0, 28.0)))
                    .on_hover_text(format!("Analyze {} ({})", if roi.is_some() { "the region" } else { "the whole image" }, crate::ui::commands::Command::RunAi.shortcut()));
                if resp.clicked() {
                    self.run(core, subject, roi, metrics);
                }
            }
            let status = match &core.ai.phase {
                Phase::Idle => None,
                Phase::Preparing { .. } => Some("Preparing images...".to_string()),
                Phase::Preview { .. } => Some("Check what will be sent".to_string()),
                Phase::Running { server, .. } => Some(match (&core.ai.live.status, core.ai.live.connected) {
                    (Some(s), _) => s.clone(),
                    (None, false) => format!("Waiting for {server}..."),
                    (None, true) => "Receiving...".to_string(),
                }),
            };
            if let Some(s) = status {
                if !matches!(core.ai.phase, Phase::Preview { .. }) {
                    ui.spinner();
                }
                ui.add(egui::Label::new(RichText::new(s).size(11.5).color(p.muted)).truncate());
            }
        });
    }

    fn live(&mut self, ui: &mut Ui, core: &mut Core) {
        let p = core.pal;
        let live = &core.ai.live;
        ui.add_space(6.0);
        let secs = live.started.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
        let mut meta = format!("{secs:.0} s");
        if let Some(u) = &live.usage {
            meta.push_str(&format!(", {} prompt + {} output tokens", u.prompt_tokens, u.completion_tokens));
        } else if live.chunks > 0 {
            // Servers stream about one token per chunk.
            meta.push_str(&format!(", about {} tokens", live.chunks));
        }
        if let Some((i, n)) = live.pass {
            meta.push_str(&format!(", pass {i} of {n}"));
        }
        ui.label(RichText::new(meta).size(11.0).color(p.faint));
        if !live.reasoning.is_empty() {
            egui::CollapsingHeader::new(RichText::new("Model reasoning").size(12.0)).id_salt("ai-live-reasoning").default_open(false).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&live.reasoning).size(11.5).color(p.muted)).wrap());
            });
        }
        egui::Frame::new().fill(p.raised).corner_radius(CornerRadius::same(6)).inner_margin(8).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("ai-live").max_height(260.0).stick_to_bottom(true).show(ui, |ui| {
                if live.text.is_empty() {
                    ui.label(RichText::new("Waiting for the first token...").size(12.0).color(p.faint));
                } else if live.markdown {
                    egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.md, &live.text);
                } else {
                    ui.add(egui::Label::new(RichText::new(&live.text).monospace().size(11.0).color(p.text)).wrap());
                }
            });
        });
        // Elapsed time keeps moving while waiting for the first token.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }

    fn history_row(&mut self, ui: &mut Ui, core: &mut Core, key: &str, shown: usize) {
        let p = core.pal;
        let count = core.ai.records(key).len();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}  Results", I::CLOCK_COUNTER_CLOCKWISE)).font(semibold(13.0)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut pick = None;
                let text = format!("{} of {count}", shown + 1);
                egui::ComboBox::from_id_salt("ai-history").selected_text(text).width(90.0).show_ui(ui, |ui| {
                    ui.set_min_width(260.0);
                    for (i, r) in core.ai.records(key).iter().enumerate().rev() {
                        let state = match &r.outcome {
                            Outcome::Done(res) => res.report.as_ref().and_then(|r| r.as_ref().ok()).and_then(overall_text).unwrap_or_else(|| "done".into()),
                            Outcome::Failed { .. } => "failed".into(),
                            Outcome::Cancelled { .. } => "cancelled".into(),
                        };
                        let frame = r.frame.map(|f| format!(", frame {}", f.index)).unwrap_or_default();
                        if ui.selectable_label(i == shown, format!("{}  {}{frame}  {state}", &r.when[11.min(r.when.len())..], r.preset)).clicked() {
                            pick = Some(i);
                        }
                    }
                    ui.separator();
                    if ui.button("Clear history for this file").clicked() {
                        pick = Some(usize::MAX);
                    }
                });
                match pick {
                    Some(usize::MAX) => core.ai.clear_history(key),
                    Some(i) => core.ai.show_record(key, i),
                    None => {}
                }
            });
        });
    }

    /// The result card of `rec`.
    fn card(&mut self, ui: &mut Ui, p: &Palette, rec: &RunRecord, subject: &Subject, out: &mut Vec<PanelAction>) {
        egui::Frame::new().fill(p.raised).stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(8)).inner_margin(10).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 5.0;
            let mut sub = rec.when.clone();
            if let Some(f) = rec.frame {
                sub.push_str(&format!(", frame {} at {}", f.index, lumen_ai::prompts::fmt_timestamp(f.timestamp_s)));
            }
            ui.label(RichText::new(&rec.preset).font(semibold(13.5)).color(p.text));
            ui.label(RichText::new(sub).size(11.0).color(p.faint));
            if let (Some(f), Some(cur)) = (rec.frame, subject.frame()) {
                if f.index != cur.index {
                    ui.label(RichText::new(format!("{}  For frame {}; boxes show on that frame only.", I::INFO, f.index)).size(11.0).color(p.warn));
                }
            }
            match &rec.outcome {
                Outcome::Done(res) => self.done(ui, p, rec, res, out),
                Outcome::Failed { message, detail, partial } => {
                    let l = ui.add(egui::Label::new(RichText::new(format!("{}  {message}", I::X_CIRCLE)).color(p.danger)).wrap());
                    if let Some(d) = detail {
                        l.on_hover_text(d);
                        ui.add(egui::Label::new(RichText::new(d).size(11.0).color(p.muted)).wrap());
                    }
                    if !partial.is_empty() {
                        raw_text(ui, p, "Partial output", partial);
                    }
                }
                Outcome::Cancelled { partial } => {
                    ui.label(RichText::new(format!("{}  Cancelled", I::STOP_CIRCLE)).color(p.muted));
                    if !partial.is_empty() {
                        raw_text(ui, p, "Partial output", partial);
                    }
                }
            }
        });
    }

    fn done(&mut self, ui: &mut Ui, p: &Palette, rec: &RunRecord, res: &RunResult, out: &mut Vec<PanelAction>) {
        for w in &res.warnings {
            ui.add(egui::Label::new(RichText::new(format!("{}  {w}", I::WARNING)).size(11.5).color(p.warn)).wrap());
        }
        match &res.report {
            Some(Ok(report)) => self.report(ui, p, rec, report, out),
            Some(Err(e)) => {
                ui.add(egui::Label::new(RichText::new(format!("{}  The answer could not be read as a report: {e}", I::WARNING)).size(12.0).color(p.warn)).wrap());
            }
            None => {}
        }
        if let Some(q) = &res.quick_score {
            match q {
                Ok(q) => {
                    ui.horizontal(|ui| {
                        badge(ui, p, &format!("{:.2}", q.mos()), score_color(p, (q.mos() * 2.0).round() as u8));
                        ui.label(RichText::new(format!("Quick score, MOS 1 to 5 (score {:.3})", q.score)).size(12.0).color(p.muted));
                    });
                }
                Err(e) => {
                    ui.label(RichText::new(e).color(p.warn));
                }
            }
        }
        if res.report.is_none() && res.quick_score.is_none() && !res.text.is_empty() {
            egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.md, &res.text);
        }
        if let Some(s) = &res.swap {
            let (txt, col) = match s.agree {
                Some(true) => ("Swap-check agrees: same preference with A and B exchanged", p.ok),
                Some(false) => ("Swap-check disagrees: the preference flips with A and B exchanged", p.warn),
                None => ("Swap-check: second pass could not be compared", p.muted),
            };
            ui.label(RichText::new(txt).size(11.5).color(col));
        }
        if !res.reasoning.is_empty() {
            egui::CollapsingHeader::new(RichText::new("Model reasoning").size(12.0)).id_salt(("ai-reasoning", &rec.when)).default_open(false).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&res.reasoning).size(11.5).color(p.muted)).wrap());
            });
        }
        if res.report.is_some() {
            raw_text(ui, p, "Raw output", &res.text);
        }
        // Footer: what was sent and what it cost.
        ui.separator();
        let mut foot = vec![res.model.clone(), res.server.clone(), format!("{:.1} s", res.elapsed_ms as f64 / 1000.0)];
        if let Some(u) = &res.usage {
            foot.push(format!("{} + {} tokens", u.prompt_tokens, u.completion_tokens));
        }
        let bytes: usize = res.sent.images.iter().map(|i| i.bytes).sum();
        foot.push(format!("{} image(s), {}", res.sent.images.len(), fmt_bytes(bytes)));
        let foot_l = ui.add(egui::Label::new(RichText::new(foot.join("  |  ")).size(10.5).color(p.faint)).wrap());
        let sent: Vec<String> = res.sent.images.iter().map(|i| format!("{} ({}x{}, {} {})", i.description, i.width, i.height, fmt_bytes(i.bytes), i.mime)).collect();
        foot_l.on_hover_text(format!("Sent to {}:\n{}", res.sent.destination, sent.join("\n")));
        for c in &res.sent.changes {
            ui.label(RichText::new(c).size(10.5).color(p.faint));
        }
        ui.horizontal(|ui| {
            if ui.button(format!("{}  Copy as Markdown", I::COPY)).clicked() {
                crate::fileops::copy_text(&markdown(rec, res));
                out.push(PanelAction::Toast("Copied the result as Markdown".into()));
            }
            if ui.button(format!("{}  Export JSON", I::EXPORT)).clicked() {
                out.push(PanelAction::ExportJson(serde_json::to_string_pretty(res).unwrap_or_default()));
            }
        });
    }

    fn report(&mut self, ui: &mut Ui, p: &Palette, rec: &RunRecord, r: &AiReport, out: &mut Vec<PanelAction>) {
        ui.add_space(2.0);
        match &r.comparison {
            Some(c) => {
                ui.horizontal(|ui| {
                    let (text, col) = match c.preferred {
                        Preference::A => ("A".to_string(), p.accent),
                        Preference::B => ("B".to_string(), Color32::from_rgb(232, 120, 60)),
                        Preference::Tie => ("=".to_string(), p.muted),
                    };
                    badge(ui, p, &text, col);
                    ui.vertical(|ui| {
                        let verdict = match c.preferred {
                            Preference::Tie => "No visible preference (tie)".to_string(),
                            x => format!("{} preferred", x.as_str()),
                        };
                        ui.label(RichText::new(verdict).font(semibold(13.0)).color(p.text));
                        let s = |v: Option<u8>| v.map(|v| format!("{v}/10")).unwrap_or_else(|| "-".into());
                        ui.label(RichText::new(format!("Confidence {:.0}%, A {}, B {}", c.confidence * 100.0, s(c.overall_a), s(c.overall_b))).size(11.5).color(p.muted));
                    });
                });
                for (a, b) in c.scores_a.iter().zip(&c.scores_b) {
                    if a.score.is_none() && b.score.is_none() {
                        continue;
                    }
                    // The reference rarely needs words: its notes are on hover.
                    ui.label(RichText::new(&a.label).size(11.5).color(p.text));
                    score_bar(ui, p, "A", a.score, &a.rationale, Some(22.0), false);
                    score_bar(ui, p, "B", b.score, &b.rationale, Some(22.0), true);
                }
            }
            None => {
                ui.horizontal(|ui| {
                    let col = r.overall.map(|s| score_color(p, s)).unwrap_or(p.muted);
                    badge(ui, p, &r.overall.map(|s| s.to_string()).unwrap_or_else(|| "-".into()), col);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Overall").font(semibold(13.0)).color(p.text));
                        ui.label(RichText::new("out of 10, the model's opinion").size(11.5).color(p.muted));
                    });
                });
                for c in r.categories.iter().filter(|c| c.score.is_some()) {
                    score_bar(ui, p, &c.label, c.score, &c.rationale, None, true);
                }
            }
        }
        if r.partial {
            ui.label(RichText::new("The answer was cut off; this report is incomplete.").size(11.5).color(p.warn));
        }
        if !r.findings.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Findings ({})", r.findings.len())).font(semibold(12.5)).color(p.text));
            let mut hovered = None;
            for (i, f) in r.findings.iter().enumerate() {
                let resp = finding_row(ui, p, i, f);
                if resp.hovered() {
                    hovered = Some(i);
                }
                if resp.clicked() {
                    if let Some(s) = f.source {
                        let dims = rec.dims[if s.label == ImageLabel::B { 1 } else { 0 }];
                        if let Some((w, h)) = dims {
                            let (w, h) = (w.max(1) as f32, h.max(1) as f32);
                            let slot = if s.label == ImageLabel::B { Slot::B } else { Slot::A };
                            out.push(PanelAction::ZoomTo { slot, rect: [s.x0 as f32 / w, s.y0 as f32 / h, s.x1 as f32 / w, s.y1 as f32 / h] });
                        }
                    }
                }
            }
            self.hovered = hovered;
        } else {
            self.hovered = None;
        }
        if !r.summary.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new("Summary").font(semibold(12.5)).color(p.text));
            ui.add(egui::Label::new(RichText::new(&r.summary).size(12.0).color(p.text)).wrap());
        }
        for w in &r.warnings {
            ui.label(RichText::new(w).size(10.5).color(p.faint));
        }
    }

    /// Numbered, severity-colored boxes of the shown result on every pane of
    /// the matching image.
    pub fn overlay(&self, ui: &Ui, core: &mut Core, subject: &Subject, panes: &[Pane]) {
        let Some(key) = subject.file_key() else { return };
        let Some(rec) = core.ai.shown_record(&key) else { return };
        if let (Some(f), Some(cur)) = (rec.frame, subject.frame()) {
            if f.index != cur.index {
                return;
            }
        }
        let Some(Ok(report)) = rec.result().and_then(|r| r.report.as_ref()) else { return };
        for pane in panes {
            let label = if pane.slot == Slot::B { ImageLabel::B } else { ImageLabel::A };
            let Some((w, h)) = rec.dims[if pane.slot == Slot::B { 1 } else { 0 }] else { continue };
            // In a wipe or split view the other image covers the rest of the
            // same place: its side shows this image's boxes dashed, so a
            // finding stays visible wherever the divider is, without being
            // mistaken for one of the other image.
            let twin = pane.twin(panes);
            let painter = ui.painter().with_clip_rect(pane.clip);
            let ghost = twin.map(|t| ui.painter().with_clip_rect(t.clip));
            let whole = twin.map(|t| pane.clip.union(t.clip)).unwrap_or(pane.clip);
            for (i, f) in report.findings.iter().enumerate() {
                let Some(s) = f.source.filter(|s| s.label == label) else { continue };
                let k = Vec2::new(pane.xf.size.x / w.max(1) as f32, pane.xf.size.y / h.max(1) as f32);
                let r = pane.xf.rect_to_screen([s.x0 as f32 * k.x, s.y0 as f32 * k.y, s.x1 as f32 * k.x, s.y1 as f32 * k.y]);
                let col = severity_color(f.severity);
                let hot = self.hovered == Some(i);
                if hot {
                    painter.rect_filled(r, 0.0, col.gamma_multiply(0.18));
                }
                painter.rect_stroke(r, 0.0, Stroke::new(if hot { 4.5 } else { 3.0 }, Color32::from_black_alpha(120)), egui::StrokeKind::Middle);
                painter.rect_stroke(r, 0.0, Stroke::new(if hot { 2.5 } else { 1.5 }, col), egui::StrokeKind::Middle);
                if let Some(g) = &ghost {
                    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
                    g.add(egui::Shape::line(pts.to_vec(), Stroke::new(3.0, Color32::from_black_alpha(90))));
                    g.extend(egui::Shape::dashed_line(&pts, Stroke::new(if hot { 2.0 } else { 1.2 }, col), 6.0, 4.0));
                }
                // Number tag above the box, or inside it at the top of the view;
                // with a twin it names the image ("B2") and stays visible on
                // either side, solid where its own image is shown.
                let text = if twin.is_some() { format!("{}{}", label.as_str(), i + 1) } else { (i + 1).to_string() };
                let tag_w = if twin.is_some() { 26.0 } else { 18.0 };
                let tag_min = if r.min.y - 18.0 >= whole.min.y { Pos2::new(r.min.x, r.min.y - 18.0) } else { r.min };
                let tag = Rect::from_min_size(tag_min, Vec2::new(tag_w, 18.0));
                let tags = ui.painter().with_clip_rect(whole);
                if pane.clip.contains(tag.center()) || twin.is_none() {
                    tags.rect_filled(tag, CornerRadius::same(3), col);
                    tags.text(tag.center(), Align2::CENTER_CENTER, text, semibold(11.0), Color32::BLACK);
                } else {
                    tags.rect_filled(tag, CornerRadius::same(3), Color32::from_black_alpha(190));
                    tags.rect_stroke(tag, CornerRadius::same(3), Stroke::new(1.0, col), egui::StrokeKind::Inside);
                    tags.text(tag.center(), Align2::CENTER_CENTER, text, semibold(11.0), col);
                }
            }
        }
    }
}

fn overall_text(r: &AiReport) -> Option<String> {
    match &r.comparison {
        Some(c) => Some(match c.preferred {
            Preference::Tie => "tie".into(),
            x => format!("{} preferred", x.as_str()),
        }),
        None => r.overall.map(|s| format!("{s}/10")),
    }
}

fn badge(ui: &mut Ui, p: &Palette, text: &str, col: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(44.0, 44.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(10), col.gamma_multiply(0.22));
    ui.painter().rect_stroke(r, CornerRadius::same(10), Stroke::new(1.5, col), egui::StrokeKind::Inside);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, text, semibold(19.0), if p.dark { Color32::WHITE } else { p.text });
}

/// "Noise  [=======   ] 7" with the rationale on hover, and below in small
/// text when `show_rationale`. `label_w` is the label column (`None`: wide).
fn score_bar(ui: &mut Ui, p: &Palette, label: &str, score: Option<u8>, rationale: &str, label_w: Option<f32>, show_rationale: bool) {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(Vec2::new(w, 16.0), Sense::hover());
    let label_w = label_w.unwrap_or((w * 0.5).min(190.0));
    let painter = ui.painter();
    painter.text(Pos2::new(r.min.x, r.center().y), Align2::LEFT_CENTER, label, FontId::proportional(11.5), p.muted);
    let bar = Rect::from_min_max(Pos2::new(r.min.x + label_w, r.center().y - 4.0), Pos2::new(r.max.x - 26.0, r.center().y + 4.0));
    painter.rect_filled(bar, CornerRadius::same(4), p.hover);
    if let Some(s) = score {
        let f = Rect::from_min_size(bar.min, Vec2::new(bar.width() * s.min(10) as f32 / 10.0, bar.height()));
        painter.rect_filled(f, CornerRadius::same(4), score_color(p, s));
    }
    painter.text(Pos2::new(r.max.x, r.center().y), Align2::RIGHT_CENTER, score.map(|s| s.to_string()).unwrap_or_else(|| "-".into()), semibold(12.0), p.text);
    if !rationale.is_empty() {
        resp.on_hover_text(rationale);
        if show_rationale {
            ui.add(egui::Label::new(RichText::new(rationale).size(10.5).color(p.faint)).wrap());
        }
    }
}

fn finding_row(ui: &mut Ui, p: &Palette, i: usize, f: &lumen_ai::Finding) -> egui::Response {
    let col = severity_color(f.severity);
    let cat = lumen_ai::prompts::CATEGORIES.iter().find(|c| c.0 == f.category).map(|c| c.1).unwrap_or(&f.category);
    let inner = ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(20.0, 18.0), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(3), col);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, (i + 1).to_string(), semibold(11.0), Color32::BLACK);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            let place = if f.tile == 0 { format!("image {}", f.image.as_str()) } else { format!("image {}, tile {}", f.image.as_str(), f.tile) };
            ui.label(RichText::new(format!("{} {}, {place}", f.severity.as_str(), cat.to_lowercase())).size(11.0).color(col));
            ui.add(egui::Label::new(RichText::new(&f.description).size(11.5).color(p.text)).wrap());
        });
    });
    let resp = ui.interact(inner.response.rect, ui.scope_id().with(("ai-finding", i)), Sense::click());
    if resp.hovered() {
        ui.painter().rect_stroke(inner.response.rect.expand(2.0), CornerRadius::same(4), Stroke::new(1.0, p.border), egui::StrokeKind::Outside);
    }
    if f.source.is_some() {
        resp.on_hover_text("Click to zoom to this area").on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn raw_text(ui: &mut Ui, p: &Palette, title: &str, text: &str) {
    egui::CollapsingHeader::new(RichText::new(title).size(12.0)).id_salt(("ai-raw", title, text.len())).default_open(false).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(("ai-raw-scroll", title)).max_height(220.0).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(text).monospace().size(10.5).color(p.muted)).wrap());
        });
    });
}

pub fn fmt_bytes(b: usize) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

/// "Copy as Markdown": the report (or free-form answer) and where it came from.
pub fn markdown(rec: &RunRecord, res: &RunResult) -> String {
    let mut md = match &res.report {
        Some(Ok(r)) => r.to_markdown(&res.preset_name),
        _ => match &res.quick_score {
            Some(Ok(q)) => format!("## {}: MOS {:.2} (score {:.3})\n", res.preset_name, q.mos(), q.score),
            _ => format!("## {}\n\n{}\n", res.preset_name, res.text.trim()),
        },
    };
    // The label already names the frame ("clip.mp4, frame 120"): add its time.
    let at = rec.frame.map(|f| format!(" at {}", lumen_ai::prompts::fmt_timestamp(f.timestamp_s))).unwrap_or_default();
    md.push_str(&format!("\n_{}{at}. {}, {} via {}. AI opinion, not a measurement._\n", rec.label, rec.when, res.model, res.server));
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_lists_fit_the_view() {
        let ids = |k| presets_for(k, &[]).into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(ids(SubjectKind::Image)[0], "quality");
        assert_eq!(ids(SubjectKind::Pair), ["compare", "encode", "freeform"]);
        assert_eq!(ids(SubjectKind::Video)[0], "video_frame");
        assert_eq!(ids(SubjectKind::VideoPair)[0], "video_compare");
        let custom = [lumen_ai::CustomPreset { name: "Mine".into(), pair: true, ..Default::default() }];
        assert!(presets_for(SubjectKind::Pair, &custom).iter().any(|p| p.id == "custom:Mine"));
        assert!(!presets_for(SubjectKind::Image, &custom).iter().any(|p| p.id == "custom:Mine"));
    }
}
