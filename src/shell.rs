//! Windows shell integration and single-instance forwarding.
//!
//! **Registration** is strictly per user (no admin rights) and lives under
//! `HKEY_CURRENT_USER`:
//! * `Software\Classes\Lumen.Image`, `Lumen.Video` - ProgIDs with icon + open command
//! * `Software\Classes\.<ext>\OpenWithProgids` - puts Lumen in "Open with" for every supported type
//! * `Software\Classes\Applications\lumen.exe` - friendly name + `SupportedTypes`
//! * `Software\Lumen\Capabilities` + `Software\RegisteredApplications\Lumen` - lists Lumen in
//!   Settings › Apps › Default apps (Windows only lets the *user* pick the default there)
//! * `Software\Classes\{Directory,Directory\Background,Drive}\shell\Lumen` - "Browse with Lumen"
//!
//! Unregistering removes exactly those keys/values again (shared `.ext` keys are only
//! deleted when they end up empty).
//!
//! **Single instance**: the first Lumen process serves the named pipe
//! `\\.\pipe\lumen-<user>`. A later launch with a file argument connects, hands over the
//! path, raises the running window and exits. Any failure (no server, busy, timeout, bad
//! reply) simply lets the new process start normally.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SUCCESS, HANDLE, HWND, LPARAM, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};

pub const PROGID_IMAGE: &str = "Lumen.Image";
pub const PROGID_VIDEO: &str = "Lumen.Video";
/// Name under `RegisteredApplications`; also used for `ms-settings:defaultapps?registeredAppUser=`.
pub const REGISTERED_NAME: &str = "Lumen";

const CLASSES: &str = r"Software\Classes";
const LUMEN_KEY: &str = r"Software\Lumen";
const CAPABILITIES: &str = r"Software\Lumen\Capabilities";
const REGISTERED_APPS: &str = r"Software\RegisteredApplications";
/// Context-menu verbs: (key, placeholder for the folder path).
const MENU_KEYS: [(&str, &str); 3] = [
    (r"Software\Classes\Directory\shell\Lumen", "%1"),
    (r"Software\Classes\Directory\Background\shell\Lumen", "%V"),
    (r"Software\Classes\Drive\shell\Lumen", "%1"),
];
const MENU_TEXT: &str = "Browse with Lumen";

// ====================================================================== pure helpers

/// Quote one argument the way `CommandLineToArgvW` expects (backslashes before a
/// quote are doubled). Always quotes so paths with spaces are safe.
pub fn quote_arg(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Backslashes right before the closing quote must be doubled.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// `"C:\...\lumen.exe" "%1"` - the shell substitutes the placeholder itself.
pub fn open_command(exe: &str, placeholder: &str) -> String {
    format!("{} \"{placeholder}\"", quote_arg(exe))
}

/// `"C:\...\lumen.exe",0` - first icon resource of the executable.
pub fn icon_ref(exe: &str) -> String {
    format!("{},0", quote_arg(exe))
}

/// Extract the executable from a registered open command (inverse of [`open_command`]).
pub fn exe_from_command(cmd: &str) -> Option<PathBuf> {
    let cmd = cmd.trim();
    let exe = if let Some(rest) = cmd.strip_prefix('"') {
        &rest[..rest.find('"')?]
    } else {
        cmd.split_whitespace().next()?
    };
    (!exe.is_empty()).then(|| PathBuf::from(exe))
}

/// Remove the `\\?\` verbatim prefix some APIs return - the shell does not like it in commands.
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Every extension Lumen registers, as (`.ext`, ProgID), lowercase, sorted, unique.
pub fn association_list() -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let images = crate::formats::NATIVE_EXTS.iter().chain(crate::formats::WIC_EXTS.iter());
    for e in images {
        out.push((format!(".{}", e.to_ascii_lowercase()), PROGID_IMAGE));
    }
    for e in crate::video::VIDEO_EXTS {
        out.push((format!(".{}", e.to_ascii_lowercase()), PROGID_VIDEO));
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

fn current_exe() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("Could not locate lumen.exe: {e}"))?;
    Ok(strip_verbatim(&exe))
}

