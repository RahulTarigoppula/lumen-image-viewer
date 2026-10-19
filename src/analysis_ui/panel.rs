//! The Analysis panel shell: header, the Metrics and AI tabs, the region of
//! interest shared by both (R draws it on the image while the panel is
//! open, Esc clears it), and the overlay pass that draws the ROI and the AI
//! finding boxes over the active mode's images.

use egui::{Align, Color32, Layout, Margin, Rect, RichText, Sense, Ui};

use super::canvas::{self, Slot};
use super::metrics_view::MetricsCtx;
use super::Subject;
use crate::app::Core;
use crate::ui::icons as I;
use crate::ui::roi::{ImageTransform, RoiEditor, RoiOptions};

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

/// Narrowest the panel gets (its rows wrap below this).
const PANEL_MIN: f32 = 280.0;

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
    /// Run the Metrics tab's selection once the panel has the subject (Shift+Q).
    run_metrics: bool,
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
            // Metrics give an answer in one click without any setup.
            tab: Tab::Metrics,
            use_roi: false,
            roi: RoiEditor::new((1.0, 1.0), opts),
            drawing: false,
            run_ai: false,
            run_metrics: false,
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
        self.run_metrics = true;
    }

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
        let esc = (self.drawing || self.roi.rect().is_some()) && !egui::Popup::is_any_open(ctx);
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
        // At most 40% of the window, so the modes' toolbars keep their room
        // (and their overflow menus) in narrow windows.
        let window = ui.ctx().content_rect().width();
        let max = (window * 0.4).clamp(PANEL_MIN, 620.0);
        egui::Panel::right("analysis-panel")
            .resizable(true)
            .default_size(360.0_f32.min(max))
            .size_range(PANEL_MIN..=max)
            .frame(egui::Frame::NONE.fill(p.panel).inner_margin(Margin { left: 12, right: 4, top: 10, bottom: 8 }))
            .show(ui, |ui| self.contents(ui, core, subject, &mut out));
        if std::mem::take(&mut self.run_ai) {
            #[cfg(feature = "ai")]
            {
                let roi = self.roi_pixels();
                let metrics = core.metrics.metric_context(subject, roi);
                self.ai.run(core, subject, roi, metrics);
            }
        }
        out
    }

    fn contents(&mut self, ui: &mut Ui, core: &mut Core, subject: &Subject, out: &mut Vec<PanelAction>) {
        #[cfg(not(feature = "ai"))]
        let _ = &out;
        let p = core.pal;
        // Content keeps a right margin, so the scroll bar never covers it.
        let pad = Margin { left: 0, right: 8, top: 0, bottom: 0 };
        egui::Frame::NONE.inner_margin(pad).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(I::CHART_LINE).size(15.0).color(p.accent));
                ui.label(RichText::new("Analysis").font(crate::ui::theme::text::title()).color(p.text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let tip = format!("Close ({})", crate::ui::commands::Command::ToggleAnalysis.shortcut());
                    if crate::ui::widgets::icon_button_sized(ui, I::X, &tip, false, 24.0, 14.0).clicked() {
                        self.toggle();
                    }
                    super::network_badge(ui, core);
                });
            });
            ui.add_space(4.0);
            let tabs: &[(&str, Tab, &str)] = &[
                ("Metrics", Tab::Metrics, "Measure the picture on this computer"),
                #[cfg(feature = "ai")]
                ("AI review", Tab::Ai, "Ask your own vision model server for an opinion"),
            ];
            let labels: Vec<&str> = tabs.iter().map(|t| t.0).collect();
            let tips: Vec<&str> = tabs.iter().map(|t| t.2).collect();
            let mut idx = tabs.iter().position(|t| t.1 == self.tab).unwrap_or(0);
            if crate::ui::widgets::segmented_with_tips(ui, &mut idx, &labels, &tips) {
                self.tab = tabs[idx].1;
            }
            ui.add_space(2.0);
            self.scope_row(ui, core, subject);
        });
        ui.add_space(2.0);
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, p.border));
        ui.add_space(8.0);
        egui::ScrollArea::vertical().id_salt(("analysis-tab", self.tab)).auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::NONE.inner_margin(pad).show(ui, |ui| {
                ui.set_width(ui.available_width());
                match self.tab {
                    Tab::Metrics => self.metrics_tab(ui, core, subject),
                    #[cfg(feature = "ai")]
                    Tab::Ai => {
                        let roi = self.roi_pixels();
                        let metrics = core.metrics.metric_context(subject, roi);
                        self.ai.ui(ui, core, subject, roi, metrics, out);
                    }
                }
            });
        });
    }

    /// What is analyzed: the whole picture, or the region drawn on it
    /// (R draws one, Esc clears it), in one compact line.
    fn scope_row(&mut self, ui: &mut Ui, core: &Core, subject: &Subject) {
        let p = core.pal;
        let whole = match subject.kind {
            k if k.is_video() => "Whole frame",
            k if k.is_pair() => "Whole image, B against A",
            _ => "Whole image",
        };
        let rect = self.roi.pixel_rect().filter(|_| self.use_roi && !self.drawing);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let (text, color) = match rect {
                _ if self.drawing => ("Drag on the image to select a region".to_string(), p.accent),
                Some((_, _, w, h)) => (format!("Region {w} x {h} px"), p.text),
                None => (whole.to_string(), p.muted),
            };
            ui.label(RichText::new(I::SELECTION).size(13.0).color(color));
            let l = ui.add(egui::Label::new(RichText::new(text).size(crate::ui::theme::text::SMALL).color(color)).truncate());
            if let Some((x, y, _, _)) = rect {
                l.on_hover_text(format!("At {x}, {y} in A's pixels"));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let link = |ui: &mut Ui, text: &str, tip: &str| ui.add(egui::Button::new(RichText::new(text).size(crate::ui::theme::text::SMALL).color(p.accent)).frame(false)).on_hover_text(tip).clicked();
                if self.drawing {
                    if link(ui, "Cancel", "Esc") {
                        self.drawing = false;
                        self.use_roi = self.roi.rect().is_some();
                    }
                } else if rect.is_some() {
                    if link(ui, "Clear", "Analyze the whole picture again (Esc)") {
                        self.roi.clear();
                        self.use_roi = false;
                    }
                    if link(ui, "Redraw", "R") {
                        self.roi.clear();
                        self.drawing = true;
                        self.use_roi = true;
                    }
                } else if link(ui, "Select region", "Analyze only part of the picture (R, then drag)") {
                    self.roi.clear();
                    self.drawing = true;
                    self.use_roi = true;
                }
            });
        });
    }

    /// The Metrics tab: local objective metrics on the subject (and the ROI).
    fn metrics_tab(&mut self, ui: &mut Ui, core: &mut Core, subject: &Subject) {
        let bits = subject.a.as_ref().and_then(|a| core.loader.get_ready(&a.path)).map(|l| l.decoded.bit_depth).unwrap_or(8);
        let pal = core.pal;
        let roi = self.roi_pixels().map(|(x, y, w, h)| lumen_analysis::Roi::new(x, y, w, h));
        let ctx = MetricsCtx { pal: &pal, subject: core.metrics.subject_of(subject, bits), roi };
        if std::mem::take(&mut self.run_metrics) {
            core.metrics.request_run();
        }
        core.metrics.ui(ui, &ctx);
        super::export_bundle_requested(core, Some(subject));
        match core.metrics.min_roi_size() {
            Some((min, note)) => self.roi.set_min_size(Some(min), note),
            None => self.roi.set_min_size(None, ""),
        }
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
        if self.tab == Tab::Metrics {
            Self::heatmap_overlay(ui, core, subject, &panes);
        }
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

    /// The Metrics tab's heatmap on every pane (B panes in A's pixels, like
    /// the ROI), with a value and A/B pixel readout under the pointer.
    fn heatmap_overlay(ui: &mut Ui, core: &mut Core, subject: &Subject, panes: &[canvas::Pane]) {
        if !core.metrics.overlay_active() {
            return;
        }
        let Some(a_size) = panes.iter().find(|p| p.slot == Slot::A).map(|p| p.xf.size) else { return };
        let img = |i: &Option<super::SubjectImage>| match i.as_ref().map(|i| &i.pixels) {
            Some(super::Pixels::Ready(img)) => Some(img.clone()),
            _ => None,
        };
        let (a, b) = (img(&subject.a), img(&subject.b));
        // B may have its own size: look its pixel up proportionally.
        let pixels = |x: f32, y: f32| {
            let pa = a.as_ref().and_then(|a| super::adapters::pixel_at(a, x, y));
            let pb = b.as_ref().and_then(|b| super::adapters::pixel_at(b, x * b.width() as f32 / a_size.x, y * b.height() as f32 / a_size.y));
            (pa, pb)
        };
        let hover = ui.input(|i| i.pointer.hover_pos()).filter(|_| !crate::ui::pointer_over_overlay(ui.ctx()) && !egui::Popup::is_any_open(ui.ctx()));
        for pane in panes {
            let xf = ImageTransform { dest: pane.xf.dest, size: a_size };
            let painter = ui.painter().with_clip_rect(pane.clip);
            let h = hover.filter(|p| pane.clip.contains(*p));
            core.metrics.paint_heatmap(ui, &painter, &xf, h, &pixels);
        }
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
