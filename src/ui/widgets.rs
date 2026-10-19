//! Small reusable widgets with a consistent look.
//!
//! Every surface should build from these instead of styling egui widgets by hand:
//!
//! | Need | Helper |
//! |---|---|
//! | Window with title row and close button | [`app_window`] |
//! | Modal dialog | [`modal`], [`modal_title`], [`modal_body`], [`dialog_buttons`], [`show_confirm`], [`show_prompt`] |
//! | Buttons | [`primary_button`], [`danger_button`], [`success_button`], [`secondary_button`], [`icon_button`] |
//! | Slider (+ value box, double-click reset, centre fill) | [`Slider`] (drop-in for `egui::Slider`) |
//! | Number field | [`value_box`] around an `egui::DragValue` |
//! | Checkbox / radio | [`checkbox`], [`radio`], [`radio_value`] |
//! | Segmented control | [`segmented`], [`segmented_value`] |
//! | Combo / collapsing carets | [`combo`], [`combo_caret`], [`collapse_caret`] |
//! | Menu rows | [`menu_item`], [`menu_toggle`], [`submenu`] |
//! | Shortcuts | [`keycap`], [`shortcut_keys`], [`paint_shortcut_keys`] |
//! | Section label | [`section_title`] |
//! | Notifications | [`Toasts`] |

use super::icons as I;
use super::theme::{self, mix, radius, size, text, Palette};
use egui::emath::Numeric;
use egui::{Align, Align2, Color32, CornerRadius, FontId, Id, Layout, Margin, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetText};
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

// ====================================================================== buttons

/// Square icon button with hover highlight and tooltip. `selected` uses the one
/// selected style (soft accent fill, accent icon).
pub fn icon_button(ui: &mut Ui, icon: &str, tip: &str, selected: bool) -> Response {
    icon_button_sized(ui, icon, tip, selected, 30.0, 17.0)
}

pub fn icon_button_sized(ui: &mut Ui, icon: &str, tip: &str, selected: bool, size: f32, font: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let vis = ui.visuals();
    let hovered = resp.hovered();
    let bg = if selected {
        vis.selection.bg_fill
    } else if resp.is_pointer_button_down_on() {
        vis.widgets.active.weak_bg_fill
    } else if hovered {
        vis.widgets.hovered.weak_bg_fill
    } else {
        Color32::TRANSPARENT
    };
    let fg = if selected {
        vis.selection.stroke.color
    } else if hovered {
        vis.strong_text_color()
    } else {
        vis.text_color()
    };
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), bg);
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(font), fg);
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, CornerRadius::same(radius::CONTROL), Stroke::new(1.0, vis.text_color()), StrokeKind::Inside);
        }
    }
    if !tip.is_empty() {
        resp.on_hover_text(tip)
    } else {
        resp
    }
}

/// Run `add` with button visuals filled with `fill` and `fg` text in every state
/// (hover lightens, press darkens), no border.
fn filled_button_scope<R>(ui: &mut Ui, fill: Color32, fg: Color32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope(|ui| {
        let lift = if theme::text_on(fill) == Color32::WHITE { Color32::WHITE } else { Color32::BLACK };
        let w = &mut ui.visuals_mut().widgets;
        for (v, t) in [(&mut w.inactive, 0.0), (&mut w.hovered, 0.10), (&mut w.active, 0.18)] {
            let c = mix(fill, lift, t);
            v.bg_fill = c;
            v.weak_bg_fill = c;
            v.bg_stroke = Stroke::NONE;
            v.fg_stroke = Stroke::new(1.0, fg);
        }
        add(ui)
    })
    .inner
}

fn button_text(text: &str) -> RichText {
    RichText::new(text).font(text::body())
}

/// The one filled accent button of a surface (Apply, Convert, Print, OK...).
/// Place it last (right-most) in a row of dialog buttons; see [`dialog_buttons`].
pub fn primary_button(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    primary_button_enabled(ui, p, true, text)
}

/// [`primary_button`] that can be disabled (drawn faded, not clickable).
pub fn primary_button_enabled(ui: &mut Ui, p: &Palette, enabled: bool, text: &str) -> Response {
    filled_button_scope(ui, p.accent_fill, p.on_accent, |ui| ui.add_enabled(enabled, egui::Button::new(button_text(text)).min_size(Vec2::new(88.0, size::CONTROL))))
}

/// Filled red button for destructive confirmations (Delete, Discard, Overwrite).
pub fn danger_button(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    filled_button_scope(ui, p.danger_fill, Color32::WHITE, |ui| ui.add(egui::Button::new(button_text(text)).min_size(Vec2::new(88.0, size::CONTROL))))
}

/// Filled green button for a "keep" call to action (one per surface).
#[allow(dead_code)] // Used by the compare surface.
pub fn success_button(ui: &mut Ui, p: &Palette, text: &str) -> Response {
    filled_button_scope(ui, p.success_fill, Color32::WHITE, |ui| ui.add(egui::Button::new(button_text(text)).min_size(Vec2::new(0.0, size::CONTROL))))
}

/// Standard bordered button at the standard control height (Cancel, Close, Back...).
pub fn secondary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(button_text(text)).min_size(Vec2::new(80.0, size::CONTROL)))
}

/// What a dialog button does: decides its fill.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonKind {
    /// Filled accent.
    Primary,
    /// Filled red, for destructive actions.
    Danger,
}

/// Right-aligned `[cancel] [confirm]` buttons, confirm last, with the standard gap
/// above. Returns `Some(true)` when confirm was clicked, `Some(false)` for cancel.
pub fn dialog_buttons(ui: &mut Ui, p: &Palette, cancel: &str, confirm: &str, kind: ButtonKind, enabled: bool) -> Option<bool> {
    let mut out = None;
    ui.add_space(theme::space::BUTTONS_GAP);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let r = match kind {
            ButtonKind::Primary => primary_button_enabled(ui, p, enabled, confirm),
            ButtonKind::Danger => ui.add_enabled_ui(enabled, |ui| danger_button(ui, p, confirm)).inner,
        };
        if r.clicked() {
            out = Some(true);
        }
        if secondary_button(ui, cancel).clicked() {
            out = Some(false);
        }
    });
    out
}

pub fn separator_v(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 22.0), Sense::hover());
    let c = ui.visuals().widgets.noninteractive.bg_stroke.color;
    ui.painter().line_segment([rect.center_top(), rect.center_bottom()], Stroke::new(1.0, c));
}

/// Section label above a card or group: 11 pt semibold, sentence case, muted.
/// Pass the text in sentence case ("Viewer & thumbnails").
pub fn section_title(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).font(text::caption()).color(p.muted));
    ui.add_space(2.0);
}

/// One key / value row (info panels): 96 pt muted key, 12.5 pt value that wraps,
/// at least 20 pt tall with 2 pt between rows. (The viewer's info panel uses a
/// grid of these rows instead; kept for one-off key / value lines.)
#[allow(dead_code)]
pub fn kv_row(ui: &mut Ui, p: &Palette, key: &str, value: &str) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 20.0;
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(size::LABEL_W, 20.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(size::LABEL_W);
                ui.add(egui::Label::new(RichText::new(key).color(p.muted).size(text::SMALL)).truncate());
            });
            ui.add(egui::Label::new(RichText::new(value).size(12.5)).wrap());
        });
    });
}

