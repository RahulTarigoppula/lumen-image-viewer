//! Visual identity: a calm graphite UI that keeps attention on the pictures.
//!
//! This module holds the design tokens every surface draws with:
//! - [`Palette`]: colors (both themes), including the single "selected" blue
//!   ([`Palette::selected_bg`] + [`Palette::accent_text`]) and the one filled
//!   primary button per surface ([`Palette::accent_fill`] + [`Palette::on_accent`]).
//! - [`text`]: the type scale (six sizes) as `FontId`s.
//! - [`space`], [`radius`] and [`size`]: spacing, corner radii and control heights.
//! - [`popover_shadow`], [`window_shadow`] and [`scrim`]: elevation.
//!
//! [`apply`] turns them into egui's style, so plain egui widgets already match;
//! the helpers in `widgets.rs` cover what egui cannot be told through its style.

use egui::epaint::text::{FontTweak, VariationCoords};
use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Vec2};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub raised: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    /// The user's accent color, for rings, carets, progress and slider fills.
    pub accent: Color32,
    /// Same as [`Palette::selected_bg`]; kept for older call sites.
    pub accent_soft: Color32,
    /// Fill of the one primary (filled) button per surface. Draw its text in [`Palette::on_accent`].
    pub accent_fill: Color32,
    /// Text and icons on [`Palette::accent_fill`]: near-black on a light accent, white on a dark one.
    pub on_accent: Color32,
    /// Accent for text and icons on panel backgrounds (selected nav item, active segment icon,
    /// links). Equal to `accent` in dark mode and darkened in light mode so it stays readable.
    pub accent_text: Color32,
    /// THE selected style: soft accent fill behind selected toggles, nav items, list rows and
    /// cells. Pair it with [`Palette::accent_text`] for the selected item's icon / text.
    pub selected_bg: Color32,
    pub danger: Color32,
    /// Fill of destructive buttons (white text, at least 4.5:1).
    pub danger_fill: Color32,
    pub ok: Color32,
    /// Fill of the "keep" call to action (white text).
    #[allow(dead_code)] // Used by the compare surface.
    pub success_fill: Color32,
    pub warn: Color32,
    pub overlay: Color32,
    /// Zebra rows, segmented-control tracks and other recessed areas.
    pub faint_bg: Color32,
    /// Text fields, value boxes and list wells.
    pub input_bg: Color32,
}

/// WCAG relative luminance of an sRGB color.
fn luminance(c: Color32) -> f32 {
    let ch = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
}

/// Mix `c` toward black by `t` (0..1).
fn darken(c: Color32, t: f32) -> Color32 {
    let f = |v: u8| (v as f32 * (1.0 - t)).round() as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// The text color with the better contrast on `fill` (near-black or white).
pub fn text_on(fill: Color32) -> Color32 {
    // Above this luminance dark text has more contrast than white (both are equal at 0.179).
    if luminance(fill) > 0.179 { Color32::from_rgb(10, 12, 16) } else { Color32::WHITE }
}

impl Palette {
    pub fn new(dark: bool, accent: [u8; 3]) -> Self {
        let accent = Color32::from_rgb(accent[0], accent[1], accent[2]);
        let soft = |a: u8| Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), a);
        if dark {
            let selected_bg = soft(56);
            Palette {
                dark,
                bg: Color32::from_rgb(17, 18, 21),
                panel: Color32::from_rgb(23, 24, 28),
                raised: Color32::from_rgb(32, 34, 39),
                hover: Color32::from_rgb(42, 45, 52),
                border: Color32::from_rgb(44, 47, 54),
                text: Color32::from_rgb(230, 232, 236),
                muted: Color32::from_rgb(150, 156, 166),
                faint: Color32::from_rgb(120, 126, 137),
                accent,
                accent_soft: selected_bg,
                accent_fill: accent,
                on_accent: text_on(accent),
                accent_text: accent,
                selected_bg,
                danger: Color32::from_rgb(240, 98, 98),
                danger_fill: Color32::from_rgb(200, 60, 60),
                ok: Color32::from_rgb(76, 194, 128),
                success_fill: Color32::from_rgb(34, 134, 84),
                warn: Color32::from_rgb(236, 180, 70),
                overlay: Color32::from_rgba_unmultiplied(16, 17, 20, 228),
                faint_bg: Color32::from_rgb(27, 28, 33),
                input_bg: Color32::from_rgb(13, 14, 16),
            }
        } else {
            let fill = darken(accent, 0.25);
            let selected_bg = soft(41);
            Palette {
                dark,
                bg: Color32::from_rgb(243, 244, 246),
                panel: Color32::from_rgb(250, 250, 251),
                raised: Color32::from_rgb(255, 255, 255),
                hover: Color32::from_rgb(228, 231, 236),
                border: Color32::from_rgb(208, 212, 220),
                text: Color32::from_rgb(28, 30, 34),
                muted: Color32::from_rgb(96, 102, 112),
                faint: Color32::from_rgb(112, 118, 128),
                accent,
                accent_soft: selected_bg,
                accent_fill: fill,
                on_accent: text_on(fill),
                accent_text: if luminance(accent) > 0.3 { darken(accent, 0.35) } else { accent },
                selected_bg,
                danger: Color32::from_rgb(214, 64, 64),
                danger_fill: Color32::from_rgb(196, 43, 28),
                ok: Color32::from_rgb(36, 150, 90),
                success_fill: Color32::from_rgb(34, 134, 84),
                warn: Color32::from_rgb(200, 140, 30),
                overlay: Color32::from_rgba_unmultiplied(250, 250, 251, 236),
                faint_bg: Color32::from_rgb(244, 245, 247),
                input_bg: Color32::WHITE,
            }
        }
    }
}

