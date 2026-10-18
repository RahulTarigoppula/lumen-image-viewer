//! Settings window: a category list on the left, grouped setting cards on the right.
//! Every change is written straight into `core.settings`; the app re-applies theme,
//! cache budget, color management and the thumbnail disk cache whenever [`show`]
//! returns true, and settings are saved to disk when the window closes.

use super::icons as I;
use super::theme::{semibold, Palette};
use super::widgets::{self, section_title};
use crate::app::Core;
use crate::settings::{ExternalEditor, FitMode, Settings, SortKey, ThemeChoice, WheelAction};
use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
enum Page {
    #[default]
    Appearance,
    Viewer,
    Browser,
    Editing,
    Performance,
    Editors,
    Integration,
    Privacy,
    About,
}

impl Page {
    const ALL: [Page; 9] = [
        Page::Appearance,
        Page::Viewer,
        Page::Browser,
        Page::Editing,
        Page::Performance,
        Page::Editors,
        Page::Integration,
        Page::Privacy,
        Page::About,
    ];
    fn label(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Viewer => "Viewer",
            Page::Browser => "Browser",
            Page::Editing => "Editing & saving",
            Page::Performance => "Performance & cache",
            Page::Editors => "External editors",
            Page::Integration => "Windows integration",
            Page::Privacy => "Privacy & data",
            Page::About => "About",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Page::Appearance => I::PALETTE,
            Page::Viewer => I::IMAGE,
            Page::Browser => I::SQUARES_FOUR,
            Page::Editing => I::PENCIL_SIMPLE,
            Page::Performance => I::GAUGE,
            Page::Editors => I::ARROW_SQUARE_OUT,
            Page::Integration => I::WINDOWS_LOGO,
            Page::Privacy => I::SHIELD_CHECK,
            Page::About => I::INFO,
        }
    }
    fn subtitle(self) -> &'static str {
        match self {
            Page::Appearance => "Theme, colors and interface size",
            Page::Viewer => "How images open, zoom and navigate",
            Page::Browser => "Thumbnail grid and file listing",
            Page::Editing => "Defaults used when saving edited images",
            Page::Performance => "Memory and disk caches",
            Page::Editors => "Programs offered for 'Open in external editor' (E)",
            Page::Integration => "File associations, Explorer menu and single window",
            Page::Privacy => "What Lumen stores, and where",
            Page::About => "Version, license and open-source credits",
        }
    }
}

/// Per-window UI state kept in egui temp memory.
#[derive(Clone, Default)]
struct State {
    page: Page,
    /// UI scale being dragged; applied on release so the slider does not jump under the pointer.
    scale_edit: Option<f32>,
    cache_size: Option<u64>,
    cache_checked_at: f64,
    shell: Option<crate::shell::ShellStatus>,
    confirm_reset: bool,
    confirm_clear_cache: bool,
    total_ram: Option<u64>,
}

const ACCENTS: [(&str, [u8; 3]); 9] = [
    ("Blue", [86, 156, 255]),
    ("Teal", [45, 190, 180]),
    ("Green", [76, 194, 128]),
    ("Amber", [236, 180, 70]),
    ("Orange", [255, 140, 66]),
    ("Red", [240, 98, 98]),
    ("Pink", [236, 98, 170]),
    ("Purple", [160, 120, 255]),
    ("Graphite", [140, 150, 165]),
];

const BACKGROUNDS: [(&str, [u8; 3]); 4] = [("Near black", [14, 14, 16]), ("Dark gray", [40, 41, 45]), ("Mid gray", [128, 128, 128]), ("White", [255, 255, 255])];

/// Show the settings window. Returns true when a setting changed (the app then re-applies
/// theme / scale / accent, cache budget, color management and the thumbnail disk cache).
pub fn show(ctx: &egui::Context, core: &mut Core, open: &mut bool) -> bool {
    let p = core.pal;
    let state_id = egui::Id::unique("settings-window-state");
    let mut st: State = ctx.data_mut(|d| d.get_temp(state_id)).unwrap_or_else(initial_state);
    let mut changed = false;

    egui::Window::new("Settings")
        .id(egui::Id::unique("settings-window"))
        .open(open)
        .default_size([760.0, 560.0])
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .min_size([640.0, 420.0])
        .resizable(true)
        .collapsible(false)
        .scroll(false)
        .frame(egui::Frame::window(&ctx.global_style()).inner_margin(Margin::ZERO).fill(p.bg))
        .show(ctx, |ui| {
            let h = ui.available_height().max(300.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                navigation(ui, &p, &mut st, h);
                let (line, _) = ui.allocate_exact_size(Vec2::new(1.0, h), Sense::hover());
                ui.painter().rect_filled(line, 0.0, p.border);
                ui.vertical(|ui| {
                    ui.set_min_height(h);
                    egui::ScrollArea::vertical().id_salt(("settings-page", st.page)).auto_shrink([false, false]).show(ui, |ui| {
                        egui::Frame::new().inner_margin(Margin { left: 22, right: 22, top: 16, bottom: 22 }).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                            page_header(ui, &p, st.page);
                            changed |= match st.page {
                                Page::Appearance => appearance(ui, &p, &mut core.settings, &mut st),
                                Page::Viewer => viewer(ui, &p, &mut core.settings),
                                Page::Browser => browser(ui, &p, &mut core.settings),
                                Page::Editing => editing(ui, &p, &mut core.settings),
                                Page::Performance => performance(ui, &p, core, &mut st),
                                Page::Editors => editors(ui, &p, core),
                                Page::Integration => integration(ui, &p, core, &mut st),
                                Page::Privacy => privacy(ui, &p, &mut st),
                                Page::About => about(ui, &p),
                            };
                        });
                    });
                });
            });
        });

    if st.confirm_reset {
        match widgets::show_confirm(
            ctx,
            "Reset all settings?",
            "Every preference returns to its default. Your favorites, recent folders, tags and ratings are kept.",
            "Reset",
            true,
            &p,
        ) {
            Some(true) => {
                st.confirm_reset = false;
                st.scale_edit = None;
                reset_settings(&mut core.settings);
                core.settings.save();
                core.toasts.success("Settings reset to defaults");
                changed = true;
            }
            Some(false) => st.confirm_reset = false,
            None => {}
        }
    }
    if st.confirm_clear_cache {
        match widgets::show_confirm(
            ctx,
            "Clear thumbnail cache?",
            "Cached thumbnails are deleted from disk and recreated when folders are browsed again.",
            "Clear cache",
            true,
            &p,
        ) {
            Some(true) => {
                st.confirm_clear_cache = false;
                match core.thumbs.clear_disk_cache() {
                    Ok(freed) => core.toasts.success(format!("Thumbnail cache cleared ({} freed)", fmt_bytes(freed))),
                    Err(e) => core.toasts.error(format!("Could not clear the thumbnail cache: {e}")),
                }
                st.cache_size = None;
            }
            Some(false) => st.confirm_clear_cache = false,
            None => {}
        }
    }

    if !*open {
        // Closing the window: commit a pending scale drag and persist everything now.
        if let Some(s) = st.scale_edit.take() {
            core.settings.ui_scale = s;
            changed = true;
        }
        core.settings.save();
        st.shell = None;
        st.cache_size = None;
    }
    ctx.data_mut(|d| d.insert_temp(state_id, st));
    changed
}

