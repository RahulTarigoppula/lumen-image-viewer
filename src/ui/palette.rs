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
    /// Commands run from the palette this session, most recent first.
    recent: Vec<Command>,
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

/// Character indices of `hay` that [`fuzzy_score`] matches with `needle`
/// (for highlighting). Empty when the needle is empty or does not match.
pub fn fuzzy_positions(hay: &str, needle: &str) -> Vec<usize> {
    let hay_l: Vec<char> = hay.to_lowercase().chars().collect();
    let needle_l: Vec<char> = needle.to_lowercase().chars().filter(|c| *c != ' ').collect();
    if needle_l.is_empty() {
        return Vec::new();
    }
    let full: Vec<char> = needle.to_lowercase().chars().collect();
    if let Some(start) = hay_l.windows(full.len()).position(|w| w == full.as_slice()) {
        return (start..start + full.len()).collect();
    }
    let mut out = Vec::new();
    let mut i = 0;
    for nc in needle_l {
        while i < hay_l.len() && hay_l[i] != nc {
            i += 1;
        }
        if i == hay_l.len() {
            return Vec::new();
        }
        out.push(i);
        i += 1;
    }
    out
}

/// One row of the list: a group heading or a command.
#[derive(Clone, Copy)]
enum Row {
    Header(&'static str),
    Item(Command),
}

impl CommandPalette {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
        self.just_opened = true;
    }