/// Five clickable stars. Returns a new rating if clicked.
pub fn star_rating(ui: &mut Ui, rating: u8, size: f32, p: &Palette) -> Option<u8> {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 1.0;
        for i in 1..=5u8 {
            let filled = i <= rating;
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size + 4.0), Sense::click());
            let color = if filled { Color32::from_rgb(250, 196, 60) } else if resp.hovered() { p.muted } else { p.faint };
            let font = if filled { super::theme::icon_fill(size) } else { FontId::proportional(size) };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, super::icons::STAR, font, color);
            if resp.clicked() {
                out = Some(if rating == i { 0 } else { i });
            }
        }
    });
    out
}

// ====================================================================== slider

/// Pixel geometry shared by painting and pointer mapping.
struct Rail {
    x0: f32,
    x1: f32,
    min: f64,
    max: f64,
    log: bool,
}

impl Rail {
    /// Log scale offset for ranges that start at 0 (so 0 is still reachable).
    fn log_c(&self) -> f64 {
        if self.min > 0.0 { 0.0 } else { (self.max.abs() * 1e-3).max(1e-6) }
    }
    fn t_of(&self, v: f64) -> f32 {
        if self.max <= self.min {
            return 0.0;
        }
        let v = v.clamp(self.min, self.max);
        let t = if self.log && self.min >= 0.0 {
            let c = self.log_c();
            ((v + c) / (self.min + c)).ln() / ((self.max + c) / (self.min + c)).ln()
        } else {
            (v - self.min) / (self.max - self.min)
        };
        t.clamp(0.0, 1.0) as f32
    }
    fn v_of(&self, t: f32) -> f64 {
        let t = t.clamp(0.0, 1.0) as f64;
        if self.log && self.min >= 0.0 {
            let c = self.log_c();
            (self.min + c) * ((self.max + c) / (self.min + c)).powf(t) - c
        } else {
            self.min + t * (self.max - self.min)
        }
    }
    fn x_of(&self, v: f64) -> f32 {
        egui::lerp(self.x0..=self.x1, self.t_of(v))
    }
}

type Formatter<'a> = Box<dyn 'a + Fn(f64, RangeInclusive<usize>) -> String>;
type Parser<'a> = Box<dyn 'a + Fn(&str) -> Option<f64>>;

/// Lumen's slider: 4 pt rail, round 14 pt knob, accent fill, and a fixed-width
/// value box ([`value_box`]) on the right.
///
/// A drop-in replacement for `egui::Slider` (same builder names: `suffix`, `prefix`,
/// `step_by`, `fixed_decimals`, `logarithmic`, `custom_formatter`, `custom_parser`,
/// `show_value`, `clamping`), plus:
/// - [`Slider::default_value`]: double-click the rail to reset to it.
/// - [`Slider::bipolar`] / [`Slider::origin`]: fill from a centre value (0 for
///   Exposure, Contrast, Hue...) with a centre tick, instead of from the left.
/// - [`Slider::width`]: rail width (default `spacing.slider_width`).
///
/// ```ignore
/// ui.add(widgets::Slider::new(&mut exposure, -5.0..=5.0).bipolar().default_value(0.0).fixed_decimals(2).suffix(" EV"));
/// ```
/// Keyboard: Tab focuses it, arrows / Page Up / Page Down / Home / End change the value.
/// Values are always clamped to the range.
pub struct Slider<'a, N: Numeric> {
    value: &'a mut N,
    range: RangeInclusive<N>,
    default: Option<f64>,
    origin: Option<f64>,
    step: Option<f64>,
    decimals: Option<usize>,
    log: bool,
    prefix: String,
    suffix: String,
    show_value: bool,
    width: Option<f32>,
    value_width: f32,
    formatter: Option<Formatter<'a>>,
    parser: Option<Parser<'a>>,
}

impl<'a, N: Numeric> Slider<'a, N> {
    pub fn new(value: &'a mut N, range: RangeInclusive<N>) -> Self {
        Slider {
            value,
            range,
            default: None,
            origin: None,
            step: None,
            decimals: None,
            log: false,
            prefix: String::new(),
            suffix: String::new(),
            show_value: true,
            width: None,
            value_width: size::VALUE_W,
            formatter: None,
            parser: None,
        }
    }
    /// Double-clicking the slider resets it to `v`.
    pub fn default_value(mut self, v: N) -> Self {
        self.default = Some(v.to_f64());
        self
    }
    /// Signed parameter: fill from 0 (or the middle of the range when 0 is outside it).
    pub fn bipolar(mut self) -> Self {
        let (a, b) = (self.range.start().to_f64(), self.range.end().to_f64());
        self.origin = Some(if a <= 0.0 && 0.0 <= b { 0.0 } else { (a + b) / 2.0 });
        self
    }
    /// Fill from `v` (drawn with a centre tick) instead of from the left end.
    pub fn origin(mut self, v: N) -> Self {
        self.origin = Some(v.to_f64());
        self
    }
    pub fn step_by(mut self, step: f64) -> Self {
        self.step = (step > 0.0).then_some(step);
        self
    }
    pub fn fixed_decimals(mut self, n: usize) -> Self {
        self.decimals = Some(n);
        self
    }
    pub fn logarithmic(mut self, log: bool) -> Self {
        self.log = log;
        self
    }
    #[allow(dead_code)] // egui::Slider compatibility.
    pub fn prefix(mut self, s: impl ToString) -> Self {
        self.prefix = s.to_string();
        self
    }
    pub fn suffix(mut self, s: impl ToString) -> Self {
        self.suffix = s.to_string();
        self
    }
    #[allow(dead_code)] // egui::Slider compatibility.
    pub fn show_value(mut self, show: bool) -> Self {
        self.show_value = show;
        self
    }
    /// Rail width in points (the value box comes on top of it).
    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }
    /// Width of the value box (default 64 pt).
    pub fn value_width(mut self, w: f32) -> Self {
        self.value_width = w;
        self
    }
    /// Accepted for `egui::Slider` compatibility; this slider always clamps.
    #[allow(dead_code)] // egui::Slider compatibility.
    pub fn clamping(self, _c: egui::SliderClamping) -> Self {
        self
    }
    pub fn custom_formatter(mut self, f: impl 'a + Fn(f64, RangeInclusive<usize>) -> String) -> Self {
        self.formatter = Some(Box::new(f));
        self
    }
    pub fn custom_parser(mut self, f: impl 'a + Fn(&str) -> Option<f64>) -> Self {
        self.parser = Some(Box::new(f));
        self
    }

    fn round(&self, v: f64, rail: &Rail, t: f32) -> f64 {
        let v = if let Some(step) = self.step {
            rail.min + ((v - rail.min) / step).round() * step
        } else if let Some(d) = self.decimals {
            let m = 10f64.powi(d as i32);
            (v * m).round() / m
        } else if N::INTEGRAL {
            v.round()
        } else {
            // Round to what one point of rail can resolve, so dragging gives tidy numbers.
            let px = (rail.x1 - rail.x0).max(1.0);
            let dv = (rail.v_of(t + 1.0 / px) - rail.v_of(t)).abs().max(1e-9);
            let d = (-dv.log10()).ceil().clamp(0.0, 6.0) as i32;
            let m = 10f64.powi(d);
            (v * m).round() / m
        };
        let v = if N::INTEGRAL { v.round() } else { v };
        v.clamp(rail.min, rail.max)
    }

    fn key_step(&self, rail: &Rail) -> f64 {
        self.step.unwrap_or(if N::INTEGRAL { 1.0 } else if let Some(d) = self.decimals { 10f64.powi(-(d as i32)).max((rail.max - rail.min) / 100.0) } else { (rail.max - rail.min) / 100.0 })
    }
}

