//! UI for the advanced batch pipeline: the reorderable step list, per-step
//! editors and named presets.

use super::convert::ConvertSettings;
use super::logic::ResizeMode;
use super::pipeline::*;
use crate::imaging::Filter;
use crate::ui::batch::hint;
use crate::ui::icons as I;
use crate::ui::theme::Palette;
use egui::{Color32, CornerRadius, RichText, Ui, Vec2};

/// Color button for a straight-alpha RGBA value. Returns true when changed.
pub fn color_rgba(ui: &mut Ui, c: &mut [u8; 4], alpha: bool) -> bool {
    let mut col = Color32::from_rgba_unmultiplied(c[0], c[1], c[2], if alpha { c[3] } else { 255 });
    if crate::ui::widgets::color_button_rgba(ui, &mut col, alpha).changed() {
        let v = col.to_srgba_unmultiplied();
        *c = [v[0], v[1], v[2], if alpha { v[3] } else { 255 }];
        true
    } else {
        false
    }
}

/// 3x3 anchor picker (0 = top-left ... 8 = bottom-right).
pub fn anchor_grid(ui: &mut Ui, p: &Palette, anchor: &mut u8) -> bool {
    let mut changed = false;
    egui::Grid::new(ui.next_auto_id()).spacing([2.0, 2.0]).striped(false).show(ui, |ui| {
        for r in 0..3u8 {
            for c in 0..3u8 {
                let i = r * 3 + c;
                let on = *anchor == i;
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::click());
                let fill = if on { p.accent } else if resp.hovered() { p.accent.gamma_multiply(0.35) } else { ui.visuals().widgets.inactive.bg_fill };
                ui.painter().rect_filled(rect, CornerRadius::same(3), fill);
                if resp.clicked() {
                    *anchor = i;
                    changed = true;
                }
            }
            ui.end_row();
        }
    });
    changed
}

fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(label);
    add(ui);
    ui.end_row();
}

fn font_combo(ui: &mut Ui, id: &str, font: &mut String) {
    crate::ui::widgets::combo(id).width(150.0).selected_text(font.as_str()).show_ui(ui, |ui| {
        for f in crate::edit::text::families() {
            if ui.selectable_label(f.name.eq_ignore_ascii_case(font), &f.name).clicked() {
                *font = f.name.clone();
            }
        }
    })
    .response
    .on_hover_text("Typeface of the text");
}

const TOKEN_HINT: &str = "Tokens: {name} {date} {taken} {taken:%Y} {n} {width} {height} {folder}";