/// The type scale. Only these sizes are used; pick by role, not by look.
// The whole scale is defined even where the app uses only part of it.
#[allow(dead_code)]
pub mod text {
    use egui::FontId;

    /// Settings page title, About app name (20 semibold).
    pub const DISPLAY: f32 = 20.0;
    /// Window, dialog and panel titles (15 semibold).
    pub const TITLE: f32 = 15.0;
    /// Card and section headers inside windows (13 semibold).
    pub const SUBTITLE: f32 = 13.0;
    /// Labels, menu items, list rows, buttons.
    pub const BODY: f32 = 13.0;
    /// Metadata, captions, hints, status bar.
    pub const SMALL: f32 = 11.5;
    /// Section labels ("Presets"): semibold, sentence case, muted.
    pub const CAPTION: f32 = 11.0;
    /// Timecode, frame numbers, rename tokens (never shortcuts).
    pub const MONO: f32 = 12.0;

    pub fn display() -> FontId {
        super::display_font(DISPLAY)
    }
    pub fn title() -> FontId {
        super::semibold(TITLE)
    }
    pub fn subtitle() -> FontId {
        super::semibold(SUBTITLE)
    }
    pub fn body() -> FontId {
        FontId::proportional(BODY)
    }
    pub fn body_strong() -> FontId {
        super::semibold(BODY)
    }
    pub fn small() -> FontId {
        FontId::proportional(SMALL)
    }
    pub fn caption() -> FontId {
        super::semibold(CAPTION)
    }
    pub fn mono() -> FontId {
        FontId::monospace(MONO)
    }
}

/// Spacing scale: 2, 4, 6, 8, 12, 16, 20, 24, 32.
// The whole scale is defined even where the app uses only part of it.
#[allow(dead_code)]
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const S: f32 = 6.0;
    pub const M: f32 = 8.0;
    pub const L: f32 = 12.0;
    pub const XL: f32 = 16.0;
    pub const XXL: f32 = 20.0;
    pub const XXXL: f32 = 24.0;
    pub const HUGE: f32 = 32.0;
    /// Inner margin of panels.
    pub const PANEL: i8 = 12;
    /// Inner margin of windows and dialogs.
    pub const WINDOW: i8 = 20;
    /// Gap between a section label and its card.
    pub const LABEL_GAP: f32 = 6.0;
    /// Gap between sections.
    pub const SECTION_GAP: f32 = 16.0;
    /// Gap above a row of dialog buttons.
    pub const BUTTONS_GAP: f32 = 16.0;
}

