//! Remappable keyboard shortcuts.
//!
//! A [`Keymap`] holds the bindings of each mode (Global, Browser, Viewer). The
//! defaults of every preset are defined once in [`Keymap::preset`]; user changes
//! are stored in `Settings::key_overrides` as "this command in this mode uses
//! exactly these keys". The active keymap is kept in a process-wide slot so that
//! [`Command::shortcut`] text (menus, F1 help, palette) always matches what the
//! keys actually do.
//!
//! Matching is exact: Ctrl+Shift+S never triggers Ctrl+S, Shift+Space never
//! triggers Space. The only exception is Shift on keys that need Shift to be
//! typed at all on common layouts (`+`, `?`, `:` ...).

use crate::settings::{KeyOverride, Settings};
use crate::ui::commands::Command;
use egui::{Key, KeyboardShortcut, Modifiers};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum KeyMode {
    #[default]
    /// Works everywhere (and while typing, when the shortcut has Ctrl/Alt or is an F-key).
    Global,
    Browser,
    Viewer,
}

impl KeyMode {
    pub const ALL: [KeyMode; 3] = [KeyMode::Global, KeyMode::Browser, KeyMode::Viewer];
    pub fn label(self) -> &'static str {
        match self {
            KeyMode::Global => "Global",
            KeyMode::Browser => "Browser",
            KeyMode::Viewer => "Viewer",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            KeyMode::Global => "global",
            KeyMode::Browser => "browser",
            KeyMode::Viewer => "viewer",
        }
    }
    pub fn from_id(s: &str) -> Option<KeyMode> {
        KeyMode::ALL.into_iter().find(|m| m.id().eq_ignore_ascii_case(s.trim()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum Preset {
    #[default]
    Lumen,
    FastStone,
}

impl Preset {
    pub const ALL: [Preset; 2] = [Preset::Lumen, Preset::FastStone];
    pub fn label(self) -> &'static str {
        match self {
            Preset::Lumen => "Lumen (default)",
            Preset::FastStone => "FastStone compatible",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Preset::Lumen => "lumen",
            Preset::FastStone => "faststone",
        }
    }
    pub fn from_id(s: &str) -> Preset {
        Preset::ALL.into_iter().find(|p| p.id().eq_ignore_ascii_case(s.trim())).unwrap_or_default()
    }
}

pub type Binding = (KeyboardShortcut, Command);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keymap {
    pub global: Vec<Binding>,
    pub browser: Vec<Binding>,
    pub viewer: Vec<Binding>,
}

// ---------------------------------------------------------------- defaults

/// Bindings shared by every preset.
fn common(mode: KeyMode) -> Vec<(&'static str, Command)> {
    use Command::*;
    let mut v: Vec<(&'static str, Command)> = match mode {
        KeyMode::Global => vec![
            ("Ctrl+K", CommandPalette),
            ("F11", ToggleFullscreen),
            ("Ctrl+,", Settings),
            ("F12", Settings),
            ("F1", ShortcutsHelp),
            ("Ctrl+Shift+O", OpenFolder),
            ("Ctrl+O", OpenFile),
            ("Ctrl+Shift+T", ToggleTheme),
            ("Ctrl+Shift+V", OpenVideoMode),
            ("Alt+F4", Exit),
        ],
        KeyMode::Browser => vec![
            ("Enter", OpenViewer),
            ("F5", Refresh),
            ("Backspace", GoUp),
            ("Alt+Up", GoUp),
            ("Alt+Left", GoBack),
            ("Alt+Right", GoForward),
            ("Ctrl+A", SelectAll),
            ("Ctrl+D", SelectNone),
            ("Ctrl+I", InvertSelection),
            ("F7", CopyTo),
            ("F8", MoveTo),
            ("Del", Delete),
            ("F2", Rename),
            ("Ctrl+F2", BatchRename),
            ("Ctrl+Shift+N", NewFolder),
            ("Ctrl+Shift+C", CopyPath),
            ("Ctrl+C", CopyFiles),
            ("Ctrl+V", PasteFiles),
            ("Ctrl+Z", Undo),
            ("Ctrl++", ThumbBigger),
            ("Ctrl+=", ThumbBigger),
            ("Ctrl+-", ThumbSmaller),
            ("Ctrl+F", FocusFilter),
            ("F3", BatchConvert),
            ("Ctrl+Shift+D", ToggleDetails),
            ("Ctrl+Shift+P", TogglePreviewPane),
            ("S", Slideshow),
            ("P", Compare),
            ("Q", ToggleFlag),
            ("\\", ToggleFlag),
            ("L", RotateLeft),
            ("R", RotateRight),
            ("E", OpenWithEditor),
            ("Alt+Enter", Properties),
            ("Alt+I", Properties),
            ("Ctrl+W", SetWallpaper),
            ("W", ShowInExplorer),
            ("Ctrl+Alt+M", OpenGpsMap),
            ("D", EditComment),
            ("Ctrl+Shift+M", CreateMultiPage),
            ("Ctrl+Shift+J", ImageStrip),
            ("Ctrl+Alt+Z", CreateZip),
        ],
        KeyMode::Viewer => vec![
            ("Esc", CloseViewer),
            ("Right", NextImage),
            ("PgDn", NextImage),
            ("Space", NextImage),
            ("Left", PrevImage),
            ("PgUp", PrevImage),
            ("Backspace", PrevImage),
            ("Shift+Space", PrevImage),
            ("Home", FirstImage),
            ("End", LastImage),
            ("+", ZoomIn),
            ("=", ZoomIn),
            ("Ctrl++", ZoomIn),
            ("Ctrl+=", ZoomIn),
            ("-", ZoomOut),
            ("Ctrl+-", ZoomOut),
            ("Ctrl+0", ZoomActual),
            ("/", ZoomFit),
            ("Shift+W", FitWidth),
            ("Shift+H", FitHeight),
            ("Ctrl+Shift+L", ToggleLockZoom),
            ("L", RotateLeft),
            ("R", RotateRight),
            ("H", FlipHorizontal),
            ("V", FlipVertical),
            ("X", Crop),
            ("Ctrl+R", Resize),
            ("Ctrl+Shift+R", CanvasSize),
            ("Ctrl+E", AdjustColors),
            ("Ctrl+L", Levels),
            ("Ctrl+M", Curves),
            ("Ctrl+J", Sharpen),
            ("Ctrl+B", Blur),
            ("Ctrl+Shift+E", Effects),
            ("Ctrl+Shift+A", AutoEnhance),
            ("T", AddText),
            ("Ctrl+Z", Undo),
            ("Ctrl+Y", Redo),
            ("Ctrl+Shift+Z", Redo),
            ("Ctrl+S", Save),
            ("Ctrl+Shift+S", SaveAs),
            ("Ctrl+C", CopyImage),
            ("Ctrl+V", PasteImage),
            ("Ctrl+W", SetWallpaper),
            ("Ctrl+P", Print),
            ("I", ToggleInfo),
            ("Ctrl+H", ToggleHistogram),
            ("M", ToggleMagnifier),
            ("F", ToggleFilmstrip),
            ("K", PlayPause),
            (".", NextFrame),
            ("Ctrl+PgDn", NextFrame),
            (",", PrevFrame),
            ("Ctrl+PgUp", PrevFrame),
            ("E", OpenWithEditor),
            ("Del", Delete),
            ("F2", Rename),
            ("F7", CopyTo),
            ("F8", MoveTo),
            ("S", Slideshow),
            ("P", Compare),
            ("Q", ToggleFlag),
            ("\\", ToggleFlag),
            ("Alt+Enter", Properties),
            ("Alt+I", Properties),
            ("Enter", ToggleFullscreen),
            ("W", ShowInExplorer),
            ("Ctrl+Alt+M", OpenGpsMap),
            ("D", EditComment),
            ("Ctrl+Shift+M", CreateMultiPage),
            ("Ctrl+Shift+J", ImageStrip),
            ("Ctrl+Alt+Z", CreateZip),
        ],
    };
    if mode != KeyMode::Global {
        const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
        const ALT_DIGITS: [&str; 10] = ["Alt+0", "Alt+1", "Alt+2", "Alt+3", "Alt+4", "Alt+5", "Alt+6", "Alt+7", "Alt+8", "Alt+9"];
        for n in 0..=5u8 {
            v.push((DIGITS[n as usize], Rate(n)));
        }
        for n in 1..=4u8 {
            v.push((DIGITS[n as usize + 5], Label(n)));
        }
        v.push((ALT_DIGITS[1], OpenWithEditor));
        for n in 2..=9u8 {
            v.push((ALT_DIGITS[n as usize], OpenWithEditorN(n)));
        }
    }
    v
}

/// Extra bindings of the Lumen preset (keys FastStone uses differently or not at all).
fn lumen_extras(mode: KeyMode) -> Vec<(&'static str, Command)> {
    use Command::*;
    match mode {
        KeyMode::Global => vec![],
        KeyMode::Browser => vec![("Space", ToggleFlag), ("`", ToggleFlag)],
        KeyMode::Viewer => vec![("Z", ZoomActual), ("C", Crop), ("`", ToggleFlag)],
    }
}

/// Extra bindings of the FastStone-compatible preset.
fn faststone_extras(mode: KeyMode) -> Vec<(&'static str, Command)> {
    use Command::*;
    match mode {
        KeyMode::Global => vec![],
        KeyMode::Browser => vec![],
        KeyMode::Viewer => vec![("A", ZoomActual), ("B", ZoomFit)],
    }
}

impl Keymap {
    pub fn preset(p: Preset) -> Keymap {
        let mut km = Keymap::default();
        for mode in KeyMode::ALL {
            let mut list = common(mode);
            list.extend(match p {
                Preset::Lumen => lumen_extras(mode),
                Preset::FastStone => faststone_extras(mode),
            });
            let out = km.bindings_mut(mode);
            for (s, c) in list {
                let sc = parse_shortcut(s).unwrap_or_else(|| panic!("bad default shortcut {s}"));
                out.push((sc, c));
            }
        }
        km
    }

    /// The preset chosen in settings with the user's overrides applied.
    pub fn from_settings(s: &Settings) -> Keymap {
        let mut km = Keymap::preset(Preset::from_id(&s.keymap_preset));
        for o in &s.key_overrides {
            let (Some(mode), Some(cmd)) = (KeyMode::from_id(&o.mode), Command::from_id(&o.command)) else { continue };
            let keys: Vec<KeyboardShortcut> = o.keys.iter().filter_map(|k| parse_shortcut(k)).collect();
            km.set_keys(mode, cmd, keys);
        }
        km
    }

    pub fn bindings(&self, mode: KeyMode) -> &[Binding] {
        match mode {
            KeyMode::Global => &self.global,
            KeyMode::Browser => &self.browser,
            KeyMode::Viewer => &self.viewer,
        }
    }

    pub fn bindings_mut(&mut self, mode: KeyMode) -> &mut Vec<Binding> {
        match mode {
            KeyMode::Global => &mut self.global,
            KeyMode::Browser => &mut self.browser,
            KeyMode::Viewer => &mut self.viewer,
        }
    }

    pub fn keys_for(&self, mode: KeyMode, cmd: Command) -> Vec<KeyboardShortcut> {
        self.bindings(mode).iter().filter(|(_, c)| *c == cmd).map(|(s, _)| *s).collect()
    }

    /// Replace every binding of `cmd` in `mode` with `keys`.
    pub fn set_keys(&mut self, mode: KeyMode, cmd: Command, keys: Vec<KeyboardShortcut>) {
        let list = self.bindings_mut(mode);
        let pos = list.iter().position(|(_, c)| *c == cmd);
        list.retain(|(_, c)| *c != cmd);
        let at = pos.unwrap_or(list.len()).min(list.len());
        for (i, k) in keys.into_iter().enumerate() {
            let k = normalize(k);
            if !list[..].iter().any(|(s, c)| *s == k && *c == cmd) {
                list.insert(at + i, (k, cmd));
            }
        }
    }

    /// Commands bound to exactly this shortcut in `mode`.
    pub fn commands_for(&self, mode: KeyMode, sc: KeyboardShortcut) -> Vec<Command> {
        let sc = normalize(sc);
        let mut v: Vec<Command> = self.bindings(mode).iter().filter(|(s, _)| *s == sc).map(|(_, c)| *c).collect();
        v.dedup();
        v
    }

    /// Other commands that would fire for `sc` when pressed in `mode` (same mode, or
    /// Global vs a mode). Used for the conflict warning in the keyboard editor.
    pub fn conflicts(&self, mode: KeyMode, sc: KeyboardShortcut, cmd: Command) -> Vec<(KeyMode, Command)> {
        let modes: &[KeyMode] = match mode {
            KeyMode::Global => &[KeyMode::Global, KeyMode::Browser, KeyMode::Viewer],
            m => {
                if m == KeyMode::Browser {
                    &[KeyMode::Browser, KeyMode::Global]
                } else {
                    &[KeyMode::Viewer, KeyMode::Global]
                }
            }
        };
        let mut out = Vec::new();
        for &m in modes {
            for c in self.commands_for(m, sc) {
                if !(m == mode && c == cmd) {
                    out.push((m, c));
                }
            }
        }
        out
    }

    /// Find the command for a key press in `mode` (exact modifier match).
    pub fn lookup(&self, mode: KeyMode, key: Key, mods: Modifiers) -> Option<Command> {
        self.bindings(mode).iter().find(|(s, _)| matches(s, key, mods)).map(|(_, c)| *c)
    }

    /// Plain letters bound without modifiers in `mode` (they must not start type-ahead).
    pub fn bound_letters(&self, mode: KeyMode) -> Vec<char> {
        self.bindings(mode)
            .iter()
            .filter(|(s, _)| s.modifiers.is_none())
            .filter_map(|(s, _)| {
                let n = s.logical_key.name();
                let mut ch = n.chars();
                match (ch.next(), ch.next()) {
                    (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
                    _ => None,
                }
            })
            .collect()
    }

    /// Overrides that turn `preset` into this keymap (stored in settings).
    pub fn diff_from(&self, preset: Preset) -> Vec<KeyOverride> {
        let base = Keymap::preset(preset);
        let mut out = Vec::new();
        for mode in KeyMode::ALL {
            for cmd in Command::all() {
                let mine = self.keys_for(mode, cmd);
                if mine != base.keys_for(mode, cmd) {
                    out.push(KeyOverride { mode: mode.id().to_string(), command: cmd.id(), keys: mine.iter().map(|k| format_shortcut(k)).collect() });
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------- active keymap

static ACTIVE: LazyLock<RwLock<Arc<Keymap>>> = LazyLock::new(|| RwLock::new(Arc::new(Keymap::preset(Preset::Lumen))));
static CONTEXT: AtomicU8 = AtomicU8::new(1);
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// Make the keymap described by `settings` the active one.
pub fn install(settings: &Settings) {
    let km = Keymap::from_settings(settings);
    if let Ok(mut g) = ACTIVE.write() {
        *g = Arc::new(km);
    }
}

pub fn active() -> Arc<Keymap> {
    ACTIVE.read().map(|g| g.clone()).unwrap_or_else(|_| Arc::new(Keymap::preset(Preset::Lumen)))
}

/// Which mode's bindings `Command::shortcut()` shows first (set by the app each frame).
pub fn set_context(mode: KeyMode) {
    CONTEXT.store(mode as u8, Ordering::Relaxed);
}

fn context() -> KeyMode {
    match CONTEXT.load(Ordering::Relaxed) {
        0 => KeyMode::Global,
        2 => KeyMode::Viewer,
        _ => KeyMode::Browser,
    }
}

/// True while the keyboard editor waits for a key: app shortcuts are suspended.
pub fn set_capturing(on: bool) {
    CAPTURING.store(on, Ordering::Relaxed);
}

pub fn capturing() -> bool {
    CAPTURING.load(Ordering::Relaxed)
}

/// Shortcut text of `cmd` for menus: current mode first, then Global, then the other mode.
pub fn shortcut_text(cmd: Command) -> String {
    let km = active();
    let ctx = context();
    let order: [KeyMode; 3] = match ctx {
        KeyMode::Viewer => [KeyMode::Viewer, KeyMode::Global, KeyMode::Browser],
        _ => [KeyMode::Browser, KeyMode::Global, KeyMode::Viewer],
    };
    for m in order.iter().take(2) {
        let keys = km.keys_for(*m, cmd);
        if !keys.is_empty() {
            return join_keys(&keys, cmd, *m);
        }
    }
    let keys = km.keys_for(order[2], cmd);
    join_keys(&keys, cmd, order[2])
}

/// Shortcut text of `cmd` in exactly `mode` ("" when unbound).
pub fn mode_text(km: &Keymap, mode: KeyMode, cmd: Command) -> String {
    join_keys(&km.keys_for(mode, cmd), cmd, mode)
}

fn join_keys(keys: &[KeyboardShortcut], cmd: Command, mode: KeyMode) -> String {
    let mut parts: Vec<String> = keys.iter().take(2).map(format_shortcut).collect();
    if cmd == Command::CycleFit && mode == KeyMode::Viewer {
        parts.push("*".into());
    }
    parts.join(" / ")
}

// ---------------------------------------------------------------- matching

/// Keys that need Shift to be typed on common layouts: Shift is ignored for them.
fn shift_free(key: Key) -> bool {
    matches!(key, Key::Plus | Key::Colon | Key::Questionmark | Key::Pipe | Key::Exclamationmark | Key::OpenCurlyBracket | Key::CloseCurlyBracket)
}

/// Ctrl is stored as `command` only, so equal shortcuts compare equal.
pub fn normalize(sc: KeyboardShortcut) -> KeyboardShortcut {
    let m = sc.modifiers;
    let mut n = Modifiers::NONE;
    n.alt = m.alt;
    n.shift = m.shift && !shift_free(sc.logical_key);
    n.command = m.command || m.ctrl || m.mac_cmd;
    KeyboardShortcut::new(n, sc.logical_key)
}

/// Exact match of a key press against a binding.
pub fn matches(sc: &KeyboardShortcut, key: Key, mods: Modifiers) -> bool {
    if key != sc.logical_key {
        return false;
    }
    let want = sc.modifiers;
    let ctrl = mods.ctrl || mods.command || mods.mac_cmd;
    let want_ctrl = want.ctrl || want.command || want.mac_cmd;
    mods.alt == want.alt && ctrl == want_ctrl && (mods.shift == want.shift || shift_free(key))
}

/// Shortcuts that may fire while a text field has keyboard focus.
pub fn works_while_typing(sc: &KeyboardShortcut) -> bool {
    let m = sc.modifiers;
    m.command || m.ctrl || m.alt || is_function_key(sc.logical_key)
}

fn is_function_key(k: Key) -> bool {
    let n = k.name();
    n.len() >= 2 && n.starts_with('F') && n[1..].chars().all(|c| c.is_ascii_digit())
}

/// Remove matching key presses from this frame's input and return their commands.
/// `lookup` gets each pressed key with its modifiers.
pub fn consume(input: &mut egui::InputState, mut lookup: impl FnMut(Key, Modifiers) -> Option<Command>) -> Vec<Command> {
    let mut out = Vec::new();
    input.events.retain(|e| {
        if let egui::Event::Key { key, pressed: true, modifiers, .. } = e {
            if let Some(c) = lookup(*key, *modifiers) {
                out.push(c);
                return false;
            }
        }
        true
    });
    out
}

// ---------------------------------------------------------------- text form

/// ASCII key name used in menus and settings.json.
pub fn key_label(k: Key) -> &'static str {
    match k {
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::PageUp => "PgUp",
        Key::PageDown => "PgDn",
        Key::Escape => "Esc",
        Key::Delete => "Del",
        Key::Insert => "Ins",
        Key::Plus => "+",
        Key::Minus => "-",
        Key::Equals => "=",
        Key::Comma => ",",
        Key::Period => ".",
        Key::Slash => "/",
        Key::Backslash => "\\",
        Key::Backtick => "`",
        Key::Semicolon => ";",
        Key::Quote => "'",
        Key::OpenBracket => "[",
        Key::CloseBracket => "]",
        Key::Colon => ":",
        Key::Pipe => "|",
        Key::Questionmark => "?",
        Key::Exclamationmark => "!",
        Key::OpenCurlyBracket => "{",
        Key::CloseCurlyBracket => "}",
        other => other.name(),
    }
}

pub fn format_shortcut(sc: &KeyboardShortcut) -> String {
    let m = sc.modifiers;
    let mut s = String::new();
    if m.command || m.ctrl || m.mac_cmd {
        s += "Ctrl+";
    }
    if m.alt {
        s += "Alt+";
    }
    if m.shift {
        s += "Shift+";
    }
    s += key_label(sc.logical_key);
    s
}

pub fn parse_key(name: &str) -> Option<Key> {
    if name.is_empty() {
        return None;
    }
    Key::ALL
        .iter()
        .copied()
        .find(|k| key_label(*k) == name || k.name().eq_ignore_ascii_case(name))
        .or_else(|| Key::from_name(name))
}

/// Parse "Ctrl+Shift+K", "Alt+2", "Ctrl++", "\\" ...
pub fn parse_shortcut(s: &str) -> Option<KeyboardShortcut> {
    let mut rest = s.trim();
    let mut m = Modifiers::NONE;
    loop {
        let lower = rest.to_ascii_lowercase();
        let strip = ["ctrl+", "control+", "cmd+", "alt+", "shift+"].iter().find(|p| lower.starts_with(**p) && rest.len() > p.len()).copied();
        match strip {
            Some(p) => {
                match p {
                    "alt+" => m.alt = true,
                    "shift+" => m.shift = true,
                    _ => m.command = true,
                }
                rest = &rest[p.len()..];
            }
            None => break,
        }
    }
    parse_key(rest).map(|k| normalize(KeyboardShortcut::new(m, k)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_have_no_duplicates() {
        for p in Preset::ALL {
            let km = Keymap::preset(p);
            for mode in KeyMode::ALL {
                let list = km.bindings(mode);
                assert!(!list.is_empty());
                for (i, (a, ca)) in list.iter().enumerate() {
                    for (b, cb) in &list[i + 1..] {
                        assert!(a != b, "{p:?} {mode:?}: {} bound to both {ca:?} and {cb:?}", format_shortcut(a));
                    }
                }
            }
            // Global shortcuts must not be shadowed by a mode binding.
            for (g, gc) in &km.global {
                for mode in [KeyMode::Browser, KeyMode::Viewer] {
                    assert!(km.commands_for(mode, *g).is_empty(), "{p:?}: global {} ({gc:?}) also bound in {mode:?}", format_shortcut(g));
                }
            }
        }
    }

    #[test]
    fn faststone_parity() {
        for p in Preset::ALL {
            let km = Keymap::preset(p);
            let has = |mode, s: &str, c: Command| km.commands_for(mode, parse_shortcut(s).unwrap()).contains(&c);
            for mode in [KeyMode::Browser, KeyMode::Viewer] {
                assert!(has(mode, "Q", Command::ToggleFlag));
                assert!(has(mode, "\\", Command::ToggleFlag));
                assert!(has(mode, "W", Command::ShowInExplorer));
                assert!(has(mode, "Alt+I", Command::Properties));
                assert!(has(mode, "Ctrl+Alt+M", Command::OpenGpsMap));
                assert!(has(mode, "D", Command::EditComment));
                for n in 2..=9u8 {
                    assert!(has(mode, &format!("Alt+{n}"), Command::OpenWithEditorN(n)));
                }
            }
            assert!(has(KeyMode::Global, "F12", Command::Settings));
            assert!(has(KeyMode::Viewer, "Shift+W", Command::FitWidth));
            assert!(has(KeyMode::Viewer, "Shift+H", Command::FitHeight));
            assert!(has(KeyMode::Viewer, "Ctrl+PgDn", Command::NextFrame));
            assert!(has(KeyMode::Viewer, "Ctrl+PgUp", Command::PrevFrame));
            assert!(has(KeyMode::Viewer, "K", Command::PlayPause));
        }
    }

    #[test]
    fn labels_follow_keymap() {
        let km = Keymap::preset(Preset::Lumen);
        assert_eq!(mode_text(&km, KeyMode::Viewer, Command::ZoomActual), "Ctrl+0 / Z");
        assert_eq!(mode_text(&km, KeyMode::Viewer, Command::PlayPause), "K");
        assert_eq!(mode_text(&km, KeyMode::Viewer, Command::ToggleFlag), "Q / \\");
        assert_eq!(mode_text(&km, KeyMode::Global, Command::Settings), "Ctrl+, / F12");
    }

    #[test]
    fn exact_modifier_matching() {
        let km = Keymap::preset(Preset::Lumen);
        let shift = Modifiers::SHIFT;
        let ctrl = Modifiers::CTRL | Modifiers::COMMAND;
        assert_eq!(km.lookup(KeyMode::Viewer, Key::Space, shift), Some(Command::PrevImage));
        assert_eq!(km.lookup(KeyMode::Viewer, Key::Space, Modifiers::NONE), Some(Command::NextImage));
        assert_eq!(km.lookup(KeyMode::Viewer, Key::S, ctrl | shift), Some(Command::SaveAs));
        assert_eq!(km.lookup(KeyMode::Viewer, Key::S, ctrl), Some(Command::Save));
        // Shift+8 ("*") must not set a color label.
        assert_eq!(km.lookup(KeyMode::Viewer, Key::Num8, shift), None);
        assert_eq!(km.lookup(KeyMode::Viewer, Key::Num2, Modifiers::ALT), Some(Command::OpenWithEditorN(2)));
        // "+" typed with Shift on a US layout still zooms.
        assert_eq!(km.lookup(KeyMode::Viewer, Key::Plus, shift), Some(Command::ZoomIn));
        assert_eq!(km.lookup(KeyMode::Browser, Key::Plus, ctrl | shift), Some(Command::ThumbBigger));
    }

    #[test]
    fn shortcut_text_roundtrip() {
        for p in Preset::ALL {
            let km = Keymap::preset(p);
            for mode in KeyMode::ALL {
                for (s, _) in km.bindings(mode) {
                    let text = format_shortcut(s);
                    assert!(text.is_ascii(), "{text}");
                    assert_eq!(parse_shortcut(&text), Some(*s), "{text}");
                }
            }
        }
    }

    #[test]
    fn overrides_roundtrip_through_settings_json() {
        let mut s = Settings::default();
        let mut km = Keymap::from_settings(&s);
        assert_eq!(km, Keymap::preset(Preset::Lumen));
        km.set_keys(KeyMode::Viewer, Command::ZoomActual, vec![parse_shortcut("Shift+A").unwrap()]);
        km.set_keys(KeyMode::Browser, Command::Slideshow, vec![]);
        s.key_overrides = km.diff_from(Preset::Lumen);
        assert_eq!(s.key_overrides.len(), 2);
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        let km2 = Keymap::from_settings(&back);
        assert_eq!(km2.keys_for(KeyMode::Viewer, Command::ZoomActual), vec![parse_shortcut("Shift+A").unwrap()]);
        assert!(km2.keys_for(KeyMode::Browser, Command::Slideshow).is_empty());
        assert_eq!(km2.diff_from(Preset::Lumen).len(), 2);
    }

    #[test]
    fn old_settings_without_keymap_fields_load() {
        let s: Settings = serde_json::from_str(r#"{"theme":"Light"}"#).unwrap();
        assert_eq!(Keymap::from_settings(&s), Keymap::preset(Preset::Lumen));
        // Unknown commands and keys in overrides are ignored, not fatal.
        let s: Settings = serde_json::from_str(r#"{"key_overrides":[{"mode":"viewer","command":"NoSuchCommand","keys":["Q"]},{"mode":"viewer","command":"ZoomIn","keys":["Bogus+"]}]}"#).unwrap();
        let km = Keymap::from_settings(&s);
        assert!(km.keys_for(KeyMode::Viewer, Command::ZoomIn).is_empty());
    }
}