/// Fresh state. With the dev automation hook active (`LUMEN_DEV_DIR`), `LUMEN_SETTINGS_PAGE`
/// (e.g. `Integration`) picks the page shown first, for automated screenshots.
fn initial_state() -> State {
    let mut st = State::default();
    if std::env::var_os("LUMEN_DEV_DIR").is_some() {
        if let Ok(name) = std::env::var("LUMEN_SETTINGS_PAGE") {
            if let Some(page) = Page::ALL.into_iter().find(|p| format!("{p:?}").eq_ignore_ascii_case(name.trim())) {
                st.page = page;
            }
        }
    }
    st
}

// ====================================================================== chrome

fn navigation(ui: &mut Ui, p: &Palette, st: &mut State, h: f32) {
    egui::Frame::new().fill(p.panel).inner_margin(Margin::symmetric(8, 12)).show(ui, |ui| {
        ui.set_width(188.0);
        ui.set_min_height(h - 24.0);
        ui.vertical(|ui| {
            ui.set_width(188.0);
            ui.set_min_height(h - 24.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            for page in Page::ALL {
                if page == Page::Privacy {
                    ui.add_space(6.0);
                }
                if nav_item(ui, p, page.icon(), page.label(), st.page == page).clicked() {
                    st.page = page;
                }
            }
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.add_space(4.0);
                ui.label(RichText::new("Changes apply instantly").size(11.0).color(p.faint));
            });
        });
    });
}

fn nav_item(ui: &mut Ui, p: &Palette, icon: &str, label: &str, selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if selected {
            p.accent_soft
        } else if resp.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(6), bg);
        if selected {
            let bar = egui::Rect::from_center_size(egui::pos2(rect.left() + 2.0, rect.center().y), Vec2::new(3.0, 16.0));
            painter.rect_filled(bar, CornerRadius::same(2), p.accent);
        }
        let icon_color = if selected { p.accent } else { p.muted };
        painter.text(egui::pos2(rect.left() + 20.0, rect.center().y), Align2::CENTER_CENTER, icon, FontId::proportional(16.0), icon_color);
        let font = if selected { semibold(13.5) } else { FontId::proportional(13.5) };
        painter.text(egui::pos2(rect.left() + 38.0, rect.center().y), Align2::LEFT_CENTER, label, font, p.text);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn page_header(ui: &mut Ui, p: &Palette, page: Page) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(page.icon()).size(22.0).color(p.accent));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(page.label()).font(semibold(19.0)).color(p.text));
            ui.label(RichText::new(page.subtitle()).size(12.0).color(p.muted));
        });
    });
    ui.add_space(8.0);
}

/// A rounded card grouping related rows.
fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    let r = egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(14, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Controls on a card need a fill that differs from the card itself.
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.bg_fill = p.hover;
            w.inactive.weak_bg_fill = p.hover;
            w.hovered.bg_fill = lerp_color(p.hover, p.text, 0.08);
            w.hovered.weak_bg_fill = w.hovered.bg_fill;
            add(ui)
        })
        .inner;
    ui.add_space(10.0);
    r
}

/// One setting: title + description on the left, control right-aligned.
/// The control closure runs in a right-to-left layout (add composite controls in reverse).
fn row(ui: &mut Ui, p: &Palette, title: &str, desc: &str, control: impl FnOnce(&mut Ui) -> bool) -> bool {
    if ui.cursor().top() > ui.min_rect().top() + 0.5 {
        let y = ui.cursor().top();
        let r = ui.max_rect();
        ui.painter().hline(r.left()..=r.right(), y, Stroke::new(1.0, p.border.gamma_multiply(0.7)));
    }
    let w = ui.available_width();
    let text_w = (w - 250.0).max(w * 0.4).max(120.0);
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(text_w);
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).color(p.text));
            if !desc.is_empty() {
                ui.add(egui::Label::new(RichText::new(desc).size(11.5).color(p.muted)).wrap());
            }
            ui.add_space(8.0);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            changed = control(ui);
        });
    });
    changed
}

