//! Files dragged onto the window from Explorer or another program.
//!
//! Lumen registers its own OLE drop target in place of winit's, so the cursor
//! shows whether a drop moves or copies, and the position and the Ctrl /
//! Shift keys of a drop are known. The files still reach egui as hovered and
//! dropped files (`App::raw_input_hook`); the browser notes where its folders
//! are on screen ([`Zone`]) and [`plan`] decides what a drop there does, the
//! way Explorer would.

use crate::fileops::{self, EFFECT_COPY, EFFECT_LINK, EFFECT_MOVE, EFFECT_NONE};
use egui::{Pos2, Rect};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use windows::core::{implement, Ref, Result as WinResult};
use windows::Win32::Foundation::{HWND, POINT, POINTL};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::HDROP;

/// What dropping files onto a folder does with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropOp {
    Move,
    Copy,
}

/// A place in the browser that takes dropped files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Zone {
    /// A folder tile, a folder row of the list or a row of the folder tree.
    Folder(PathBuf),
    /// The empty area of the grid or list: the folder shown (with Ctrl or Shift only).
    Background(PathBuf),
}

/// What a drop at some point does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropPlan {
    /// Copy or move `files` into `dest`; `background`: dropped on the empty
    /// area of the folder shown.
    Transfer { files: Vec<PathBuf>, dest: PathBuf, op: DropOp, background: bool },
    /// Over a folder that already holds every file: nothing happens.
    Nothing,
    /// Not over a folder: the files are opened, as before.
    Open,
}

/// Files dragged over the window from another program.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OsDrag {
    pub files: Vec<PathBuf>,
    /// The pointer in window pixels (client area), when known.
    pub px: Option<(f32, f32)>,
    pub ctrl: bool,
    pub shift: bool,
    /// The effects the source allows (copy, move and link bits; 0 = unknown).
    pub allowed: u32,
}

impl OsDrag {
    /// The pointer in points (egui's coordinates).
    pub fn pos(&self, pixels_per_point: f32) -> Option<Pos2> {
        let ppp = if pixels_per_point > 0.0 { pixels_per_point } else { 1.0 };
        self.px.map(|(x, y)| Pos2::new(x / ppp, y / ppp))
    }
}

/// What the browser shows while files from another program are dragged over it.
#[derive(Clone, Debug, PartialEq)]
pub struct Hover {
    /// The pointer, in points.
    pub pos: Pos2,
    /// Where a drop would go (the last frame's answer), and whether that is
    /// the empty area of the folder shown.
    pub target: Option<(PathBuf, bool)>,
}

impl Hover {
    /// The folder `path` drawn at `rect` is under the pointer and takes the drop.
    pub fn on_folder(&self, path: &Path, rect: Rect) -> bool {
        rect.contains(self.pos) && self.target.as_ref().is_some_and(|(d, bg)| !bg && d == path)
    }

    /// A drop on the empty area `rect` goes into the folder shown.
    pub fn on_background(&self, rect: Rect) -> bool {
        rect.contains(self.pos) && self.target.as_ref().is_some_and(|(_, bg)| *bg)
    }
}

/// Explorer's rule for a drop onto a folder: Ctrl copies, Shift moves;
/// otherwise files on the folder's drive are moved and files from another
/// drive are copied. (Ctrl+Shift, a shortcut in Explorer, copies.)
pub fn drop_op(files: &[PathBuf], dest: &Path, ctrl: bool, shift: bool) -> DropOp {
    if ctrl {
        return DropOp::Copy;
    }
    if shift {
        return DropOp::Move;
    }
    let drive = volume_key(dest);
    if drive.is_some() && files.iter().all(|f| volume_key(f) == drive) {
        DropOp::Move
    } else {
        DropOp::Copy
    }
}

/// The drive a path is on, "c:" or "\\server\share" in lower case (None for
/// a relative path).
pub fn volume_key(path: &Path) -> Option<String> {
    use std::path::{Component, Prefix};
    match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(format!("{}:", (d as char).to_ascii_lowercase())),
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                Some(format!(r"\\{}\{}", server.to_string_lossy(), share.to_string_lossy()).to_lowercase())
            }
            _ => None,
        },
        _ => None,
    }
}

/// The `files` a drop into `dest` changes: not those already in it, not
/// `dest` itself and not a folder that holds it.
pub fn movable(files: &[PathBuf], dest: &Path) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|f| !f.parent().is_some_and(|p| fileops::same_path(p, dest)) && !fileops::is_within(dest, f))
        .cloned()
        .collect()
}

