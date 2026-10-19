//! Settings window: a category list on the left, grouped setting cards on the right.
//! Every change is written straight into `core.settings`; the app re-applies theme,
//! cache budget, color management and the thumbnail disk cache whenever [`show`]
//! returns true, and settings are saved to disk when the window closes.

use super::icons as I;
use super::theme::{self, mix, radius, size, text, Palette};
use super::widgets::{self, section_title};
use crate::app::Core;
use crate::settings::{ExternalEditor, FitMode, Settings, SortKey, ThemeChoice, ThumbCaption, WheelAction};
use crate::ui::commands::Command;
use crate::ui::keymap::{self, KeyMode, Keymap, Preset};
use egui::KeyboardShortcut;
use egui::{Align, Align2, Color32, CornerRadius, FontId, Layout, Margin, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
enum Page {
    #[default]
    Appearance,
    Viewer,
    Browser,
    Editing,
    Keyboard,
    Performance,
    Editors,
    Integration,
    #[cfg(feature = "ai")]
    Ai,
    Privacy,
    About,
}

impl Page {
    const ALL: &[Page] = &[
        Page::Appearance,
        Page::Viewer,
        Page::Browser,
        Page::Editing,
        Page::Keyboard,
        Page::Performance,
        Page::Editors,
        Page::Integration,
        #[cfg(feature = "ai")]
        Page::Ai,
        Page::Privacy,
        Page::About,
    ];
    fn label(self) -> &'static str {
        match self {
            Page::Appearance => "Appearance",
            Page::Viewer => "Viewer",
            Page::Browser => "Browser",
            Page::Editing => "Editing & saving",
            Page::Keyboard => "Keyboard",
            Page::Performance => "Performance & cache",
            Page::Editors => "External editors",
            Page::Integration => "Windows integration",
            #[cfg(feature = "ai")]
            Page::Ai => "AI review",
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
            Page::Keyboard => I::KEYBOARD,
            Page::Performance => I::GAUGE,
            Page::Editors => I::ARROW_SQUARE_OUT,
            Page::Integration => I::WINDOWS_LOGO,
            #[cfg(feature = "ai")]
            Page::Ai => I::SPARKLE,
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
            Page::Keyboard => "Click a shortcut to change it, + to add another",
            Page::Performance => "Memory and disk caches",
            Page::Editors => "Programs offered for 'Open in external editor' (E)",
            Page::Integration => "File associations, Explorer menu and single window",
            #[cfg(feature = "ai")]
            Page::Ai => "Get an opinion on your pictures from your own vision model server",
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
    kb_mode: KeyMode,
    kb_search: String,
    capture: Option<Capture>,
    conflict: Option<PendingConflict>,
    confirm_reset_keys: bool,
    confirm_preset: Option<Preset>,
    /// egui pass in which the window last drew; a gap means it was just opened.
    last_pass: u64,
    /// A text field had keyboard focus at the end of the last frame (egui drops
    /// the focus on Esc before the field sees it).
    editing: bool,
    /// Shortcut row under the pointer (or holding keyboard focus) last frame:
    /// only that row shows its remove / add buttons.
    kb_hot: Option<Command>,
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

    // Each opening starts with an empty shortcut search.
    let pass = ctx.cumulative_pass_nr();
    if st.last_pass + 1 < pass {
        st.kb_search.clear();
        st.capture = None;
        st.conflict = None;
    }
    st.last_pass = pass;

    // Esc closes Settings unless it only leaves a text field (or a key is being captured).
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !ctx.text_edit_focused() && !st.editing && st.capture.is_none() {
        *open = false;
    }
    // Size and position come from the app window every frame (nothing remembered),
    // so the window always fits, at any interface size and after maximize / restore,
    // and keeps the same width on every page. 32 pt stay free on every side, so the
    // status bar remains readable.
    let cap = widgets::AppWindow::cap(ctx);
    let size = Vec2::new(cap.x.min(900.0), cap.y);
    widgets::app_window("settings", "Settings")
        .open(open)
        .fixed_size(size)
        .anchored()
        .fill(p.panel)
        .body_margin(Margin::ZERO)
        .show(ctx, |ui| {
            let full = ui.available_rect_before_wrap();
            // Narrow window (large interface size): the category list shows icons only.
            let compact = full.width() < 600.0;
            let nav_w = if compact { 52.0 } else { 200.0 };
            let nav_rect = Rect::from_min_size(full.min, Vec2::new(nav_w, full.height()));
            let page_rect = Rect::from_min_max(egui::pos2(nav_rect.right() + 1.0, full.top()), full.max);
            let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(nav_rect).layout(Layout::top_down(Align::Min)));
            nav_ui.set_clip_rect(nav_rect.intersect(ui.clip_rect()));
            navigation(&mut nav_ui, &p, &mut st, compact);
            // The page sits on the darker background, rounded where it meets the list
            // (and at the window's own corner).
            ui.painter().rect_filled(page_rect, CornerRadius { nw: 8, ne: 0, sw: 0, se: radius::WINDOW - 1 }, p.bg);
            let mut page_ui = ui.new_child(egui::UiBuilder::new().max_rect(page_rect).layout(Layout::top_down(Align::Min)));
            page_ui.set_clip_rect(page_rect.intersect(ui.clip_rect()));
            ui.advance_cursor_after_rect(full);
            let ui = &mut page_ui;
            let side = if page_rect.width() < 420.0 { 12 } else { 24 };
            egui::ScrollArea::vertical().id_salt(("settings-page", st.page)).auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::new().inner_margin(Margin { left: side, right: side, top: 18, bottom: 24 }).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    page_header(ui, &p, st.page);
                    changed |= match st.page {
                        Page::Appearance => appearance(ui, &p, &mut core.settings, &mut st),
                        Page::Viewer => viewer(ui, &p, &mut core.settings),
                        Page::Browser => browser(ui, &p, &mut core.settings),
                        Page::Editing => editing(ui, &p, &mut core.settings),
                        Page::Keyboard => keyboard(ui, &p, core, &mut st),
                        Page::Performance => performance(ui, &p, core, &mut st),
                        Page::Editors => editors(ui, &p, core),
                        Page::Integration => integration(ui, &p, core, &mut st),
                        #[cfg(feature = "ai")]
                        Page::Ai => ai_page(ui, &p, core),
                        Page::Privacy => privacy(ui, &p, &mut st),
                        Page::About => about(ui, &p),
                    };
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

    if st.page != Page::Keyboard || !*open {
        st.capture = None;
        keymap::set_capturing(false);
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
    st.editing = ctx.text_edit_focused();
    ctx.data_mut(|d| d.insert_temp(state_id, st));
    changed
}

/// Show `page` (by name, e.g. "Keyboard") the next time the settings window draws.
pub fn open_page(ctx: &egui::Context, page: &str) {
    let state_id = egui::Id::unique("settings-window-state");
    let mut st: State = ctx.data_mut(|d| d.get_temp(state_id)).unwrap_or_else(initial_state);
    if let Some(pg) = Page::ALL.iter().copied().find(|p| format!("{p:?}").eq_ignore_ascii_case(page.trim())) {
        st.page = pg;
    }
    ctx.data_mut(|d| d.insert_temp(state_id, st));
}

/// Fresh state. With the dev automation hook active (`LUMEN_DEV_DIR`), `LUMEN_SETTINGS_PAGE`
/// (e.g. `Integration`) picks the page shown first, for automated screenshots.
fn initial_state() -> State {
    let mut st = State::default();
    if std::env::var_os("LUMEN_DEV_DIR").is_some() {
        if let Ok(name) = std::env::var("LUMEN_SETTINGS_PAGE") {
            if let Some(page) = Page::ALL.iter().copied().find(|p| format!("{p:?}").eq_ignore_ascii_case(name.trim())) {
                st.page = page;
            }
        }
    }
    st
}

// ====================================================================== chrome

/// The category list fills `ui`'s rect. It scrolls when the window is too short for
/// every entry; `compact` shows icons only (names in tooltips).
fn navigation(ui: &mut Ui, p: &Palette, st: &mut State, compact: bool) {
    let rect = ui.max_rect();
    const ITEM_H: f32 = 32.0;
    const GAP: f32 = 2.0;
    let list_h = Page::ALL.len() as f32 * (ITEM_H + GAP) + 6.0;
    let footer_h = 28.0;
    let show_footer = !compact && rect.height() >= list_h + 24.0 + footer_h;
    let list_rect = Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.max.y - if show_footer { footer_h } else { 0.0 }));
    let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect).layout(Layout::top_down(Align::Min)));
    egui::ScrollArea::vertical().id_salt("settings-nav").auto_shrink([false, false]).show(&mut list_ui, |ui| {
        egui::Frame::new().inner_margin(Margin { left: if compact { 6 } else { 10 }, right: if compact { 6 } else { 10 }, top: 2, bottom: 12 }).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = GAP;
            for &page in Page::ALL {
                if page == Page::Privacy {
                    ui.add_space(6.0);
                }
                let mut resp = nav_item(ui, p, page.icon(), page.label(), st.page == page, compact);
                if compact {
                    resp = resp.on_hover_text(page.label());
                }
                if resp.clicked() {
                    st.page = page;
                }
            }
        });
    });
    if show_footer {
        let pos = egui::pos2(rect.min.x + 20.0, rect.max.y - footer_h / 2.0 - 2.0);
        ui.painter().with_clip_rect(rect).text(pos, Align2::LEFT_CENTER, "Changes apply instantly", text::small(), p.faint);
    }
    ui.advance_cursor_after_rect(rect);
}