impl<N: Numeric> egui::Widget for Slider<'_, N> {
    fn ui(self, ui: &mut Ui) -> Response {
        let p = theme::palette(ui.ctx());
        let (min, max) = (self.range.start().to_f64(), self.range.end().to_f64());
        let (min, max) = (min.min(max), min.max(max));
        // Without an explicit width the rail takes `slider_width`, shrunk so the rail
        // and its value box never run past the available width.
        let value_room = if self.show_value { self.value_width + 4.0 + ui.spacing().item_spacing.x } else { 0.0 };
        let rail_w = self.width.unwrap_or_else(|| ui.spacing().slider_width.min(ui.available_width() - value_room)).max(40.0);
        let h = ui.spacing().interact_size.y.max(20.0);
        let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(rail_w, h), Sense::click_and_drag());
        const KNOB_R: f32 = 7.0;
        let rail = Rail { x0: rect.left() + KNOB_R, x1: rect.right() - KNOB_R, min, max, log: self.log };
        let old = self.value.to_f64();
        let mut v = old;

        if resp.double_clicked() {
            if let Some(d) = self.default {
                v = d;
            }
        } else if let Some(pos) = resp.interact_pointer_pos() {
            if resp.is_pointer_button_down_on() || resp.dragged() {
                let t = (pos.x - rail.x0) / (rail.x1 - rail.x0).max(1.0);
                v = self.round(rail.v_of(t), &rail, t);
            }
        }
        if resp.has_focus() {
            ui.memory_mut(|m| m.set_focus_lock_filter(resp.id, egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() }));
            let step = self.key_step(&rail);
            let (dec, inc, pg_dn, pg_up, home, end) = ui.input(|i| {
                (
                    i.num_presses(egui::Key::ArrowLeft) + i.num_presses(egui::Key::ArrowDown),
                    i.num_presses(egui::Key::ArrowRight) + i.num_presses(egui::Key::ArrowUp),
                    i.num_presses(egui::Key::PageDown),
                    i.num_presses(egui::Key::PageUp),
                    i.key_pressed(egui::Key::Home),
                    i.key_pressed(egui::Key::End),
                )
            });
            let delta = (inc as f64 - dec as f64) * step + (pg_up as f64 - pg_dn as f64) * step * 10.0;
            if delta != 0.0 {
                v = (v + delta).clamp(min, max);
                if N::INTEGRAL {
                    v = v.round();
                }
            }
            if home {
                v = min;
            }
            if end {
                v = max;
            }
        }
        if v != old {
            *self.value = N::from_f64(v);
            resp.mark_changed();
        }
        let shown = self.value.to_f64();
        resp.widget_info(|| egui::WidgetInfo::slider(ui.is_enabled(), shown, ""));

        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            let cy = rect.center().y;
            let rail_rect = Rect::from_min_max(egui::pos2(rect.left(), cy - 2.0), egui::pos2(rect.right(), cy + 2.0));
            let track = if p.dark { mix(p.border, p.muted, 0.18) } else { p.border };
            painter.rect_filled(rail_rect, CornerRadius::same(2), track);
            let fill = if p.dark { p.accent } else { p.accent_fill };
            let kx = rail.x_of(shown);
            let from = match self.origin {
                Some(o) => rail.x_of(o),
                None => rail_rect.left(),
            };
            let (a, b) = (from.min(kx), from.max(kx));
            if b - a > 0.5 {
                painter.rect_filled(Rect::from_min_max(egui::pos2(a, rail_rect.top()), egui::pos2(b, rail_rect.bottom())), CornerRadius::same(2), fill);
            }
            if self.origin.is_some() {
                painter.line_segment([egui::pos2(from, cy - 5.0), egui::pos2(from, cy + 5.0)], Stroke::new(1.0, p.muted));
            }
            let hot = resp.hovered() || resp.dragged() || resp.has_focus();
            let r = KNOB_R + ui.ctx().animate_bool_with_time(resp.id.with("knob"), hot, 0.1) * 1.0;
            let c = egui::pos2(kx, cy);
            let knob = Rect::from_center_size(c, Vec2::splat(2.0 * r));
            let shadow = egui::Shadow { offset: [0, 1], blur: 4, spread: 0, color: Color32::from_black_alpha(if p.dark { 110 } else { 50 }) };
            painter.add(shadow.as_shape(knob, CornerRadius::same(r.round() as u8)));
            painter.circle(c, r, Color32::WHITE, Stroke::new(1.0, if p.dark { Color32::from_black_alpha(60) } else { p.border }));
            if resp.has_focus() {
                painter.circle_stroke(c, r + 2.5, Stroke::new(1.5, p.accent));
            }
        }
        let mut resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

        if self.show_value {
            ui.add_space(4.0);
            let speed = if N::INTEGRAL { ((max - min) / 300.0).max(0.25) } else { (max - min) / 300.0 };
            let mut dv = egui::DragValue::new(self.value).range(self.range.clone()).speed(speed).prefix(self.prefix).suffix(self.suffix);
            if let Some(d) = self.decimals {
                dv = dv.fixed_decimals(d);
            }
            if let Some(f) = self.formatter {
                dv = dv.custom_formatter(move |v, r| f(v, r));
            }
            if let Some(f) = self.parser {
                dv = dv.custom_parser(move |s| f(s));
            }
            let vr = value_box_w(ui, self.value_width, dv);
            if vr.changed() {
                resp.mark_changed();
            }
            resp = resp.union(vr);
        }
        resp
    }
}

/// A number field: fixed 64 x 24 pt box, value right-aligned, in the text-field
/// look. Wrap any `egui::DragValue`:
/// `widgets::value_box(ui, egui::DragValue::new(&mut w).range(1..=60000).suffix(" px"))`.
pub fn value_box(ui: &mut Ui, dv: egui::DragValue<'_>) -> Response {
    value_box_w(ui, size::VALUE_W, dv)
}

/// [`value_box`] with a custom width (e.g. 80 for "60000 px").
pub fn value_box_w(ui: &mut Ui, w: f32, dv: egui::DragValue<'_>) -> Response {
    let h = size::COMPACT;
    ui.scope(|ui| {
        let bg = ui.visuals().text_edit_bg_color();
        let border = ui.visuals().widgets.inactive.bg_stroke;
        let hover_border = ui.visuals().widgets.hovered.bg_stroke;
        let wv = &mut ui.visuals_mut().widgets;
        for v in [&mut wv.inactive, &mut wv.hovered, &mut wv.active] {
            v.weak_bg_fill = bg;
            v.bg_fill = bg;
            v.expansion = 0.0;
        }
        wv.inactive.bg_stroke = border;
        wv.hovered.bg_stroke = hover_border;
        ui.spacing_mut().button_padding.x = 6.0;
        // The layout's main alignment right-aligns the number inside the box.
        ui.allocate_ui_with_layout(Vec2::new(w, h), Layout::right_to_left(Align::Center).with_main_align(Align::Max), |ui| ui.add(dv.min_size(Vec2::new(w, h)))).inner
    })
    .inner
}