/// iOS/Windows-style switch.
fn toggle(ui: &mut Ui, p: &Palette, on: &mut bool) -> Response {
    let size = Vec2::new(38.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let value = *on;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, value, ""));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let off_bg = if p.dark { Color32::from_rgb(60, 64, 72) } else { Color32::from_rgb(196, 200, 208) };
        let bg = lerp_color(off_bg, p.accent, t);
        let radius = rect.height() / 2.0;
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(radius as u8), bg);
        if resp.hovered() {
            painter.rect_stroke(rect, CornerRadius::same(radius as u8), Stroke::new(1.0, p.text.gamma_multiply(0.35)), StrokeKind::Outside);
        }
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        painter.circle_filled(egui::pos2(x, rect.center().y), radius - 3.5, Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Fixed-width left-to-right box for sliders inside a right-to-left row.
fn slider_box(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> bool) -> bool {
    ui.allocate_ui_with_layout(Vec2::new(240.0, 24.0), Layout::left_to_right(Align::Center), |ui| {
        ui.spacing_mut().slider_width = 160.0;
        add(ui)
    })
    .inner
}

fn slider_box_resp(ui: &mut Ui, slider: egui::Slider<'_>) -> Response {
    ui.allocate_ui_with_layout(Vec2::new(240.0, 24.0), Layout::left_to_right(Align::Center), |ui| {
        ui.spacing_mut().slider_width = 160.0;
        ui.add(slider)
    })
    .inner
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn toggle_row(ui: &mut Ui, p: &Palette, title: &str, desc: &str, value: &mut bool) -> bool {
    row(ui, p, title, desc, |ui| toggle(ui, p, value).changed())
}

/// Segmented choice (call inside a right-to-left row: options are added in reverse).
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.spacing_mut().item_spacing.x = 2.0;
    for (v, label) in options.iter().rev() {
        if ui.selectable_label(*value == *v, *label).clicked() && *value != *v {
            *value = *v;
            changed = true;
        }
    }
    changed
}

fn swatch(ui: &mut Ui, p: &Palette, color: [u8; 3], selected: bool, round: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let c = Color32::from_rgb(color[0], color[1], color[2]);
        let r = if round { CornerRadius::same(10) } else { CornerRadius::same(5) };
        let inner = rect.shrink(if selected { 3.0 } else { 1.0 });
        ui.painter().rect_filled(inner, r, c);
        ui.painter().rect_stroke(inner, r, Stroke::new(1.0, p.border), StrokeKind::Outside);
        if selected {
            ui.painter().rect_stroke(rect, r, Stroke::new(2.0, p.text), StrokeKind::Inside);
        } else if resp.hovered() {
            ui.painter().rect_stroke(rect, r, Stroke::new(1.0, p.muted), StrokeKind::Inside);
        }
    }
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn hint(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(11.5).color(p.muted)).wrap());
}

fn status_chip(ui: &mut Ui, p: &Palette, ok: bool, on: &str, off: &str) {
    let (icon, color, text) = if ok { (I::CHECK_CIRCLE, p.ok, on) } else { (I::MINUS_CIRCLE, p.faint, off) };
    ui.label(RichText::new(format!("{icon} {text}")).size(12.0).color(color));
}

// ====================================================================== pages

fn appearance(ui: &mut Ui, p: &Palette, s: &mut Settings, st: &mut State) -> bool {
    let mut changed = false;
    section_title(ui, p, "Theme");
    card(ui, p, |ui| {
        changed |= row(ui, p, "App theme", "System follows the Windows light/dark setting.", |ui| {
            segmented(ui, &mut s.theme, &[(ThemeChoice::Dark, "Dark"), (ThemeChoice::Light, "Light"), (ThemeChoice::System, "System")])
        });
        changed |= row(ui, p, "Accent color", "Used for selections, highlights and focus.", |ui| {
            let mut c = false;
            ui.spacing_mut().item_spacing.x = 3.0;
            c |= ui.color_edit_button_srgb(&mut s.accent).on_hover_text("Custom color").changed();
            ui.add_space(4.0);
            for (name, rgb) in ACCENTS.iter().rev() {
                if swatch(ui, p, *rgb, s.accent == *rgb, true, name).clicked() {
                    s.accent = *rgb;
                    c = true;
                }
            }
            c
        });
        changed |= row(ui, p, "Interface size", "Scales all text and controls.", |ui| {
            let mut c = false;
            let mut v = st.scale_edit.unwrap_or(s.ui_scale);
            let resp = slider_box_resp(ui, egui::Slider::new(&mut v, 0.8..=2.0).step_by(0.05).custom_formatter(|x, _| format!("{:.0}%", x * 100.0)).custom_parser(|t| t.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|x| x / 100.0)));
            if resp.changed() {
                st.scale_edit = Some(v);
            }
            if st.scale_edit.is_some() && !resp.dragged() {
                s.ui_scale = v.clamp(0.8, 2.0);
                st.scale_edit = None;
                c = true;
            }
            if (s.ui_scale - 1.0).abs() > 0.001 && ui.small_button("Reset").clicked() {
                s.ui_scale = 1.0;
                st.scale_edit = None;
                c = true;
            }
            c
        });
    });

    section_title(ui, p, "Viewer & thumbnails");
    card(ui, p, |ui| {
        changed |= row(ui, p, "Viewer background", "Color behind images in the viewer.", |ui| {
            let mut c = false;
            ui.spacing_mut().item_spacing.x = 3.0;
            c |= ui.color_edit_button_srgb(&mut s.viewer_background).on_hover_text("Custom color").changed();
            ui.add_space(4.0);
            for (name, rgb) in BACKGROUNDS.iter().rev() {
                if swatch(ui, p, *rgb, s.viewer_background == *rgb, false, name).clicked() {
                    s.viewer_background = *rgb;
                    c = true;
                }
            }
            c
        });
        changed |= toggle_row(ui, p, "Checkerboard behind transparency", "Shows a checker pattern where images are transparent.", &mut s.checkerboard_alpha);
        changed |= toggle_row(ui, p, "Thumbnail captions", "File names under thumbnails in the browser grid.", &mut s.show_thumb_captions);
    });
    changed
}