/// Editor for one step's parameters.
pub fn step_editor(ui: &mut Ui, p: &Palette, step: &mut Step, id: usize) {
    let gid = |s: &str| format!("adv-{s}-{id}");
    egui::Grid::new(gid("grid")).num_columns(2).spacing([10.0, 6.0]).striped(false).show(ui, |ui| match step {
        Step::Resize(r) => {
            row(ui, "Mode", |ui| {
                crate::ui::widgets::combo(gid("rmode")).width(150.0).selected_text(r.spec.mode.label()).show_ui(ui, |ui| {
                    for m in ResizeMode::ALL {
                        ui.selectable_value(&mut r.spec.mode, m, m.label());
                    }
                })
                .response
                .on_hover_text("How the new size is given: a percentage, a box to fit in, or one side");
            });
            match r.spec.mode {
                ResizeMode::None => {}
                ResizeMode::Percent => row(ui, "Scale", |ui| {
                    crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.spec.percent).range(1.0..=1000.0).suffix(" %")).on_hover_text("New size as a percentage of the original");
                }),
                ResizeMode::Fit => row(ui, "Box", |ui| {
                    ui.horizontal(|ui| {
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.spec.a).range(1..=60000).suffix(" px")).on_hover_text("Largest width; the shape of the image is kept");
                        ui.label("x");
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.spec.b).range(1..=60000).suffix(" px")).on_hover_text("Largest height; the shape of the image is kept");
                    });
                }),
                _ => row(ui, "Size", |ui| {
                    crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut r.spec.a).range(1..=60000).suffix(" px")).on_hover_text("New length of that side; the other side follows the shape");
                }),
            }
            row(ui, "", |ui| {
                crate::ui::widgets::checkbox(ui, &mut r.spec.no_upscale, "Don't enlarge smaller images").on_hover_text("Images already smaller than the target keep their size");
            });
            row(ui, "Filter", |ui| {
                crate::ui::widgets::combo(gid("rfilter")).width(150.0).selected_text(r.filter.label()).show_ui(ui, |ui| {
                    for f in Filter::ALL {
                        ui.selectable_value(&mut r.filter, f, f.label());
                    }
                })
                .response
                .on_hover_text("How pixels are blended when resizing; Lanczos is sharpest");
            });
        }
        Step::Crop(c) => {
            row(ui, "Crop by", |ui| {
                ui.horizontal(|ui| {
                    let mut i = [CropMode::Aspect, CropMode::Size, CropMode::Rect].iter().position(|m| *m == c.mode).unwrap_or(0);
                    if crate::ui::widgets::segmented_with_tips(ui, &mut i, &["Aspect ratio", "Size", "Rectangle"], &["Keep the largest area with this width-to-height shape", "Keep an area of this many pixels", "Keep an exact area given by its corner and size"]) {
                        c.mode = [CropMode::Aspect, CropMode::Size, CropMode::Rect][i];
                    }
                });
            });
            match c.mode {
                CropMode::Aspect => {
                    row(ui, "Ratio", |ui| {
                        ui.horizontal(|ui| {
                            crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.aspect_w).range(0.1..=100.0).speed(0.1)).on_hover_text("Width part of the shape, e.g. 16 in 16:9");
                            ui.label(":");
                            crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.aspect_h).range(0.1..=100.0).speed(0.1)).on_hover_text("Height part of the shape, e.g. 9 in 16:9");
                            for (a, b) in [(1.0, 1.0), (3.0, 2.0), (4.0, 3.0), (16.0, 9.0)] {
                                if ui.small_button(format!("{a}:{b}")).on_hover_text("Use this shape").clicked() {
                                    c.aspect_w = a;
                                    c.aspect_h = b;
                                }
                            }
                        });
                    });
                }
                CropMode::Size | CropMode::Rect => {
                    if c.mode == CropMode::Rect {
                        row(ui, "Left, top", |ui| {
                            ui.horizontal(|ui| {
                                crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.x).range(0..=60000).suffix(" px")).on_hover_text("Distance of the area from the left edge");
                                crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.y).range(0..=60000).suffix(" px")).on_hover_text("Distance of the area from the top edge");
                            });
                        });
                    }
                    row(ui, "Size", |ui| {
                        ui.horizontal(|ui| {
                            crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.w).range(1..=60000).suffix(" px"));
                            ui.label("x");
                            crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.h).range(1..=60000).suffix(" px"));
                        });
                    });
                }
            }
            if c.mode != CropMode::Rect {
                row(ui, "Position", |ui| {
                    anchor_grid(ui, p, &mut c.anchor);
                });
            }
        }
        Step::Canvas(c) => {
            row(ui, "Canvas", |ui| {
                ui.horizontal(|ui| {
                    let mut i = [CanvasMode::Add, CanvasMode::Absolute, CanvasMode::Percent].iter().position(|m| *m == c.mode).unwrap_or(0);
                    if crate::ui::widgets::segmented_with_tips(ui, &mut i, &["Add to sides", "Size", "Percent"], &["Add the same space on every side", "Make the canvas an exact size (smaller trims)", "Make the canvas a percentage of the image size"]) {
                        c.mode = [CanvasMode::Add, CanvasMode::Absolute, CanvasMode::Percent][i];
                    }
                });
            });
            match c.mode {
                CanvasMode::Add => row(ui, "Per side", |ui| {
                    crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.add).range(0..=20000).suffix(" px")).on_hover_text("Space added on each side");
                }),
                CanvasMode::Absolute => row(ui, "Size", |ui| {
                    ui.horizontal(|ui| {
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.w).range(1..=40000).suffix(" px"));
                        ui.label("x");
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.h).range(1..=40000).suffix(" px"));
                    });
                }),
                CanvasMode::Percent => row(ui, "Size", |ui| {
                    ui.horizontal(|ui| {
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.percent_w).range(1.0..=1000.0).suffix(" %"));
                        ui.label("x");
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut c.percent_h).range(1.0..=1000.0).suffix(" %"));
                    });
                }),
            }
            if c.mode != CanvasMode::Add {
                row(ui, "Anchor", |ui| {
                    anchor_grid(ui, p, &mut c.anchor);
                });
            }
            row(ui, "Color", |ui| {
                color_rgba(ui, &mut c.color, true);
            });
        }
        Step::Rotate(r) => {
            row(ui, "Rotate", |ui| {
                ui.horizontal(|ui| {
                    for (q, l) in [(0u8, "0"), (1, "90 cw"), (2, "180"), (3, "90 ccw")] {
                        ui.selectable_value(&mut r.quarter, q, l).on_hover_text("Turn by this many degrees (cw = clockwise, ccw = counter-clockwise)");
                    }
                });
            });
            row(ui, "Fine angle", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut r.angle, -45.0..=45.0).bipolar().default_value(0.0).suffix(" deg").fixed_decimals(1)).on_hover_text("Small extra rotation to straighten (double-click to reset)");
            });
            if r.angle != 0.0 {
                row(ui, "", |ui| {
                    ui.horizontal(|ui| {
                        crate::ui::widgets::checkbox(ui, &mut r.crop, "Crop corners").on_hover_text("Cut away the empty corners a tilt leaves, otherwise fill them with the color");
                        if !r.crop {
                            color_rgba(ui, &mut r.bg, true);
                        }
                    });
                });
            }
            row(ui, "Flip", |ui| {
                ui.horizontal(|ui| {
                    crate::ui::widgets::checkbox(ui, &mut r.flip_h, "Horizontal").on_hover_text("Mirror left to right");
                    crate::ui::widgets::checkbox(ui, &mut r.flip_v, "Vertical").on_hover_text("Mirror top to bottom");
                });
            });
            row(ui, "Apply to", |ui| {
                ui.horizontal(|ui| {
                    let mut i = [RotateWhen::Always, RotateWhen::Portrait, RotateWhen::Landscape].iter().position(|m| *m == r.when).unwrap_or(0);
                    if crate::ui::widgets::segmented_with_tips(ui, &mut i, &["All", "Portrait", "Landscape"], &["Rotate every image", "Rotate only images taller than wide", "Rotate only images wider than tall"]) {
                        r.when = [RotateWhen::Always, RotateWhen::Portrait, RotateWhen::Landscape][i];
                    }
                });
            });
        }
        Step::Adjust(a) => {
            row(ui, "Brightness", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut a.brightness, -100.0..=100.0).bipolar().default_value(0.0).fixed_decimals(0)).on_hover_text("Make the image lighter or darker (double-click to reset)");
            });
            row(ui, "Contrast", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut a.contrast, -100.0..=100.0).bipolar().default_value(0.0).fixed_decimals(0)).on_hover_text("Increase or reduce the difference between light and dark (double-click to reset)");
            });
            row(ui, "Gamma", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut a.gamma, 0.2..=3.0).origin(1.0).default_value(1.0).fixed_decimals(2)).on_hover_text("Brighten or darken the midtones, keeping black and white (1 = unchanged)");
            });
            row(ui, "Saturation", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut a.saturation, -100.0..=100.0).bipolar().default_value(0.0).fixed_decimals(0)).on_hover_text("Make colors more vivid or more muted (double-click to reset)");
            });
        }
        Step::AutoLevels => row(ui, "", |ui| {
            hint(ui, p, "Stretches each image's levels automatically.");
        }),
        Step::Effect { effect } => row(ui, "Effect", |ui| {
            ui.horizontal(|ui| {
                crate::ui::widgets::segmented_value(ui, effect, &[(EffectKind::Grayscale, "Grayscale"), (EffectKind::Sepia, "Sepia"), (EffectKind::Invert, "Invert")]);
            });
        }),
        Step::Sharpen(s) => {
            row(ui, "Amount", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut s.amount, 0.0..=500.0).suffix(" %").fixed_decimals(0)).on_hover_text("How strongly edges are sharpened");
            });
            row(ui, "Radius", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut s.radius, 0.2..=10.0).suffix(" px").fixed_decimals(1)).on_hover_text("Size of the detail sharpened: small for fine texture, large for outlines");
            });
            row(ui, "Threshold", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut s.threshold, 0..=64)).on_hover_text("Skip smooth areas so noise and skin are not sharpened (0 = sharpen everything)");
            });
        }
        Step::Border(b) => {
            row(ui, "Width", |ui| {
                crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut b.width).range(0.0..=2000.0).suffix(" px")).on_hover_text("Border width on each side");
            });
            row(ui, "Bottom", |ui| {
                crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut b.bottom).range(0.0..=4000.0).suffix(" px")).on_hover_text("Extra space at the bottom, like a print frame or a caption strip");
            });
            row(ui, "Color", |ui| {
                color_rgba(ui, &mut b.color, true);
            });
            row(ui, "Line", |ui| {
                ui.horizontal(|ui| {
                    crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut b.line_width).range(0.0..=200.0).suffix(" px")).on_hover_text("Thin line drawn around the image inside the border (0 = none)");
                    color_rgba(ui, &mut b.line_color, true);
                });
            });
            row(ui, "Corner radius", |ui| {
                crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut b.radius).range(0.0..=2000.0).suffix(" px")).on_hover_text("Round the corners by this much (0 = square)");
            });
            row(ui, "", |ui| {
                crate::ui::widgets::checkbox(ui, &mut b.shadow, "Drop shadow").on_hover_text("Draw a soft shadow under the framed image");
            });
            if b.shadow {
                row(ui, "Shadow", |ui| {
                    ui.horizontal(|ui| {
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut b.shadow_blur).range(0.0..=500.0).prefix("blur ")).on_hover_text("How soft the shadow is");
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut b.shadow_offset).range(-500.0..=500.0).prefix("offset ")).on_hover_text("How far the shadow is moved down and right");
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut b.shadow_opacity).range(0.0..=100.0).suffix(" %")).on_hover_text("How dark the shadow is");
                    });
                });
                row(ui, "Behind", |ui| {
                    color_rgba(ui, &mut b.shadow_bg, true);
                });
            }
        }
        Step::Text(t) => {
            row(ui, "Text", |ui| {
                ui.add(egui::TextEdit::multiline(&mut t.text).desired_rows(2).desired_width(220.0)).on_hover_text("Text to stamp; tokens like {name} or {taken} are replaced per image");
            });
            row(ui, "", |ui| {
                hint(ui, p, TOKEN_HINT);
            });
            row(ui, "Font", |ui| {
                ui.horizontal(|ui| {
                    font_combo(ui, &gid("tfont"), &mut t.font);
                    crate::ui::widgets::checkbox(ui, &mut t.bold, "B").on_hover_text("Bold");
                    crate::ui::widgets::checkbox(ui, &mut t.italic, "I").on_hover_text("Italic");
                });
            });
            row(ui, "Size", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut t.size_pct, 0.5..=30.0).suffix(" % of short side").fixed_decimals(1)).on_hover_text("Text height relative to the image, so it looks the same on every size");
            });
            row(ui, "Color", |ui| {
                ui.horizontal(|ui| {
                    color_rgba(ui, &mut t.color, true);
                    ui.add(crate::ui::widgets::Slider::new(&mut t.opacity, 0.0..=100.0).suffix(" %").fixed_decimals(0)).on_hover_text("How solid the text is (0 = invisible)");
                });
            });
            row(ui, "Position", |ui| {
                ui.horizontal(|ui| {
                    anchor_grid(ui, p, &mut t.anchor);
                    crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut t.margin_pct).range(0.0..=40.0).speed(0.1).prefix("margin ").suffix(" %")).on_hover_text("Distance from the image edge");
                });
            });
            row(ui, "Align", |ui| {
                ui.horizontal(|ui| {
                    crate::ui::widgets::segmented_value(ui, &mut t.align, &[(0, "Left"), (1, "Center"), (2, "Right")]);
                });
            });
            row(ui, "Outline", |ui| {
                ui.horizontal(|ui| {
                    crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut t.outline_pct).range(0.0..=30.0).speed(0.2).suffix(" %")).on_hover_text("Outline around the letters so they read on any background (0 = none)");
                    color_rgba(ui, &mut t.outline_color, true);
                });
            });
            row(ui, "", |ui| {
                ui.horizontal(|ui| {
                    crate::ui::widgets::checkbox(ui, &mut t.shadow, "Shadow").on_hover_text("Soft shadow behind the letters");
                    crate::ui::widgets::checkbox(ui, &mut t.background, "Background").on_hover_text("Solid box behind the text");
                    if t.background {
                        color_rgba(ui, &mut t.bg_color, true);
                    }
                });
            });
        }
        Step::Watermark(w) => {
            row(ui, "Source", |ui| {
                ui.horizontal(|ui| {
                    let mut i = if w.source == WatermarkSource::Text { 0 } else { 1 };
                    if crate::ui::widgets::segmented_with_tips(ui, &mut i, &["Text", "Image (PNG logo)"], &["Stamp a line of text", "Stamp a picture, e.g. a logo with a transparent background"]) {
                        w.source = if i == 0 { WatermarkSource::Text } else { WatermarkSource::Image };
                    }
                });
            });
            match w.source {
                WatermarkSource::Text => {
                    row(ui, "Text", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut w.text).desired_width(220.0)).on_hover_text("Watermark text; tokens like {name} or {taken} are replaced per image");
                    });
                    row(ui, "", |ui| {
                        hint(ui, p, TOKEN_HINT);
                    });
                    row(ui, "Font", |ui| {
                        ui.horizontal(|ui| {
                            font_combo(ui, &gid("wfont"), &mut w.font);
                            crate::ui::widgets::checkbox(ui, &mut w.bold, "B").on_hover_text("Bold");
                            color_rgba(ui, &mut w.color, false);
                        });
                    });
                }
                WatermarkSource::Image => row(ui, "Image", |ui| {
                    let label = w.image.as_ref().map(|p| crate::ui::batch::file_name(p)).unwrap_or_else(|| "Choose...".into());
                    if ui.add(egui::Button::new(format!("{}  {label}", I::FILE_IMAGE)).truncate()).on_hover_text("Choose the logo image").clicked() {
                        if let Some(f) = rfd::FileDialog::new().add_filter("Images", &["png", "webp", "gif", "bmp", "jpg", "jpeg", "tif", "tiff"]).pick_file() {
                            w.image = Some(f);
                        }
                    }
                }),
            }
            row(ui, "Size", |ui| {
                let suffix = if w.source == WatermarkSource::Image { " % of width" } else { " % of short side" };
                ui.add(crate::ui::widgets::Slider::new(&mut w.size_pct, 0.5..=100.0).suffix(suffix).fixed_decimals(1)).on_hover_text("Watermark size relative to the image, so it looks the same on every size");
            });
            row(ui, "Opacity", |ui| {
                ui.add(crate::ui::widgets::Slider::new(&mut w.opacity, 0.0..=100.0).suffix(" %").fixed_decimals(0)).on_hover_text("How solid the watermark is (lower lets the photo show through)");
            });
            row(ui, "Layout", |ui| {
                ui.horizontal(|ui| {
                    let mut i = w.tiled as usize;
                    if crate::ui::widgets::segmented_with_tips(ui, &mut i, &["Single", "Tiled"], &["One watermark at the chosen position", "Repeat the watermark across the whole image"]) {
                        w.tiled = i == 1;
                    }
                });
            });
            if w.tiled {
                row(ui, "Angle", |ui| {
                    ui.add(crate::ui::widgets::Slider::new(&mut w.angle, -90.0..=90.0).bipolar().default_value(0.0).suffix(" deg").fixed_decimals(0)).on_hover_text("Tilt of the repeated watermarks");
                });
                row(ui, "Gap", |ui| {
                    ui.add(crate::ui::widgets::Slider::new(&mut w.gap_pct, 0.0..=60.0).suffix(" %").fixed_decimals(0)).on_hover_text("Space between the repeated watermarks");
                });
            } else {
                row(ui, "Position", |ui| {
                    ui.horizontal(|ui| {
                        anchor_grid(ui, p, &mut w.anchor);
                        crate::ui::widgets::value_box(ui, egui::DragValue::new(&mut w.margin_pct).range(0.0..=40.0).speed(0.1).prefix("margin ").suffix(" %")).on_hover_text("Distance from the image edge");
                    });
                });
            }
        }
        Step::Dpi { dpi } => {
            row(ui, "Resolution", |ui| {
                ui.horizontal(|ui| {
                    crate::ui::widgets::value_box(ui, egui::DragValue::new(dpi).range(1..=4800).suffix(" dpi")).on_hover_text("Print size hint stored in the file: dots per inch when printed");
                    for d in [72u16, 96, 150, 300] {
                        if ui.small_button(d.to_string()).on_hover_text("Use this resolution").clicked() {
                            *dpi = d;
                        }
                    }
                });
            });
            row(ui, "", |ui| {
                hint(ui, p, "Written to JPEG, PNG and BMP output. Pixels are not changed.");
            });
        }
    });
}