// ====================================================================== checkbox / radio

fn choice_layout(ui: &mut Ui, text: WidgetText) -> (Rect, Response, Option<std::sync::Arc<egui::Galley>>) {
    let icon = 16.0;
    let gap = 8.0;
    let wrap_w = (ui.available_width() - icon - gap).max(40.0);
    let galley = (!text.is_empty()).then(|| text.into_galley(ui, None, wrap_w, egui::TextStyle::Body));
    let text_size = galley.as_ref().map_or(Vec2::ZERO, |g| g.size());
    let w = icon + if galley.is_some() { gap + text_size.x } else { 0.0 };
    let h = ui.spacing().interact_size.y.max(text_size.y + 4.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    (rect, resp, galley)
}

fn paint_choice_text(ui: &Ui, rect: Rect, galley: Option<std::sync::Arc<egui::Galley>>, p: &Palette) {
    if let Some(g) = galley {
        let pos = egui::pos2(rect.left() + 24.0, rect.center().y - g.size().y / 2.0);
        ui.painter().galley(pos, g, p.text);
    }
}

/// Checkbox: 16 pt box, radius 4. Off = 1.5 pt muted border (text on hover);
/// on = accent fill with a 2 pt check in `on_accent`. Drop-in for `ui.checkbox`.
pub fn checkbox(ui: &mut Ui, checked: &mut bool, text: impl Into<WidgetText>) -> Response {
    let p = theme::palette(ui.ctx());
    let text: WidgetText = text.into();
    let label = text.text().to_string();
    let (rect, mut resp, galley) = choice_layout(ui, text);
    if resp.clicked() {
        *checked = !*checked;
        resp.mark_changed();
    }
    let on = *checked;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, ui.is_enabled(), on, &label));
    if ui.is_rect_visible(rect) {
        let b = Rect::from_center_size(egui::pos2(rect.left() + 8.0, rect.center().y), Vec2::splat(16.0));
        let painter = ui.painter();
        let cr = CornerRadius::same(radius::SMALL);
        if on {
            let fill = if resp.hovered() { mix(p.accent_fill, theme::text_on(p.accent_fill), 0.1) } else { p.accent_fill };
            painter.rect_filled(b, cr, fill);
            let s = b.shrink(4.0);
            painter.add(egui::Shape::line(
                vec![egui::pos2(s.left(), s.center().y + 0.5), egui::pos2(s.left() + s.width() * 0.38, s.bottom() - 0.5), egui::pos2(s.right(), s.top() + 1.0)],
                Stroke::new(2.0, p.on_accent),
            ));
        } else {
            let border = if resp.hovered() { p.text } else { p.muted };
            painter.rect_filled(b, cr, if resp.is_pointer_button_down_on() { p.hover } else { Color32::TRANSPARENT });
            painter.rect_stroke(b, cr, Stroke::new(1.5, border), StrokeKind::Inside);
        }
        if resp.has_focus() {
            painter.rect_stroke(b.expand(2.5), CornerRadius::same(radius::CONTROL), Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
        paint_choice_text(ui, rect, galley, &p);
    }
    resp
}

/// Radio button: 16 pt circle; selected = accent fill with a 6 pt `on_accent` dot.
/// Drop-in for `ui.radio`.
pub fn radio(ui: &mut Ui, selected: bool, text: impl Into<WidgetText>) -> Response {
    let p = theme::palette(ui.ctx());
    let text: WidgetText = text.into();
    let label = text.text().to_string();
    let (rect, resp, galley) = choice_layout(ui, text);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, ui.is_enabled(), selected, &label));
    if ui.is_rect_visible(rect) {
        let c = egui::pos2(rect.left() + 8.0, rect.center().y);
        let painter = ui.painter();
        if selected {
            let fill = if resp.hovered() { mix(p.accent_fill, theme::text_on(p.accent_fill), 0.1) } else { p.accent_fill };
            painter.circle_filled(c, 8.0, fill);
            painter.circle_filled(c, 3.0, p.on_accent);
        } else {
            let border = if resp.hovered() { p.text } else { p.muted };
            painter.circle(c, 7.25, if resp.is_pointer_button_down_on() { p.hover } else { Color32::TRANSPARENT }, Stroke::new(1.5, border));
        }
        if resp.has_focus() {
            painter.circle_stroke(c, 10.5, Stroke::new(1.5, p.accent));
        }
        paint_choice_text(ui, rect, galley, &p);
    }
    resp
}

/// Radio button that sets `*current = alt` when clicked. Drop-in for `ui.radio_value`.
pub fn radio_value<T: PartialEq>(ui: &mut Ui, current: &mut T, alt: T, text: impl Into<WidgetText>) -> Response {
    let mut resp = radio(ui, *current == alt, text);
    if resp.clicked() && *current != alt {
        *current = alt;
        resp.mark_changed();
    }
    resp
}

// ====================================================================== segmented

