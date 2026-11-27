//! The report branding dialog: company or team name, footer line, logo and
//! accent color of every report page. It edits the defaults (saved in the
//! settings, for every report) or, from a project, that project's own
//! branding (saved in the project; the logo is copied into its folder).
//! Changes apply at once, like the settings.

use super::icons as I;
use super::settings_window::{custom_swatch, swatch};
use super::theme::{text, Palette};
use super::widgets;
use egui::{Color32, CornerRadius, RichText, Sense, Stroke, Ui, Vec2};
use lumen_core::report::{brand, Branding, ACCENT};
use std::path::{Path, PathBuf};

/// Accent colors offered as swatches (any other through the picker).
const ACCENTS: [(&str, [u8; 3]); 8] = [
    ("Lumen blue", ACCENT),
    ("Navy", [28, 52, 110]),
    ("Teal", [0, 128, 128]),
    ("Green", [34, 139, 84]),
    ("Orange", [226, 108, 20]),
    ("Red", [200, 30, 60]),
    ("Purple", [110, 60, 180]),
    ("Graphite", [70, 76, 86]),
];

/// Where the branding being edited lives.
pub enum Target<'a> {
    /// The defaults in the settings (the logo is copied into Lumen's data folder).
    Defaults(&'a mut Branding),
    /// A project: its own branding when `own` is `Some`, else it uses the
    /// defaults (shown and edited as such).
    Project { own: &'a mut Option<Branding>, defaults: &'a mut Branding, root: &'a Path },
}

/// What happened in the dialog this frame.
#[derive(Default)]
pub struct Outcome {
    /// Still open.
    pub open: bool,
    /// The project's own branding changed (save the project).
    pub project_changed: bool,
    pub error: Option<String>,
}

pub struct BrandingDialog {
    just_opened: bool,
    /// The logo on screen: its file and its texture (or why it could not be read).
    preview: Option<(PathBuf, Result<egui::TextureHandle, String>)>,
    /// Why the last picked logo was not taken.
    error: Option<String>,
}

impl Default for BrandingDialog {
    fn default() -> Self {
        BrandingDialog { just_opened: true, preview: None, error: None }
    }
}

/// Keep a copy of the picked logo with the branding that uses it: in the
/// project's `.lumen` folder (stored relative, so the project folder can be
/// moved or shared) or in Lumen's data folder (stored as a full path). When
/// the copy fails the original file is used where it is.
pub fn keep_logo(src: &Path, root: Option<&Path>) -> String {
    let ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).filter(|e| !e.is_empty()).unwrap_or_else(|| "png".into());
    let name = format!("report-logo.{ext}");
    let dir = match root {
        Some(r) => r.join(".lumen"),
        None => crate::settings::data_dir(),
    };
    let dst = dir.join(&name);
    let same = src.to_string_lossy().eq_ignore_ascii_case(&dst.to_string_lossy());
    if same || (std::fs::create_dir_all(&dir).is_ok() && std::fs::copy(src, &dst).is_ok()) {
        match root {
            Some(_) => format!(".lumen/{name}"),
            None => dst.to_string_lossy().into_owned(),
        }
    } else {
        log::warn!("could not copy the logo to {}; using it from {}", dst.display(), src.display());
        src.to_string_lossy().into_owned()
    }
}

/// The branding a report uses: the project's own (when it has one) or the
/// defaults, with the logo as a full path for the worker thread.
pub fn effective(own: Option<&Branding>, defaults: &Branding, root: Option<&Path>) -> Branding {
    match own {
        Some(b) => b.resolved(root),
        None => defaults.resolved(None),
    }
}

/// One row of an export dialog: which branding the report gets, and (when
/// `editable`) a button to change it. True when the button was clicked.
pub fn summary_row(ui: &mut Ui, p: &Palette, b: &Branding, whose: &str, editable: bool) -> bool {
    ui.label(RichText::new("Branding").size(text::SMALL).color(p.muted));
    let mut clicked = false;
    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        let c = b.accent();
        ui.painter().rect_filled(dot, CornerRadius::same(3), Color32::from_rgb(c[0], c[1], c[2]));
        let button_w = if editable { 96.0 } else { 0.0 };
        let w = (ui.available_width() - button_w).max(60.0);
        ui.allocate_ui_with_layout(Vec2::new(w, 20.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_width(w);
            ui.add(egui::Label::new(RichText::new(format!("{}  ({whose})", b.summary())).color(p.text)).truncate());
        });
        if editable {
            clicked = widgets::secondary_button(ui, &format!("{}  Edit...", I::PAINT_BRUSH)).on_hover_text("Company or team name, logo, accent color and footer line of the report").clicked();
        }
    });
    clicked
}