fn viewer(ui: &mut Ui, p: &Palette, s: &mut Settings) -> bool {
    let mut changed = false;
    section_title(ui, p, "Opening & navigation");
    card(ui, p, |ui| {
        changed |= row(ui, p, "Mouse wheel", "Hold Ctrl for the other action.", |ui| segmented(ui, &mut s.wheel_action, &[(WheelAction::Navigate, "Next / previous"), (WheelAction::Zoom, "Zoom")]));
        changed |= row(ui, p, "Default fit", "How images are sized when opened.", |ui| {
            let mut c = false;
            egui::ComboBox::from_id_salt("settings-fit").width(190.0).selected_text(s.fit_mode.label()).show_ui(ui, |ui| {
                for m in FitMode::ALL {
                    c |= ui.selectable_value(&mut s.fit_mode, m, m.label()).changed();
                }
            });
            c
        });
        changed |= toggle_row(ui, p, "Open viewer in full screen", "Images opened from the browser or Explorer fill the screen.", &mut s.open_viewer_fullscreen);
        changed |= toggle_row(ui, p, "Loop at folder end", "Going past the last image wraps to the first.", &mut s.loop_navigation);
        changed |= toggle_row(ui, p, "Preload neighbours", "Decode the next and previous images in the background for instant switching.", &mut s.preload_neighbors);
        changed |= toggle_row(ui, p, "Show info overlay", "File name, size and EXIF summary over the image.", &mut s.show_info_overlay);
    });

    section_title(ui, p, "Zoom");
    card(ui, p, |ui| {
        changed |= toggle_row(ui, p, "Smooth zoom-in filtering", "Off shows crisp pixels when enlarging (good for pixel art).", &mut s.smooth_zoom_in);
        changed |= toggle_row(ui, p, "Animated zoom", "Ease between zoom levels.", &mut s.animate_zoom);
        changed |= row(ui, p, "Magnifier zoom", "Magnification of the loupe.", |ui| {
            slider_box(ui, |ui| ui.add(egui::Slider::new(&mut s.magnifier_zoom, 1.5..=8.0).step_by(0.5).custom_formatter(|x, _| format!("{x:.1}×"))).changed())
        });
        changed |= row(ui, p, "Magnifier size", "Diameter of the loupe.", |ui| {
            slider_box(ui, |ui| ui.add(egui::Slider::new(&mut s.magnifier_size, 120.0..=600.0).step_by(10.0).suffix(" px")).changed())
        });
    });
    changed
}

fn browser(ui: &mut Ui, p: &Palette, s: &mut Settings) -> bool {
    let mut changed = false;
    section_title(ui, p, "Grid");
    card(ui, p, |ui| {
        changed |= row(ui, p, "Thumbnail size", "Also adjustable with Ctrl + mouse wheel in the browser.", |ui| {
            slider_box(ui, |ui| ui.add(egui::Slider::new(&mut s.thumb_size, 80.0..=384.0).step_by(4.0).suffix(" px")).changed())
        });
        changed |= toggle_row(ui, p, "Show folders in grid", "Sub-folders appear as tiles before the images.", &mut s.show_folders_in_grid);
        changed |= toggle_row(ui, p, "Show hidden files", "Include files and folders marked hidden. Applies when a folder is next opened.", &mut s.show_hidden_files);
    });
    section_title(ui, p, "Sorting");
    card(ui, p, |ui| {
        changed |= row(ui, p, "Default sort", "", |ui| {
            let mut c = false;
            let icon = if s.sort_desc { I::SORT_DESCENDING } else { I::SORT_ASCENDING };
            if ui.selectable_label(s.sort_desc, icon).on_hover_text(if s.sort_desc { "Descending" } else { "Ascending" }).clicked() {
                s.sort_desc = !s.sort_desc;
                c = true;
            }
            egui::ComboBox::from_id_salt("settings-sort").width(170.0).selected_text(s.sort_key.label()).show_ui(ui, |ui| {
                for k in SortKey::ALL {
                    c |= ui.selectable_value(&mut s.sort_key, k, k.label()).changed();
                }
            });
            c
        });
        changed |= toggle_row(ui, p, "Folders first", "Keep folders above files regardless of sort order.", &mut s.folders_first);
    });
    section_title(ui, p, "Safety");
    card(ui, p, |ui| {
        changed |= toggle_row(ui, p, "Confirm before delete", "Ask before moving files to the Recycle Bin.", &mut s.confirm_delete);
    });
    changed
}