/// Segmented control: one 28 pt control with equal-width segments in a recessed
/// track; the selected segment is raised. Returns true when the selection changed.
/// Use a combo instead when there are more than about five options. Narrow
/// space shrinks the segments (labels clip) instead of wrapping.
pub fn segmented(ui: &mut Ui, selected: &mut usize, labels: &[&str]) -> bool {
    let p = theme::palette(ui.ctx());
    let n = labels.len().max(1);
    let font = text::body();
    let galleys: Vec<_> = labels.iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), font.clone(), p.text)).collect();
    let natural = galleys.iter().map(|g| g.size().x).fold(0.0, f32::max) + 24.0;
    let inset = 2.0;
    let avail = ui.available_width();
    let seg_w = if natural * n as f32 + 2.0 * inset > avail { ((avail - 2.0 * inset) / n as f32).max(32.0) } else { natural };
    let (rect, whole) = ui.allocate_exact_size(Vec2::new(seg_w * n as f32 + 2.0 * inset, size::CONTROL), Sense::hover());
    let id = whole.id;
    let mut changed = false;
    let mut hovered = None;
    for i in 0..n {
        let r = Rect::from_min_size(egui::pos2(rect.left() + inset + i as f32 * seg_w, rect.top() + inset), Vec2::new(seg_w, rect.height() - 2.0 * inset));
        let resp = ui.interact(r, id.with(i), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let sel = *selected == i;
        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, ui.is_enabled(), sel, labels.get(i).copied().unwrap_or("")));
        if resp.hovered() {
            hovered = Some(i);
        }
        if resp.clicked() && *selected != i {
            *selected = i;
            changed = true;
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(r, CornerRadius::same(radius::SMALL), Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
    }
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let track = if p.dark { Color32::from_black_alpha(80) } else { Color32::from_black_alpha(14) };
        painter.rect_filled(rect, CornerRadius::same(radius::CONTROL), track);
        let target = rect.left() + inset + (*selected).min(n - 1) as f32 * seg_w;
        let x = ui.ctx().animate_value_with_time(id.with("x"), target, 0.12);
        let sel_rect = Rect::from_min_size(egui::pos2(x, rect.top() + inset), Vec2::new(seg_w, rect.height() - 2.0 * inset));
        if let Some(h) = hovered.filter(|h| *h != *selected) {
            let hr = Rect::from_min_size(egui::pos2(rect.left() + inset + h as f32 * seg_w, rect.top() + inset), sel_rect.size());
            painter.rect_filled(hr, CornerRadius::same(radius::SMALL), if p.dark { Color32::from_white_alpha(8) } else { Color32::from_black_alpha(8) });
        }
        // An index past the end selects nothing (the value is set elsewhere, e.g. a
        // combo next to the control).
        if *selected < n {
            let raised = if p.dark { Color32::from_rgb(62, 66, 75) } else { Color32::WHITE };
            let sh = egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(if p.dark { 90 } else { 30 }) };
            painter.add(sh.as_shape(sel_rect, CornerRadius::same(radius::SMALL)));
            painter.rect_filled(sel_rect, CornerRadius::same(radius::SMALL), raised);
            if !p.dark {
                painter.rect_stroke(sel_rect, CornerRadius::same(radius::SMALL), Stroke::new(1.0, p.border), StrokeKind::Inside);
            }
        }
        for (i, g) in galleys.into_iter().enumerate() {
            let r = Rect::from_min_size(egui::pos2(rect.left() + inset + i as f32 * seg_w, rect.top() + inset), sel_rect.size());
            let color = if i == *selected || hovered == Some(i) { p.text } else { p.muted };
            let pos = egui::pos2((r.center().x - g.size().x / 2.0).max(r.left() + 4.0), r.center().y - g.size().y / 2.0);
            painter.with_clip_rect(r.shrink2(Vec2::new(4.0, 0.0)).intersect(ui.clip_rect())).galley(pos, g, color);
        }
    }
    changed
}

/// [`segmented`] over values: `options` pairs each value with its label.
pub fn segmented_value<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut idx = options.iter().position(|(v, _)| *v == *value).unwrap_or(usize::MAX);
    let labels: Vec<&str> = options.iter().map(|(_, l)| *l).collect();
    if segmented(ui, &mut idx, &labels) {
        if let Some((v, _)) = options.get(idx) {
            *value = *v;
            return true;
        }
    }
    false
}

// ====================================================================== carets

/// Paint a Phosphor caret glyph centred on `center`, rotated by `angle` radians
/// (0 = pointing right).
fn paint_caret(painter: &egui::Painter, center: Pos2, angle: f32, size: f32, color: Color32) {
    let galley = painter.layout_no_wrap(I::CARET_RIGHT.to_string(), FontId::proportional(size), color);
    let half = galley.size() / 2.0;
    let (s, c) = angle.sin_cos();
    let rotated = Vec2::new(half.x * c - half.y * s, half.x * s + half.y * c);
    painter.add(egui::epaint::TextShape::new(center - rotated, galley, color).with_angle(angle));
}

/// `ComboBox::icon` painter: a 12 pt muted caret instead of egui's filled triangle.
/// Usually reached through [`combo`].
pub fn combo_caret(ui: &Ui, rect: Rect, visuals: &egui::style::WidgetVisuals, _open: bool) {
    let p = theme::palette(ui.ctx());
    let color = if visuals.fg_stroke.width > 1.0 { p.text } else { p.muted };
    paint_caret(ui.painter(), rect.center(), std::f32::consts::FRAC_PI_2, size::ICON_CARET, color);
}

/// `ComboBox::from_id_salt(id)` with Lumen's caret. Use it for every combo box.
pub fn combo(id_salt: impl egui::AsIdSalt) -> egui::ComboBox<'static> {
    egui::ComboBox::from_id_salt(id_salt).icon(combo_caret)
}

/// `CollapsingHeader::icon` painter: a 12 pt muted caret that rotates from right
/// to down as the header opens.
/// `egui::CollapsingHeader::new("Advanced").icon(widgets::collapse_caret)`.
#[allow(dead_code)] // For the analysis panel and other collapsing headers.
pub fn collapse_caret(ui: &mut Ui, openness: f32, response: &Response) {
    let p = theme::palette(ui.ctx());
    let color = if response.hovered() { p.text } else { p.muted };
    paint_caret(ui.painter(), response.rect.center(), openness * std::f32::consts::FRAC_PI_2, size::ICON_CARET, color);
}

// ====================================================================== menus

fn menu_row(ui: &mut Ui, icon: &str, icon_color: Option<Color32>, label: &str, right: &str) -> Response {
    let p = theme::palette(ui.ctx());
    let label_g = ui.painter().layout_no_wrap(label.to_string(), text::body(), p.text);
    let right_g = (!right.is_empty()).then(|| ui.painter().layout_no_wrap(right.to_string(), text::small(), p.faint));
    let right_w = right_g.as_ref().map_or(0.0, |g| g.size().x + 24.0);
    let want = 32.0 + label_g.size().x + right_w + 12.0;
    // Like a button in a justified menu: the menu grows to its widest row and every
    // row then fills that width.
    let resp = ui.allocate_response(Vec2::new(want, size::MENU_ROW), Sense::click());
    let rect = resp.rect;
    if ui.is_rect_visible(rect) {
        let hot = resp.hovered() || resp.has_focus();
        let painter = ui.painter();
        if hot {
            painter.rect_filled(rect, CornerRadius::same(radius::CONTROL), p.hover);
        }
        let enabled = ui.is_enabled();
        let ic = icon_color.unwrap_or(if hot && enabled { p.text } else { p.muted });
        if !icon.is_empty() {
            painter.text(egui::pos2(rect.left() + 16.0, rect.center().y), Align2::CENTER_CENTER, icon, FontId::proportional(size::ICON), ic);
        }
        painter.galley(egui::pos2(rect.left() + 32.0, rect.center().y - label_g.size().y / 2.0), label_g, p.text);
        let x = rect.right() - 10.0;
        if let Some(g) = right_g {
            painter.galley(egui::pos2(x - g.size().x, rect.center().y - g.size().y / 2.0), g, p.faint);
        }
    }
    resp
}

/// A 28 pt menu row: 14 pt icon (centred), label, shortcut right-aligned in faint
/// 11.5 pt. Close the menu yourself on click (`ui.close()`).
pub fn menu_item(ui: &mut Ui, icon: &str, label: &str, shortcut: &str) -> Response {
    menu_row(ui, icon, None, label, shortcut)
}

/// A menu row for an on/off setting: shows an accent check mark in the icon column
/// while on (instead of "Toggle ..." labels).
pub fn menu_toggle(ui: &mut Ui, on: bool, label: &str, shortcut: &str) -> Response {
    let p = theme::palette(ui.ctx());
    menu_row(ui, if on { I::CHECK } else { "" }, Some(p.accent_text), label, shortcut)
}