fn exe_file_name(exe: &Path) -> String {
    exe.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_else(|| "lumen.exe".into())
}

// ====================================================================== registry

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win_err(op: &str, path: &str, e: WIN32_ERROR) -> anyhow::Error {
    let msg = windows::core::Error::from_hresult(e.to_hresult()).message();
    anyhow::anyhow!("{op} HKCU\\{path}: {}", msg.trim())
}

/// Owned registry key handle, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn sam(flags: &[REG_SAM_FLAGS]) -> REG_SAM_FLAGS {
    REG_SAM_FLAGS(flags.iter().fold(0, |a, f| a | f.0))
}

fn reg_create(path: &str) -> anyhow::Result<Key> {
    let p = wide(path);
    let mut h = HKEY::default();
    let r = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, sam(&[KEY_READ, KEY_WRITE]), None, &mut h, None) };
    if r != ERROR_SUCCESS {
        return Err(win_err("Could not create", path, r));
    }
    Ok(Key(h))
}

fn reg_open(path: &str) -> Option<Key> {
    let p = wide(path);
    let mut h = HKEY::default();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()), None, KEY_READ, &mut h) };
    (r == ERROR_SUCCESS).then_some(Key(h))
}

/// Set a REG_SZ value (`name` = "" for the key's default value), creating the key if needed.
fn reg_set(path: &str, name: &str, value: &str) -> anyhow::Result<()> {
    let key = reg_create(path)?;
    let n = wide(name);
    let data = wide(value);
    let bytes: Vec<u8> = data.iter().flat_map(|u| u.to_le_bytes()).collect();
    let r = unsafe { RegSetValueExW(key.0, PCWSTR(n.as_ptr()), None, REG_SZ, Some(&bytes)) };
    if r != ERROR_SUCCESS {
        return Err(win_err("Could not write", path, r));
    }
    Ok(())
}

fn reg_get(path: &str, name: &str) -> Option<String> {
    let key = reg_open(path)?;
    let n = wide(name);
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    let r = unsafe { RegQueryValueExW(key.0, PCWSTR(n.as_ptr()), None, Some(&mut ty), None, Some(&mut len)) };
    if r != ERROR_SUCCESS || len == 0 {
        return if r == ERROR_SUCCESS { Some(String::new()) } else { None };
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
    let mut len2 = (buf.len() * 2) as u32;
    let r = unsafe { RegQueryValueExW(key.0, PCWSTR(n.as_ptr()), None, Some(&mut ty), Some(buf.as_mut_ptr() as *mut u8), Some(&mut len2)) };
    if r != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(len2 as usize / 2);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(String::from_utf16_lossy(&buf))
}

fn reg_value_names(path: &str) -> Vec<String> {
    let Some(key) = reg_open(path) else { return Vec::new() };
    let mut out = Vec::new();
    let mut buf = vec![0u16; 16384];
    for i in 0..10_000u32 {
        let mut len = buf.len() as u32;
        let r = unsafe { RegEnumValueW(key.0, i, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
        if r == ERROR_NO_MORE_ITEMS || r != ERROR_SUCCESS {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

fn reg_exists(path: &str) -> bool {
    reg_open(path).is_some()
}

/// True when the key has neither subkeys nor values.
fn reg_is_empty(path: &str) -> bool {
    let Some(key) = reg_open(path) else { return false };
    let (mut subkeys, mut values) = (0u32, 0u32);
    let r = unsafe { RegQueryInfoKeyW(key.0, None, None, None, Some(&mut subkeys), None, None, Some(&mut values), None, None, None, None) };
    r == ERROR_SUCCESS && subkeys == 0 && values == 0
}

fn reg_delete_tree(path: &str) -> anyhow::Result<()> {
    let p = wide(path);
    let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr())) };
    if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
        return Err(win_err("Could not delete", path, r));
    }
    Ok(())
}

fn reg_delete_value(path: &str, name: &str) -> anyhow::Result<()> {
    let (p, n) = (wide(path), wide(name));
    let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()), PCWSTR(n.as_ptr())) };
    if r != ERROR_SUCCESS && r != ERROR_FILE_NOT_FOUND {
        return Err(win_err("Could not delete value in", path, r));
    }
    Ok(())
}