fn nav_item(ui: &mut Ui, p: &Palette, icon: &str, label: &str, selected: bool, compact: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let bg = if selected {
            p.selected_bg
        } else if resp.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(radius::CONTROL), bg);
        if resp.has_focus() {
            painter.rect_stroke(rect, CornerRadius::same(radius::CONTROL), Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
        let icon_color = if selected { p.accent_text } else { p.muted };
        if compact {
            painter.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(16.0), icon_color);
        } else {
            painter.text(egui::pos2(rect.left() + 18.0, rect.center().y), Align2::CENTER_CENTER, icon, FontId::proportional(16.0), icon_color);
            let font = if selected { text::body_strong() } else { text::body() };
            let text_clip = Rect::from_min_max(egui::pos2(rect.left() + 36.0, rect.top()), egui::pos2(rect.right() - 4.0, rect.bottom()));
            painter.with_clip_rect(text_clip.intersect(ui.clip_rect())).text(egui::pos2(rect.left() + 36.0, rect.center().y), Align2::LEFT_CENTER, label, font, p.text);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn page_header(ui: &mut Ui, p: &Palette, page: Page) {
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.add(egui::Label::new(RichText::new(page.label()).font(text::display()).color(p.text)).truncate());
    ui.add(egui::Label::new(RichText::new(page.subtitle()).color(p.muted)).wrap());
    ui.spacing_mut().item_spacing.y = 4.0;
    ui.add_space(10.0);
}

/// A rounded card grouping related rows.
fn card<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    let r = egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(Margin::symmetric(16, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Controls on a card need a fill that differs from the card itself.
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.bg_fill = p.hover;
            w.inactive.weak_bg_fill = p.hover;
            w.hovered.bg_fill = mix(p.hover, p.text, 0.08);
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
    // The control's width is measured each frame. When it leaves too little room for
    // the text, the control goes below the text (and wraps) instead of overlapping it.
    let width_id = ui.scope_id().with(("settings-row-control", title));
    let control_w: Option<f32> = ui.data(|d| d.get_temp(width_id));
    let min_text = 160.0f32.min(w * 0.45);
    let side_by_side = match control_w {
        Some(cw) => cw + 16.0 + min_text <= w,
        None => w >= 520.0,
    };
    let text = |ui: &mut Ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.add(egui::Label::new(RichText::new(title).color(p.text)).wrap());
        if !desc.is_empty() {
            ui.add(egui::Label::new(RichText::new(desc).size(text::SMALL).color(p.muted)).wrap());
        }
    };
    let mut changed = false;
    let measured = if side_by_side {
        let text_w = (w - control_w.unwrap_or(250.0) - 16.0).max(min_text);
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(text_w);
                ui.add_space(7.0);
                text(ui);
                ui.add_space(7.0);
            });
            let r = ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                changed = control(ui);
            });
            r.response.rect.width()
        })
        .inner
    } else {
        ui.add_space(8.0);
        text(ui);
        ui.add_space(4.0);
        let r = ui.with_layout(Layout::right_to_left(Align::Min).with_main_wrap(true), |ui| {
            changed = control(ui);
        });
        ui.add_space(8.0);
        r.response.rect.width()
    };
    if control_w.is_none_or(|cw| (cw - measured).abs() > 0.5) {
        ui.data_mut(|d| d.insert_temp(width_id, measured));
        ui.ctx().request_repaint();
    }
    changed
}