/// The zone under `pos`: a folder before the empty area it lies on.
pub fn hit(zones: &[(Rect, Zone)], pos: Pos2) -> Option<&Zone> {
    let mut background = None;
    for (r, z) in zones {
        if r.contains(pos) {
            match z {
                Zone::Folder(_) => return Some(z),
                Zone::Background(_) => background = background.or(Some(z)),
            }
        }
    }
    background
}

/// What dropping `drag`'s files at `pos` does, given the zones on screen.
pub fn plan(zones: &[(Rect, Zone)], pos: Option<Pos2>, drag: &OsDrag) -> DropPlan {
    let (dest, background) = match pos.and_then(|q| hit(zones, q)) {
        Some(Zone::Folder(d)) => (d.clone(), false),
        Some(Zone::Background(d)) if drag.ctrl || drag.shift => (d.clone(), true),
        _ => return DropPlan::Open,
    };
    let files = movable(&drag.files, &dest);
    if files.is_empty() {
        return DropPlan::Nothing;
    }
    let op = match (drop_op(&files, &dest, drag.ctrl, drag.shift), drag.allowed & EFFECT_MOVE != 0, drag.allowed & EFFECT_COPY != 0) {
        // The source allows only the other one (files from a read-only place cannot be moved).
        (DropOp::Move, false, true) => DropOp::Copy,
        (DropOp::Copy, true, false) => DropOp::Move,
        (op, _, _) => op,
    };
    DropPlan::Transfer { files, dest, op, background }
}

/// The cursor effect that tells what `plan` does.
pub fn effect_of(plan: &DropPlan) -> u32 {
    match plan {
        DropPlan::Transfer { op: DropOp::Move, .. } => EFFECT_MOVE,
        DropPlan::Transfer { op: DropOp::Copy, .. } | DropPlan::Open => EFFECT_COPY,
        DropPlan::Nothing => EFFECT_NONE,
    }
}

/// `wanted`, or else an effect the source allows (opening dropped files works
/// with any of them).
pub fn allowed_effect(wanted: u32, allowed: u32) -> u32 {
    if wanted == EFFECT_NONE || allowed & wanted != 0 {
        return wanted;
    }
    [EFFECT_COPY, EFFECT_MOVE, EFFECT_LINK].into_iter().find(|e| allowed & e != 0).unwrap_or(EFFECT_NONE)
}

// ------------------------------------------------------------------ shared state

/// What the drop target (called by OLE between frames) and the UI share.
struct Shared {
    hover: Option<OsDrag>,
    dropped: Option<OsDrag>,
    /// The effect the UI chose for the pointer's place ([`show_effect`]).
    effect: u32,
    ctx: Option<egui::Context>,
    installed: bool,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { hover: None, dropped: None, effect: EFFECT_COPY, ctx: None, installed: false });

fn shared() -> MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

fn repaint(ctx: Option<egui::Context>) {
    if let Some(c) = ctx {
        c.request_repaint();
    }
}

/// The files dragged over the window right now (None: no drag).
pub fn hovering() -> Option<OsDrag> {
    shared().hover.clone()
}

/// The files dropped since the last call.
pub fn take_dropped() -> Option<OsDrag> {
    shared().dropped.take()
}

/// Lumen's drop target is registered (winit's hovered and dropped files no longer come).
pub fn installed() -> bool {
    shared().installed
}

/// The cursor to show for the pointer's place: what a drop there would do.
pub fn show_effect(plan: &DropPlan) {
    shared().effect = effect_of(plan);
}

/// Dev hook: act as if files from another program were dragged over the
/// window (`drag`, its pointer in window pixels), dropped (`drop`: where the
/// drag is), or the drag left (neither).
pub fn simulate(drag: Option<OsDrag>, drop: bool) {
    let ctx = {
        let mut guard = shared();
        let s = &mut *guard;
        match (drag, drop) {
            (Some(d), false) => s.hover = Some(d),
            (d, true) => {
                let d = d.or_else(|| s.hover.take());
                s.hover = None;
                s.dropped = d;
            }
            (None, false) => s.hover = None,
        }
        s.ctx.clone()
    };
    repaint(ctx);
}