/// Corner radii.
// The whole scale is defined even where the app uses only part of it.
#[allow(dead_code)]
pub mod radius {
    /// Checkbox, keycap, small chip.
    pub const SMALL: u8 = 4;
    /// Buttons, inputs, list-row highlight.
    pub const CONTROL: u8 = 6;
    /// Cards, menus, popovers, thumbnails, toasts.
    pub const CARD: u8 = 8;
    /// Windows, dialogs, floating bars, edge panels.
    pub const WINDOW: u8 = 12;
}

/// Control and row heights (points).
// The whole scale is defined even where the app uses only part of it.
#[allow(dead_code)]
pub mod size {
    /// Standard control: button, text box, combo, segmented control.
    pub const CONTROL: f32 = 28.0;
    /// Compact control: toolbar chips, presets, controls inside tables.
    pub const COMPACT: f32 = 24.0;
    /// Menu and command palette rows.
    pub const MENU_ROW: f32 = 28.0;
    /// Tree and details rows.
    pub const LIST_ROW: f32 = 26.0;
    /// Key / value rows (info panel, metadata).
    pub const KV_ROW: f32 = 22.0;
    /// Toolbar height (36 pt buttons inside 4 pt margins).
    pub const TOOLBAR: f32 = 44.0;
    /// Status bar height.
    pub const STATUS_BAR: f32 = 24.0;
    /// Icon glyph sizes: toolbar, menus / rows / labelled buttons, carets.
    pub const ICON_TOOLBAR: f32 = 16.0;
    pub const ICON: f32 = 14.0;
    pub const ICON_CARET: f32 = 12.0;
    /// Width of number value boxes next to sliders and in forms.
    pub const VALUE_W: f32 = 64.0;
    /// Width of the label column in tool and form rows.
    pub const LABEL_W: f32 = 96.0;
}

/// Shadow of popovers, menus, toasts and floating bars.
pub fn popover_shadow(p: &Palette) -> Shadow {
    Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(if p.dark { 110 } else { 28 }) }
}

/// Shadow of windows and dialogs.
pub fn window_shadow(p: &Palette) -> Shadow {
    Shadow { offset: [0, 12], blur: 40, spread: 0, color: Color32::from_black_alpha(if p.dark { 150 } else { 40 }) }
}

/// Backdrop behind modals and the command palette.
pub fn scrim(p: &Palette) -> Color32 {
    Color32::from_black_alpha(if p.dark { 90 } else { 40 })
}

fn load_system_font(names: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    for n in names {
        let p = std::path::Path::new(&dir).join("Fonts").join(n);
        if let Ok(b) = std::fs::read(&p) {
            return Some(b);
        }
    }
    None
}

/// Install the UI fonts. The first frame only needs Segoe UI and the icon
/// fonts; the symbol, CJK and emoji fallbacks (about 35 MB of font files,
/// slow to read on a cold start) are loaded on a worker and swapped in when
/// ready, so file names in other scripts render a moment later.
pub fn install_fonts(ctx: &egui::Context) {
    ctx.set_fonts(fonts(false));
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new().name("lumen-fonts".into()).spawn(move || {
        ctx.set_fonts(fonts(true));
        ctx.request_repaint();
    });
    if let Err(e) = spawned {
        log::warn!("could not load fallback fonts in the background: {e}");
    }
}

/// A variable-font instance: the same file at another weight / optical size.
fn instance(blob: &egui::epaint::text::Blob, wght: f32, opsz: f32) -> FontData {
    FontData::from_blob(blob.clone(), 0).tweak(FontTweak { coords: VariationCoords::new([(b"wght", wght), (b"opsz", opsz)]), ..Default::default() })
}

