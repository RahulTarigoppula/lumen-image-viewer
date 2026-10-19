//! The Analysis panel shell: header, the Metrics and AI tabs, the region of
//! interest shared by both (R draws it on the image while the panel is
//! open, Esc clears it), and the overlay pass that draws the ROI and the AI
//! finding boxes over the active mode's images.

use egui::{Align, Color32, Layout, Margin, Rect, RichText, Sense, Ui};

use super::canvas::{self, Slot};
use super::Subject;
use crate::app::Core;
use crate::ui::icons as I;
use crate::ui::roi::{ImageTransform, RoiEditor, RoiOptions};
use crate::ui::theme::semibold;

/// Something the app (or the active mode) must do for the panel.
pub enum PanelAction {
    /// Zoom the view to `rect` (fractions 0..1 of image `slot`: x0, y0, x1, y1).
    ZoomTo { slot: Slot, rect: [f32; 4] },
    /// Open Settings at a page ("Ai").
    OpenSettings(&'static str),
    Toast(String),
    /// Save this JSON where the user picks.
    ExportJson(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Tab {
    Metrics,
    #[cfg(feature = "ai")]
    Ai,
}

pub struct AnalysisPanel {
    pub open: bool,
    pub tab: Tab,
    /// Analyze the ROI instead of the whole image.
    pub use_roi: bool,
    /// In image A's pixels as displayed (B panes show it proportionally).
    roi: RoiEditor,
    /// R was pressed: the next drag on the image draws a new ROI.
    drawing: bool,
    /// Run AI once the panel has the subject (Ctrl+Shift+Q, dev hook).
    run_ai: bool,
    #[cfg(feature = "ai")]
    pub ai: super::ai_view::AiView,
}

impl Default for AnalysisPanel {
    fn default() -> Self {
        let mut opts = RoiOptions::region(Color32::from_rgb(90, 200, 255));
        // Esc is handled before the app's shortcuts (see `pre_shortcuts`).
        opts.esc_clears = false;
        AnalysisPanel {
            open: false,
            #[cfg(feature = "ai")]
            tab: Tab::Ai,
            #[cfg(not(feature = "ai"))]
            tab: Tab::Metrics,
            use_roi: false,
            roi: RoiEditor::new((1.0, 1.0), opts),
            drawing: false,
            run_ai: false,
            #[cfg(feature = "ai")]
            ai: Default::default(),
        }
    }
}

impl AnalysisPanel {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.drawing = false;
        }
    }

    /// Ctrl+Shift+Q: open the AI tab and run.
    pub fn request_ai_run(&mut self) {
        self.open = true;
        #[cfg(feature = "ai")]
        {
            self.tab = Tab::Ai;
            self.run_ai = true;
        }
    }

    /// Shift+Q: open the Metrics tab and run the selected metrics.
    pub fn request_metrics_run(&mut self) {
        self.open = true;
        self.tab = Tab::Metrics;
        self.run_metrics();
    }

    /// Local metrics to give the AI as context, as (name, value, unit).
    /// Empty until the Metrics tab produces results.
    pub fn metric_context(&self) -> Vec<(String, f64, String)> {
        Vec::new()
    }

    /// Stub for the Metrics tab's Run (filled in with the local metrics).
    pub fn run_metrics(&mut self) {}

    /// The ROI in A's pixels (x, y, w, h) when the scope is ROI.
    pub fn roi_pixels(&self) -> Option<(u32, u32, u32, u32)> {
        self.roi.pixel_rect().filter(|_| self.use_roi)
    }

    /// Sets the ROI (in pixels of an image of `size`) or clears it.
    pub fn set_roi(&mut self, size: (u32, u32), rect: Option<(u32, u32, u32, u32)>) {
        self.roi.set_image_size((size.0 as f32, size.1 as f32));
        self.roi.set_rect(rect.map(|(x, y, w, h)| [x as f32, y as f32, (x + w) as f32, (y + h) as f32]));
        self.use_roi = rect.is_some();
        self.drawing = false;
    }

    /// True while a modal of the panel (the send preview) needs the keyboard.
    pub fn modal_open(&self, core: &Core) -> bool {
        #[cfg(feature = "ai")]
        return core.ai.preview_open();
        #[cfg(not(feature = "ai"))]
        {
            let _ = core;
            false
        }
    }

    /// Keys of the panel that must win over the mode's shortcuts: R starts
    /// drawing a ROI (instead of Rotate right), Esc stops drawing or clears it.
    /// Call before the keymap runs, only in modes that show the panel.
    pub fn pre_shortcuts(&mut self, ctx: &egui::Context) {
        if !self.open || ctx.egui_wants_keyboard_input() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::R)) {
            self.roi.clear();
            self.drawing = true;
            self.use_roi = true;
        }
        let esc = self.drawing || self.roi.rect().is_some();
        if esc && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            if self.drawing {
                self.drawing = false;
            } else {
                self.roi.clear();
                self.use_roi = false;
            }
        }
    }

    /// The right-side panel. Call before the mode's central panel.
    pub fn side_panel(&mut self, ui: &mut Ui, core: &mut Core, subject: &Subject) -> Vec<PanelAction> {
        let mut out = Vec::new();
        if !self.open {
            return out;
        }
        let p = core.pal;
        egui::Panel::right("analysis-panel")
            .resizable(true)
            .default_size(360.0)
            .size_range(300.0..=620.0)
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(Margin::same(12)))
            .show(ui, |ui| self.contents(ui, core, subject, &mut out));
        if std::mem::take(&mut self.run_ai) {
            #[cfg(feature = "ai")]
            {
                let roi = self.roi_pixels();
                self.ai.run(core, subject, roi, self.metric_context());
            }
        }
        out
    }

    fn contents(&mut self, ui: &mut Ui, core: &mut Core, subject: &Subject, out: &mut Vec<PanelAction>) {
        #[cfg(not(feature = "ai"))]
        let _ = &out;
        let p = core.pal;
        ui.horizontal(|ui| {
            ui.label(RichText::new(I::CHART_LINE).size(17.0).color(p.accent));
            ui.label(RichText::new("Analysis").font(semibold(15.5)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let tip = format!("Close ({})", crate::ui::commands::Command::ToggleAnalysis.shortcut());
                if crate::ui::widgets::icon_button_sized(ui, I::X, &tip, false, 24.0, 14.0).clicked() {
                    self.toggle();
                }
                super::network_badge(ui, core);
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.selectable_value(&mut self.tab, Tab::Metrics, format!("{}  Metrics", I::GAUGE));
            #[cfg(feature = "ai")]
            ui.selectable_value(&mut self.tab, Tab::Ai, format!("{}  AI", I::SPARKLE));
        });
        ui.add_space(4.0);
        self.scope_row(ui, core, subject);
        ui.separator();
        egui::ScrollArea::vertical().id_salt(("analysis-tab", self.tab)).auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match self.tab {
                Tab::Metrics => self.metrics_tab(ui, core, subject),
                #[cfg(feature = "ai")]
                Tab::Ai => {
                    let roi = self.roi_pixels();
                    let metrics = self.metric_context();
                    self.ai.ui(ui, core, subject, roi, metrics, out);
                }
            }
        });
    }

    /// Whole image or ROI, with the ROI's size and how to draw it.
    fn scope_row(&mut self, ui: &mut Ui, core: &Core, subject: &Subject) {
        let p = core.pal;
        let whole = if subject.kind.is_video() { "Whole frame" } else { "Whole image" };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Region").size(12.0).color(p.muted));
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.selectable_value(&mut self.use_roi, false, whole);
            if ui.selectable_value(&mut self.use_roi, true, "ROI").clicked() && self.roi.rect().is_none() {
                self.drawing = true;
            }
        });
        if !self.use_roi {
            return;
        }
        ui.horizontal(|ui| match self.roi.pixel_rect() {
            Some((x, y, w, h)) if !self.drawing => {
                ui.label(RichText::new(format!("{w} x {h} px at {x}, {y}")).size(11.5).color(p.text));
                if ui.small_button("Redraw").on_hover_text("Or press R").clicked() {
                    self.roi.clear();
                    self.drawing = true;
                }
                if ui.small_button("Clear").on_hover_text("Or press Esc").clicked() {
                    self.roi.clear();
                    self.use_roi = false;
                }
            }
            _ => {
                let text = if self.drawing { "Drag on the image to draw the region (Esc stops)" } else { "Press R, then drag on the image" };
                ui.label(RichText::new(text).size(11.5).color(p.accent));
            }
        });
    }

    /// The Metrics tab. Placeholder until the local objective metrics are
    /// wired in (see `run_metrics` and `metric_context`).
    fn metrics_tab(&mut self, ui: &mut Ui, core: &mut Core, _subject: &Subject) {
        let p = core.pal;
        ui.add_space(6.0);
        ui.label(RichText::new("Coming in this build").font(semibold(13.5)).color(p.text));
        ui.add(egui::Label::new(RichText::new("Objective metrics (PSNR, SSIM, Delta E, sharpness, noise, exposure) for the whole image or the ROI.").size(12.0).color(p.muted)).wrap());
    }

    /// Draws the ROI editor and the AI finding boxes on the panes the mode
    /// published this frame, and the panel's dialogs. Call after the mode's UI.
    pub fn overlay(&mut self, ui: &mut Ui, core: &mut Core, subject: Option<&Subject>) -> Vec<PanelAction> {
        let out = Vec::new();
        #[cfg(feature = "ai")]
        super::send_preview::show(ui.ctx(), core);
        // Only the viewer, compare and video modes have a subject.
        let Some(subject) = subject else { return out };
        if !self.open {
            self.sending_pill(ui, core);
            return out;
        }
        let panes = canvas::panes(ui.ctx());
        #[cfg(feature = "ai")]
        if self.tab == Tab::Ai {
            self.ai.overlay(ui, core, subject, &panes);
        }
        #[cfg(not(feature = "ai"))]
        let _ = subject;
        // The ROI is kept in A's displayed pixels; every pane (B too) shows it
        // through its own placement, so it covers the same region everywhere.
        let Some(a_size) = panes.iter().find(|p| p.slot == Slot::A).map(|p| p.xf.size) else { return out };
        if !self.use_roi && !self.drawing {
            return out;
        }
        for (i, pane) in panes.iter().enumerate() {
            let xf = ImageTransform { dest: pane.xf.dest, size: a_size };
            let on_image = pane.clip.intersect(pane.xf.dest);
            let area = if self.drawing { on_image } else { self.roi.rect().map(|r| xf.rect_to_screen(r).expand(10.0).intersect(pane.clip)).unwrap_or(Rect::NOTHING) };
            if !area.is_positive() {
                continue;
            }
            let painter = ui.painter().with_clip_rect(pane.clip);
            let resp = ui.interact(area, egui::Id::unique(("analysis-roi", i)), Sense::drag());
            self.roi.show(ui, &painter, &xf, &resp);
            if self.drawing && resp.drag_stopped() {
                self.drawing = false;
            }
        }
        out
    }

    /// "Sending to host:port" at the bottom left while the panel is closed.
    fn sending_pill(&self, ui: &Ui, core: &Core) {
        #[cfg(feature = "ai")]
        if let Some(server) = core.ai.sending_to() {
            use egui::{CornerRadius, FontId, Pos2, Vec2};
            let r = ui.max_rect();
            let text = format!("{}  Sending to {server}", I::WIFI_HIGH);
            let g = ui.painter().layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
            // Top right, below the mode toolbars and clear of the transport bars.
            let size = g.size() + Vec2::new(18.0, 10.0);
            let rect = Rect::from_min_size(Pos2::new(r.max.x - size.x - 14.0, r.min.y + 64.0), size);
            ui.painter().rect_filled(rect, CornerRadius::same(12), core.pal.accent.gamma_multiply(0.9));
            ui.painter().galley(rect.min + Vec2::new(9.0, 5.0), g, Color32::WHITE);
        }
        #[cfg(not(feature = "ai"))]
        let _ = (ui, core);
    }
}