/// A menu row that opens a submenu (caret on the right), aligned with [`menu_item`].
pub fn submenu<R>(ui: &mut Ui, icon: &str, label: &str, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    use egui::AtomExt as _;
    let p = theme::palette(ui.ctx());
    // 9 pt padding + a 14 pt icon + a 9 pt gap puts the icon centre at 16 and the
    // label at 32, as in `menu_row`.
    let icon = RichText::new(icon).size(size::ICON).color(p.muted).atom_size(Vec2::new(14.0, 20.0));
    let caret = RichText::new(I::CARET_RIGHT).size(size::ICON_CARET).color(p.muted);
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = Vec2::new(9.0, 0.0);
        let button = egui::Button::new((icon, RichText::new(label), egui::Atom::grow(), caret)).gap(9.0).min_size(Vec2::new(0.0, size::MENU_ROW));
        egui::containers::menu::SubMenuButton::from_button(button).ui(ui, add).1.map(|r| r.inner)
    })
    .inner
}

// ====================================================================== keycaps

fn split_shortcut(s: &str) -> Vec<Vec<String>> {
    s.split(" / ")
        .filter(|g| !g.trim().is_empty())
        .map(|g| {
            let mut keys = Vec::new();
            let mut rest = g.trim();
            for m in ["Ctrl+", "Alt+", "Shift+"] {
                if let Some(r) = rest.strip_prefix(m) {
                    keys.push(m.trim_end_matches('+').to_string());
                    rest = r;
                }
            }
            if !rest.is_empty() {
                keys.push(rest.to_string());
            }
            keys
        })
        .collect()
}

fn keycap_colors(p: &Palette) -> (Color32, Color32) {
    (if p.dark { mix(p.raised, p.text, 0.07) } else { p.faint_bg }, p.border)
}

/// One keycap chip ("Ctrl", "K"): 20 pt tall, 11.5 pt semibold muted text,
/// raised fill, 1 pt border, radius 4.
pub fn keycap(ui: &mut Ui, p: &Palette, key: &str) -> Response {
    let g = ui.painter().layout_no_wrap(key.to_string(), theme::semibold(text::SMALL), p.muted);
    let size = Vec2::new((g.size().x + 12.0).max(20.0), 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let (fill, border) = keycap_colors(p);
        ui.painter().rect(rect, CornerRadius::same(radius::SMALL), fill, Stroke::new(1.0, border), StrokeKind::Inside);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, p.muted);
    }
    resp
}

/// A shortcut as keycaps: "Ctrl+Shift+K / F2" becomes [Ctrl]+[Shift]+[K] or [F2].
pub fn shortcut_keys(ui: &mut Ui, p: &Palette, shortcut: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        for (gi, group) in split_shortcut(shortcut).iter().enumerate() {
            if gi > 0 {
                ui.label(RichText::new("or").size(text::SMALL).color(p.faint));
            }
            for (ki, k) in group.iter().enumerate() {
                if ki > 0 {
                    ui.label(RichText::new("+").size(text::SMALL).color(p.faint));
                }
                keycap(ui, p, k);
            }
        }
    });
}

/// Width [`paint_shortcut_keys`] needs for `shortcut`.
pub fn shortcut_keys_width(painter: &egui::Painter, shortcut: &str) -> f32 {
    let w = |t: &str, f: FontId| painter.layout_no_wrap(t.to_string(), f, Color32::WHITE).size().x;
    let mut total = 0.0;
    for (gi, group) in split_shortcut(shortcut).iter().enumerate() {
        if gi > 0 {
            total += 12.0 + w("or", FontId::proportional(text::SMALL));
        }
        for (ki, k) in group.iter().enumerate() {
            if ki > 0 {
                total += 6.0 + w("+", FontId::proportional(text::SMALL));
            }
            total += (w(k, theme::semibold(text::SMALL)) + 12.0).max(20.0);
        }
    }
    total
}

/// Paint `shortcut` as keycaps ending at `right_center` (for custom-painted rows such
/// as the command palette). Returns the width used.
pub fn paint_shortcut_keys(painter: &egui::Painter, p: &Palette, right_center: Pos2, shortcut: &str) -> f32 {
    let (fill, border) = keycap_colors(p);
    let mut x = right_center.x;
    let groups = split_shortcut(shortcut);
    let gap = 3.0;
    for (gi, group) in groups.iter().enumerate().rev() {
        for (ki, k) in group.iter().enumerate().rev() {
            let g = painter.layout_no_wrap(k.clone(), theme::semibold(text::SMALL), p.muted);
            let w = (g.size().x + 12.0).max(20.0);
            let rect = Rect::from_min_size(egui::pos2(x - w, right_center.y - 10.0), Vec2::new(w, 20.0));
            painter.rect(rect, CornerRadius::same(radius::SMALL), fill, Stroke::new(1.0, border), StrokeKind::Inside);
            painter.galley(rect.center() - g.size() / 2.0, g, p.muted);
            x -= w;
            if ki > 0 {
                let plus = painter.layout_no_wrap("+".into(), FontId::proportional(text::SMALL), p.faint);
                x -= gap;
                painter.galley(egui::pos2(x - plus.size().x, right_center.y - plus.size().y / 2.0), plus.clone(), p.faint);
                x -= plus.size().x + gap;
            }
        }
        if gi > 0 {
            let or = painter.layout_no_wrap("or".into(), FontId::proportional(text::SMALL), p.faint);
            x -= 6.0;
            painter.galley(egui::pos2(x - or.size().x, right_center.y - or.size().y / 2.0), or.clone(), p.faint);
            x -= or.size().x + 6.0;
        }
    }
    right_center.x - x
}

// ====================================================================== windows

/// Builder for a tool / settings window with Lumen's chrome: a 40 pt title row
/// (left-aligned 15 pt semibold title, 28 pt close button), no title band, radius 12,
/// the window shadow, a 20 pt body margin, sized to at most 960 x 720 (and 64 pt
/// less than the app window) and centred when it opens. Drag the title row to move it.
///
/// ```ignore
/// widgets::app_window("batch-convert", "Batch convert & resize")
///     .open(&mut open)
///     .default_size(780.0, 680.0)
///     .min_size(520.0, 360.0)
///     .show(ctx, |ui| { ... });
/// ```
/// Resizable windows fill their body (use `batch::window_body` for an exact-size
/// body with a pinned footer); fixed-width windows set [`AppWindow::width`].
pub struct AppWindow<'a> {
    id: Id,
    title: String,
    open: Option<&'a mut bool>,
    default_size: Option<Vec2>,
    min_size: Option<Vec2>,
    fixed_size: Option<Vec2>,
    width: Option<f32>,
    resizable: bool,
    anchored: bool,
    body_margin: Margin,
    fill: Option<Color32>,
}

/// Start an [`AppWindow`]. `id_salt` must be stable; `title` is plain text (no icon).
pub fn app_window<'a>(id_salt: impl std::hash::Hash + std::fmt::Debug, title: impl Into<String>) -> AppWindow<'a> {
    AppWindow {
        id: Id::unique(("app-window", id_salt)),
        title: title.into(),
        open: None,
        default_size: None,
        min_size: None,
        fixed_size: None,
        width: None,
        resizable: false,
        anchored: false,
        body_margin: Margin { left: theme::space::WINDOW, right: theme::space::WINDOW, top: 2, bottom: 16 },
        fill: None,
    }
}

