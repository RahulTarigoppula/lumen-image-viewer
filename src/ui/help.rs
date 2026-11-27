//! The keyboard shortcuts window (F1): its rows, generated from the active
//! keymap and the mouse settings, and its search.

use super::keymap::{self, KeyMode, Keymap};
use super::palette::fuzzy_score;
use crate::settings::WheelAction;

/// One row: a command and its keys, or a mouse action and what it does.
pub struct Row {
    /// The command's icon (shortcut rows only).
    pub icon: Option<&'static str>,
    /// What the row does.
    pub label: String,
    /// The keys ("Ctrl+P / F12") or the mouse action ("Ctrl + wheel").
    pub keys: String,
}

pub struct Section {
    pub title: String,
    pub rows: Vec<Row>,
    /// Mouse rows show the action in words, not as keycaps.
    pub mouse: bool,
}

/// Every section: the shortcuts of each keymap mode, then the mouse in the
/// browser, the viewer (following the wheel setting) and Compare.
pub fn sections(km: &Keymap, wheel: WheelAction) -> Vec<Section> {
    let mut out = Vec::new();
    for mode in KeyMode::ALL {
        let rows = keymap::commands_of(mode)
            .into_iter()
            .filter_map(|c| {
                let keys = keymap::mode_text(km, mode, c);
                (!keys.is_empty()).then(|| Row { icon: Some(c.icon()), label: c.label(), keys })
            })
            .collect();
        out.push(Section { title: mode.label().to_string(), rows, mouse: false });
    }
    for (title, rows) in mouse_rows(wheel) {
        let rows = rows.into_iter().map(|(keys, label)| Row { icon: None, label: label.to_string(), keys: keys.to_string() }).collect();
        out.push(Section { title: title.to_string(), rows, mouse: true });
    }
    out
}