fn editing(ui: &mut Ui, p: &Palette, s: &mut Settings) -> bool {
    let mut changed = false;
    section_title(ui, p, "Saving");
    card(ui, p, |ui| {
        changed |= row(ui, p, "JPEG quality", "Default for Save and Save as. 85-95 is visually lossless for most photos.", |ui| {
            slider_box(ui, |ui| ui.add(egui::Slider::new(&mut s.jpeg_quality, 1..=100)).changed())
        });
        changed |= toggle_row(ui, p, "Keep EXIF metadata", "Copy camera data, date taken and GPS into saved files.", &mut s.keep_exif_on_save);
        changed |= toggle_row(ui, p, "Lossless JPEG rotation", "Rotate and flip JPEGs without re-encoding when possible.", &mut s.lossless_jpeg_rotate);
    });
    section_title(ui, p, "Color");
    card(ui, p, |ui| {
        changed |= toggle_row(ui, p, "Color management", "Convert images with embedded ICC profiles (Adobe RGB, Display P3, ...) to sRGB for accurate colors.", &mut s.color_manage);
    });
    changed
}

fn performance(ui: &mut Ui, p: &Palette, core: &mut Core, st: &mut State) -> bool {
    let mut changed = false;
    let now = ui.input(|i| i.time);
    if st.cache_size.is_none() || now - st.cache_checked_at > 5.0 {
        st.cache_size = Some(core.thumbs.disk_cache_size());
        st.cache_checked_at = now;
    }
    if st.total_ram.is_none() {
        st.total_ram = Some(total_ram());
    }
    let ram = st.total_ram.unwrap_or(0);
    let s = &mut core.settings;

    section_title(ui, p, "Memory");
    card(ui, p, |ui| {
        let desc = if ram > 0 {
            format!("Decoded images kept in RAM for instant back-and-forth. This PC has {} of memory.", fmt_bytes(ram))
        } else {
            "Decoded images kept in RAM for instant back-and-forth.".to_string()
        };
        changed |= row(ui, p, "Image cache budget", &desc, |ui| {
            slider_box(ui, |ui| ui.add(egui::Slider::new(&mut s.cache_budget_mb, 256..=8192).logarithmic(true).step_by(64.0).suffix(" MB")).changed())
        });
        if ram > 0 && (s.cache_budget_mb << 20) > ram / 2 {
            ui.label(RichText::new(format!("{}  More than half of this PC's memory - other programs may slow down.", I::WARNING)).size(11.5).color(p.warn));
            ui.add_space(6.0);
        }
    });

    section_title(ui, p, "Thumbnail cache");
    card(ui, p, |ui| {
        changed |= toggle_row(ui, p, "Cache thumbnails on disk", "Folders you have visited before open instantly.", &mut s.thumb_disk_cache);
        let size = st.cache_size.unwrap_or(0);
        row(ui, p, "Disk cache", &format!("{} used", fmt_bytes(size)), |ui| {
            if ui.add_enabled(size > 0, egui::Button::new(format!("{}  Clear", I::BROOM))).clicked() {
                st.confirm_clear_cache = true;
            }
            if ui.button(format!("{}  Open folder", I::FOLDER_OPEN)).clicked() {
                open_folder(&crate::settings::cache_dir().join("thumbs"));
            }
            false
        });
    });
    changed
}

fn editors(ui: &mut Ui, p: &Palette, core: &mut Core) -> bool {
    let mut changed = false;
    section_title(ui, p, "Editors");
    let eds = &mut core.settings.external_editors;
    card(ui, p, |ui| {
        if eds.is_empty() {
            ui.add_space(10.0);
            ui.label(RichText::new("No external editors yet").color(p.text));
            hint(ui, p, "Add Photoshop, GIMP, Paint.NET, Krita or any other program. Without an editor, E shows the Windows 'Open with' dialog.");
            ui.add_space(10.0);
            return;
        }
        let n = eds.len();
        let mut action: Option<(usize, i32)> = None; // (index, -1 up / +1 down / 0 remove)
        for i in 0..n {
            if i > 0 {
                let y = ui.cursor().top();
                let r = ui.max_rect();
                ui.painter().hline(r.left()..=r.right(), y, Stroke::new(1.0, p.border.gamma_multiply(0.7)));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let badge = if i == 0 { RichText::new(I::STAR).color(p.accent) } else { RichText::new(format!("{}", i + 1)).color(p.faint) };
                ui.add_sized([18.0, 20.0], egui::Label::new(badge)).on_hover_text(if i == 0 { "Default - opened with E" } else { "" });
                let ed = &mut eds[i];
                let w = (ui.available_width() - 110.0).max(120.0);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    changed |= ui.add(egui::TextEdit::singleline(&mut ed.name).hint_text("Name").desired_width(w.min(260.0))).changed();
                    let exists = ed.exe.is_file();
                    let path = ed.exe.display().to_string();
                    let text = if exists { RichText::new(&path).size(11.0).color(p.muted) } else { RichText::new(format!("{}  {path} (not found)", I::WARNING)).size(11.0).color(p.warn) };
                    ui.add(egui::Label::new(text).truncate()).on_hover_text(&path);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    if widgets::icon_button_sized(ui, I::TRASH, "Remove", false, 26.0, 15.0).clicked() {
                        action = Some((i, 0));
                    }
                    ui.add_enabled_ui(i + 1 < n, |ui| {
                        if widgets::icon_button_sized(ui, I::ARROW_DOWN, "Move down", false, 26.0, 15.0).clicked() {
                            action = Some((i, 1));
                        }
                    });
                    ui.add_enabled_ui(i > 0, |ui| {
                        if widgets::icon_button_sized(ui, I::ARROW_UP, "Move up (the first editor opens with E)", false, 26.0, 15.0).clicked() {
                            action = Some((i, -1));
                        }
                    });
                });
            });
            ui.add_space(6.0);
        }
        if let Some((i, op)) = action {
            match op {
                0 => {
                    eds.remove(i);
                }
                -1 if i > 0 => eds.swap(i, i - 1),
                1 if i + 1 < eds.len() => eds.swap(i, i + 1),
                _ => {}
            }
            changed = true;
        }
    });
    ui.horizontal(|ui| {
        if ui.button(format!("{}  Add editor...", I::PLUS)).clicked() {
            if let Some(exe) = rfd::FileDialog::new().set_title("Choose an editor program").add_filter("Programs", &["exe"]).pick_file() {
                if core.settings.external_editors.iter().any(|e| same_path(&e.exe, &exe)) {
                    core.toasts.info("That editor is already in the list");
                } else {
                    core.settings.external_editors.push(ExternalEditor { name: editor_name_from_exe(&exe), exe });
                    changed = true;
                }
            }
        }
        if ui.button(format!("{}  Find installed", I::MAGNIFYING_GLASS)).on_hover_text("Look for common image editors in Program Files").clicked() {
            let mut added = 0;
            for ed in find_installed_editors() {
                if !core.settings.external_editors.iter().any(|e| same_path(&e.exe, &ed.exe)) {
                    core.settings.external_editors.push(ed);
                    added += 1;
                }
            }
            if added > 0 {
                core.toasts.success(format!("Added {added} editor(s)"));
                changed = true;
            } else {
                core.toasts.info("No new editors found");
            }
        }
    });
    ui.add_space(4.0);
    hint(ui, p, "The first editor in the list opens with E in the browser and viewer. The file is passed as the first argument.");
    changed
}