impl<'a> AppWindow<'a> {
    /// The close button sets this to false.
    pub fn open(mut self, open: &'a mut bool) -> Self {
        self.open = Some(open);
        self
    }
    /// Resizable window with this initial size (capped to the screen).
    pub fn default_size(mut self, w: f32, h: f32) -> Self {
        self.default_size = Some(Vec2::new(w, h));
        self.resizable = true;
        self
    }
    pub fn min_size(mut self, w: f32, h: f32) -> Self {
        self.min_size = Some(Vec2::new(w, h));
        self
    }
    /// Exact size every frame (still capped to the screen); not resizable.
    pub fn fixed_size(mut self, size: Vec2) -> Self {
        self.fixed_size = Some(size);
        self.resizable = false;
        self
    }
    /// Auto-height window of this width (About, dialogs-as-windows).
    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }
    /// Keep the window centred on the app window (not movable).
    pub fn anchored(mut self) -> Self {
        self.anchored = true;
        self
    }
    /// Margin around the body (default 20 left / right, 16 bottom).
    pub fn body_margin(mut self, m: Margin) -> Self {
        self.body_margin = m;
        self
    }

    /// Window background (default `raised`; Settings uses `panel` behind its list).
    pub fn fill(mut self, fill: Color32) -> Self {
        self.fill = Some(fill);
        self
    }

    /// The largest size a window may take on the current screen.
    pub fn cap(ctx: &egui::Context) -> Vec2 {
        let s = ctx.content_rect().size();
        Vec2::new((s.x - 64.0).min(960.0).max(240.0), (s.y - 64.0).min(720.0).max(160.0))
    }

    pub fn show<R>(self, ctx: &egui::Context, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
        let p = theme::palette(ctx);
        let screen = ctx.content_rect();
        let cap = Self::cap(ctx);
        let pos_id = self.id.with("pos");
        let pass = ctx.cumulative_pass_nr();
        // (top-left, pass it was drawn in); a gap in passes means the window was reopened.
        let stored: Option<(Pos2, u64)> = ctx.data(|d| d.get_temp(pos_id));
        let stored = stored.filter(|(_, last)| last + 1 >= pass).map(|(pos, _)| pos);
        let frame = egui::Frame::new()
            .fill(self.fill.unwrap_or(p.raised))
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(radius::WINDOW))
            .shadow(theme::window_shadow(&p))
            .inner_margin(Margin::ZERO);
        let mut win = egui::Window::new(self.title.as_str()).id(self.id).title_bar(false).collapsible(false).frame(frame).constrain(true).scroll(false);
        if let Some(open) = self.open.as_deref() {
            if !*open {
                return None;
            }
        }
        if let Some(fs) = self.fixed_size {
            win = win.fixed_size(fs.min(cap)).resizable(false);
        } else if let Some(ds) = self.default_size {
            let min = self.min_size.unwrap_or(Vec2::new(320.0, 240.0)).min(cap);
            win = win.default_size(ds.min(cap)).min_size(min).max_size(screen.size() - Vec2::splat(32.0)).resizable(true);
        } else {
            win = win.resizable(false);
            if let Some(w) = self.width {
                win = win.default_width(w.min(cap.x));
            }
        }
        if self.anchored {
            win = win.anchor(Align2::CENTER_CENTER, Vec2::ZERO).movable(false);
        } else {
            win = win.movable(false);
            win = match stored {
                Some(pos) => win.pivot(Align2::LEFT_TOP).current_pos(pos),
                None => win.pivot(Align2::CENTER_CENTER).current_pos(screen.center()),
            };
        }
        let mut close = false;
        let mut drag = Vec2::ZERO;
        let title = self.title.clone();
        let fixed_w = if self.resizable || self.fixed_size.is_some() { None } else { self.width.map(|w| w.min(cap.x)) };
        let body_margin = self.body_margin;
        let anchored = self.anchored;
        let r = win.show(ctx, |ui| {
            if let Some(w) = fixed_w {
                ui.set_width(w);
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            let w = ui.available_width();
            let (head, head_resp) = ui.allocate_exact_size(Vec2::new(w, 40.0), if anchored { Sense::hover() } else { Sense::drag() });
            if head_resp.dragged() {
                drag = head_resp.drag_delta();
            }
            let close_rect = Rect::from_center_size(egui::pos2(head.right() - 8.0 - 14.0, head.center().y + 2.0), Vec2::splat(28.0));
            let close_resp = ui.interact(close_rect, ui.scope_id().with("close"), Sense::click()).on_hover_text("Close");
            close_resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Close"));
            if close_resp.clicked() {
                close = true;
            }
            let painter = ui.painter();
            let title_clip = Rect::from_min_max(head.min, egui::pos2(close_rect.left() - 8.0, head.max.y));
            painter.with_clip_rect(title_clip.intersect(ui.clip_rect())).text(
                egui::pos2(head.left() + theme::space::WINDOW as f32, head.center().y + 2.0),
                Align2::LEFT_CENTER,
                &title,
                text::title(),
                p.text,
            );
            let hot = close_resp.hovered();
            if hot {
                painter.rect_filled(close_rect, CornerRadius::same(radius::CONTROL), p.hover);
            }
            painter.text(close_rect.center(), Align2::CENTER_CENTER, I::X, FontId::proportional(16.0), if hot { p.text } else { p.muted });
            ui.spacing_mut().item_spacing.y = 4.0;
            egui::Frame::new().inner_margin(body_margin).show(ui, |ui| add(ui)).inner
        });
        if let Some(r) = &r {
            ctx.data_mut(|d| d.insert_temp(pos_id, (r.response.rect.min + drag, pass)));
        }
        if close {
            if let Some(open) = self.open {
                *open = false;
            }
        }
        r.and_then(|r| r.inner)
    }
}

/// Modal dialog: scrim behind it, raised card with radius 12, the window shadow
/// and a 20 pt margin, `width` points wide. Build the content with [`modal_title`],
/// [`modal_body`] and [`dialog_buttons`]. Esc / Enter handling is up to the caller.
pub fn modal<R>(ctx: &egui::Context, id: Id, width: f32, add: impl FnOnce(&mut Ui) -> R) -> egui::ModalResponse<R> {
    let p = theme::palette(ctx);
    let frame = egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(radius::WINDOW))
        .shadow(theme::window_shadow(&p))
        .inner_margin(Margin::same(theme::space::WINDOW));
    let w = width.min(ctx.content_rect().width() - 48.0).max(200.0);
    egui::Modal::new(id).frame(frame).backdrop_color(theme::scrim(&p)).show(ctx, |ui| {
        ui.set_width(w);
        add(ui)
    })
}

/// Dialog title: 15 pt semibold, then an 8 pt gap.
pub fn modal_title(ui: &mut Ui, title: &str) {
    let p = theme::palette(ui.ctx());
    ui.add(egui::Label::new(RichText::new(title).font(text::title()).color(p.text)).wrap());
    ui.add_space(8.0);
}

/// Dialog body text: 13 pt muted, wrapping.
pub fn modal_body(ui: &mut Ui, text: &str) {
    let p = theme::palette(ui.ctx());
    ui.add(egui::Label::new(RichText::new(text).color(p.muted)).wrap());
}

