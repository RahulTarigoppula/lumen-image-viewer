//! The send preview (plan 3.1). Before anything leaves the computer, a sheet
//! lists the destination URL, the model, every image (thumbnail, pixel
//! size, encoded bytes and what it is) and the exact prompt text. Buttons:
//! Send, Cancel, and "Do not ask again for this server".

use egui::{Align, CornerRadius, Layout, RichText, Stroke, Vec2};

use super::ai::Phase;
use super::ai_view::fmt_bytes;
use crate::app::Core;
use crate::ui::icons as I;

enum Choice {
    Send { trust: bool },
    Cancel,
}

/// Shows the sheet while a prepared request waits for confirmation.
pub fn show(ctx: &egui::Context, core: &mut Core) {
    let p = core.pal;
    let card = match &core.ai.phase {
        Phase::Preview { req, .. } => core.ai.card(&req.model).cloned(),
        _ => return,
    };
    let Phase::Preview { req, thumbs, .. } = &mut core.ai.phase else { return };
    if thumbs.len() != req.images.len() {
        *thumbs = req
            .images
            .iter()
            .enumerate()
            .map(|(i, im)| ctx.load_texture(format!("ai-send-preview-{i}"), crate::render::color_image_from_rgba(&im.thumbnail), egui::TextureOptions::LINEAR))
            .collect();
    }
    let screen = ctx.content_rect();
    let mut choice = None;
    egui::Modal::new(egui::Id::unique("ai-send-preview")).show(ctx, |ui| {
        ui.set_width((screen.width() - 80.0).clamp(320.0, 640.0));
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.label(RichText::new(format!("{}  Send to your AI server?", I::PAPER_PLANE_TILT)).font(crate::ui::theme::text::title()).color(p.text));
        ui.label(RichText::new("This is exactly what leaves this computer. Nothing has been sent yet.").size(12.0).color(p.muted));
        ui.add_space(4.0);
        let row = |ui: &mut egui::Ui, k: &str, v: RichText| {
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(Vec2::new(80.0, 16.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_width(80.0);
                    ui.label(RichText::new(k).size(12.0).color(p.muted));
                });
                ui.add(egui::Label::new(v).wrap());
            });
        };
        row(ui, "To", RichText::new(&req.destination).monospace().size(12.0).color(p.text));
        row(ui, "Model", RichText::new(format!("{}, {}", req.model, req.preset_name)).size(12.5).color(p.text));
        let b64 = req.total_bytes * 4 / 3;
        let n = req.images.len();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(80.0, 16.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_width(80.0);
                ui.label(RichText::new("Size").size(12.0).color(p.muted));
            });
            let text = format!("{n} image{}, {}, about {} prompt tokens", if n == 1 { "" } else { "s" }, fmt_bytes(req.total_bytes), req.estimated_prompt_tokens);
            ui.add(egui::Label::new(RichText::new(text).size(12.0).color(p.text)).wrap())
                .on_hover_text(format!("{} as base64 on the wire; the answer is limited to {} tokens", fmt_bytes(b64), req.max_completion_tokens));
        });
        if let Some(w) = card.as_ref().and_then(|c| req.context_warning(c)) {
            ui.label(RichText::new(format!("{}  {w}", I::WARNING)).size(12.0).color(p.warn));
        }
        for n in &req.notes {
            ui.add(egui::Label::new(RichText::new(format!("{}  {n}", I::INFO)).size(11.5).color(p.muted)).wrap());
        }
        let max_h = (screen.height() * 0.55).max(160.0);
        egui::ScrollArea::vertical().id_salt("ai-send-preview-scroll").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(10.0, 10.0);
                for (im, tex) in req.images.iter().zip(thumbs.iter()) {
                    egui::Frame::new().fill(p.raised).stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(6)).inner_margin(6).show(ui, |ui| {
                        ui.set_width(140.0);
                        ui.vertical(|ui| {
                            let ts = tex.size_vec2();
                            let k = (128.0 / ts.x).min(90.0 / ts.y).min(1.0);
                            ui.add(egui::Image::new((tex.id(), ts * k)));
                            ui.label(RichText::new(format!("{}x{} {}, {}", im.width, im.height, mime_name(im.mime), fmt_bytes(im.bytes))).size(11.0).color(p.text));
                            ui.add(egui::Label::new(RichText::new(&im.description).size(10.5).color(p.muted)).wrap());
                        });
                    });
                }
            });
            ui.add_space(4.0);
            egui::CollapsingHeader::new(RichText::new("System prompt").size(crate::ui::theme::text::BODY)).icon(crate::ui::widgets::collapse_caret).id_salt("ai-preview-system").default_open(false).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&req.system_prompt).monospace().size(10.5).color(p.muted)).wrap());
            });
            egui::CollapsingHeader::new(RichText::new("Message").size(crate::ui::theme::text::BODY)).icon(crate::ui::widgets::collapse_caret).id_salt("ai-preview-user").default_open(true).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&req.user_prompt).monospace().size(10.5).color(p.text)).wrap());
            });
            if let Some(s) = &req.swapped {
                ui.label(RichText::new(format!("A second pass with A and B exchanged sends {} more image(s), {}.", s.images.len(), fmt_bytes(s.total_bytes))).size(11.5).color(p.muted));
            }
        });
        ui.label(
            RichText::new(format!(
                "{}  Pixels only: re-encoded without EXIF, GPS or color profile. File names and capture settings only if turned on in Settings > AI review.",
                I::SHIELD_CHECK
            ))
            .size(11.0)
            .color(p.faint),
        );
        ui.add_space(4.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if crate::ui::widgets::primary_button(ui, &p, &format!("{}  Send", I::PAPER_PLANE_TILT)).clicked() {
                choice = Some(Choice::Send { trust: false });
            }
            if crate::ui::widgets::secondary_button(ui, "Cancel").clicked() {
                choice = Some(Choice::Cancel);
            }
            if ui.button("Send, don't ask again").on_hover_text(format!("Send now, and skip this preview for {} from now on (Settings > AI review to undo)", req.server)).clicked() {
                choice = Some(Choice::Send { trust: true });
            }
        });
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            choice = Some(Choice::Cancel);
        }
        if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !ui.ctx().text_edit_focused() {
            choice = Some(Choice::Send { trust: false });
        }
    });
    match choice {
        Some(Choice::Send { trust }) => core.ai.send_previewed(trust),
        Some(Choice::Cancel) => core.ai.cancel(),
        None => {}
    }
}

fn mime_name(m: &str) -> &str {
    match m {
        "image/png" => "PNG",
        "image/jpeg" => "JPEG",
        other => other,
    }
}