fn integration(ui: &mut Ui, p: &Palette, core: &mut Core, st: &mut State) -> bool {
    let mut changed = false;
    if st.shell.is_none() {
        st.shell = Some(crate::shell::status());
    }
    let status = st.shell.clone().unwrap_or_default();
    let mut refresh = false;

    section_title(ui, p, "File types");
    card(ui, p, |ui| {
        row(ui, p, "Open with Lumen", "Adds Lumen to 'Open with' for all supported image and video types and lists it under Windows Default apps. Per-user - no administrator rights needed.", |ui| {
            if status.file_types {
                if ui.button("Unregister").clicked() {
                    match crate::shell::unregister_file_types() {
                        Ok(()) => core.toasts.success("File associations removed"),
                        Err(e) => core.toasts.error(format!("Could not remove file associations: {e}")),
                    }
                    refresh = true;
                }
            }
            let label = if status.file_types { if status.stale() { "Update" } else { "Re-register" } } else { "Register" };
            if ui.add(egui::Button::new(RichText::new(label).color(if status.file_types { p.text } else { Color32::WHITE })).fill(if status.file_types { p.raised } else { p.accent })).clicked() {
                match crate::shell::register_file_types() {
                    Ok(n) => core.toasts.success(format!("Lumen registered for {n} file types")),
                    Err(e) => core.toasts.error(format!("Could not register file types: {e}")),
                }
                refresh = true;
            }
            ui.add_space(6.0);
            status_chip(ui, p, status.file_types, "Registered", "Not registered");
            false
        });
        if status.stale() {
            if let Some(r) = &status.registered_exe {
                ui.label(RichText::new(format!("{}  Registered for another copy: {}. Click Update to point Explorer at this one.", I::WARNING, r.display())).size(11.5).color(p.warn));
                ui.add_space(6.0);
            }
        }
        row(ui, p, "Make Lumen the default viewer", "Windows only lets you choose defaults yourself. Register first, then pick Lumen for the types you want in Settings › Default apps.", |ui| {
            if ui.add_enabled(status.file_types, egui::Button::new(format!("{}  Open Default apps", I::ARROW_SQUARE_OUT))).clicked() {
                if let Err(e) = crate::shell::open_default_apps() {
                    core.toasts.error(e.to_string());
                }
            }
            false
        });
    });

    section_title(ui, p, "Explorer");
    card(ui, p, |ui| {
        row(ui, p, "'Browse with Lumen' menu", "Right-click any folder, drive or folder background in Explorer to open it in Lumen's browser.", |ui| {
            if status.context_menu {
                if ui.button("Remove").clicked() {
                    match crate::shell::unregister_context_menu() {
                        Ok(()) => core.toasts.success("Explorer menu entry removed"),
                        Err(e) => core.toasts.error(format!("Could not remove the Explorer menu entry: {e}")),
                    }
                    refresh = true;
                }
            } else if ui.add(egui::Button::new(RichText::new("Add").color(Color32::WHITE)).fill(p.accent)).clicked() {
                match crate::shell::register_context_menu() {
                    Ok(()) => core.toasts.success("'Browse with Lumen' added to Explorer"),
                    Err(e) => core.toasts.error(format!("Could not add the Explorer menu entry: {e}")),
                }
                refresh = true;
            }
            ui.add_space(6.0);
            status_chip(ui, p, status.context_menu, "Added", "Not added");
            false
        });
        hint(ui, p, "On Windows 11 the entry appears under 'Show more options' (Shift+F10).");
        ui.add_space(8.0);
    });

    section_title(ui, p, "Windows");
    card(ui, p, |ui| {
        changed |= toggle_row(
            ui,
            p,
            "Reuse running window",
            "Files opened from Explorer while Lumen is running open in the existing window instead of a new one.",
            &mut core.settings.reuse_window,
        );
    });

    if status.file_types || status.context_menu {
        ui.horizontal(|ui| {
            if ui.button(RichText::new(format!("{}  Remove all integration", I::TRASH)).color(p.danger)).on_hover_text("Undo everything Lumen added to the registry").clicked() {
                match crate::shell::unregister_all() {
                    Ok(()) => core.toasts.success("All Windows integration removed"),
                    Err(e) => core.toasts.error(format!("Could not remove everything: {e}")),
                }
                refresh = true;
            }
        });
    }
    if refresh {
        st.shell = Some(crate::shell::status());
    }
    changed
}