impl BrandingDialog {
    pub fn show(&mut self, ctx: &egui::Context, p: &Palette, target: Target) -> Outcome {
        match target {
            Target::Defaults(d) => self.form(ctx, p, d, None, None),
            Target::Project { own, defaults, root } => {
                let mut scope = own.is_some();
                let mut out = match own.as_mut() {
                    Some(b) => {
                        let before = b.clone();
                        let out = self.form(ctx, p, b, Some(root), Some(&mut scope));
                        Outcome { project_changed: *b != before, ..out }
                    }
                    None => self.form(ctx, p, defaults, None, Some(&mut scope)),
                };
                // Switching between the default and the project's own branding:
                // the project starts from a copy of the defaults (its logo too).
                if scope != own.is_some() {
                    *own = scope.then(|| {
                        let mut b = defaults.clone();
                        if let Some(l) = defaults.logo_path(None) {
                            b.logo = keep_logo(&l, Some(root));
                        }
                        b
                    });
                    out.project_changed = true;
                    self.preview = None;
                }
                out
            }
        }
    }

    /// The fields for `b`. `logo_root`: the project folder when `b` is a
    /// project's own branding. `scope`: the default / project choice (shown
    /// from a project only).
    fn form(&mut self, ctx: &egui::Context, p: &Palette, b: &mut Branding, logo_root: Option<&Path>, scope: Option<&mut bool>) -> Outcome {
        let mut out = Outcome { open: true, ..Default::default() };
        let logo_path = b.logo_path(logo_root);
        self.update_preview(ctx, logo_path.clone());
        let (mut pick, mut done, mut clear) = (false, false, false);
        widgets::modal(ctx, egui::Id::new("report-branding"), 460.0, |ui| {
            widgets::modal_title(ui, "Report branding");
            widgets::modal_body(ui, "Your company's look on every page of the report: PowerPoint, PDF and web page. Empty fields keep Lumen's look.");
            ui.add_space(8.0);
            if let Some(scope) = scope {
                widgets::segmented_value(ui, scope, &[(false, "Default for all reports"), (true, "This project only")]);
                let hint = if *scope { "Saved in this project; its reports use it instead of the default." } else { "Saved in Settings; every report uses it unless a project has its own." };
                ui.label(RichText::new(hint).size(text::SMALL).color(p.muted));
                ui.add_space(6.0);
            }
            let full = ui.available_width();
            ui.label(RichText::new("Company or team").size(text::SMALL).color(p.muted));
            let r = ui.add(egui::TextEdit::singleline(&mut b.company).hint_text("e.g. Acme Camera Lab").desired_width(full)).on_hover_text("On the cover and at the foot of every page");
            if std::mem::take(&mut self.just_opened) {
                r.request_focus();
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Footer line").size(text::SMALL).color(p.muted));
            ui.add(egui::TextEdit::singleline(&mut b.footer).hint_text("e.g. Confidential").desired_width(full)).on_hover_text("At the foot of every page and on the cover");
            ui.add_space(8.0);
            ui.label(RichText::new("Logo").size(text::SMALL).color(p.muted));
            ui.horizontal(|ui| {
                // The logo on white, as it sits in the report.
                let (rect, _) = ui.allocate_exact_size(Vec2::new(132.0, 44.0), Sense::hover());
                ui.painter().rect(rect, CornerRadius::same(6), Color32::WHITE, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
                let note = |t: &str, c: Color32| ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, t, egui::FontId::proportional(11.0), c);
                match &self.preview {
                    Some((_, Ok(tex))) => {
                        let size = tex.size_vec2();
                        let inner = rect.shrink(6.0);
                        let s = (inner.width() / size.x.max(1.0)).min(inner.height() / size.y.max(1.0));
                        let r = egui::Rect::from_center_size(inner.center(), size * s);
                        ui.painter().image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                    }
                    Some((_, Err(_))) => {
                        note("Cannot read it", Color32::from_rgb(180, 40, 40));
                    }
                    None => {
                        note("No logo", Color32::from_gray(140));
                    }
                }
                ui.vertical(|ui| {
                    let name = logo_path.as_ref().and_then(|l| l.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "None".into());
                    let r = ui.add(egui::Label::new(RichText::new(name).color(p.text)).truncate());
                    if let Some(l) = &logo_path {
                        r.on_hover_text(l.display().to_string());
                    }
                    ui.horizontal(|ui| {
                        if widgets::secondary_button(ui, &format!("{}  Choose...", I::IMAGE)).on_hover_text("A PNG or JPEG of any shape; transparent PNGs work best").clicked() {
                            pick = true;
                        }
                        if !b.logo.is_empty() && widgets::secondary_button(ui, "Remove").clicked() {
                            b.logo.clear();
                        }
                    });
                });
            });
            let kept = if logo_root.is_some() { "A copy of the logo is kept in the project folder." } else { "A copy of the logo is kept with Lumen's settings." };
            ui.label(RichText::new(kept).size(text::SMALL).color(p.muted));
            if let Some(e) = &self.error {
                ui.add(egui::Label::new(RichText::new(e).size(text::SMALL).color(p.warn)).wrap());
            }
            ui.add_space(8.0);
            ui.label(RichText::new("Accent color").size(text::SMALL).color(p.muted));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let current = b.accent();
                for (name, rgb) in ACCENTS {
                    if swatch(ui, p, rgb, current == rgb, name).clicked() {
                        b.accent = (rgb != ACCENT).then_some(rgb);
                    }
                }
                ui.add_space(6.0);
                let presets: Vec<[u8; 3]> = ACCENTS.iter().map(|(_, c)| *c).collect();
                let mut c = current;
                if custom_swatch(ui, p, &mut c, &presets) {
                    b.accent = Some(c);
                }
            });
            ui.label(RichText::new("For rules, labels and table headers; image badges keep their colors.").size(text::SMALL).color(p.muted));
            match widgets::dialog_buttons(ui, p, "Clear all", "Done", widgets::ButtonKind::Primary, true) {
                Some(true) => done = true,
                Some(false) => clear = true,
                None => {}
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                done = true;
            }
        });
        if clear {
            *b = Branding::default();
        }
        if pick {
            let dlg = rfd::FileDialog::new().set_title("Logo for the report").add_filter("PNG or JPEG", &["png", "jpg", "jpeg"]);
            if let Some(path) = crate::devhook::pick_file(dlg) {
                match brand::read_logo(&path) {
                    Ok(_) => {
                        b.logo = keep_logo(&path, logo_root);
                        self.preview = None;
                        self.error = None;
                    }
                    Err(e) => {
                        let msg = format!("Could not read the logo: {e:#}");
                        self.error = Some(msg.clone());
                        out.error = Some(msg);
                    }
                }
            }
        }
        out.open = !done;
        out
    }

    /// Load the logo's texture when its file changed.
    fn update_preview(&mut self, ctx: &egui::Context, path: Option<PathBuf>) {
        let Some(path) = path else {
            self.preview = None;
            return;
        };
        if self.preview.as_ref().is_some_and(|(p, _)| *p == path) {
            return;
        }
        let tex = brand::read_logo(&path)
            .map(|img| {
                // As the report shows it: without transparent margins.
                let small = lumen_core::imaging::resize_to_fit(&brand::trim_transparent(&img), 396, 132);
                let ci = egui::ColorImage::from_rgba_unmultiplied([small.width() as usize, small.height() as usize], small.as_raw());
                ctx.load_texture("report-logo-preview", ci, egui::TextureOptions::LINEAR)
            })
            .map_err(|e| format!("{e:#}"));
        self.preview = Some((path, tex));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_is_copied_into_the_project() {
        let root = std::env::temp_dir().join(format!("lumen-logo-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("Company Logo.PNG");
        std::fs::write(&src, b"png bytes").unwrap();
        let stored = keep_logo(&src, Some(&root));
        assert_eq!(stored, ".lumen/report-logo.png");
        assert_eq!(std::fs::read(root.join(".lumen").join("report-logo.png")).unwrap(), b"png bytes");
        // Picking the kept copy again leaves it alone.
        assert_eq!(keep_logo(&root.join(".lumen").join("report-logo.png"), Some(&root)), ".lumen/report-logo.png");
        let b = Branding { logo: stored, ..Default::default() };
        assert_eq!(effective(Some(&b), &Branding::default(), Some(&root)).logo, root.join(".lumen").join("report-logo.png").to_string_lossy());
        // A project without its own uses the defaults.
        let d = Branding { company: "Acme".into(), ..Default::default() };
        assert_eq!(effective(None, &d, Some(&root)), d);
        let _ = std::fs::remove_dir_all(&root);
    }
}