fn reg_delete_if_empty(path: &str) {
    if reg_is_empty(path) {
        let p = wide(path);
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()));
        }
    }
}

fn notify_shell() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Collects errors while continuing, so unregistering removes as much as it can.
#[derive(Default)]
struct Errors(Vec<anyhow::Error>);

impl Errors {
    fn note(&mut self, r: anyhow::Result<()>) {
        if let Err(e) = r {
            self.0.push(e);
        }
    }
    fn finish(self) -> anyhow::Result<()> {
        match self.0.len() {
            0 => Ok(()),
            1 => Err(self.0.into_iter().next().unwrap_or_else(|| anyhow::anyhow!("unknown error"))),
            n => Err(anyhow::anyhow!("{} (and {} more errors)", self.0[0], n - 1)),
        }
    }
}

// ====================================================================== public API

/// Snapshot of what is registered, for the settings page.
#[derive(Clone, Debug, Default)]
pub struct ShellStatus {
    pub file_types: bool,
    pub context_menu: bool,
    /// Executable the file associations point at (may differ from this copy).
    pub registered_exe: Option<PathBuf>,
    pub current_exe: Option<PathBuf>,
}

impl ShellStatus {
    /// Registered, but for another copy of lumen.exe (moved or a second install).
    pub fn stale(&self) -> bool {
        match (&self.registered_exe, &self.current_exe) {
            (Some(r), Some(c)) => !r.as_os_str().eq_ignore_ascii_case(c.as_os_str()),
            _ => false,
        }
    }
}

pub fn status() -> ShellStatus {
    let cmd = reg_get(&format!(r"{CLASSES}\{PROGID_IMAGE}\shell\open\command"), "");
    ShellStatus {
        file_types: cmd.is_some() && is_registered(),
        context_menu: is_context_menu_registered(),
        registered_exe: cmd.as_deref().and_then(exe_from_command),
        current_exe: current_exe().ok(),
    }
}

/// True when Lumen's file-type ProgIDs are registered for this user.
pub fn is_registered() -> bool {
    reg_exists(&format!(r"{CLASSES}\{PROGID_IMAGE}\shell\open\command"))
}

pub fn is_context_menu_registered() -> bool {
    reg_exists(&format!(r"{}\command", MENU_KEYS[0].0))
}

/// Register ProgIDs, "Open with" entries, the Applications key and Default-apps
/// capabilities. Returns the number of extensions registered.
pub fn register_file_types() -> anyhow::Result<usize> {
    let exe = current_exe()?;
    let exe_s = exe.to_string_lossy().to_string();
    let icon = icon_ref(&exe_s);
    let cmd = open_command(&exe_s, "%1");
    let list = association_list();

    for (progid, desc) in [(PROGID_IMAGE, "Lumen image"), (PROGID_VIDEO, "Lumen video")] {
        let base = format!(r"{CLASSES}\{progid}");
        reg_set(&base, "", desc)?;
        reg_set(&base, "FriendlyTypeName", desc)?;
        reg_set(&format!(r"{base}\DefaultIcon"), "", &icon)?;
        reg_set(&format!(r"{base}\shell\open\command"), "", &cmd)?;
    }

    let app = format!(r"{CLASSES}\Applications\{}", exe_file_name(&exe));
    reg_set(&app, "FriendlyAppName", "Lumen")?;
    reg_set(&format!(r"{app}\DefaultIcon"), "", &icon)?;
    reg_set(&format!(r"{app}\shell\open\command"), "", &cmd)?;

    reg_set(CAPABILITIES, "ApplicationName", "Lumen")?;
    reg_set(CAPABILITIES, "ApplicationDescription", "Fast, private image & video viewer, browser and editor")?;
    reg_set(CAPABILITIES, "ApplicationIcon", &icon)?;

    for (ext, progid) in &list {
        reg_set(&format!(r"{CLASSES}\{ext}\OpenWithProgids"), progid, "")?;
        reg_set(&format!(r"{app}\SupportedTypes"), ext, "")?;
        reg_set(&format!(r"{CAPABILITIES}\FileAssociations"), ext, progid)?;
    }
    reg_set(REGISTERED_APPS, REGISTERED_NAME, CAPABILITIES)?;
    notify_shell();
    Ok(list.len())
}