/// The reorderable list of steps with an editor for the selected one.
pub fn pipeline_ui(ui: &mut Ui, p: &Palette, pipe: &mut Pipeline, selected: &mut Option<usize>) {
    let n = pipe.steps.len();
    if n == 0 {
        hint(ui, p, "No steps yet. Add steps; they run from top to bottom on every image.");
    }
    let mut action: Option<(usize, i32)> = None; // (index, -1 up, +1 down, 0 delete, 2 duplicate)
    for i in 0..n {
        let sel = *selected == Some(i);
        let fill = if sel { p.accent.gamma_multiply(0.14) } else { Color32::TRANSPARENT };
        egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::symmetric(6, 3)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let e = &mut pipe.steps[i];
                crate::ui::widgets::checkbox(ui, &mut e.enabled, "").on_hover_text("Enable or skip this step");
                ui.label(RichText::new(format!("{}.", i + 1)).color(p.faint));
                let enabled = e.enabled;
                let (label, summary) = (e.step.label(), e.step.summary());
                // Buttons first (right to left) so the summary truncates in the space left.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if ui.small_button(I::TRASH).on_hover_text("Remove").clicked() {
                        action = Some((i, 0));
                    }
                    if ui.add_enabled(i + 1 < n, egui::Button::new(I::ARROW_DOWN).small()).on_hover_text("Move down").clicked() {
                        action = Some((i, 1));
                    }
                    if ui.add_enabled(i > 0, egui::Button::new(I::ARROW_UP).small()).on_hover_text("Move up").clicked() {
                        action = Some((i, -1));
                    }
                    if ui.small_button(I::COPY_SIMPLE).on_hover_text("Duplicate").clicked() {
                        action = Some((i, 2));
                    }
                    if ui.small_button(if sel { I::CARET_UP } else { I::PENCIL_SIMPLE }).on_hover_text("Edit").clicked() {
                        *selected = if sel { None } else { Some(i) };
                    }
                    ui.add_space(6.0);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let text = RichText::new(label).strong().color(if enabled { p.text } else { p.faint });
                        let r = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                        let s = ui.add(egui::Label::new(RichText::new(&summary).size(11.5).color(p.muted)).truncate().sense(egui::Sense::click())).on_hover_text(&summary);
                        if r.clicked() || s.clicked() {
                            *selected = if sel { None } else { Some(i) };
                        }
                    });
                });
            });
            if sel {
                ui.add_space(4.0);
                ui.indent(("adv-edit", i), |ui| step_editor(ui, p, &mut pipe.steps[i].step, i));
                ui.add_space(4.0);
            }
        });
    }
    match action {
        Some((i, 0)) => {
            pipe.steps.remove(i);
            *selected = None;
        }
        Some((i, 2)) => {
            let e = pipe.steps[i].clone();
            pipe.steps.insert(i + 1, e);
            *selected = Some(i + 1);
        }
        Some((i, d)) => {
            let to = (i as i32 + d).max(0) as usize;
            pipe.move_step(i, to);
            if *selected == Some(i) {
                *selected = Some(to);
            } else if *selected == Some(to) {
                *selected = Some(i);
            }
        }
        None => {}
    }
    ui.add_space(4.0);
    ui.menu_button(format!("{}  Add step", I::PLUS), |ui| {
        for t in Step::templates() {
            if ui.button(t.label()).clicked() {
                pipe.push(t);
                *selected = Some(pipe.steps.len() - 1);
                ui.close();
            }
        }
    })
    .response
    .on_hover_text("Add a step such as crop, watermark or border to the end of the list");
}