fn fonts(fallbacks: bool) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let mut prop: Vec<String> = Vec::new();
    // Windows 11: Segoe UI Variable (the system face; "Text" optical size for UI text,
    // "Display" for large titles). Older Windows: the static Segoe UI files.
    let variable = load_system_font(&["SegUIVar.ttf"]).map(|b| -> egui::epaint::text::Blob { Arc::new(b) });
    if let Some(blob) = &variable {
        fonts.font_data.insert("segoe".into(), Arc::new(instance(blob, 400.0, 10.5)));
        prop.push("segoe".into());
    } else if let Some(b) = load_system_font(&["segoeui.ttf"]) {
        fonts.font_data.insert("segoe".into(), Arc::new(FontData::from_owned(b)));
        prop.push("segoe".into());
    }
    fonts.font_data.insert(
        "phosphor".into(),
        Arc::new(FontData::from_static(include_bytes!("../../assets/Phosphor.ttf"))),
    );
    fonts.font_data.insert(
        "phosphor-fill".into(),
        Arc::new(FontData::from_static(include_bytes!("../../assets/Phosphor-Fill.ttf"))),
    );
    // CJK / symbols fallback so file names in any script render.
    for (key, files) in [
        ("symbol", &["seguisym.ttf"][..]),
        ("cjk", &["msyh.ttc", "YuGothR.ttc", "malgun.ttf"][..]),
        ("emoji", &["seguiemj.ttf"][..]),
    ] {
        if let Some(b) = fallbacks.then(|| load_system_font(files)).flatten() {
            fonts.font_data.insert(key.into(), Arc::new(FontData::from_owned(b)));
        }
    }
    let default_prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut family = prop.clone();
    family.push("phosphor".into());
    family.extend(default_prop.iter().cloned());
    for k in ["symbol", "cjk", "emoji"] {
        if fonts.font_data.contains_key(k) {
            family.push(k.into());
        }
    }
    fonts.families.insert(FontFamily::Proportional, family.clone());

    let with_family = |first: &str| {
        let mut f = vec![first.to_string()];
        f.extend(family.iter().cloned());
        f
    };
    if let Some(blob) = &variable {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(instance(blob, 600.0, 10.5)));
        fonts.font_data.insert("segoe-display".into(), Arc::new(instance(blob, 600.0, 20.0)));
        fonts.families.insert(FontFamily::Name("semibold".into()), with_family("segoe-semibold"));
        fonts.families.insert(FontFamily::Name("display".into()), with_family("segoe-display"));
    } else if let Some(b) = load_system_font(&["seguisb.ttf", "segoeuib.ttf"]) {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(FontData::from_owned(b)));
        fonts.families.insert(FontFamily::Name("semibold".into()), with_family("segoe-semibold"));
        fonts.families.insert(FontFamily::Name("display".into()), with_family("segoe-semibold"));
    } else {
        fonts.families.insert(FontFamily::Name("semibold".into()), family.clone());
        fonts.families.insert(FontFamily::Name("display".into()), family.clone());
    }
    let mut fill = vec!["phosphor-fill".to_string()];
    fill.extend(family.iter().cloned());
    fonts.families.insert(FontFamily::Name("icons-fill".into()), fill);

    if let Some(b) = load_system_font(&["CascadiaMono.ttf", "consola.ttf"]) {
        fonts.font_data.insert("mono".into(), Arc::new(FontData::from_owned(b)));
        let mut m = vec!["mono".to_string()];
        m.extend(fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default());
        m.push("phosphor".into());
        fonts.families.insert(FontFamily::Monospace, m);
    }
    fonts
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}

/// Semibold at the "Display" optical size, for large titles (see [`text::display`]).
pub fn display_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("display".into()))
}

pub fn icon_fill(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons-fill".into()))
}

/// Linear blend between two opaque colors.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn palette_id() -> egui::Id {
    egui::Id::unique("lumen-palette")
}

/// The palette last installed by [`apply`], for widgets that only get a `Ui`
/// (e.g. `impl egui::Widget`). Prefer passing `&Palette` where one is at hand.
pub fn palette(ctx: &egui::Context) -> Palette {
    ctx.data(|d| d.get_temp::<Palette>(palette_id())).unwrap_or_else(|| Palette::new(true, [86, 156, 255]))
}

