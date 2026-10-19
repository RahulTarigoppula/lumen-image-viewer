//! Where images are on screen this frame. Each mode calls [`publish`] for
//! every image it draws (a viewer image, a compare pane, a video frame), and
//! the analysis overlay reads the list after the mode's UI ran, so ROI and
//! finding boxes line up with the image whatever the zoom, pan or layout.
//! Entries only live for the frame they were published in.

use crate::ui::roi::ImageTransform;
use egui::{Context, Id, Rect};

/// Which subject image a pane shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    A,
    B,
}

/// One image drawn this frame.
#[derive(Clone, Copy, Debug)]
pub struct Pane {
    pub slot: Slot,
    /// Image pixels (of the displayed image) to screen points.
    pub xf: ImageTransform,
    /// The part of the screen the image may cover (pane, wipe half...).
    pub clip: Rect,
}

impl Pane {
    /// The other image drawn at the same place, beyond a divider (wipe or
    /// split view): A and B share the screen, each visible on its side of
    /// the line, so a point of one image lies on the same point of the
    /// other.
    pub fn twin<'a>(&self, panes: &'a [Pane]) -> Option<&'a Pane> {
        let close = |a: Rect, b: Rect| (a.min - b.min).length() <= 2.0 && (a.max - b.max).length() <= 2.0;
        panes.iter().find(|o| o.slot != self.slot && close(o.xf.dest, self.xf.dest) && !o.clip.intersect(self.clip).is_positive())
    }
}

#[derive(Clone, Default)]
struct Frame {
    pass: u64,
    panes: Vec<Pane>,
    /// Controls drawn over the images (the viewer's floating toolbar).
    occluders: Vec<Rect>,
}

fn id() -> Id {
    Id::unique("analysis-canvas-panes")
}

/// Records that image `slot`, `size` pixels, is drawn into `dest`, visible
/// inside `clip`.
pub fn publish(ctx: &Context, slot: Slot, dest: Rect, size: (u32, u32), clip: Rect) {
    if !dest.is_positive() || size.0 == 0 || size.1 == 0 {
        return;
    }
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| {
        let f: &mut Frame = d.get_temp_mut_or_default(id());
        if f.pass != pass {
            *f = Frame { pass, ..Default::default() };
        }
        f.panes.push(Pane { slot, xf: ImageTransform::new(dest, size), clip });
    });
}

/// Records a control drawn over the images this frame (the viewer's
/// floating toolbar), which overlays such as heatmaps leave uncovered.
pub fn occlude(ctx: &Context, rect: Rect) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| {
        let f: &mut Frame = d.get_temp_mut_or_default(id());
        if f.pass != pass {
            *f = Frame { pass, ..Default::default() };
        }
        f.occluders.push(rect);
    });
}

/// `clip` without the controls drawn over the images: up to four rectangles
/// per control (above, below, left and right of it).
pub fn uncovered(ctx: &Context, clip: Rect) -> Vec<Rect> {
    let pass = ctx.cumulative_pass_nr();
    let occluders = ctx.data(|d| d.get_temp::<Frame>(id())).filter(|f| f.pass == pass).map(|f| f.occluders).unwrap_or_default();
    let mut parts = vec![clip];
    for o in occluders {
        parts = parts.into_iter().flat_map(|r| subtract(r, o)).collect();
    }
    parts
}

/// `r` minus `o`, as rectangles.
fn subtract(r: Rect, o: Rect) -> Vec<Rect> {
    let o = o.intersect(r);
    if !o.is_positive() {
        return vec![r];
    }
    let v = [
        Rect::from_min_max(r.min, egui::pos2(r.max.x, o.min.y)),
        Rect::from_min_max(egui::pos2(r.min.x, o.max.y), r.max),
        Rect::from_min_max(egui::pos2(r.min.x, o.min.y), egui::pos2(o.min.x, o.max.y)),
        Rect::from_min_max(egui::pos2(o.max.x, o.min.y), egui::pos2(r.max.x, o.max.y)),
    ];
    v.into_iter().filter(|x| x.is_positive()).collect()
}

/// The panes published so far this frame.
pub fn panes(ctx: &Context) -> Vec<Pane> {
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|d| d.get_temp::<Frame>(id())).filter(|f| f.pass == pass).map(|f| f.panes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtracting_a_toolbar_leaves_the_rest() {
        let view = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        let bar = Rect::from_min_max(egui::pos2(30.0, 80.0), egui::pos2(70.0, 95.0));
        let parts = subtract(view, bar);
        assert_eq!(parts.len(), 4);
        let area: f32 = parts.iter().map(|r| r.area()).sum();
        assert_eq!(area, view.area() - bar.area());
        assert!(parts.iter().all(|r| !r.intersects(bar.shrink(0.5))));
        // Nothing to take away.
        assert_eq!(subtract(view, Rect::from_min_max(egui::pos2(200.0, 0.0), egui::pos2(210.0, 5.0))), vec![view]);
    }
}
