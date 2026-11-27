//! Rating, flag and color label controls, the same in the browser's preview
//! pane, the viewer's Info panel and both right-click menus. A click asks for
//! the same command as the key, so undo, notices and "next image after
//! rating" work alike, and the tooltips name the keys the keymap really uses.

use super::commands::Command;
use super::icons as I;
use super::theme::{self, Palette};
use super::tips;
use super::widgets;
use crate::tags::{Tag, LABEL_COLORS, LABEL_NAMES};
use egui::{Align2, Color32, FontId, Sense, Stroke, Ui, Vec2};

/// Room between the stars, the flag and the color labels.
const GAP: f32 = 8.0;

fn flag_w(size: f32) -> f32 {
    size + 4.0
}

fn label_w(size: f32) -> f32 {
    size + 2.0
}

/// Width of the stars and the flag (the first line when the row wraps).
fn head_width(size: f32) -> f32 {
    widgets::stars_width(size) + GAP + flag_w(size)
}

/// Width of the whole row on one line at star size `size`.
pub fn row_width(size: f32) -> f32 {
    head_width(size) + GAP + 5.0 * label_w(size)
}

/// True when the row needs two lines (labels below) in `width` points.
pub fn wraps(width: f32, size: f32) -> bool {
    width < row_width(size)
}

/// The command a click on star `n` runs (`clears`: it is the lit star).
fn star_command(n: u8, clears: bool) -> Command {
    Command::Rate(if clears { 0 } else { n })
}

/// The command a click on label `l` runs (`on`: the file has that label).
fn label_command(l: u8, on: bool) -> Command {
    Command::Label(if on { 0 } else { l })
}

/// "3 stars  (3)": what a click on star `n` does, with `key` (the command's shortcut).
fn star_text(n: u8, clears: bool, key: &str) -> String {
    let what = if clears { "Clear rating".to_string() } else { format!("{n} star{}", if n == 1 { "" } else { "s" }) };
    tips::with_shortcut(&what, key)
}

fn flag_text(flagged: bool, key: &str) -> String {
    tips::with_shortcut(if flagged { "Remove flag" } else { "Flag" }, key)
}

/// "Red label  (6)" or "Clear the red label".
fn label_text(l: u8, on: bool, key: &str) -> String {
    let name = LABEL_NAMES[l as usize % LABEL_NAMES.len()];
    let what = if on { format!("Clear the {} label", name.to_lowercase()) } else { format!("{name} label") };
    tips::with_shortcut(&what, key)
}

/// Tooltip of star `n`, with the key the active keymap uses.
pub fn star_tip(n: u8, clears: bool) -> String {
    star_text(n, clears, &star_command(n, clears).shortcut())
}

pub fn flag_tip(flagged: bool) -> String {
    flag_text(flagged, &Command::ToggleFlag.shortcut())
}

pub fn label_tip(l: u8, on: bool) -> String {
    label_text(l, on, &label_command(l, on).shortcut())
}

/// Stars, flag and the five color labels of `tag`. Returns the command a click
/// asks for (the caller runs it like the key). The labels go on a second line
/// when the row does not fit.
pub fn tag_row(ui: &mut Ui, tag: Tag, size: f32, p: &Palette) -> Option<Command> {
    let mut out = None;
    let two_lines = wraps(ui.available_width(), size);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        if let Some(r) = widgets::stars(ui, tag.rating, size, p, |n, clears| Some(star_tip(n, clears))) {
            out = Some(Command::Rate(r));
        }
        ui.add_space(GAP);
        if flag_button(ui, tag.flagged, size, p) {
            out = Some(Command::ToggleFlag);
        }
        if !two_lines {
            ui.add_space(GAP);
            if let Some(c) = labels(ui, tag.label, size, p) {
                out = Some(c);
            }
        }
    });
    if two_lines {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if let Some(c) = labels(ui, tag.label, size, p) {
                out = Some(c);
            }
        });
    }
    out
}

fn flag_button(ui: &mut Ui, flagged: bool, size: f32, p: &Palette) -> bool {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(flag_w(size)), Sense::click());
    let color = if flagged { p.accent_text } else if resp.hovered() { p.muted } else { p.faint };
    let font = if flagged { theme::icon_fill(size) } else { FontId::proportional(size) };
    ui.painter().text(r.center(), Align2::CENTER_CENTER, I::FLAG, font, color);
    resp.on_hover_text(flag_tip(flagged)).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn labels(ui: &mut Ui, label: u8, size: f32, p: &Palette) -> Option<Command> {
    let mut out = None;
    for l in 1..=5u8 {
        let [r, g, b] = LABEL_COLORS[l as usize];
        let col = Color32::from_rgb(r, g, b);
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(label_w(size)), Sense::click());
        let c = rect.center();
        let on = label == l;
        if on {
            ui.painter().circle_filled(c, size * 0.47, col);
            ui.painter().circle_stroke(c, size * 0.5, Stroke::new(1.5, p.text));
        } else if resp.hovered() {
            ui.painter().circle_filled(c, size * 0.42, col);
        } else {
            ui.painter().circle_filled(c, size * 0.36, col.gamma_multiply(0.8));
        }
        if resp.on_hover_text(label_tip(l, on)).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            out = Some(label_command(l, on));
        }
    }
    out
}

/// The "Rate & tag" submenu (browser and viewer right-click menus): every
/// rating, the flag and every color label, with their keys. `item` draws one
/// command row and runs it when clicked.
pub fn rate_tag_menu(ui: &mut Ui, mut item: impl FnMut(&mut Ui, Command)) {
    widgets::submenu(ui, I::STAR, "Rate & tag", |ui| {
        for n in 0..=5 {
            item(ui, Command::Rate(n));
        }
        ui.separator();
        item(ui, Command::ToggleFlag);
        for n in 0..=5 {
            item(ui, Command::Label(n));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltips_name_the_keys_of_the_keymap() {
        use crate::ui::keymap::{mode_text, KeyMode, Keymap, Preset};
        for preset in Preset::ALL {
            let km = Keymap::preset(preset);
            for mode in [KeyMode::Browser, KeyMode::Viewer] {
                let key = |c: Command| mode_text(&km, mode, c);
                assert_eq!(star_text(3, false, &key(star_command(3, false))), "3 stars  (3)");
                assert_eq!(star_text(1, false, &key(star_command(1, false))), "1 star  (1)");
                assert_eq!(star_text(2, true, &key(star_command(2, true))), "Clear rating  (0)");
                // Space is no flag key in FastStone's browser: the tooltip names the real keys.
                assert_eq!(flag_text(false, &key(Command::ToggleFlag)), "Flag  (Q / \\)");
                assert_eq!(flag_text(true, &key(Command::ToggleFlag)), "Remove flag  (Q / \\)");
                assert_eq!(label_text(1, false, &key(label_command(1, false))), "Red label  (6)");
                // Purple has no key, and no key clears a label: no brackets then.
                assert_eq!(label_text(5, false, &key(label_command(5, false))), "Purple label");
                assert_eq!(label_text(2, true, &key(label_command(2, true))), "Clear the yellow label");
            }
        }
        assert_eq!(star_command(4, true), Command::Rate(0));
        assert_eq!(label_command(3, false), Command::Label(3));
    }

    #[test]
    fn row_wraps_only_when_narrow() {
        assert!(!wraps(row_width(13.0), 13.0));
        assert!(wraps(row_width(13.0) - 1.0, 13.0));
        // The browser's default sidebar (260 pt less its margins) fits the row on one line.
        assert!(!wraps(236.0, 13.0));
    }
}