fn privacy(ui: &mut Ui, p: &Palette, st: &mut State) -> bool {
    section_title(ui, p, "Offline by design");
    card(ui, p, |ui| {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(I::SHIELD_CHECK).size(28.0).color(p.ok));
            ui.vertical(|ui| {
                ui.label(RichText::new("Lumen never connects to the internet").font(semibold(14.0)).color(p.text));
                hint(ui, p, "No telemetry, no analytics, no accounts, no update checks and no cloud. Your pictures, tags and ratings never leave this PC.");
            });
        });
        ui.add_space(10.0);
    });

    section_title(ui, p, "Where data is stored");
    let data = crate::settings::data_dir();
    let cache = crate::settings::cache_dir();
    card(ui, p, |ui| {
        path_row(ui, p, "Settings, tags & ratings", &data);
        path_row(ui, p, "Caches (thumbnails)", &cache);
        let portable = is_portable();
        row(
            ui,
            p,
            "Portable mode",
            if portable {
                "Active - all data lives in the LumenData folder next to lumen.exe."
            } else {
                "Create an empty file named portable.txt next to lumen.exe to keep all data beside the program (e.g. on a USB stick)."
            },
            |ui| {
                status_chip(ui, p, portable, "On", "Off");
                false
            },
        );
    });

    section_title(ui, p, "Reset");
    card(ui, p, |ui| {
        row(ui, p, "Reset all settings", "Restore every preference to its default. Favorites, recent folders, tags and ratings are kept.", |ui| {
            if ui.button(RichText::new(format!("{}  Reset...", I::ARROW_COUNTER_CLOCKWISE)).color(p.danger)).clicked() {
                st.confirm_reset = true;
            }
            false
        });
    });
    false
}

fn path_row(ui: &mut Ui, p: &Palette, title: &str, path: &Path) {
    row(ui, p, title, &path.display().to_string(), |ui| {
        if ui.button(format!("{}  Open folder", I::FOLDER_OPEN)).clicked() {
            open_folder(path);
        }
        false
    });
}

fn about(ui: &mut Ui, p: &Palette) -> bool {
    card(ui, p, |ui| {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(egui::include_image!("../../assets/lumen-256.png")).fit_to_exact_size(Vec2::splat(56.0)));
            ui.vertical(|ui| {
                ui.label(RichText::new("Lumen").font(semibold(22.0)).color(p.text));
                ui.label(RichText::new(format!("Version {}  ·  MIT License", env!("CARGO_PKG_VERSION"))).color(p.muted));
                ui.hyperlink_to(RichText::new("github.com/RahulTarigoppula/lumen-image-viewer").size(12.0), env!("CARGO_PKG_REPOSITORY"));
            });
        });
        ui.add_space(6.0);
        hint(ui, p, "A fast, private image & video viewer, browser, editor and converter for Windows.");
        ui.add_space(10.0);
    });

    section_title(ui, p, "Open-source credits");
    card(ui, p, |ui| {
        ui.add_space(6.0);
        egui::Grid::new("credits").num_columns(3).spacing([16.0, 5.0]).striped(false).show(ui, |ui| {
            for (name, what, license) in CREDITS {
                ui.label(RichText::new(*name).color(p.text));
                ui.label(RichText::new(*what).size(12.0).color(p.muted));
                ui.label(RichText::new(*license).size(12.0).color(p.faint));
                ui.end_row();
            }
        });
        ui.add_space(6.0);
    });
    hint(ui, p, "Thank you to everyone who builds and maintains these projects. FFmpeg is used as separately linked libraries under the LGPL; you may replace them with your own build.");
    false
}

const CREDITS: &[(&str, &str, &str)] = &[
    ("egui / eframe", "User interface", "MIT / Apache-2.0"),
    ("image-rs", "Image decoding & encoding", "MIT / Apache-2.0"),
    ("fast_image_resize", "High-quality resampling", "MIT / Apache-2.0"),
    ("libwebp (webp)", "WebP encoding", "BSD-3-Clause"),
    ("jpeg-encoder", "JPEG encoding", "MIT / Apache-2.0"),
    ("img-parts", "Metadata preservation", "MIT / Apache-2.0"),
    ("kamadak-exif", "EXIF reading", "BSD-2-Clause"),
    ("qcms", "ICC color management", "MIT"),
    ("FFmpeg", "Video decoding", "LGPL-2.1+"),
    ("dav1d", "AV1 decoding", "BSD-2-Clause"),
    ("Phosphor Icons", "Icons", "MIT"),
    ("windows-rs", "Windows APIs", "MIT / Apache-2.0"),
    ("rayon, crossbeam", "Parallelism", "MIT / Apache-2.0"),
    ("serde, serde_json", "Settings storage", "MIT / Apache-2.0"),
    ("rfd, trash, arboard", "Dialogs, Recycle Bin, clipboard", "MIT / Apache-2.0"),
    ("lru, parking_lot, chrono", "Utilities", "MIT / Apache-2.0"),
];

// ====================================================================== helpers

/// Reset preferences while keeping user data (favorites, history, window state).
fn reset_settings(s: &mut Settings) {
    let mut fresh = Settings::default();
    fresh.favorites = std::mem::take(&mut s.favorites);
    fresh.recent_folders = std::mem::take(&mut s.recent_folders);
    fresh.last_folder = s.last_folder.take();
    fresh.copy_move_targets = std::mem::take(&mut s.copy_move_targets);
    fresh.window_maximized = s.window_maximized;
    fresh.first_run_done = s.first_run_done;
    *s = fresh;
}

