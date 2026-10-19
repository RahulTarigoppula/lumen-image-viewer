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
            f.pass = pass;
            f.panes.clear();
        }
        f.panes.push(Pane { slot, xf: ImageTransform::new(dest, size), clip });
    });
}

/// The panes published so far this frame.
pub fn panes(ctx: &Context) -> Vec<Pane> {
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|d| d.get_temp::<Frame>(id())).filter(|f| f.pass == pass).map(|f| f.panes).unwrap_or_default()
}
