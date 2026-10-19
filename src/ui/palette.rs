//! Ctrl+K command palette with fuzzy search.

use super::commands::Command;
use super::theme::Palette;
use egui::{Align2, Key, RichText, Vec2};

#[derive(Default)]
pub struct CommandPalette {
    pub open: bool,
    query: String,
    selected: usize,
    just_opened: bool,
}

/// Subsequence fuzzy score; higher is better. None when not matching.
pub fn fuzzy_score(hay: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay_l = hay.to_lowercase();
    let needle_l = needle.to_lowercase();
    if let Some(pos) = hay_l.find(&needle_l) {
        return Some(1000 - pos as i32 * 2 - hay.len() as i32 / 4);
    }
    let mut score = 0;
    let mut hi = hay_l.char_indices().peekable();
    let mut last = None;
    for nc in needle_l.chars() {
        if nc == ' ' {
            continue;
        }
        loop {
            let (i, hc) = hi.next()?;
            if hc == nc {
                let word_start = i == 0 || hay_l[..i].ends_with(' ');
                score += if word_start { 12 } else { 3 };
                if last == Some(i.saturating_sub(1)) {
                    score += 6;
                }
                last = Some(i);
                break;
            }
        }
    }
    Some(score)
}

impl CommandPalette {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
        self.just_opened = true;
    }

    pub fn show(&mut self, ctx: &egui::Context, p: &Palette, available: &dyn Fn(Command) -> bool) -> Option<Command> {
        if !self.open {
            return None;
        }
        let mut picked = None;
        let mut items: Vec<(Command, i32)> = Command::palette()
            .into_iter()
            .filter(|c| available(*c))
            .filter_map(|c| fuzzy_score(&c.label(), &self.query).map(|s| (c, s)))
            .collect();
        items.sort_by(|a, b| b.1.cmp(&a.1));
        if self.selected >= items.len() {
            self.selected = items.len().saturating_sub(1);
        }
        let screen = ctx.content_rect();
        egui::Area::new(egui::Id::unique("command-palette"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, (screen.height() * 0.12).max(40.0)))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).fill(p.raised).inner_margin(10).show(ui, |ui| {
                    ui.set_width(560.0_f32.min(screen.width() - 40.0));
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text(format!("{}  Type a command...", super::icons::MAGNIFYING_GLASS))
                            .desired_width(f32::INFINITY)
                            .font(egui::FontId::proportional(16.0))
                            .frame(egui::Frame::NONE),
                    );
                    if self.just_opened {
                        te.request_focus();
                        self.just_opened = false;
                    }
                    if te.changed() {
                        self.selected = 0;
                    }
                    ui.separator();
                    let (up, down, enter, esc) = ui.input(|i| {
                        (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter), i.key_pressed(Key::Escape))
                    });
                    if up {
                        self.selected = self.selected.saturating_sub(1);
                    }
                    if down && self.selected + 1 < items.len() {
                        self.selected += 1;
                    }
                    if esc {
                        self.open = false;
                    }
                    if enter {
                        if let Some((c, _)) = items.get(self.selected) {
                            picked = Some(*c);
                        }
                    }
                    egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                        for (i, (c, _)) in items.iter().enumerate() {
                            let sel = i == self.selected;
                            let resp = ui
                                .horizontal(|ui| {
                                    let w = ui.available_width();
                                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), egui::Sense::click());
                                    if sel || resp.hovered() {
                                        ui.painter().rect_filled(rect, 6.0, if sel { p.accent_soft } else { p.hover });
                                    }
                                    let painter = ui.painter();
                                    painter.text(rect.left_center() + Vec2::new(14.0, 0.0), Align2::CENTER_CENTER, c.icon(), egui::FontId::proportional(15.0), p.muted);
                                    painter.text(rect.left_center() + Vec2::new(30.0, 0.0), Align2::LEFT_CENTER, c.label(), egui::FontId::proportional(13.5), p.text);
                                    painter.text(rect.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, c.shortcut(), egui::FontId::proportional(11.5), p.faint);
                                    if sel {
                                        resp.scroll_to_me(None);
                                    }
                                    resp
                                })
                                .inner;
                            if resp.clicked() {
                                picked = Some(*c);
                            }
                        }
                        if items.is_empty() {
                            ui.label(RichText::new("No matching commands").color(p.muted));
                        }
                    });
                });
            });
        if picked.is_some() {
            self.open = false;
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("Rotate left", "rot").unwrap() > fuzzy_score("Batch convert / resize...", "rot").unwrap_or(-1));
        assert!(fuzzy_score("Adjust colors...", "adjc").is_some());
        assert!(fuzzy_score("Zoom in", "xyz").is_none());
    }
}