/// What the mouse does in each mode, as (action, effect) pairs. The viewer's
/// wheel rows follow Settings > Viewer > Mouse wheel.
fn mouse_rows(wheel: WheelAction) -> [(&'static str, Vec<(&'static str, &'static str)>); 3] {
    let (wheel_does, ctrl_wheel_does) = match wheel {
        WheelAction::Navigate => ("Next / previous image (change in Settings > Viewer)", "Zoom at the pointer"),
        WheelAction::Zoom => ("Zoom at the pointer (change in Settings > Viewer)", "Next / previous image"),
    };
    [
        (
            "Mouse in the browser",
            vec![
                ("Double click", "Open the image, video or folder"),
                ("Ctrl + click", "Add a file to the selection, or take it out"),
                ("Shift + click", "Select every file from the last one clicked"),
                ("Drag on empty space", "Select the files in a rectangle (Ctrl adds them)"),
                ("Drag onto a folder", "Move the files there (another drive: copy; Shift moves, Ctrl copies)"),
                ("Drag between files", "Arrange the folder by hand"),
                ("Drag out of the window", "Hand the files to Explorer or another program"),
                ("Right click", "Menu for the selected files"),
                ("Ctrl + wheel", "Thumbnail size"),
                ("Back / forward buttons", "Previous / next folder"),
            ],
        ),
        (
            "Mouse in the viewer",
            vec![
                ("Wheel", wheel_does),
                ("Ctrl + wheel", ctrl_wheel_does),
                ("Drag", "Pan a zoomed image"),
                ("Press & hold", "Magnifier loupe (when the image fits the window)"),
                ("Middle click", "Switch between fit and 100%"),
                ("Click a video", "Play / pause it (drag on its bar to seek)"),
                ("Double click", "Back to the browser"),
                ("Right click", "Menu: zoom, rotate, edit, save, share"),
                ("Back / forward buttons", "Previous / next image"),
            ],
        ),
        (
            "Mouse in Compare",
            vec![
                ("Wheel", "Zoom at the pointer (every pane when linked)"),
                ("Drag", "Pan (every pane when linked)"),
                ("Middle drag", "Pan, also while marking up"),
                ("Click", "Pick the pane that Z, rating and the menu act on"),
                ("Double click", "Fit every pane"),
                ("Right click", "Menu of the pane"),
            ],
        ),
    ]
}

/// Scores at or above this come from the query as typed (in the label or the keys).
const STRONG: i32 = 500;

/// How well a row matches `query`: the command palette's fuzzy score of the
/// label, or a hit on the keys ("p", "ctrl+p", "f6", "wheel"). None when
/// neither matches.
pub fn row_score(label: &str, keys: &str, query: &str) -> Option<i32> {
    let q = query.trim();
    if q.is_empty() {
        return Some(0);
    }
    let squash = |s: &str| s.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let qk = squash(q);
    // One shortcut equal to the query is the best hit ("p" finds P, not every
    // Ctrl+P); a longer query may also be part of one ("wheel", "shift+").
    let key_score = keys
        .split(" / ")
        .map(squash)
        .filter_map(|k| {
            if k == qk {
                Some(1100)
            } else if qk.chars().count() >= 2 && k.contains(&qk) {
                Some(900)
            } else {
                None
            }
        })
        .max();
    match (fuzzy_score(label, q), key_score) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The sections with only the rows that match `query`, best first within a
/// section, and without the sections that have none. When some rows contain
/// the query as typed, the loose (letters in order) matches are left out, so
/// "flag" lists the flag commands only.
pub fn filtered(sections: Vec<Section>, query: &str) -> Vec<Section> {
    if query.trim().is_empty() {
        return sections;
    }
    let scored: Vec<(Section, Vec<Option<i32>>)> = sections
        .into_iter()
        .map(|s| {
            let scores = s.rows.iter().map(|r| row_score(&r.label, &r.keys, query)).collect();
            (s, scores)
        })
        .collect();
    let strong = scored.iter().any(|(_, sc)| sc.iter().flatten().any(|&v| v >= STRONG));
    let min = if strong { STRONG } else { i32::MIN };
    scored
        .into_iter()
        .filter_map(|(mut s, scores)| {
            let mut keep: Vec<(Row, i32)> = s.rows.drain(..).zip(scores).filter_map(|(r, sc)| sc.filter(|&v| v >= min).map(|v| (r, v))).collect();
            if keep.is_empty() {
                return None;
            }
            // Stable: equal scores keep the keymap's order.
            keep.sort_by(|a, b| b.1.cmp(&a.1));
            s.rows = keep.into_iter().map(|(r, _)| r).collect();
            Some(s)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::keymap::Preset;

    fn labels(sections: &[Section], title: &str) -> Vec<String> {
        sections.iter().find(|s| s.title == title).map(|s| s.rows.iter().map(|r| r.label.clone()).collect()).unwrap_or_default()
    }

    #[test]
    fn search_finds_commands_by_name_and_by_key() {
        let km = Keymap::preset(Preset::Lumen);
        let all = || sections(&km, WheelAction::Navigate);
        let print = filtered(all(), "print");
        assert_eq!(labels(&print, "Browser").first().map(String::as_str), Some("Print..."));
        assert_eq!(labels(&print, "Viewer").first().map(String::as_str), Some("Print..."));
        // By its keys: Ctrl+P prints in the browser and the viewer.
        let ctrl_p = filtered(all(), "ctrl+p");
        assert!(labels(&ctrl_p, "Browser").contains(&"Print...".to_string()));
        assert!(labels(&ctrl_p, "Viewer").contains(&"Print...".to_string()));
        // A single key finds the commands on exactly that key first.
        let p = filtered(all(), "p");
        assert_eq!(labels(&p, "Browser").first().map(String::as_str), Some("Compare selected images"));
        // "flag" lists the flag commands, not every label with f, l, a and g in order.
        let flag = filtered(all(), "flag");
        assert!(!flag.is_empty());
        for s in &flag {
            for r in &s.rows {
                assert!(r.label.to_lowercase().contains("flag") || r.keys.to_lowercase().contains("flag"), "{}", r.label);
            }
        }
        // Nothing matches: no sections at all.
        assert!(filtered(all(), "qqqzzz").is_empty());
        // An empty query keeps everything.
        assert_eq!(filtered(all(), " ").len(), all().len());
    }

    #[test]
    fn mouse_rows_follow_the_mode_and_the_wheel_setting() {
        let km = Keymap::preset(Preset::Lumen);
        let rows = |wheel, title: &str| sections(&km, wheel).into_iter().find(|s| s.title == title).map(|s| s.rows).unwrap_or_default();
        let effect = |rows: &[Row], keys: &str| rows.iter().find(|r| r.keys == keys).map(|r| r.label.clone()).unwrap_or_default();
        let browser = rows(WheelAction::Navigate, "Mouse in the browser");
        assert_eq!(effect(&browser, "Back / forward buttons"), "Previous / next folder");
        assert_eq!(effect(&browser, "Double click"), "Open the image, video or folder");
        let viewer = rows(WheelAction::Navigate, "Mouse in the viewer");
        assert!(effect(&viewer, "Wheel").starts_with("Next / previous image"));
        assert_eq!(effect(&viewer, "Ctrl + wheel"), "Zoom at the pointer");
        assert_eq!(effect(&viewer, "Back / forward buttons"), "Previous / next image");
        let zoom = rows(WheelAction::Zoom, "Mouse in the viewer");
        assert!(effect(&zoom, "Wheel").starts_with("Zoom at the pointer"));
        assert_eq!(effect(&zoom, "Ctrl + wheel"), "Next / previous image");
        assert!(!rows(WheelAction::Zoom, "Mouse in Compare").is_empty());
        // Searching mouse rows by their action.
        let wheel = filtered(sections(&km, WheelAction::Navigate), "wheel");
        assert!(wheel.iter().any(|s| s.title == "Mouse in the viewer"));
        assert!(wheel.iter().all(|s| s.mouse || s.rows.iter().all(|r| r.label.to_lowercase().contains("wheel"))));
    }
}