fn open_folder(path: &Path) {
    let _ = std::fs::create_dir_all(path);
    crate::fileops::open_with_default(path);
}

fn is_portable() -> bool {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("portable.txt").exists())).unwrap_or(false)
}

fn total_ram() -> u64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    match unsafe { GlobalMemoryStatusEx(&mut m) } {
        Ok(()) => m.ullTotalPhys,
        Err(_) => 0,
    }
}

/// "1.5 GB", "320 KB", "0 bytes".
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

/// "C:\...\paintdotnet.exe" → "Paintdotnet"; known programs get their proper name.
fn editor_name_from_exe(exe: &Path) -> String {
    let stem = exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let known = [
        ("photoshop", "Photoshop"),
        ("paintdotnet", "Paint.NET"),
        ("gimp", "GIMP"),
        ("krita", "Krita"),
        ("mspaint", "Paint"),
        ("inkscape", "Inkscape"),
        ("i_view", "IrfanView"),
        ("darktable", "darktable"),
        ("lightroom", "Lightroom Classic"),
        ("photo", "Affinity Photo"),
    ];
    let lower = stem.to_ascii_lowercase();
    if let Some((_, n)) = known.iter().find(|(k, _)| lower.starts_with(k)) {
        return n.to_string();
    }
    let mut c = stem.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => "Editor".into(),
    }
}

/// Look for well-known image editors in the usual install locations.
fn find_installed_editors() -> Vec<ExternalEditor> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(v) = std::env::var_os(var) {
            let pb = PathBuf::from(v);
            if !roots.contains(&pb) {
                roots.push(pb);
            }
        }
    }
    let mut out: Vec<ExternalEditor> = Vec::new();
    let mut push = |name: &str, exe: PathBuf| {
        if exe.is_file() && !out.iter().any(|e| same_path(&e.exe, &exe)) {
            out.push(ExternalEditor { name: name.into(), exe });
        }
    };
    for root in &roots {
        push("Paint.NET", root.join(r"paint.net\paintdotnet.exe"));
        push("Krita", root.join(r"Krita (x64)\bin\krita.exe"));
        push("Inkscape", root.join(r"Inkscape\bin\inkscape.exe"));
        push("darktable", root.join(r"darktable\bin\darktable.exe"));
        push("IrfanView", root.join(r"IrfanView\i_view64.exe"));
        push("Affinity Photo 2", root.join(r"Affinity\Photo 2\Photo.exe"));
        push("Lightroom Classic", root.join(r"Adobe\Adobe Lightroom Classic\Lightroom.exe"));
        // Versioned folders: "GIMP 2", "GIMP 3"..., "Adobe Photoshop 2025"...
        for (dir, prefix, bin, exe_prefix, name) in [(root.clone(), "GIMP", "bin", "gimp-", "GIMP"), (root.join("Adobe"), "Adobe Photoshop", "", "Photoshop", "Photoshop")] {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                if !e.file_name().to_string_lossy().starts_with(prefix) {
                    continue;
                }
                let bin_dir = if bin.is_empty() { e.path() } else { e.path().join(bin) };
                let Ok(files) = std::fs::read_dir(&bin_dir) else { continue };
                let mut exes: Vec<PathBuf> = files
                    .flatten()
                    .map(|f| f.path())
                    .filter(|f| {
                        let n = f.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                        n.starts_with(&exe_prefix.to_ascii_lowercase()) && n.ends_with(".exe") && !n.contains("console")
                    })
                    .collect();
                exes.sort();
                if let Some(x) = exes.into_iter().next() {
                    push(name, x);
                }
            }
        }
    }
    if let Some(windir) = std::env::var_os("WINDIR") {
        push("Paint", PathBuf::from(windir).join(r"System32\mspaint.exe"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting() {
        assert_eq!(fmt_bytes(0), "0 bytes");
        assert_eq!(fmt_bytes(1023), "1023 bytes");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(250 << 20), "250 MB");
        assert_eq!(fmt_bytes(16 << 30), "16.0 GB");
    }

    #[test]
    fn editor_names() {
        assert_eq!(editor_name_from_exe(Path::new(r"C:\p\paint.net\paintdotnet.exe")), "Paint.NET");
        assert_eq!(editor_name_from_exe(Path::new(r"C:\p\GIMP 2\bin\gimp-2.10.exe")), "GIMP");
        assert_eq!(editor_name_from_exe(Path::new(r"C:\tools\myeditor.exe")), "Myeditor");
    }

    #[test]
    fn reset_keeps_user_data() {
        let mut s = Settings { ui_scale: 1.7, jpeg_quality: 50, favorites: vec![PathBuf::from(r"C:\fav")], ..Default::default() };
        s.recent_folders.push(PathBuf::from(r"C:\recent"));
        reset_settings(&mut s);
        assert_eq!(s.ui_scale, 1.0);
        assert_eq!(s.jpeg_quality, Settings::default().jpeg_quality);
        assert_eq!(s.favorites, vec![PathBuf::from(r"C:\fav")]);
        assert_eq!(s.recent_folders, vec![PathBuf::from(r"C:\recent")]);
    }

    #[test]
    fn old_settings_files_get_new_defaults() {
        let s: Settings = serde_json::from_str(r#"{"ui_scale": 1.25}"#).unwrap_or_default();
        assert_eq!(s.ui_scale, 1.25);
        assert!(s.reuse_window);
    }
}