/// Remove everything [`register_file_types`] added (best effort, reports the first error).
pub fn unregister_file_types() -> anyhow::Result<()> {
    let mut errs = Errors::default();
    // Union of the current list and whatever an older version recorded in Capabilities.
    let mut exts: Vec<String> = association_list().into_iter().map(|(e, _)| e).collect();
    exts.extend(reg_value_names(&format!(r"{CAPABILITIES}\FileAssociations")).into_iter().filter(|e| e.starts_with('.')));
    exts.sort();
    exts.dedup();
    for ext in &exts {
        let owp = format!(r"{CLASSES}\{ext}\OpenWithProgids");
        if reg_exists(&owp) {
            errs.note(reg_delete_value(&owp, PROGID_IMAGE));
            errs.note(reg_delete_value(&owp, PROGID_VIDEO));
            reg_delete_if_empty(&owp);
            reg_delete_if_empty(&format!(r"{CLASSES}\{ext}"));
        }
    }
    errs.note(reg_delete_tree(&format!(r"{CLASSES}\{PROGID_IMAGE}")));
    errs.note(reg_delete_tree(&format!(r"{CLASSES}\{PROGID_VIDEO}")));
    let mut apps = vec!["lumen.exe".to_string()];
    if let Ok(exe) = current_exe() {
        apps.push(exe_file_name(&exe));
    }
    apps.dedup();
    for a in apps {
        errs.note(reg_delete_tree(&format!(r"{CLASSES}\Applications\{a}")));
    }
    errs.note(reg_delete_value(REGISTERED_APPS, REGISTERED_NAME));
    errs.note(reg_delete_tree(CAPABILITIES));
    reg_delete_if_empty(LUMEN_KEY);
    notify_shell();
    errs.finish()
}

/// Add "Browse with Lumen" to the Explorer context menu of folders, folder backgrounds and drives.
pub fn register_context_menu() -> anyhow::Result<()> {
    let exe = current_exe()?;
    let exe_s = exe.to_string_lossy().to_string();
    let icon = icon_ref(&exe_s);
    for (key, placeholder) in MENU_KEYS {
        reg_set(key, "", MENU_TEXT)?;
        reg_set(key, "Icon", &icon)?;
        reg_set(&format!(r"{key}\command"), "", &open_command(&exe_s, placeholder))?;
    }
    notify_shell();
    Ok(())
}

pub fn unregister_context_menu() -> anyhow::Result<()> {
    let mut errs = Errors::default();
    for (key, _) in MENU_KEYS {
        errs.note(reg_delete_tree(key));
    }
    notify_shell();
    errs.finish()
}

/// Remove all registrations (file types + context menu).
pub fn unregister_all() -> anyhow::Result<()> {
    let a = unregister_file_types();
    let b = unregister_context_menu();
    a.and(b)
}