/// Windows 11 style switch: off = outlined track with a muted knob, on = accent
/// track with an `on_accent` knob.
fn toggle(ui: &mut Ui, p: &Palette, on: &mut bool) -> Response {
    let size = Vec2::new(40.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let value = *on;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, value, ""));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let r = rect.height() / 2.0;
        let cr = CornerRadius::same(r as u8);
        let painter = ui.painter();
        let hovered = resp.hovered();
        let off_border = if hovered { p.text } else { p.muted };
        let on_fill = if hovered { mix(p.accent_fill, theme::text_on(p.accent_fill), 0.1) } else { p.accent_fill };
        let track = Color32::from_rgba_unmultiplied(on_fill.r(), on_fill.g(), on_fill.b(), (t * 255.0) as u8);
        painter.rect_filled(rect, cr, track);
        if t < 1.0 {
            painter.rect_stroke(rect, cr, Stroke::new(1.0, off_border.gamma_multiply(1.0 - t)), StrokeKind::Inside);
        }
        if resp.has_focus() {
            painter.rect_stroke(rect.expand(3.0), CornerRadius::same(r as u8 + 3), Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
        let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
        let knob = mix(off_border, p.on_accent, t);
        let kr = if resp.is_pointer_button_down_on() { r - 4.0 } else if hovered { r - 4.5 } else { r - 5.0 };
        painter.circle_filled(egui::pos2(x, rect.center().y), kr, knob);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Fixed-width box for a slider (rail + value box) inside a right-to-left row.
/// The box is 260 points wide, or the room left when that is less.
fn slider_box<N: egui::emath::Numeric>(ui: &mut Ui, slider: widgets::Slider<'_, N>) -> Response {
    let w = 260.0f32.min(ui.available_width()).max(150.0);
    ui.allocate_ui_with_layout(Vec2::new(w, 24.0), Layout::left_to_right(Align::Center), |ui| {
        let rail = w - size::VALUE_W - 4.0 - ui.spacing().item_spacing.x;
        ui.add(slider.width(rail))
    })
    .inner
}

/// An `egui::Slider` in the same box (the AI page still uses egui's slider).
#[cfg(feature = "ai")]
fn slider_box_resp(ui: &mut Ui, slider: egui::Slider<'_>) -> Response {
    let w = 260.0f32.min(ui.available_width()).max(150.0);
    ui.allocate_ui_with_layout(Vec2::new(w, 24.0), Layout::left_to_right(Align::Center), |ui| {
        ui.spacing_mut().slider_width = w - 80.0;
        ui.add(slider)
    })
    .inner
}

fn toggle_row(ui: &mut Ui, p: &Palette, title: &str, desc: &str, value: &mut bool) -> bool {
    row(ui, p, title, desc, |ui| toggle(ui, p, value).changed())
}

/// Segmented choice (one control; options in reading order).
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    widgets::segmented_value(ui, value, options)
}

/// A round color swatch: 1 pt ring so dark and light colors both show on the card,
/// and a 2 pt accent ring (2 pt out) when selected.
fn swatch(ui: &mut Ui, p: &Palette, color: [u8; 3], selected: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let c = Color32::from_rgb(color[0], color[1], color[2]);
        let center = rect.center();
        let painter = ui.painter();
        painter.circle_filled(center, 8.0, c);
        painter.circle_stroke(center, 8.0, Stroke::new(1.0, swatch_ring(p)));
        if selected {
            painter.circle_stroke(center, 11.0, Stroke::new(2.0, p.accent));
        } else if resp.hovered() {
            painter.circle_stroke(center, 11.0, Stroke::new(1.0, p.muted));
        }
        if resp.has_focus() && !selected {
            painter.circle_stroke(center, 11.0, Stroke::new(1.5, p.accent));
        }
    }
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn swatch_ring(p: &Palette) -> Color32 {
    if p.dark { Color32::from_white_alpha(70) } else { Color32::from_black_alpha(50) }
}

/// The "custom color" swatch: a circle with an eyedropper that opens a color picker.
/// Shows the color itself (and the selected ring) when `value` is not one of `presets`.
fn custom_swatch(ui: &mut Ui, p: &Palette, value: &mut [u8; 3], presets: &[[u8; 3]]) -> bool {
    let custom = !presets.contains(value);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let c = rect.center();
        let painter = ui.painter();
        let fill = if custom { Color32::from_rgb(value[0], value[1], value[2]) } else { p.faint_bg };
        painter.circle_filled(c, 8.0, fill);
        painter.circle_stroke(c, 8.0, Stroke::new(1.0, swatch_ring(p)));
        let glyph = if custom { theme::text_on(fill) } else if resp.hovered() { p.text } else { p.muted };
        painter.text(c, Align2::CENTER_CENTER, I::EYEDROPPER, FontId::proportional(size::ICON_CARET), glyph);
        if custom {
            painter.circle_stroke(c, 11.0, Stroke::new(2.0, p.accent));
        } else if resp.hovered() || resp.has_focus() {
            painter.circle_stroke(c, 11.0, Stroke::new(1.0, p.muted));
        }
    }
    let resp = resp.on_hover_text("Custom color").on_hover_cursor(egui::CursorIcon::PointingHand);
    let mut changed = false;
    egui::Popup::from_toggle_button_response(&resp).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        let mut c = Color32::from_rgb(value[0], value[1], value[2]);
        if egui::color_picker::color_picker_color32(ui, &mut c, egui::color_picker::Alpha::Opaque) {
            *value = [c.r(), c.g(), c.b()];
            changed = true;
        }
    });
    changed
}

fn hint(ui: &mut Ui, p: &Palette, t: &str) {
    ui.add(egui::Label::new(RichText::new(t).size(text::SMALL).color(p.muted)).wrap());
}

fn status_chip(ui: &mut Ui, p: &Palette, ok: bool, on: &str, off: &str) {
    let (icon, color, text) = if ok { (I::CHECK_CIRCLE, p.ok, on) } else { (I::MINUS_CIRCLE, p.faint, off) };
    ui.label(RichText::new(format!("{icon} {text}")).size(text::SMALL).color(color));
}