/// A dropped file handed to egui (as egui-winit's own).
#[derive(Debug)]
pub struct DroppedPath(pub PathBuf);

impl egui::DroppedFile for DroppedPath {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

// ------------------------------------------------------------------ OLE drop target

/// Register Lumen's drop target for the main window `hwnd`, in place of winit's.
pub fn install(hwnd: isize, ctx: &egui::Context) -> bool {
    shared().ctx = Some(ctx.clone());
    let target: IDropTarget = Target { hwnd }.into();
    let h = HWND(hwnd as _);
    // SAFETY: `h` is the main window, created on this (the UI) thread, where
    // winit already initialized OLE. Registering keeps its own reference.
    let res = unsafe {
        let _ = RevokeDragDrop(h);
        RegisterDragDrop(h, &target)
    };
    match res {
        Ok(()) => {
            shared().installed = true;
            true
        }
        Err(e) => {
            log::warn!("could not register the file drop target: {e}");
            false
        }
    }
}

#[implement(IDropTarget)]
struct Target {
    hwnd: isize,
}

impl Target {
    /// `pt` (screen pixels) in the window's client area.
    fn client(&self, pt: &POINTL) -> Option<(f32, f32)> {
        let mut p = POINT { x: pt.x, y: pt.y };
        // SAFETY: a plain in / out parameter; the window outlives its drop target.
        unsafe { ScreenToClient(HWND(self.hwnd as _), &mut p) }.as_bool().then_some((p.x as f32, p.y as f32))
    }