fn shell_open(target: &str) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (op, file) = (wide("open"), wide(target));
    let h = unsafe { ShellExecuteW(None, PCWSTR(op.as_ptr()), PCWSTR(file.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    h.0 as isize > 32
}

/// Open Settings › Default apps, on Lumen's own page when Windows supports it.
pub fn open_default_apps() -> anyhow::Result<()> {
    if shell_open(&format!("ms-settings:defaultapps?registeredAppUser={REGISTERED_NAME}")) || shell_open("ms-settings:defaultapps") {
        Ok(())
    } else {
        Err(anyhow::anyhow!("Could not open Windows Settings › Default apps"))
    }
}

// ====================================================================== single instance

const MAGIC: &[u8; 4] = b"LMN1";
const MAX_MESSAGE: usize = 1 << 20;
const ACK: u8 = 0x06;

/// Pipe name unique per user: `\\.\pipe\lumen-<user>`.
pub fn pipe_name() -> String {
    // Include the Windows session id so separate logon sessions (fast user
    // switching, RDP) never talk to each other's instances.
    let mut session = 0u32;
    unsafe {
        let _ = windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut session);
    }
    format!(r"\\.\pipe\lumen-{}-s{session}", sanitize_user(&std::env::var("USERNAME").unwrap_or_default()))
}

/// Make a user name safe for a pipe name (ASCII alnum, `-_.`; others hex-escaped).
pub fn sanitize_user(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push_str(&format!("x{:04x}", c as u32));
        }
        if out.len() >= 96 {
            break;
        }
    }
    if out.is_empty() {
        "user".into()
    } else {
        out
    }
}

/// Wire format: u32 LE body length, then body = `LMN1` + UTF-16LE paths each NUL-terminated.
/// UTF-16 keeps any Windows file name lossless.
pub fn encode_message(paths: &[PathBuf]) -> Vec<u8> {
    let mut body = MAGIC.to_vec();
    for p in paths {
        for u in p.as_os_str().encode_wide() {
            body.extend(u.to_le_bytes());
        }
        body.extend([0, 0]);
    }
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend(body);
    out
}

/// Decode a message body (without the length prefix). None when malformed.
pub fn decode_body(body: &[u8]) -> Option<Vec<PathBuf>> {
    if body.len() > MAX_MESSAGE || !body.starts_with(MAGIC) || (body.len() - MAGIC.len()) % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body[MAGIC.len()..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(units.split(|&u| u == 0).filter(|s| !s.is_empty()).map(|s| PathBuf::from(OsString::from_wide(s))).collect())
}

struct Server {
    rx: crossbeam_channel::Receiver<Vec<PathBuf>>,
    ctx: std::sync::Arc<OnceLock<egui::Context>>,
}

static SERVER: OnceLock<Server> = OnceLock::new();

/// Claim the instance pipe and serve forwarded paths on a background thread.
/// Returns false when another Lumen already owns it (or on any error).
pub fn start_instance_server() -> bool {
    if SERVER.get().is_some() {
        return true;
    }
    let Some(h) = create_pipe_instance(true) else { return false };
    let (tx, rx) = crossbeam_channel::unbounded();
    let ctx = std::sync::Arc::new(OnceLock::new());
    let ctx2 = ctx.clone();
    let raw = h.0 as isize; // HANDLE is not Send; move the raw value.
    let spawned = std::thread::Builder::new().name("lumen-instance".into()).spawn(move || serve(HANDLE(raw as _), tx, ctx2));
    if spawned.is_err() {
        unsafe {
            let _ = CloseHandle(h);
        }
        return false;
    }
    let _ = SERVER.set(Server { rx, ctx });
    true
}

/// Create one instance of the server pipe. The first call claims the name
/// exclusively (FILE_FLAG_FIRST_PIPE_INSTANCE) so a second Lumen can detect us.
fn create_pipe_instance(first: bool) -> Option<HANDLE> {
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};
    let name = wide(&pipe_name());
    let mut flags = PIPE_ACCESS_DUPLEX;
    if first {
        flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let h = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            None,
        )
    };
    if h.is_invalid() {
        None
    } else {
        Some(h)
    }
}