/// A shortcut binding shown as keycaps on a clickable, frameless button (`None` =
/// waiting for a key: "Press a key..." on the selected style). `conflict` adds a
/// warning ring.
fn key_button(ui: &mut Ui, p: &Palette, keys: Option<&str>, conflict: bool, size: Vec2) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let cr = CornerRadius::same(radius::CONTROL);
        match keys {
            None => {
                painter.rect(rect, cr, p.selected_bg, Stroke::new(1.0, p.accent), StrokeKind::Inside);
                painter.text(egui::pos2(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, "Press a key...", text::body(), p.accent_text);
            }
            Some(k) => {
                if resp.hovered() || resp.has_focus() {
                    painter.rect(rect, cr, p.hover, Stroke::new(1.0, if resp.has_focus() { p.accent } else { p.border }), StrokeKind::Inside);
                }
                if conflict {
                    painter.rect_stroke(rect, cr, Stroke::new(1.0, p.warn), StrokeKind::Inside);
                }
                let w = widgets::shortcut_keys_width(painter, k);
                widgets::paint_shortcut_keys(painter, p, egui::pos2(rect.left() + 6.0 + w, rect.center().y), k);
            }
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
            ui.spacing_mut().item_spacing.x = 2.0;
            let presets: Vec<[u8; 3]> = ACCENTS.iter().map(|(_, rgb)| *rgb).collect();
            c |= custom_swatch(ui, p, &mut s.accent, &presets);
            ui.add_space(6.0);
            for (name, rgb) in ACCENTS.iter().rev() {
                if swatch(ui, p, *rgb, s.accent == *rgb, name).clicked() {
                    s.accent = *rgb;
                    c = true;
                }
            }
            c
        });
        changed |= row(ui, p, "Interface size", "Scales all text and controls.", |ui| {
            let mut c = false;
            let mut v = st.scale_edit.unwrap_or(s.ui_scale);
            let resp = slider_box(ui, widgets::Slider::new(&mut v, 0.75..=2.0).step_by(0.05).default_value(1.0).custom_formatter(|x, _| format!("{:.0}%", x * 100.0)).custom_parser(|t| t.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|x| x / 100.0)));
            if resp.changed() {
                st.scale_edit = Some(v);
            }
            if st.scale_edit.is_some() && !resp.dragged() {
                s.ui_scale = v.clamp(0.75, 2.0);
                st.scale_edit = None;
                c = true;
            }
            if (s.ui_scale - 1.0).abs() > 0.001 && ui.add(egui::Button::new("Reset").frame_when_inactive(false)).on_hover_text("Back to 100%").clicked() {
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
            ui.spacing_mut().item_spacing.x = 2.0;
            let presets: Vec<[u8; 3]> = BACKGROUNDS.iter().map(|(_, rgb)| *rgb).collect();
            c |= custom_swatch(ui, p, &mut s.viewer_background, &presets);
            ui.add_space(6.0);
            for (name, rgb) in BACKGROUNDS.iter().rev() {
                if swatch(ui, p, *rgb, s.viewer_background == *rgb, name).clicked() {
                    s.viewer_background = *rgb;
                    c = true;
                }
            }
            c
        });
        changed |= toggle_row(ui, p, "Checkerboard behind transparency", "Shows a checker pattern where images are transparent.", &mut s.checkerboard_alpha);
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
            crate::ui::widgets::combo("settings-fit").width(190.0).selected_text(s.fit_mode.label()).show_ui(ui, |ui| {
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
        changed |= toggle_row(ui, p, "Show panels when the mouse touches a screen edge", "Edit tools on the left, file info on the right, thumbnails at the bottom.", &mut s.edge_panels);
    });

    section_title(ui, p, "Zoom");
    card(ui, p, |ui| {
        changed |= toggle_row(ui, p, "Smooth zoom-in filtering", "Off shows crisp pixels when enlarging (good for pixel art).", &mut s.smooth_zoom_in);
        changed |= toggle_row(ui, p, "Animated zoom", "Ease between zoom levels.", &mut s.animate_zoom);
        changed |= row(ui, p, "Magnifier zoom", "Magnification of the loupe.", |ui| {
            slider_box(ui, widgets::Slider::new(&mut s.magnifier_zoom, 1.5..=8.0).step_by(0.5).default_value(Settings::default().magnifier_zoom).custom_formatter(|x, _| format!("{x:.1}×"))).changed()
        });
        changed |= row(ui, p, "Magnifier size", "Diameter of the loupe.", |ui| {
            slider_box(ui, widgets::Slider::new(&mut s.magnifier_size, 120.0..=600.0).step_by(10.0).default_value(Settings::default().magnifier_size).suffix(" px")).changed()
        });
    });
    changed
}