    /// Draw the palette (scrim, 600 pt panel 18% from the top, search field, rows with
    /// keycap shortcuts). Returns the command to run. Esc or a click outside closes it.
    pub fn show(&mut self, ctx: &egui::Context, p: &Palette, available: &dyn Fn(Command) -> bool) -> Option<Command> {
        use super::theme::{radius, size, text};
        use egui::{Color32, CornerRadius, FontId, Margin, Stroke};
        if !self.open {
            return None;
        }
        let mut picked = None;
        let all: Vec<Command> = Command::palette().into_iter().filter(|c| available(*c)).collect();
        let mut items: Vec<(Command, i32)> = all.iter().filter_map(|c| fuzzy_score(&c.label(), &self.query).map(|s| (*c, s))).collect();
        items.sort_by(|a, b| b.1.cmp(&a.1));
        // With an empty query the recent commands come first, under their own heading.
        let mut rows: Vec<Row> = Vec::new();
        if self.query.trim().is_empty() {
            let recent: Vec<Command> = self.recent.iter().copied().filter(|c| all.contains(c)).take(5).collect();
            if !recent.is_empty() {
                rows.push(Row::Header("Recent"));
                rows.extend(recent.iter().map(|c| Row::Item(*c)));
                rows.push(Row::Header("All commands"));
            }
            rows.extend(items.iter().filter(|(c, _)| !recent.contains(c)).map(|(c, _)| Row::Item(*c)));
        } else {
            rows.extend(items.iter().map(|(c, _)| Row::Item(*c)));
        }
        let commands: Vec<Command> = rows.iter().filter_map(|r| if let Row::Item(c) = r { Some(*c) } else { None }).collect();
        if self.selected >= commands.len() {
            self.selected = commands.len().saturating_sub(1);
        }
        let screen = ctx.content_rect();
        let width = 600.0_f32.min(screen.width() - 32.0);
        let area = egui::Modal::default_area(egui::Id::unique("command-palette")).anchor(Align2::CENTER_TOP, Vec2::new(0.0, (screen.height() * 0.18).max(32.0)));
        let frame = egui::Frame::new()
            .fill(p.raised)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(radius::WINDOW))
            .shadow(super::theme::window_shadow(p))
            .inner_margin(Margin::same(8));
        let modal = egui::Modal::new(egui::Id::unique("command-palette-modal")).area(area).frame(frame).backdrop_color(super::theme::scrim(p)).show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            // Search field: 40 pt container, 16 pt magnifier 12 pt from the left, 8 pt gap.
            let (field, _) = ui.allocate_exact_size(Vec2::new(width, 40.0), egui::Sense::hover());
            ui.painter().rect(field, CornerRadius::same(radius::CARD), p.input_bg, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
            ui.painter().text(egui::pos2(field.left() + 20.0, field.center().y), Align2::CENTER_CENTER, super::icons::MAGNIFYING_GLASS, FontId::proportional(16.0), p.muted);
            let text_rect = egui::Rect::from_min_max(egui::pos2(field.left() + 36.0, field.top()), egui::pos2(field.right() - 12.0, field.bottom()));
            let te = ui
                .scope_builder(egui::UiBuilder::new().max_rect(text_rect).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("Type a command").desired_width(text_rect.width()).font(FontId::proportional(15.0)).frame(egui::Frame::NONE))
                })
                .inner;
            if self.just_opened {
                te.request_focus();
                self.just_opened = false;
            }
            if te.changed() {
                self.selected = 0;
            }
            ui.add_space(8.0);
            let (up, down, enter) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter)));
            if up {
                self.selected = self.selected.saturating_sub(1);
            }
            if down && self.selected + 1 < commands.len() {
                self.selected += 1;
            }
            if enter {
                if let Some(c) = commands.get(self.selected) {
                    picked = Some(*c);
                }
            }
            egui::ScrollArea::vertical().max_height(400.0_f32.min(screen.height() * 0.6)).auto_shrink([false, true]).show(ui, |ui| {
                let mut index = 0;
                for row in &rows {
                    match row {
                        Row::Header(h) => {
                            let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), egui::Sense::hover());
                            ui.painter().text(egui::pos2(r.left() + 10.0, r.bottom() - 6.0), Align2::LEFT_BOTTOM, *h, text::caption(), p.muted);
                        }
                        Row::Item(c) => {
                            let sel = index == self.selected;
                            let w = ui.available_width();
                            let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), egui::Sense::click());
                            if sel || resp.hovered() {
                                ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), if sel { p.selected_bg } else { p.hover });
                            }
                            let painter = ui.painter();
                            painter.text(rect.left_center() + Vec2::new(16.0, 0.0), Align2::CENTER_CENTER, c.icon(), FontId::proportional(size::ICON), if sel { p.accent_text } else { p.muted });
                            let keys_w = super::widgets::paint_shortcut_keys(painter, p, rect.right_center() - Vec2::new(16.0, 0.0), &c.shortcut());
                            // The label, with the characters that match the query in the accent color.
                            let label = c.label();
                            let hits = fuzzy_positions(&label, &self.query);
                            let mut job = egui::text::LayoutJob::default();
                            for (ci, ch) in label.chars().enumerate() {
                                let hit = hits.contains(&ci);
                                let fmt = egui::TextFormat {
                                    font_id: if hit { super::theme::semibold(text::BODY) } else { text::body() },
                                    color: if hit { p.accent_text } else { p.text },
                                    ..Default::default()
                                };
                                job.append(&ch.to_string(), 0.0, fmt);
                            }
                            let galley = painter.layout_job(job);
                            let clip = egui::Rect::from_min_max(egui::pos2(rect.left() + 36.0, rect.top()), egui::pos2(rect.right() - keys_w - 20.0, rect.bottom()));
                            painter.with_clip_rect(clip.intersect(ui.clip_rect())).galley(egui::pos2(rect.left() + 36.0, rect.center().y - galley.size().y / 2.0), galley, Color32::PLACEHOLDER);
                            if sel && (up || down || te.changed()) {
                                resp.scroll_to_me(None);
                            }
                            if resp.clicked() {
                                picked = Some(*c);
                            }
                            index += 1;
                        }
                    }
                }
                if commands.is_empty() {
                    ui.add_space(12.0);
                    ui.vertical_centered(|ui| ui.label(RichText::new("No matching commands").color(p.muted)));
                    ui.add_space(12.0);
                }
            });
        });
        if modal.should_close() {
            self.open = false;
        }
        if let Some(c) = picked {
            self.open = false;
            self.recent.retain(|r| *r != c);
            self.recent.insert(0, c);
            self.recent.truncate(8);
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
        assert_eq!(fuzzy_positions("Rotate left", "rot"), vec![0, 1, 2]);
        assert_eq!(fuzzy_positions("Adjust colors...", "adjc"), vec![0, 1, 2, 7]);
        assert!(fuzzy_positions("Zoom in", "xyz").is_empty());
    }
}