/// Paths forwarded by other launches since the last call. Call once per frame.
pub fn poll_forwarded(ctx: &egui::Context) -> Vec<PathBuf> {
    let Some(s) = SERVER.get() else { return Vec::new() };
    s.ctx.get_or_init(|| ctx.clone());
    s.rx.try_iter().flatten().collect()
}

fn pipe_read_exact(h: HANDLE, buf: &mut [u8]) -> bool {
    use windows::Win32::Storage::FileSystem::ReadFile;
    let mut off = 0;
    while off < buf.len() {
        let mut n = 0u32;
        if unsafe { ReadFile(h, Some(&mut buf[off..]), Some(&mut n), None) }.is_err() || n == 0 {
            return false;
        }
        off += n as usize;
    }
    true
}

/// Accept loop: every connected client is handled on its own short-lived
/// thread, and a fresh pipe instance is created for the next client, so a
/// slow or hung client can never block other launches.
fn serve(first: HANDLE, tx: crossbeam_channel::Sender<Vec<PathBuf>>, ctx: std::sync::Arc<OnceLock<egui::Context>>) {
    use windows::Win32::System::Pipes::ConnectNamedPipe;
    let mut h = first;
    let mut backoff = 50u64;
    loop {
        let connected = match unsafe { ConnectNamedPipe(h, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if connected {
            backoff = 50;
            let raw = h.0 as isize;
            let (tx, ctx) = (tx.clone(), ctx.clone());
            let _ = std::thread::Builder::new().name("lumen-instance-client".into()).spawn(move || handle_client(HANDLE(raw as _), tx, ctx));
            match create_pipe_instance(false) {
                Some(n) => h = n,
                None => {
                    std::thread::sleep(Duration::from_millis(500));
                    match create_pipe_instance(false) {
                        Some(n) => h = n,
                        None => return,
                    }
                }
            }
        } else {
            // Transient failure: back off instead of giving up for good.
            std::thread::sleep(Duration::from_millis(backoff));
            backoff = (backoff * 2).min(2000);
            unsafe {
                let _ = windows::Win32::System::Pipes::DisconnectNamedPipe(h);
            }
        }
    }
}

fn handle_client(h: HANDLE, tx: crossbeam_channel::Sender<Vec<PathBuf>>, ctx: std::sync::Arc<OnceLock<egui::Context>>) {
    use windows::Win32::Storage::FileSystem::{FlushFileBuffers, WriteFile};
    use windows::Win32::System::Pipes::DisconnectNamedPipe;
    let mut len = [0u8; 4];
    if pipe_read_exact(h, &mut len) {
        let n = u32::from_le_bytes(len) as usize;
        if n <= MAX_MESSAGE {
            let mut body = vec![0u8; n];
            if pipe_read_exact(h, &mut body) {
                if let Some(paths) = decode_body(&body) {
                    if !paths.is_empty() && tx.send(paths).is_ok() {
                        if let Some(c) = ctx.get() {
                            c.request_repaint();
                        }
                    }
                    unsafe {
                        let _ = WriteFile(h, Some(&[ACK]), None, None);
                        let _ = FlushFileBuffers(h);
                    }
                }
            }
        }
    }
    unsafe {
        let _ = DisconnectNamedPipe(h);
        let _ = CloseHandle(h);
    }
}

/// Hand `paths` to a running Lumen and raise its window. True = the running
/// instance accepted them and this process should exit.
pub fn forward_to_running(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let paths: Vec<PathBuf> = paths.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
    let msg = encode_message(&paths);
    let name = pipe_name();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("lumen-forward".into()).spawn(move || {
        let _ = tx.send(try_forward(&name, &msg));
    });
    // A hung instance must never block this launch: give up after a few seconds.
    spawned.is_ok() && matches!(rx.recv_timeout(Duration::from_secs(3)), Ok(true))
}

fn try_forward(name: &str, msg: &[u8]) -> bool {
    use std::io::{Read, Write};
    let mut file = None;
    for _ in 0..50 {
        match std::fs::OpenOptions::new().read(true).write(true).open(name) {
            Ok(f) => {
                file = Some(f);
                break;
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false, // no running instance
        }
    }
    let Some(mut f) = file else { return false };
    let pid = server_pid(&f);
    if let Some(pid) = pid {
        // We were started by Explorer and may take the foreground; let the running instance do so too.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid);
        }
    }
    if f.write_all(msg).and_then(|_| f.flush()).is_err() {
        return false;
    }
    let mut ack = [0u8; 1];
    let ok = f.read_exact(&mut ack).is_ok() && ack[0] == ACK;
    if ok {
        if let Some(pid) = pid {
            bring_to_front(pid);
        }
    }
    ok
}

fn server_pid(f: &std::fs::File) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
    let mut pid = 0u32;
    unsafe { GetNamedPipeServerProcessId(HANDLE(f.as_raw_handle() as _), &mut pid) }.ok()?;
    (pid != 0).then_some(pid)
}

/// Restore (if minimized) and activate the main window of process `pid`.
fn bring_to_front(pid: u32) {
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, GW_OWNER, SW_RESTORE};
    struct Search {
        pid: u32,
        found: Option<isize>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
        let s = unsafe { &mut *(lp.0 as *mut Search) };
        let mut wpid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut wpid)) };
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.map(|o| !o.is_invalid()).unwrap_or(false);
        if wpid == s.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() && !owned {
            s.found = Some(hwnd.0 as isize);
            return false.into();
        }
        true.into()
    }
    let mut s = Search { pid, found: None };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut s as *mut Search as isize));
    }
    if let Some(raw) = s.found {
        let hwnd = HWND(raw as _);
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

// ====================================================================== tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_plain_and_spaces() {
        assert_eq!(quote_arg(r"C:\Apps\lumen.exe"), r#""C:\Apps\lumen.exe""#);
        assert_eq!(quote_arg(r"C:\Program Files\Lumen\lumen.exe"), r#""C:\Program Files\Lumen\lumen.exe""#);
    }

    #[test]
    fn quoting_trailing_backslash_and_quotes() {
        assert_eq!(quote_arg(r"C:\dir\"), r#""C:\dir\\""#);
        assert_eq!(quote_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote_arg(""), r#""""#);
    }

    #[test]
    fn commands_and_icons() {
        let exe = r"C:\Program Files\Lumen\lumen.exe";
        assert_eq!(open_command(exe, "%1"), r#""C:\Program Files\Lumen\lumen.exe" "%1""#);
        assert_eq!(open_command(exe, "%V"), r#""C:\Program Files\Lumen\lumen.exe" "%V""#);
        assert_eq!(icon_ref(exe), r#""C:\Program Files\Lumen\lumen.exe",0"#);
        assert_eq!(exe_from_command(&open_command(exe, "%1")), Some(PathBuf::from(exe)));
        assert_eq!(exe_from_command(r"C:\x\lumen.exe %1"), Some(PathBuf::from(r"C:\x\lumen.exe")));
        assert_eq!(exe_from_command("   "), None);
        assert_eq!(exe_from_command("\"unterminated"), None);
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\a\lumen.exe")), PathBuf::from(r"C:\a\lumen.exe"));
        assert_eq!(strip_verbatim(Path::new(r"\\?\UNC\srv\share\l.exe")), PathBuf::from(r"\\srv\share\l.exe"));
        assert_eq!(strip_verbatim(Path::new(r"C:\a\lumen.exe")), PathBuf::from(r"C:\a\lumen.exe"));
    }

    #[test]
    fn association_list_is_complete_and_unique() {
        let list = association_list();
        let mut names: Vec<&str> = list.iter().map(|(e, _)| e.as_str()).collect();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "duplicates");
        assert!(list.iter().all(|(e, _)| e.starts_with('.') && e[1..].chars().all(|c| !c.is_ascii_uppercase())));
        assert!(list.contains(&(".jpg".to_string(), PROGID_IMAGE)));
        assert!(list.contains(&(".heic".to_string(), PROGID_IMAGE)));
        for v in crate::video::VIDEO_EXTS {
            assert!(list.iter().any(|(e, p)| e == &format!(".{v}") && (*p == PROGID_VIDEO || crate::formats::NATIVE_EXTS.contains(v) || crate::formats::WIC_EXTS.contains(v))));
        }
        let expected = crate::formats::NATIVE_EXTS.len() + crate::formats::WIC_EXTS.len() + crate::video::VIDEO_EXTS.len();
        assert!(list.len() <= expected && list.len() > 30);
    }

    #[test]
    fn user_names_are_sanitized() {
        assert_eq!(sanitize_user("Rahul"), "rahul");
        assert_eq!(sanitize_user("a b"), "ax0020b");
        assert_eq!(sanitize_user(""), "user");
        assert_eq!(sanitize_user("José"), "josx00e9");
        assert!(sanitize_user(&"x".repeat(500)).len() <= 100);
        assert!(pipe_name().starts_with(r"\\.\pipe\lumen-"));
    }

    #[test]
    fn message_round_trip() {
        let paths = vec![PathBuf::from(r"C:\Pictures\a b.jpg"), PathBuf::from(r"D:\写真\猫.png")];
        let msg = encode_message(&paths);
        let len = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
        assert_eq!(len, msg.len() - 4);
        assert_eq!(decode_body(&msg[4..]), Some(paths));
        assert_eq!(decode_body(&encode_message(&[])[4..]), Some(vec![]));
    }

    #[test]
    fn message_round_trip_unpaired_surrogate() {
        let raw: Vec<u16> = r"C:\x".encode_utf16().chain([0xD800]).collect();
        let p = PathBuf::from(OsString::from_wide(&raw));
        assert_eq!(decode_body(&encode_message(std::slice::from_ref(&p))[4..]), Some(vec![p]));
    }

    #[test]
    fn malformed_messages_are_rejected() {
        assert_eq!(decode_body(b""), None);
        assert_eq!(decode_body(b"XXXX\x41\x00"), None);
        assert_eq!(decode_body(b"LMN1\x41"), None); // odd length
        let mut big = MAGIC.to_vec();
        big.resize(MAX_MESSAGE + 2, 0);
        assert_eq!(decode_body(&big), None);
    }

    /// Touches the real registry (HKCU only) and cleans up after itself.
    /// Run with `cargo test -- --ignored registry_round_trip`.
    #[test]
    #[ignore]
    fn registry_round_trip() {
        if is_registered() || is_context_menu_registered() {
            eprintln!("Lumen is already registered on this machine; not touching it.");
            return;
        }
        let n = register_file_types().expect("register");
        assert_eq!(n, association_list().len());
        register_context_menu().expect("menu");
        let st = status();
        assert!(st.file_types && st.context_menu && !st.stale(), "{st:?}");
        assert!(reg_value_names(&format!(r"{CLASSES}\.jpg\OpenWithProgids")).iter().any(|v| v == PROGID_IMAGE));
        assert_eq!(reg_get(REGISTERED_APPS, REGISTERED_NAME).as_deref(), Some(CAPABILITIES));
        unregister_all().expect("unregister");
        let st = status();
        assert!(!st.file_types && !st.context_menu);
        assert!(!reg_value_names(&format!(r"{CLASSES}\.jpg\OpenWithProgids")).iter().any(|v| v == PROGID_IMAGE));
        assert!(reg_get(REGISTERED_APPS, REGISTERED_NAME).is_none());
        assert!(!reg_exists(CAPABILITIES));
        for (key, _) in MENU_KEYS {
            assert!(!reg_exists(key));
        }
    }
}