/// Named presets of all batch convert settings.
pub struct PresetUi {
    file: PresetFile,
    name: String,
    chosen: Option<String>,
    msg: Option<(bool, String)>,
    confirm_replace: bool,
}

impl PresetUi {
    pub fn new() -> Self {
        PresetUi { file: load_presets(), name: String::new(), chosen: None, msg: None, confirm_replace: false }
    }

    fn save(&mut self, s: &ConvertSettings) {
        let name = self.name.trim().to_string();
        self.file.upsert(Preset { name: name.clone(), settings: s.clone() });
        self.msg = Some(match save_presets(&self.file) {
            Ok(()) => (true, format!("Saved preset '{name}'")),
            Err(e) => (false, format!("Could not save presets: {e}")),
        });
        self.chosen = Some(name);
        self.confirm_replace = false;
    }

    pub fn ui(&mut self, ui: &mut Ui, p: &Palette, s: &mut ConvertSettings) {
        ui.horizontal_wrapped(|ui| {
            let label = self.chosen.clone().unwrap_or_else(|| if self.file.presets.is_empty() { "No presets".into() } else { "Choose a preset".into() });
            crate::ui::widgets::combo("bc-presets").width(170.0).selected_text(label).show_ui(ui, |ui| {
                for pr in &self.file.presets {
                    if ui.selectable_label(self.chosen.as_deref() == Some(pr.name.as_str()), &pr.name).clicked() {
                        self.chosen = Some(pr.name.clone());
                        self.name = pr.name.clone();
                    }
                }
            })
            .response
            .on_hover_text("Saved settings you can load again");
            let chosen = self.chosen.clone().and_then(|c| self.file.get(&c).cloned());
            if ui.add_enabled(chosen.is_some(), egui::Button::new(format!("{}  Load", I::DOWNLOAD_SIMPLE))).on_hover_text("Replace the current settings with this preset").clicked() {
                if let Some(pr) = &chosen {
                    // Keep the source-related choice: presets describe the output.
                    let include = s.include_subfolders;
                    *s = pr.settings.clone();
                    s.include_subfolders = include;
                    self.msg = Some((true, format!("Loaded preset '{}'", pr.name)));
                }
            }
            if ui.add_enabled(chosen.is_some(), egui::Button::new(I::TRASH)).on_hover_text("Delete this preset").clicked() {
                if let Some(pr) = chosen {
                    self.file.remove(&pr.name);
                    self.chosen = None;
                    self.msg = Some(match save_presets(&self.file) {
                        Ok(()) => (true, format!("Deleted preset '{}'", pr.name)),
                        Err(e) => (false, format!("Could not save presets: {e}")),
                    });
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.name).hint_text("Preset name").desired_width(170.0)).on_hover_text("Name for saving the current settings as a preset");
            let ok = !self.name.trim().is_empty();
            if ui.add_enabled(ok, egui::Button::new(format!("{}  Save current", I::FLOPPY_DISK))).on_hover_text("Save the current settings under this name").on_disabled_hover_text("Type a preset name first").clicked() {
                if self.file.get(self.name.trim()).is_some() {
                    self.confirm_replace = true;
                } else {
                    self.save(s);
                }
            }
        });
        if self.confirm_replace {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Replace the existing preset '{}'?", self.name.trim())).color(p.warn));
                if ui.small_button("Replace").clicked() {
                    self.save(s);
                }
                if ui.small_button("Keep it").clicked() {
                    self.confirm_replace = false;
                }
            });
        }
        if let Some((ok, m)) = &self.msg {
            ui.label(RichText::new(m).size(11.5).color(if *ok { p.ok } else { p.danger }));
        }
    }
}

impl Default for PresetUi {
    fn default() -> Self {
        Self::new()
    }
}
