//! Visual identity: a calm graphite UI that keeps attention on the pictures.

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
    pub accent: Color32,
    pub accent_soft: Color32,
    pub danger: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub overlay: Color32,
}

impl Palette {
    pub fn new(dark: bool, accent: [u8; 3]) -> Self {
        let accent = Color32::from_rgb(accent[0], accent[1], accent[2]);
        if dark {
            Palette {
                dark,
                bg: Color32::from_rgb(17, 18, 21),
                panel: Color32::from_rgb(23, 24, 28),
                raised: Color32::from_rgb(32, 34, 39),
                hover: Color32::from_rgb(42, 45, 52),
                border: Color32::from_rgb(44, 47, 54),
                text: Color32::from_rgb(230, 232, 236),
                muted: Color32::from_rgb(150, 156, 166),
                faint: Color32::from_rgb(98, 104, 114),
                accent,
                accent_soft: accent.gamma_multiply(0.28),
                danger: Color32::from_rgb(240, 98, 98),
                ok: Color32::from_rgb(76, 194, 128),
                warn: Color32::from_rgb(236, 180, 70),
                overlay: Color32::from_rgba_unmultiplied(16, 17, 20, 228),
            }
        } else {
            Palette {
                dark,
                bg: Color32::from_rgb(243, 244, 246),
                panel: Color32::from_rgb(250, 250, 251),
                raised: Color32::from_rgb(255, 255, 255),
                hover: Color32::from_rgb(228, 231, 236),
                border: Color32::from_rgb(218, 221, 227),
                text: Color32::from_rgb(28, 30, 34),
                muted: Color32::from_rgb(96, 102, 112),
                faint: Color32::from_rgb(150, 156, 166),
                accent,
                accent_soft: accent.gamma_multiply(0.22),
                danger: Color32::from_rgb(214, 64, 64),
                ok: Color32::from_rgb(36, 150, 90),
                warn: Color32::from_rgb(200, 140, 30),
                overlay: Color32::from_rgba_unmultiplied(250, 250, 251, 236),
            }
        }
    }
}

fn load_system_font(names: &[&str]) -> Option<Vec<u8>> {
    let dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
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

fn fonts(fallbacks: bool) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let mut prop: Vec<String> = Vec::new();
    if let Some(b) = load_system_font(&["segoeui.ttf"]) {
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

    if let Some(b) = load_system_font(&["seguisb.ttf", "segoeuib.ttf"]) {
        fonts.font_data.insert("segoe-semibold".into(), Arc::new(FontData::from_owned(b)));
        let mut f = vec!["segoe-semibold".to_string()];
        f.extend(family.iter().cloned());
        fonts.families.insert(FontFamily::Name("semibold".into()), f);
    } else {
        fonts.families.insert(FontFamily::Name("semibold".into()), family.clone());
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

pub fn icon_fill(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons-fill".into()))
}

pub fn apply(ctx: &egui::Context, p: &Palette, scale: f32) {
    let mut visuals = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    visuals.panel_fill = p.panel;
    visuals.window_fill = p.raised;
    visuals.faint_bg_color = if p.dark { Color32::from_rgb(27, 28, 33) } else { Color32::from_rgb(244, 245, 247) };
    visuals.extreme_bg_color = if p.dark { Color32::from_rgb(13, 14, 16) } else { Color32::WHITE };
    visuals.code_bg_color = p.raised;
    visuals.window_stroke = Stroke::new(1.0, p.border);
    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.window_shadow = Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if p.dark { 140 } else { 50 }) };
    visuals.popup_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(if p.dark { 120 } else { 40 }) };
    visuals.hyperlink_color = p.accent;
    visuals.selection.bg_fill = p.accent.gamma_multiply(if p.dark { 0.55 } else { 0.35 });
    visuals.selection.stroke = Stroke::new(1.0, p.text);
    visuals.warn_fg_color = p.warn;
    visuals.error_fg_color = p.danger;
    visuals.override_text_color = None;
    visuals.slider_trailing_fill = true;
    visuals.striped = true;

    let r = CornerRadius::same(6);
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
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.accent.gamma_multiply(0.7);
    w.active.weak_bg_fill = p.accent.gamma_multiply(0.5);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
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
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.interact_size = Vec2::new(28.0, 26.0);
        style.spacing.window_margin = Margin::same(14);
        style.spacing.menu_margin = Margin::same(6);
        style.spacing.slider_width = 180.0;
        style.spacing.combo_width = 160.0;
        style.spacing.icon_width = 16.0;
        style.spacing.icon_width_inner = 9.0;
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = true;
        style.text_styles.insert(TextStyle::Body, FontId::proportional(13.5));
        style.text_styles.insert(TextStyle::Button, FontId::proportional(13.5));
        style.text_styles.insert(TextStyle::Small, FontId::proportional(11.5));
        style.text_styles.insert(TextStyle::Monospace, FontId::monospace(12.5));
        style.text_styles.insert(TextStyle::Heading, semibold(17.0));
        style.interaction.tooltip_delay = 0.35;
        style.animation_time = 0.12;
    });
    ctx.set_zoom_factor(scale.clamp(0.6, 2.5));
}