pub fn apply(ctx: &egui::Context, p: &Palette, scale: f32) {
    ctx.data_mut(|d| d.insert_temp(palette_id(), *p));
    let mut visuals = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.raised;
    visuals.faint_bg_color = p.faint_bg;
    visuals.extreme_bg_color = p.input_bg;
    visuals.code_bg_color = p.raised;
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.window_corner_radius = CornerRadius::same(radius::WINDOW);
    visuals.menu_corner_radius = CornerRadius::same(radius::CARD);
    visuals.window_shadow = window_shadow(p);
    visuals.popup_shadow = popover_shadow(p);
    visuals.hyperlink_color = p.accent_text;
    // One selected style everywhere: a soft accent fill with accent text / icons.
    visuals.selection.bg_fill = p.selected_bg;
    visuals.selection.stroke = Stroke::new(1.0, p.accent_text);
    visuals.warn_fg_color = p.warn;
    visuals.error_fg_color = p.danger;
    visuals.override_text_color = None;
    visuals.slider_trailing_fill = true;
    visuals.striped = true;

    let r = CornerRadius::same(radius::CONTROL);
    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = p.panel;
    w.noninteractive.weak_bg_fill = p.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = r;
    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.hovered.bg_fill = p.hover;
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_stroke = Stroke::new(1.0, mix(p.border, p.muted, 0.35));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    // Pressed: a step past hover, never a saturated accent flash.
    let pressed = if p.dark { mix(p.hover, p.text, 0.08) } else { mix(p.hover, p.text, 0.06) };
    w.active.bg_fill = pressed;
    w.active.weak_bg_fill = pressed;
    w.active.bg_stroke = Stroke::new(1.0, mix(p.border, p.muted, 0.5));
    // egui uses the active text color for every `.strong()` label, so it must
    // contrast with the panel background in both themes (not plain white).
    w.active.fg_stroke = Stroke::new(1.5, if p.dark { Color32::WHITE } else { Color32::from_rgb(10, 12, 16) });
    w.active.corner_radius = r;
    w.active.expansion = 0.0;
    w.open.bg_fill = p.hover;
    w.open.weak_bg_fill = p.hover;
    w.open.bg_stroke = Stroke::new(1.0, p.border);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r;

    let theme = if p.dark { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_theme(theme);
    ctx.set_visuals_of(theme, visuals);
    ctx.style_mut_of(theme, |style| {
        // Density: 24 pt controls with 4 pt between rows (28 pt rhythm).
        style.spacing.item_spacing = Vec2::new(8.0, 4.0);
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        style.spacing.interact_size = Vec2::new(28.0, 24.0);
        style.spacing.window_margin = Margin::same(16);
        style.spacing.menu_margin = Margin::same(4);
        style.spacing.slider_width = 180.0;
        style.spacing.slider_rail_height = 4.0;
        style.spacing.combo_width = 160.0;
        style.spacing.icon_width = 16.0;
        style.spacing.icon_width_inner = 10.0;
        style.spacing.icon_spacing = 6.0;
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = true;
        style.text_styles.insert(TextStyle::Body, text::body());
        style.text_styles.insert(TextStyle::Button, text::body());
        style.text_styles.insert(TextStyle::Small, text::small());
        style.text_styles.insert(TextStyle::Monospace, text::mono());
        style.text_styles.insert(TextStyle::Heading, text::title());
        style.interaction.tooltip_delay = super::TOOLTIP_DELAY;
        style.animation_time = 0.12;
    });
    ctx.set_zoom_factor(scale.clamp(0.6, 2.5));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn primary_buttons_are_readable_for_every_accent() {
        for rgb in [[86, 156, 255], [45, 190, 180], [76, 194, 128], [236, 180, 70], [255, 140, 66], [240, 98, 98], [236, 98, 170], [160, 120, 255], [140, 150, 165], [20, 20, 120], [250, 250, 250]] {
            for dark in [true, false] {
                let p = Palette::new(dark, rgb);
                assert!(contrast(p.accent_fill, p.on_accent) >= 4.5, "{rgb:?} dark={dark}");
            }
        }
    }

    #[test]
    fn faint_text_is_readable() {
        for dark in [true, false] {
            let p = Palette::new(dark, [86, 156, 255]);
            assert!(contrast(p.faint, p.panel) >= 4.0, "dark={dark}");
            assert!(contrast(Color32::WHITE, p.danger_fill) >= 4.5, "dark={dark}");
        }
    }
}