    /// DragEnter / DragOver: note the pointer, the keys and the effects the
    /// source allows, and answer with the effect the UI chose for that place.
    fn over(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) {
        let px = self.client(pt);
        let (ctrl, shift) = (keys.0 & MK_CONTROL.0 != 0, keys.0 & MK_SHIFT.0 != 0);
        // SAFETY: OLE passes a valid pointer (checked anyway): in, the effects
        // the source allows; out, the effect to show.
        let allowed = if effect.is_null() { 0 } else { unsafe { (*effect).0 } };
        let (answer, ctx) = {
            let mut guard = shared();
            let s = &mut *guard;
            match s.hover.as_mut() {
                Some(h) => {
                    let changed = h.px != px || h.ctrl != ctrl || h.shift != shift;
                    h.px = px;
                    h.ctrl = ctrl;
                    h.shift = shift;
                    h.allowed = allowed;
                    (allowed_effect(s.effect, allowed), if changed { s.ctx.clone() } else { None })
                }
                None => (EFFECT_NONE, None),
            }
        };
        if !effect.is_null() {
            // SAFETY: as above.
            unsafe { *effect = DROPEFFECT(answer) };
        }
        repaint(ctx);
    }
}

/// The paths a drag carries (none when it holds no files).
fn file_list(data: &IDataObject) -> Vec<PathBuf> {
    let format = FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
    // SAFETY: asks for a global memory block, released right after it is read.
    unsafe {
        let Ok(mut medium) = data.GetData(&format) else { return Vec::new() };
        let files = if medium.tymed == TYMED_HGLOBAL.0 as u32 { fileops::hdrop_files(HDROP(medium.u.hGlobal.0)) } else { Vec::new() };
        ReleaseStgMedium(&mut medium);
        files
    }
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(&self, pdataobj: Ref<'_, IDataObject>, grfkeystate: MODIFIERKEYS_FLAGS, pt: &POINTL, pdweffect: *mut DROPEFFECT) -> WinResult<()> {
        let files = pdataobj.as_ref().map(file_list).unwrap_or_default();
        {
            let mut s = shared();
            s.hover = (!files.is_empty()).then(|| OsDrag { files, ..Default::default() });
            // Until the UI has seen where the pointer is: copy, as before.
            s.effect = EFFECT_COPY;
        }
        self.over(grfkeystate, pt, pdweffect);
        Ok(())
    }

    fn DragOver(&self, grfkeystate: MODIFIERKEYS_FLAGS, pt: &POINTL, pdweffect: *mut DROPEFFECT) -> WinResult<()> {
        self.over(grfkeystate, pt, pdweffect);
        Ok(())
    }

    fn DragLeave(&self) -> WinResult<()> {
        let ctx = {
            let mut s = shared();
            s.hover = None;
            s.ctx.clone()
        };
        repaint(ctx);
        Ok(())
    }

    fn Drop(&self, pdataobj: Ref<'_, IDataObject>, grfkeystate: MODIFIERKEYS_FLAGS, pt: &POINTL, pdweffect: *mut DROPEFFECT) -> WinResult<()> {
        let files = pdataobj.as_ref().map(file_list).unwrap_or_default();
        let px = self.client(pt);
        let (ctrl, shift) = (grfkeystate.0 & MK_CONTROL.0 != 0, grfkeystate.0 & MK_SHIFT.0 != 0);
        // SAFETY: OLE passes a valid pointer (checked anyway).
        let allowed = if pdweffect.is_null() { 0 } else { unsafe { (*pdweffect).0 } };
        let (effect, ctx) = {
            let mut s = shared();
            s.hover = None;
            let effect = allowed_effect(s.effect, allowed);
            if !files.is_empty() {
                s.dropped = Some(OsDrag { files, px, ctrl, shift, allowed });
            }
            (effect, s.ctx.clone())
        };
        // Lumen moves the files itself (an optimized move), so the source is
        // told "none": it must not delete anything (Shell docs, "Handling
        // Optimized Move Operations").
        if !pdweffect.is_null() {
            // SAFETY: as above.
            unsafe { *pdweffect = DROPEFFECT(if effect == EFFECT_MOVE { EFFECT_NONE } else { effect }) };
        }
        repaint(ctx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn explorer_rule_for_move_or_copy() {
        let files = [p(r"C:\Photos\a.jpg"), p(r"c:\Photos\b.jpg")];
        assert_eq!(drop_op(&files, Path::new(r"C:\Holiday"), false, false), DropOp::Move, "same drive moves");
        assert_eq!(drop_op(&files, Path::new(r"D:\Holiday"), false, false), DropOp::Copy, "other drive copies");
        assert_eq!(drop_op(&files, Path::new(r"C:\Holiday"), true, false), DropOp::Copy, "Ctrl copies");
        assert_eq!(drop_op(&files, Path::new(r"D:\Holiday"), false, true), DropOp::Move, "Shift moves");
        assert_eq!(drop_op(&files, Path::new(r"C:\Holiday"), true, true), DropOp::Copy, "Ctrl+Shift copies");
        // One file from another drive: copy them all.
        assert_eq!(drop_op(&[p(r"C:\a.jpg"), p(r"E:\b.jpg")], Path::new(r"C:\x"), false, false), DropOp::Copy);
        // Network shares: the same share moves, another share or server copies.
        assert_eq!(drop_op(&[p(r"\\nas\photos\a.jpg")], Path::new(r"\\NAS\Photos\2024"), false, false), DropOp::Move);
        assert_eq!(drop_op(&[p(r"\\nas\photos\a.jpg")], Path::new(r"\\nas\backup"), false, false), DropOp::Copy);
        assert_eq!(drop_op(&[p(r"\\?\C:\a.jpg")], Path::new(r"C:\x"), false, false), DropOp::Move, "long path prefix");
        assert_eq!(drop_op(&[p("a.jpg")], Path::new("x"), false, false), DropOp::Copy, "unknown drives copy");
        assert_eq!(volume_key(Path::new(r"\\?\UNC\Nas\Photos\a")), Some(r"\\nas\photos".into()));
        assert_eq!(volume_key(Path::new(r"d:\")), Some("d:".into()));
    }

    #[test]
    fn files_already_in_the_folder_are_left_out() {
        let files = [p(r"C:\Photos\a.jpg"), p(r"C:\Other\b.jpg"), p(r"C:\Photos"), p(r"C:\Photos\Holiday")];
        // a.jpg and the Holiday folder are in C:\Photos already (case and a
        // trailing separator do not matter); C:\Photos is the target itself.
        assert_eq!(movable(&files, Path::new(r"c:\photos\")), vec![p(r"C:\Other\b.jpg")]);
        // A folder never goes into itself or a folder inside it.
        assert_eq!(movable(&[p(r"C:\Photos")], Path::new(r"C:\Photos\Holiday\2024")), Vec::<PathBuf>::new());
        assert_eq!(movable(&[p(r"C:\Photos")], Path::new(r"C:\Photos2")), vec![p(r"C:\Photos")]);
    }

    #[test]
    fn drop_plans() {
        let r = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1));
        let zones = vec![
            (r(0.0, 0.0, 800.0, 600.0), Zone::Background(p(r"C:\Photos"))),
            (r(10.0, 10.0, 100.0, 100.0), Zone::Folder(p(r"C:\Photos\Holiday"))),
            (r(0.0, 700.0, 200.0, 720.0), Zone::Folder(p(r"D:\Backup"))),
        ];
        fn drag(files: &[&str], ctrl: bool, shift: bool) -> OsDrag {
            OsDrag { files: files.iter().map(|f| p(f)).collect(), px: None, ctrl, shift, allowed: EFFECT_COPY | EFFECT_MOVE | EFFECT_LINK }
        }
        let at = |x: f32, y: f32| Some(Pos2::new(x, y));
        let outside = drag(&[r"C:\Downloads\a.jpg"], false, false);
        // Onto a folder tile on the same drive: move.
        assert_eq!(
            plan(&zones, at(50.0, 50.0), &outside),
            DropPlan::Transfer { files: vec![p(r"C:\Downloads\a.jpg")], dest: p(r"C:\Photos\Holiday"), op: DropOp::Move, background: false }
        );
        // Onto a tree row of another drive: copy.
        assert!(matches!(plan(&zones, at(50.0, 710.0), &outside), DropPlan::Transfer { op: DropOp::Copy, .. }));
        // On the empty area: open, unless Ctrl (copy) or Shift (move) is held.
        assert_eq!(plan(&zones, at(400.0, 300.0), &outside), DropPlan::Open);
        assert_eq!(
            plan(&zones, at(400.0, 300.0), &drag(&[r"C:\Downloads\a.jpg"], true, false)),
            DropPlan::Transfer { files: vec![p(r"C:\Downloads\a.jpg")], dest: p(r"C:\Photos"), op: DropOp::Copy, background: true }
        );
        assert!(matches!(plan(&zones, at(400.0, 300.0), &drag(&[r"E:\a.jpg"], false, true)), DropPlan::Transfer { op: DropOp::Move, background: true, .. }));
        // Outside every zone, or the position is unknown: open.
        assert_eq!(plan(&zones, at(900.0, 650.0), &outside), DropPlan::Open);
        assert_eq!(plan(&zones, None, &outside), DropPlan::Open);
        // Files dropped on the folder they are in: nothing.
        assert_eq!(plan(&zones, at(50.0, 50.0), &drag(&[r"C:\Photos\Holiday\a.jpg"], false, false)), DropPlan::Nothing);
        assert_eq!(plan(&zones, at(400.0, 300.0), &drag(&[r"C:\Photos\a.jpg"], true, false)), DropPlan::Nothing);
        // A source that only allows copying: copy even on the same drive.
        let copy_only = OsDrag { allowed: EFFECT_COPY, ..outside.clone() };
        assert!(matches!(plan(&zones, at(50.0, 50.0), &copy_only), DropPlan::Transfer { op: DropOp::Copy, .. }));
        // The window pixels of a drag in points.
        let px = OsDrag { px: Some((300.0, 150.0)), ..outside };
        assert_eq!(px.pos(1.5), Some(Pos2::new(200.0, 100.0)));
        assert_eq!(px.pos(0.0), Some(Pos2::new(300.0, 150.0)));
    }

    #[test]
    fn cursor_effects() {
        let all = EFFECT_COPY | EFFECT_MOVE | EFFECT_LINK;
        assert_eq!(effect_of(&DropPlan::Open), EFFECT_COPY);
        assert_eq!(effect_of(&DropPlan::Nothing), EFFECT_NONE);
        assert_eq!(allowed_effect(EFFECT_MOVE, all), EFFECT_MOVE);
        assert_eq!(allowed_effect(EFFECT_COPY, EFFECT_LINK), EFFECT_LINK, "opening works with any effect");
        assert_eq!(allowed_effect(EFFECT_NONE, all), EFFECT_NONE);
        assert_eq!(allowed_effect(EFFECT_COPY, 0), EFFECT_NONE);
        let hover = Hover { pos: Pos2::new(50.0, 50.0), target: Some((p(r"C:\A"), false)) };
        let rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(100.0, 100.0));
        assert!(hover.on_folder(Path::new(r"C:\A"), rect));
        assert!(!hover.on_folder(Path::new(r"C:\B"), rect));
        assert!(!hover.on_background(rect));
        assert!(!hover.on_folder(Path::new(r"C:\A"), Rect::from_min_max(Pos2::new(60.0, 0.0), Pos2::new(100.0, 100.0))));
    }
}