fn browser(ui: &mut Ui, p: &Palette, s: &mut Settings) -> bool {
    let mut changed = false;
    section_title(ui, p, "Grid");
    card(ui, p, |ui| {
        changed |= row(ui, p, "Thumbnail size", "Also adjustable with Ctrl + mouse wheel in the browser.", |ui| {
            slider_box(ui, widgets::Slider::new(&mut s.thumb_size, 80.0..=384.0).step_by(4.0).default_value(Settings::default().thumb_size).suffix(" px")).changed()
        });
        changed |= row(ui, p, "Thumbnail captions", "Text under thumbnails in the grid. Also in the View menu.", |ui| {
            let mut c = false;
            let cur = s.caption();
            crate::ui::widgets::combo("settings-captions").width(210.0).selected_text(cur.label()).show_ui(ui, |ui| {
                for k in ThumbCaption::ALL {
                    if ui.selectable_label(cur == k, k.label()).clicked() && cur != k {
                        s.set_thumb_caption(k);
                        c = true;
                    }
                }
            });
            c
        });
        changed |= toggle_row(ui, p, "Show folders in grid", "Sub-folders appear as tiles before the images.", &mut s.show_folders_in_grid);
        changed |= toggle_row(ui, p, "Show hidden files", "Include files and folders marked hidden. Applies when a folder is next opened.", &mut s.show_hidden_files);
    });
    section_title(ui, p, "Sorting");
    card(ui, p, |ui| {
        changed |= row(ui, p, "Default sort", "", |ui| {
            let mut c = false;
            let icon = if s.sort_desc { I::SORT_DESCENDING } else { I::SORT_ASCENDING };
            if widgets::icon_button_sized(ui, icon, if s.sort_desc { "Descending" } else { "Ascending" }, false, 28.0, 16.0).clicked() {
                s.sort_desc = !s.sort_desc;
                c = true;
            }
            crate::ui::widgets::combo("settings-sort").width(170.0).selected_text(s.sort_key.label()).show_ui(ui, |ui| {
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
            slider_box(ui, widgets::Slider::new(&mut s.jpeg_quality, 1..=100).default_value(Settings::default().jpeg_quality)).changed()
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
            slider_box(ui, widgets::Slider::new(&mut s.cache_budget_mb, 256..=8192).logarithmic(true).step_by(64.0).default_value(Settings::default().cache_budget_mb).suffix(" MB").value_width(80.0)).changed()
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
            let clicked = if status.file_types { ui.button(label).clicked() } else { widgets::primary_button(ui, p, label).clicked() };
            if clicked {
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
        row(ui, p, "Make Lumen the default viewer", "Windows only lets you choose defaults yourself. Register first, then pick Lumen for the types you want in Settings > Default apps.", |ui| {
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
            } else if widgets::primary_button(ui, p, "Add").clicked() {
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
                ui.label(RichText::new("Lumen never connects to the internet").font(text::subtitle()).color(p.text));
                hint(ui, p, "No telemetry, no analytics, no accounts, no update checks and no cloud. Your pictures, tags and ratings never leave this PC.");
                #[cfg(feature = "ai")]
                hint(ui, p, "The optional AI analysis (off by default) talks only to the server you set in Settings > AI review, and only when you press Run.");
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

// ====================================================================== AI

/// Settings > AI: the master switch, the server (IP, port, HTTPS, LAN-only,
/// API key), Test connection, the model, and advanced options. The flow is:
/// type the IP, press Test connection (a model is picked), done.
#[cfg(feature = "ai")]
fn ai_page(ui: &mut Ui, p: &Palette, core: &mut Core) -> bool {
    use lumen_ai::{CoordConvention, ImageEncoding, StepStatus, TestStep};
    let ai = &mut core.ai;
    section_title(ui, p, "General");
    card(ui, p, |ui| {
        if toggle_row(
            ui,
            p,
            "Turn on AI review",
            "Lets the Analysis panel (Ctrl+Q) ask your own vision model server, for example Qwen on vLLM, about the picture you are viewing.",
            &mut ai.settings.enabled,
        ) {
            ai.dirty = true;
        }
        hint(
            ui,
            p,
            "Only when you press Run AI review, Lumen sends the pixels you choose (the picture or the region you draw), downscaled and re-encoded without EXIF, GPS or color profile, and the prompt. File names and capture settings only if you turn them on under Advanced. Requests go to the server below and nowhere else, and you see them before they are sent.",
        );
        ui.add_space(8.0);
    });

    let enabled = ai.settings.enabled;
    section_title(ui, p, "Server");
    card(ui, p, |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            row(ui, p, "Server address", "IP address or host name, for example 192.168.1.20 (host:port or a URL also work)", |ui| {
                let resp = ui.add(egui::TextEdit::singleline(&mut ai.host_edit).hint_text("192.168.1.20").desired_width(230.0));
                if resp.lost_focus() {
                    ai.apply_host();
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    ai.test_connection();
                }
                false
            });
            if let Some(e) = &ai.host_error {
                ui.add(egui::Label::new(RichText::new(format!("{}  {e}", I::WARNING)).size(11.5).color(p.danger)).wrap());
                ui.add_space(4.0);
            }
            let has_key = ai.has_key();
            let key_desc = if has_key {
                "Saved in Windows Credential Manager (or an encrypted file in the data folder), never in settings.json."
            } else if ai.typed_key() {
                "Used for Test connection and Run until Lumen closes. Save keeps it in Windows Credential Manager."
            } else {
                "Only if your server needs one (vllm serve --api-key)."
            };
            row(ui, p, "API key", key_desc, |ui| {
                if has_key {
                    if ui.button(format!("{}  Forget key", I::TRASH)).clicked() {
                        ai.forget_key();
                    }
                    status_chip(ui, p, true, "Key saved", "");
                } else {
                    let save = ui
                        .add_enabled(ai.typed_key(), egui::Button::new("Save"))
                        .on_hover_text("Keep the key in Windows Credential Manager for next time");
                    let resp = ui.add(egui::TextEdit::singleline(&mut ai.key_edit).password(true).hint_text("none").desired_width(170.0));
                    if save.clicked() {
                        ai.save_key();
                    }
                    // Enter tries the key at once; only Save stores it.
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && ai.typed_key() {
                        ai.test_connection();
                    }
                }
                false
            });
            let key_in_use = has_key || ai.typed_key();
            for w in ai.settings.warnings(key_in_use) {
                ui.add(egui::Label::new(RichText::new(format!("{}  {w}", I::WARNING)).size(11.5).color(p.warn)).wrap());
            }
            row(ui, p, "Test connection", "Checks the server, lists its models and picks one that accepts images", |ui| {
                let testing = ai.testing();
                if ui.add_enabled(!testing, egui::Button::new(RichText::new(format!("{}  Test connection", I::PLUGS_CONNECTED)).color(Color32::WHITE)).fill(crate::analysis_ui::look::accent_fill(p))).clicked() {
                    ai.test_connection();
                }
                if testing {
                    ui.spinner();
                }
                false
            });
            if !ai.steps.is_empty() || ai.testing() {
                ui.add_space(2.0);
                for step in TestStep::ALL {
                    let done = ai.steps.iter().find(|s| s.step == step);
                    ui.horizontal(|ui| {
                        match done.map(|s| s.status) {
                            Some(StepStatus::Ok) => ui.label(RichText::new(I::CHECK_CIRCLE).color(p.ok)),
                            Some(StepStatus::Warning) => ui.label(RichText::new(I::WARNING).color(p.warn)),
                            Some(StepStatus::Failed) => ui.label(RichText::new(I::X_CIRCLE).color(p.danger)),
                            Some(StepStatus::Skipped) => ui.label(RichText::new(I::MINUS_CIRCLE).color(p.faint)),
                            None if ai.testing() => ui.spinner(),
                            None => ui.label(RichText::new(I::MINUS_CIRCLE).color(p.faint)),
                        };
                        ui.allocate_ui_with_layout(Vec2::new(62.0, 16.0), Layout::left_to_right(Align::Center), |ui| {
                            ui.set_width(62.0);
                            ui.label(RichText::new(step.label()).size(12.0).color(p.text));
                        });
                        if let Some(s) = done {
                            let ms = if s.elapsed_ms > 0 { format!("  ({} ms)", s.elapsed_ms) } else { String::new() };
                            ui.add(egui::Label::new(RichText::new(format!("{}{ms}", s.message)).size(11.5).color(p.muted)).wrap());
                        }
                    });
                }
                ui.add_space(6.0);
            }
        });
    });

    section_title(ui, p, "Model");
    card(ui, p, |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            let current = ai.settings.model.clone().unwrap_or_default();
            let desc = if current.is_empty() { "Test connection lists the server's models and picks one".to_string() } else { ai.vision_label(&current) };
            row(ui, p, "Model", &desc, |ui| {
                if ui.add_enabled(ai.probing().is_none() && !current.is_empty(), egui::Button::new("Test vision")).on_hover_text("Sends a tiny gray test image to check that the model accepts images").clicked() {
                    ai.probe_vision(&current);
                }
                if ui.button(I::ARROW_CLOCKWISE).on_hover_text("Refresh the model list").clicked() {
                    ai.refresh_models();
                }
                let mut pick = None;
                egui::ComboBox::from_id_salt("settings-ai-model")
                    .selected_text(if current.is_empty() { "None".to_string() } else { current.clone() })
                    .width(230.0)
                    .show_ui(ui, |ui| {
                        if ai.models.is_empty() {
                            ui.label(RichText::new("Press Test connection to list the models").size(12.0).color(p.muted));
                        }
                        for m in &ai.models {
                            let mut text = format!("{}  ({})", m.id, ai.vision_label(&m.id));
                            if let Some(n) = m.max_model_len {
                                text.push_str(&format!(", {n} context"));
                            }
                            if ui.selectable_label(m.id == current, text).clicked() {
                                pick = Some(m.id.clone());
                            }
                        }
                    });
                if let Some(m) = pick {
                    ai.set_model(&m);
                }
                false
            });
            row(ui, p, "Model id", "Or type the id exactly as the server serves it", |ui| {
                let resp = ui.add(egui::TextEdit::singleline(&mut ai.model_edit).hint_text("Qwen/Qwen3.5-9B").desired_width(230.0));
                if resp.lost_focus() {
                    let m = ai.model_edit.clone();
                    ai.set_model(&m);
                }
                false
            });
        });
    });

    section_title(ui, p, "Connection");
    card(ui, p, |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            row(ui, p, "Port", "Used when the address has no port (vLLM's default is 8000)", |ui| {
                let r = ui.add(egui::DragValue::new(&mut ai.settings.port).range(1..=65535));
                if r.changed() {
                    ai.port_edited();
                }
                false
            });
            if toggle_row(ui, p, "Use HTTPS", "For a server behind TLS (a reverse proxy, or vLLM with --ssl-certfile)", &mut ai.settings.https) {
                ai.https_edited();
            }
            if toggle_row(
                ui,
                p,
                "Only allow local network servers",
                "Loopback, 10.x, 172.16-31.x, 192.168.x, link-local and IPv6 ULA addresses. Public servers also need HTTPS.",
                &mut ai.settings.lan_only,
            ) {
                ai.dirty = true;
            }
            if ai.settings.lan_only && toggle_row(ui, p, "Also allow Tailscale addresses", "100.64.0.0/10 (CGNAT)", &mut ai.settings.allow_cgnat) {
                ai.dirty = true;
            }
        });
    });

    ui.add_enabled_ui(enabled, |ui| {
        egui::CollapsingHeader::new(RichText::new("Advanced options").size(12.5).color(p.muted)).id_salt("settings-ai-advanced").default_open(false).show(ui, |ui| {
            let s = &mut ai.settings;
            let mut changed = false;
            card(ui, p, |ui| {
                let num = |ui: &mut Ui, title: &str, desc: &str, v: &mut u32, range: std::ops::RangeInclusive<u32>, suffix: &str| {
                    row(ui, p, title, desc, |ui| ui.add(egui::DragValue::new(v).range(range).suffix(suffix)).changed())
                };
                changed |= num(ui, "Connect timeout", "TCP and TLS connection", &mut s.timeout_connect_s, 1..=60, " s");
                changed |= num(ui, "First token timeout", "Covers model warm-up and reading large images", &mut s.timeout_first_token_s, 5..=1800, " s");
                changed |= num(ui, "Total timeout", "The whole request", &mut s.timeout_total_s, 10..=3600, " s");
                changed |= num(ui, "Max output tokens", "Longer answers need more; reports need about 1500", &mut s.max_completion_tokens, 64..=16384, "");
                changed |= row(ui, p, "Temperature", "Low values give repeatable scores", |ui| slider_box_resp(ui, egui::Slider::new(&mut s.temperature, 0.0..=1.5).fixed_decimals(2)).changed());
                changed |= num(ui, "Overview size", "Long side of the downscaled full view", &mut s.max_side_overview, 512..=4096, " px");
                changed |= num(ui, "Tile size", "Side of native-resolution ROI tiles (multiple of 32)", &mut s.tile_side, 256..=2048, " px");
                changed |= num(ui, "Images per request", "Must not exceed the server's --limit-mm-per-prompt", &mut s.max_images, 1..=16, "");
                changed |= row(ui, p, "Image encoding", "PNG is lossless; JPEG (q95) is smaller for slow links", |ui| segmented(ui, &mut s.encoding, &[(ImageEncoding::Png, "PNG"), (ImageEncoding::Jpeg, "JPEG")]));
                changed |= row(ui, p, "Box coordinates", "How the model reports boxes: 0..1000 (Qwen3, Gemma) or pixels (Qwen2.5-VL)", |ui| {
                    segmented(ui, &mut s.coord_convention, &[(CoordConvention::Relative1000, "0..1000"), (CoordConvention::AbsolutePixels, "Pixels")])
                });
                let mut thinking = !s.disable_thinking;
                if toggle_row(ui, p, "Let the model think first", "Slower; sometimes better at subtle artifacts (Qwen3.x thinking mode)", &mut thinking) {
                    s.disable_thinking = !thinking;
                    changed = true;
                }
                changed |= toggle_row(ui, p, "Swap-check comparisons", "Runs A vs B twice with A and B exchanged and reports whether the preference holds", &mut s.swap_check);
                changed |= toggle_row(ui, p, "Blind (no metrics)", "Leave Lumen's measured metrics out of the prompt", &mut s.blind);
            });
            section_title(ui, p, "Privacy");
            card(ui, p, |ui| {
                changed |= toggle_row(
                    ui,
                    p,
                    "Always confirm before sending",
                    "Show the send preview for every request, except to servers marked 'Do not ask again' (otherwise only the first time for each server)",
                    &mut s.always_confirm,
                );
                changed |= toggle_row(ui, p, "Include file name", "Send the file name with the image", &mut s.include_filename);
                changed |= toggle_row(ui, p, "Include capture settings", "Send camera, lens and exposure from EXIF (never GPS)", &mut s.include_exif);
                if !s.trusted_servers.is_empty() {
                    let n = s.trusted_servers.len();
                    let desc = format!("{n} server(s) skip the send preview: {}", s.trusted_servers.join(", "));
                    changed |= row(ui, p, "Trusted servers", &desc, |ui| {
                        let c = ui.button("Clear").clicked();
                        if c {
                            s.trusted_servers.clear();
                        }
                        c
                    });
                }
            });
            if changed {
                ai.dirty = true;
            }
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
                ui.add(egui::Label::new(RichText::new("Lumen").font(text::display()).color(p.text)).truncate());
                ui.add(egui::Label::new(RichText::new(format!("Version {}  ·  MIT License", env!("CARGO_PKG_VERSION"))).color(p.muted)).wrap());
                ui.add(egui::Hyperlink::from_label_and_url(RichText::new("github.com/RahulTarigoppula/lumen-image-viewer").size(text::SMALL), env!("CARGO_PKG_REPOSITORY")));
            });
        });
        ui.add_space(6.0);
        hint(ui, p, "A fast, private image & video viewer, browser, editor and converter for Windows.");
        ui.add_space(10.0);
    });

    section_title(ui, p, "Open-source credits");
    card(ui, p, |ui| {
        ui.add_space(6.0);
        if ui.available_width() >= 480.0 {
            egui::Grid::new("credits").num_columns(3).spacing([16.0, 5.0]).striped(false).show(ui, |ui| {
                for (name, what, license) in CREDITS {
                    ui.label(RichText::new(*name).color(p.text));
                    ui.label(RichText::new(*what).size(text::SMALL).color(p.muted));
                    ui.label(RichText::new(*license).size(text::SMALL).color(p.faint));
                    ui.end_row();
                }
            });
        } else {
            // Narrow: one entry per block, so nothing runs past the card.
            for (name, what, license) in CREDITS {
                ui.add(egui::Label::new(RichText::new(*name).color(p.text)).wrap());
                ui.add(egui::Label::new(RichText::new(format!("{what}  ·  {license}")).size(text::SMALL).color(p.muted)).wrap());
                ui.add_space(4.0);
            }
        }
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
    #[cfg(feature = "ai")]
    ("ureq, rustls, ring", "AI server connection (optional)", "MIT / Apache-2.0 / ISC"),
    #[cfg(feature = "ai")]
    ("egui_commonmark, pulldown-cmark", "Markdown answers (optional)", "MIT / Apache-2.0"),
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

// ====================================================================== keyboard

/// Waiting for a key press to (re)bind `cmd` in `mode`; `replace` is the binding being changed.
#[derive(Clone, Copy, PartialEq)]
struct Capture {
    mode: KeyMode,
    cmd: Command,
    replace: Option<KeyboardShortcut>,
}

/// A captured shortcut that is already used elsewhere: replace, keep both or cancel.
#[derive(Clone)]
struct PendingConflict {
    cap: Capture,
    sc: KeyboardShortcut,
    others: Vec<(KeyMode, Command)>,
}

fn set_command_keys(core: &mut Core, mode: KeyMode, cmd: Command, keys: Vec<KeyboardShortcut>) {
    let preset = Preset::from_id(&core.settings.keymap_preset);
    let mut km = Keymap::from_settings(&core.settings);
    km.set_keys(mode, cmd, keys);
    core.settings.key_overrides = km.diff_from(preset);
    keymap::install(&core.settings);
}

/// Bind `sc` to the captured command (replacing the chosen binding), optionally
/// removing it from the commands it conflicts with.
fn apply_capture(core: &mut Core, cap: Capture, sc: KeyboardShortcut, steal_from: &[(KeyMode, Command)]) {
    let preset = Preset::from_id(&core.settings.keymap_preset);
    let mut km = Keymap::from_settings(&core.settings);
    for (m, c) in steal_from {
        let keys: Vec<KeyboardShortcut> = km.keys_for(*m, *c).into_iter().filter(|k| *k != sc).collect();
        km.set_keys(*m, *c, keys);
    }
    let mut keys = km.keys_for(cap.mode, cap.cmd);
    match cap.replace.and_then(|r| keys.iter().position(|k| *k == r)) {
        Some(i) => keys[i] = sc,
        None => keys.push(sc),
    }
    km.set_keys(cap.mode, cap.cmd, keys);
    core.settings.key_overrides = km.diff_from(preset);
    keymap::install(&core.settings);
}

fn keyboard(ui: &mut Ui, p: &Palette, core: &mut Core, st: &mut State) -> bool {
    let mut changed = false;
    let preset = Preset::from_id(&core.settings.keymap_preset);

    // ---- key capture
    if let Some(cap) = st.capture {
        keymap::set_capturing(true);
        let pressed = ui.input_mut(|i| {
            let mut got: Option<(egui::Key, egui::Modifiers)> = None;
            i.events.retain(|e| match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if got.is_none() {
                        got = Some((*key, *modifiers));
                    }
                    false
                }
                egui::Event::Text(_) => false,
                _ => true,
            });
            got
        });
        if let Some((key, mods)) = pressed {
            if key == egui::Key::Escape && mods.is_none() {
                st.capture = None;
            } else {
                let sc = keymap::normalize(KeyboardShortcut::new(mods, key));
                let km = keymap::active();
                let others = km.conflicts(cap.mode, sc, cap.cmd);
                st.capture = None;
                if others.is_empty() {
                    apply_capture(core, cap, sc, &[]);
                    changed = true;
                } else {
                    st.conflict = Some(PendingConflict { cap, sc, others });
                }
            }
        }
    } else {
        keymap::set_capturing(false);
    }

    section_title(ui, p, "Preset");
    card(ui, p, |ui| {
        row(ui, p, "Shortcut preset", "FastStone compatible adds A for actual size and leaves Space and ` free.", |ui| {
            crate::ui::widgets::combo("settings-keymap-preset").width(200.0).selected_text(preset.label()).show_ui(ui, |ui| {
                for pr in Preset::ALL {
                    if ui.selectable_label(pr == preset, pr.label()).clicked() && pr != preset {
                        if core.settings.key_overrides.is_empty() {
                            core.settings.keymap_preset = pr.id().to_string();
                            keymap::install(&core.settings);
                            changed = true;
                        } else {
                            st.confirm_preset = Some(pr);
                        }
                    }
                }
            });
            false
        });
        let n = core.settings.key_overrides.len();
        row(ui, p, "Custom shortcuts", &if n == 0 { "None - every key works as in the preset.".to_string() } else { format!("{n} command(s) changed from the preset.") }, |ui| {
            if ui.add_enabled(n > 0, egui::Button::new(format!("{}  Reset to preset", I::ARROW_COUNTER_CLOCKWISE))).clicked() {
                st.confirm_reset_keys = true;
            }
            false
        });
    });

    // ---- conflict prompt
    if let Some(c) = st.conflict.clone() {
        let names: Vec<String> = c.others.iter().map(|(m, cmd)| format!("'{}' ({})", cmd.label(), m.label())).collect();
        egui::Frame::new().fill(p.warn.gamma_multiply(0.12)).stroke(Stroke::new(1.0, p.warn)).corner_radius(CornerRadius::same(8)).inner_margin(Margin::same(10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(format!("{}  {} is already used by {}.", I::WARNING, keymap::format_shortcut(&c.sc), names.join(", "))).color(p.text)).wrap());
            ui.horizontal_wrapped(|ui| {
                if ui.button(format!("Use for '{}' only", c.cap.cmd.label())).clicked() {
                    apply_capture(core, c.cap, c.sc, &c.others);
                    st.conflict = None;
                    changed = true;
                }
                if ui.button("Keep both").on_hover_text("The binding of the current mode wins; Global shortcuts lose to mode shortcuts.").clicked() {
                    apply_capture(core, c.cap, c.sc, &[]);
                    st.conflict = None;
                    changed = true;
                }
                if ui.button("Cancel").clicked() {
                    st.conflict = None;
                }
            });
        });
        ui.add_space(8.0);
    }

    // ---- mode tabs + search
    // Narrow window: the search box goes on its own line.
    let wide = ui.available_width() >= 560.0;
    let search = |ui: &mut Ui, text: &mut String| {
        ui.label(RichText::new(I::MAGNIFYING_GLASS).color(p.muted));
        let w = 220.0f32.min(ui.available_width() - 8.0).max(80.0);
        let r = ui.add(egui::TextEdit::singleline(text).hint_text("Search commands or keys").desired_width(w));
        // Esc clears the search first (staying in the field), then leaves the field;
        // only an Esc outside the field closes Settings.
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) && !text.is_empty() {
            text.clear();
            r.request_focus();
        }
    };
    ui.horizontal(|ui| {
        let modes: Vec<(KeyMode, &str)> = KeyMode::ALL.iter().map(|m| (*m, m.label())).collect();
        if widgets::segmented_value(ui, &mut st.kb_mode, &modes) {
            st.capture = None;
        }
        if wide {
            ui.add_space(12.0);
            search(ui, &mut st.kb_search);
        }
    });
    if !wide {
        ui.horizontal(|ui| search(ui, &mut st.kb_search));
    }
    hint(
        ui,
        p,
        match st.kb_mode {
            KeyMode::Global => "Global shortcuts work in every mode. Those with Ctrl, Alt or an F-key also work while typing in a text box.",
            KeyMode::Browser => "Used in the thumbnail browser. Arrow keys, Home / End and Page Up / Down always move the selection.",
            KeyMode::Viewer => "Used while viewing an image. Numpad * always cycles the fit mode.",
        },
    );
    ui.add_space(6.0);

    let km = keymap::active();
    let base = Keymap::preset(preset);
    let mode = st.kb_mode;
    let q = st.kb_search.trim().to_lowercase();
    // Every shortcut button of this mode has the same width, so the remove and add
    // buttons line up in columns from row to row.
    let widest = Command::all().into_iter().flat_map(|c| km.keys_for(mode, c)).map(|k| widgets::shortcut_keys_width(ui.painter(), &keymap::format_shortcut(&k))).fold(0.0, f32::max);
    let press_w = ui.painter().layout_no_wrap("Press a key...".into(), text::body(), p.text).size().x;
    let key_w = (widest.max(press_w) + 16.0).clamp(64.0, 200.0);
    let btn_h = 26.0;
    let mut hot = None;
    card(ui, p, |ui| {
        let mut first = true;
        for cmd in Command::all() {
            let keys = km.keys_for(mode, cmd);
            let key_text: Vec<String> = keys.iter().map(keymap::format_shortcut).collect();
            if !q.is_empty() && !cmd.label().to_lowercase().contains(&q) && !key_text.iter().any(|k| k.to_lowercase() == q || k.to_lowercase().contains(&q)) {
                continue;
            }
            if !first {
                let y = ui.cursor().top();
                let r = ui.max_rect();
                ui.painter().hline(r.left()..=r.right(), y, Stroke::new(1.0, p.border.gamma_multiply(0.7)));
            }
            first = false;
            // Name column, then the keys (wrapping onto more lines when narrow), then
            // the restore button at the far right.
            let w = ui.available_width();
            let name_w = 250.0f32.min(w * 0.42);
            let customized = keys != base.keys_for(mode, cmd);
            let restore_w = if customized { 30.0 } else { 0.0 };
            let capturing_row = st.capture.is_some_and(|c| c.mode == mode && c.cmd == cmd);
            let show_edit = st.kb_hot == Some(cmd) || capturing_row;
            let mut focused = false;
            let row = ui.horizontal(|ui| {
                ui.set_min_height(34.0);
                ui.allocate_ui_with_layout(Vec2::new(name_w, 26.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_width(name_w);
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.add_sized([18.0, 20.0], egui::Label::new(RichText::new(cmd.icon()).size(size::ICON).color(p.muted)));
                    ui.add(egui::Label::new(RichText::new(cmd.label()).color(p.text)).truncate());
                });
                let keys_w = (ui.available_width() - restore_w).max(60.0);
                ui.allocate_ui_with_layout(Vec2::new(keys_w, 26.0), Layout::left_to_right(Align::Center).with_main_wrap(true), |ui| {
                    ui.set_width(keys_w);
                    ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                    for (k, text) in keys.iter().zip(&key_text) {
                        let capturing_this = st.capture == Some(Capture { mode, cmd, replace: Some(*k) });
                        let conflicts = km.conflicts(mode, *k, cmd);
                        let b = key_button(ui, p, if capturing_this { None } else { Some(text) }, !conflicts.is_empty(), Vec2::new(key_w, btn_h));
                        focused |= b.has_focus();
                        let b = if !conflicts.is_empty() {
                            let names: Vec<String> = conflicts.iter().map(|(m, c)| format!("{} ({})", c.label(), m.label())).collect();
                            b.on_hover_text(format!("Also bound to {}", names.join(", ")))
                        } else {
                            b.on_hover_text("Click, then press the new key")
                        };
                        if b.clicked() {
                            st.capture = Some(Capture { mode, cmd, replace: Some(*k) });
                            st.conflict = None;
                        }
                        let x = ui.scope(|ui| {
                            ui.set_opacity(if show_edit { 1.0 } else { 0.0 });
                            widgets::icon_button_sized(ui, I::X, "Remove this shortcut", false, 22.0, 12.0)
                        });
                        focused |= x.inner.has_focus();
                        if x.inner.clicked() {
                            let rest: Vec<KeyboardShortcut> = keys.iter().copied().filter(|x| x != k).collect();
                            set_command_keys(core, mode, cmd, rest);
                            changed = true;
                        }
                        ui.add_space(4.0);
                    }
                    let adding = st.capture == Some(Capture { mode, cmd, replace: None });
                    let add = if adding {
                        key_button(ui, p, None, false, Vec2::new(key_w, btn_h))
                    } else {
                        ui.scope(|ui| {
                            ui.set_opacity(if show_edit || keys.is_empty() { 1.0 } else { 0.0 });
                            widgets::icon_button_sized(ui, I::PLUS, "Add a shortcut", false, btn_h, 14.0)
                        })
                        .inner
                    };
                    focused |= add.has_focus();
                    if add.clicked() {
                        st.capture = Some(Capture { mode, cmd, replace: None });
                        st.conflict = None;
                    }
                });
                if customized {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button_sized(ui, I::ARROW_COUNTER_CLOCKWISE, "Restore the preset shortcuts for this command", false, btn_h, 14.0).clicked() {
                            set_command_keys(core, mode, cmd, base.keys_for(mode, cmd));
                            changed = true;
                        }
                    });
                }
            });
            if focused || ui.rect_contains_pointer(row.response.rect) {
                hot = Some(cmd);
            }
        }
        if first {
            ui.add_space(8.0);
            ui.label(RichText::new("No commands match the search").color(p.muted));
            ui.add_space(8.0);
        }
    });
    if st.kb_hot != hot {
        st.kb_hot = hot;
        ui.ctx().request_repaint();
    }

    if st.confirm_reset_keys {
        match widgets::show_confirm(ui.ctx(), "Reset shortcuts?", "All custom shortcuts are removed and the preset's keys apply again.", "Reset", true, p) {
            Some(true) => {
                st.confirm_reset_keys = false;
                core.settings.key_overrides.clear();
                keymap::install(&core.settings);
                core.toasts.success("Shortcuts reset to the preset");
                changed = true;
            }
            Some(false) => st.confirm_reset_keys = false,
            None => {}
        }
    }
    if let Some(pr) = st.confirm_preset {
        let body = format!("Switching to '{}' removes your {} custom shortcut(s).", pr.label(), core.settings.key_overrides.len());
        match widgets::show_confirm(ui.ctx(), "Change preset?", &body, "Switch preset", true, p) {
            Some(true) => {
                st.confirm_preset = None;
                core.settings.key_overrides.clear();
                core.settings.keymap_preset = pr.id().to_string();
                keymap::install(&core.settings);
                changed = true;
            }
            Some(false) => st.confirm_preset = None,
            None => {}
        }
    }
    changed
}