// ====================================================================== toasts

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub until: Instant,
    born: Instant,
}

/// Bottom-centre notifications. Use `success` for completed actions ("Path copied"),
/// `info` for neutral news and `error` for failures (shown longer).
#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub fn info(&mut self, t: impl Into<String>) {
        self.push(t.into(), ToastKind::Info, 2.5);
    }
    pub fn success(&mut self, t: impl Into<String>) {
        self.push(t.into(), ToastKind::Success, 2.5);
    }
    pub fn error(&mut self, t: impl Into<String>) {
        self.push(t.into(), ToastKind::Error, 6.0);
    }
    fn push(&mut self, text: String, kind: ToastKind, secs: f32) {
        self.items.retain(|t| t.text != text);
        let now = Instant::now();
        self.items.push(Toast { text, kind, until: now + Duration::from_secs_f32(secs), born: now });
        if self.items.len() > 4 {
            self.items.remove(0);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, p: &Palette) {
        let now = Instant::now();
        self.items.retain(|t| t.until > now);
        if self.items.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let mut y = screen.max.y - 44.0;
        let mut animating = false;
        for (i, t) in self.items.iter().enumerate().rev() {
            let (icon, color) = match t.kind {
                ToastKind::Info => (I::INFO, p.accent_text),
                ToastKind::Success => (I::CHECK_CIRCLE, p.ok),
                ToastKind::Error => (I::WARNING_CIRCLE, p.danger),
            };
            // Slide up 8 pt and fade in over 160 ms.
            let age = now.duration_since(t.born).as_secs_f32();
            let k = (age / 0.16).min(1.0);
            let k = 1.0 - (1.0 - k) * (1.0 - k);
            animating |= k < 1.0;
            // Above the panels and the status bar but below floating windows and
            // dialogs, so a toast never hides a window's buttons.
            let resp = egui::Area::new(egui::Id::unique(("toast", i)))
                .order(egui::Order::Background)
                .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, y - screen.max.y + 8.0 * (1.0 - k)))
                .interactable(true)
                .show(ctx, |ui| {
                    ui.multiply_opacity(k);
                    egui::Frame::new()
                        .fill(p.raised)
                        .stroke(Stroke::new(1.0, p.border))
                        .shadow(theme::popover_shadow(p))
                        .corner_radius(CornerRadius::same(radius::CARD))
                        .inner_margin(Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_max_width(560.0);
                            ui.set_min_height(20.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                ui.label(RichText::new(icon).color(color).size(16.0));
                                ui.add(egui::Label::new(RichText::new(&t.text).font(text::body()).color(p.text)).wrap());
                            });
                        })
                });
            y -= resp.response.rect.height() + 8.0;
        }
        if animating {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

// ====================================================================== dialogs

/// Simple modal-like text prompt state (rename, new folder, ...).
pub struct Prompt {
    pub title: String,
    pub value: String,
    pub hint: String,
    pub focus_requested: bool,
    /// Shown under the field (e.g. "a file with that name exists"); the prompt stays open.
    pub error: Option<String>,
}

impl Prompt {
    pub fn new(title: &str, value: &str, hint: &str) -> Self {
        Prompt { title: title.into(), value: value.into(), hint: hint.into(), focus_requested: false, error: None }
    }
}

/// Returns Some(true)=OK, Some(false)=Cancel, None=still open. Enter = OK, Esc = Cancel.
pub fn show_prompt(ctx: &egui::Context, prompt: &mut Prompt) -> Option<bool> {
    let p = theme::palette(ctx);
    let mut result = None;
    modal(ctx, egui::Id::unique("prompt"), 400.0, |ui| {
        modal_title(ui, &prompt.title);
        ui.add_space(4.0);
        let te = ui.add(egui::TextEdit::singleline(&mut prompt.value).hint_text(&prompt.hint).desired_width(f32::INFINITY).min_size(Vec2::new(0.0, size::CONTROL)));
        if !prompt.focus_requested {
            te.request_focus();
            // Select the base name (without extension) like Explorer does.
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), te.id) {
                let end = prompt.value.rfind('.').filter(|&i| i > 0).unwrap_or(prompt.value.len());
                let range = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(prompt.value[..end].chars().count()));
                state.cursor.set_char_range(Some(range));
                state.store(ui.ctx(), te.id);
            }
            prompt.focus_requested = true;
        } else if prompt.error.is_some() && !te.has_focus() && !te.lost_focus() {
            te.request_focus();
        }
        if te.changed() {
            prompt.error = None;
        }
        if te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            result = Some(true);
        }
        if let Some(e) = &prompt.error {
            ui.add_space(6.0);
            ui.add(egui::Label::new(RichText::new(e).color(p.danger).size(12.5)).wrap());
        }
        if let Some(r) = dialog_buttons(ui, &p, "Cancel", "OK", ButtonKind::Primary, true) {
            result = Some(r);
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(false);
        }
    });
    result
}

/// Confirmation dialog. Returns Some(true) confirm / Some(false) cancel.
/// Esc cancels. Enter confirms only when the action is not destructive (`danger`);
/// see [`show_confirm_with`] for a destructive action that is undoable.
pub fn show_confirm(ctx: &egui::Context, title: &str, body: &str, confirm: &str, danger: bool, p: &Palette) -> Option<bool> {
    let kind = if danger { ButtonKind::Danger } else { ButtonKind::Primary };
    show_confirm_with(ctx, title, body, confirm, kind, !danger, p)
}

/// [`show_confirm`] with the button style and Enter behaviour chosen separately, e.g.
/// a red "Delete" that still confirms on Enter because the Recycle Bin can undo it
/// (as in Explorer).
pub fn show_confirm_with(ctx: &egui::Context, title: &str, body: &str, confirm: &str, kind: ButtonKind, enter_confirms: bool, p: &Palette) -> Option<bool> {
    let mut result = None;
    modal(ctx, egui::Id::unique(("confirm", title)), 400.0, |ui| {
        modal_title(ui, title);
        modal_body(ui, body);
        if let Some(r) = dialog_buttons(ui, p, "Cancel", confirm, kind, true) {
            result = Some(r);
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            result = Some(false);
        }
        if enter_confirms && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            result = Some(true);
        }
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_splitting() {
        assert_eq!(split_shortcut("Ctrl+Shift+K"), vec![vec!["Ctrl", "Shift", "K"]]);
        assert_eq!(split_shortcut("Ctrl++ / F2"), vec![vec!["Ctrl".to_string(), "+".to_string()], vec!["F2".to_string()]]);
        assert_eq!(split_shortcut("+"), vec![vec!["+"]]);
        assert!(split_shortcut("").is_empty());
    }

    #[test]
    fn rail_mapping_round_trips() {
        for log in [false, true] {
            let r = Rail { x0: 0.0, x1: 100.0, min: 0.0, max: 8192.0, log };
            for v in [0.0, 1.0, 256.0, 8192.0] {
                assert!((r.v_of(r.t_of(v)) - v).abs() < 1e-6 * v.max(1.0), "log={log} v={v}");
            }
        }
        let r = Rail { x0: 0.0, x1: 100.0, min: -5.0, max: 5.0, log: false };
        assert_eq!(r.x_of(0.0), 50.0);
    }
}
