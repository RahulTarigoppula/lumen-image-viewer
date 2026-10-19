//! File-system operations and Windows shell integration.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// Pick a non-existing destination path by appending " (2)", " (3)", ...
pub fn unique_path(target: &Path) -> PathBuf {
    if !target.exists() {
        return target.to_path_buf();
    }
    let parent = target.parent().unwrap_or(Path::new("."));
    let stem = target.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for i in 2..100_000 {
        let p = parent.join(format!("{stem} ({i}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    target.to_path_buf()
}

pub fn delete_to_trash(paths: &[PathBuf]) -> Result<()> {
    trash::delete_all(paths).map_err(|e| anyhow!("Could not move to Recycle Bin: {e}"))
}

/// Copy or move files into `dest_dir`. Returns (source, destination) pairs that succeeded.
/// A name that already exists in `dest_dir` gets a number appended, so nothing is
/// ever overwritten and every move can be undone.
pub fn transfer(paths: &[PathBuf], dest_dir: &Path, do_move: bool) -> (Vec<(PathBuf, PathBuf)>, Vec<String>) {
    let mut done = Vec::new();
    let mut errors = Vec::new();
    for src in paths {
        let Some(name) = src.file_name() else { continue };
        // A folder copied or moved into itself (or a sub-folder) would recurse forever.
        if src.is_dir() && is_within(dest_dir, src) {
            errors.push(format!("{}: cannot copy or move a folder into itself", src.display()));
            continue;
        }
        let dst = dest_dir.join(name);
        if dst == *src && do_move {
            continue;
        }
        let dst = unique_path(&dst);
        let res = if do_move {
            std::fs::rename(src, &dst).or_else(|_| {
                // Cross-volume move: copy then delete.
                copy_any(src, &dst).and_then(|_| remove_any(src))
            })
        } else {
            copy_any(src, &dst)
        };
        match res {
            Ok(()) => done.push((src.clone(), dst)),
            Err(e) => errors.push(format!("{}: {e}", src.display())),
        }
    }
    (done, errors)
}

/// Move (or rename) one file or folder to an exact destination, never overwriting.
pub fn move_path(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Err(anyhow!("{} already exists", dst.display()));
    }
    if let Some(parent) = dst.parent() {
        if !parent.is_dir() {
            return Err(anyhow!("folder {} no longer exists", parent.display()));
        }
    }
    std::fs::rename(src, dst).or_else(|_| copy_any(src, dst).and_then(|_| remove_any(src))).with_context(|| format!("{}", src.display()))
}

/// `path` is `dir` or lies below it (case-insensitive, as on NTFS).
pub fn is_within(path: &Path, dir: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (p, d) = (norm(path), norm(dir));
    p == d || p.starts_with(&format!("{d}\\"))
}

/// `path` spelled the way it is stored on disk: drive letter in upper case and
/// each existing component in its real case ("c:\photos\2024" ->
/// "C:\Photos\2024"). Windows paths are case-insensitive, but the folder tree,
/// the back / forward history and the title compare paths exactly, so a typed
/// path must match the one listed. Components that cannot be found are kept as
/// given.
pub fn true_case(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => out.push(p.as_os_str().to_string_lossy().to_uppercase()),
                _ => out.push(p.as_os_str()),
            },
            Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                let wild = name.to_string_lossy().contains(['*', '?']);
                let real = if wild { None } else { disk_name(&out.join(name)) };
                out.push(real.as_deref().unwrap_or(name));
            }
        }
    }
    out
}

/// The name of the existing file or folder `path` as stored on disk.
#[cfg(windows)]
fn disk_name(path: &Path) -> Option<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `data` is a valid out-parameter; the search handle is closed right away.
    let h = unsafe { FindFirstFileW(&windows::core::HSTRING::from(path.as_os_str()), &mut data) }.ok()?;
    let _ = unsafe { FindClose(h) };
    let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
    (len > 0).then(|| std::ffi::OsString::from_wide(&data.cFileName[..len]))
}

#[cfg(not(windows))]
fn disk_name(_path: &Path) -> Option<std::ffi::OsString> {
    None
}

fn copy_any(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_any(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(anyhow!("Name cannot be empty"));
    }
    if new_name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')) {
        return Err(anyhow!("A file name can't contain any of: < > : \" / \\ | ? *"));
    }
    let target = path.with_file_name(new_name);
    if target == path {
        return Ok(target);
    }
    // Allow case-only renames on case-insensitive file systems.
    let case_only = target.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
    if target.exists() && !case_only {
        return Err(anyhow!("\"{new_name}\" already exists"));
    }
    std::fs::rename(path, &target).with_context(|| format!("Rename failed for {}", path.display()))?;
    Ok(target)
}

pub fn new_folder(parent: &Path, name: &str) -> Result<PathBuf> {
    let p = unique_path(&parent.join(name.trim()));
    std::fs::create_dir(&p)?;
    Ok(p)
}

pub fn show_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

pub fn open_with_default(path: &Path) {
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

pub fn open_with(exe: &Path, path: &Path) -> Result<()> {
    std::process::Command::new(exe).arg(path).spawn()?;
    Ok(())
}

/// Show the Windows "Open with" chooser.
pub fn open_with_dialog(path: &Path) {
    let _ = std::process::Command::new("rundll32.exe")
        .arg("shell32.dll,OpenAs_RunDLL")
        .arg(path)
        .spawn();
}

pub fn properties_dialog(path: &Path) {
    #[cfg(windows)]
    unsafe {
        use windows::core::{w, HSTRING};
        use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW};
        let file = HSTRING::from(path.as_os_str());
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            lpVerb: w!("properties"),
            lpFile: windows::core::PCWSTR(file.as_ptr()),
            nShow: 1,
            ..Default::default()
        };
        let _ = ShellExecuteExW(&mut info);
    }
}

/// Set the desktop wallpaper. `style`: 0 fill, 1 fit, 2 stretch, 3 tile, 4 center, 5 span.
pub fn set_wallpaper(image_path: &Path, style: u8) -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Registry::*;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let (ws, tile) = match style {
            0 => ("10", "0"),
            1 => ("6", "0"),
            2 => ("2", "0"),
            3 => ("0", "1"),
            4 => ("0", "0"),
            _ => ("22", "0"),
        };
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, windows::core::w!("Control Panel\\Desktop"), Some(0), KEY_SET_VALUE, &mut key).is_ok() {
            for (name, val) in [("WallpaperStyle", ws), ("TileWallpaper", tile)] {
                let wide: Vec<u16> = val.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                let _ = RegSetValueExW(key, &windows::core::HSTRING::from(name), Some(0), REG_SZ, Some(bytes));
            }
            let _ = RegCloseKey(key);
        }
        let mut wide: Vec<u16> = image_path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(wide.as_mut_ptr() as *mut _),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .map_err(|e| anyhow!("Could not set wallpaper: {e}"))?;
    }
    Ok(())
}

pub fn copy_image_to_clipboard(img: &image::RgbaImage) -> Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_image(arboard::ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: std::borrow::Cow::Borrowed(img.as_raw()),
    })?;
    Ok(())
}

pub fn paste_image_from_clipboard() -> Result<image::RgbaImage> {
    let mut cb = arboard::Clipboard::new()?;
    let data = cb.get_image().map_err(|_| anyhow!("The clipboard doesn't contain an image"))?;
    image::RgbaImage::from_raw(data.width as u32, data.height as u32, data.bytes.into_owned())
        .ok_or_else(|| anyhow!("Invalid clipboard image"))
}

pub fn copy_text(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

/// Copy files to the clipboard so they can be pasted in Explorer.
pub fn copy_files_to_clipboard(paths: &[PathBuf]) -> Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set().file_list(paths)?;
    Ok(())
}

pub fn paste_files_from_clipboard() -> Result<Vec<PathBuf>> {
    let mut cb = arboard::Clipboard::new()?;
    Ok(cb.get().file_list()?)
}

/// Logical drives ("C:\", "D:\", ...).
pub fn drives() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(windows)]
    unsafe {
        let mask = windows::Win32::Storage::FileSystem::GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) != 0 {
                out.push(PathBuf::from(format!("{}:\\", (b'A' + i as u8) as char)));
            }
        }
    }
    out
}

pub fn volume_label(drive: &Path) -> Option<String> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let root = windows::core::HSTRING::from(drive.as_os_str());
        let mut name = [0u16; 261];
        if GetVolumeInformationW(&root, Some(&mut name), None, None, None, None).is_ok() {
            let len = name.iter().position(|&c| c == 0).unwrap_or(0);
            let s = String::from_utf16_lossy(&name[..len]);
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Lowercased known folders for the sidebar.
pub fn quick_access() -> Vec<(String, PathBuf, &'static str)> {
    use crate::ui::icons as I;
    let mut v = Vec::new();
    let mut push = |name: &str, p: Option<PathBuf>, icon: &'static str| {
        if let Some(p) = p {
            if p.exists() {
                v.push((name.to_string(), p, icon));
            }
        }
    };
    push("Pictures", dirs::picture_dir(), I::IMAGES);
    push("Desktop", dirs::desktop_dir(), I::DESKTOP);
    push("Downloads", dirs::download_dir(), I::DOWNLOAD_SIMPLE);
    push("Documents", dirs::document_dir(), I::FILE_TEXT);
    push("Videos", dirs::video_dir(), I::FILM_STRIP);
    push("Home", dirs::home_dir(), I::HOUSE);
    v
}

/// Result of dragging files out of Lumen onto another program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOutcome {
    Cancelled,
    Copied,
    /// The target moved the files (e.g. Shift-drag onto an Explorer folder).
    Moved,
    Linked,
}

/// The shell's modal drag loop swallows the button-up: tell the window (and so
/// winit / egui) that the left button is no longer held.
fn release_mouse_button(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, PostMessageW, WM_LBUTTONUP};
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_ok() {
            let _ = ScreenToClient(hwnd, &mut pt);
        }
        let lp = ((pt.y as u32 & 0xFFFF) << 16) | (pt.x as u32 & 0xFFFF);
        let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), LPARAM(lp as isize));
    }
}

/// Start a Windows shell drag-and-drop of `files` (all in one folder), so they can be
/// dropped on Explorer, the desktop, a mail client, an editor ... Blocks until the drop
/// ends (the shell runs its own modal loop). Uses the shell's own data object (the same
/// one Explorer uses, CF_HDROP + shell IDList formats) and the default drop source.
pub fn drag_out(files: &[PathBuf]) -> Result<DragOutcome> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{ILFindLastID, ILFree, SHCreateDataObject, SHDoDragDrop, SHParseDisplayName};

    let Some(first) = files.first() else { return Ok(DragOutcome::Cancelled) };
    let parent = first.parent().ok_or_else(|| anyhow!("no parent folder"))?;
    let files: Vec<&PathBuf> = files.iter().filter(|f| f.parent() == Some(parent)).collect();

    unsafe {
        let parse = |p: &Path| -> Result<*mut ITEMIDLIST> {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(&HSTRING::from(p.as_os_str()), None, &mut pidl, 0, None).with_context(|| format!("{}", p.display()))?;
            Ok(pidl)
        };
        // Full PIDLs of the files; the parent PIDL is the first one minus its last id.
        let mut full: Vec<*mut ITEMIDLIST> = Vec::new();
        let result = (|| -> Result<DragOutcome> {
            for f in &files {
                full.push(parse(f)?);
            }
            let folder = parse(parent)?;
            let children: Vec<*const ITEMIDLIST> = full.iter().map(|p| ILFindLastID(*p) as *const ITEMIDLIST).collect();
            let data: windows::core::Result<IDataObject> = SHCreateDataObject(Some(folder as *const _), Some(&children), None);
            let data = match data {
                Ok(d) => d,
                Err(e) => {
                    ILFree(Some(folder as *const _));
                    return Err(anyhow!("could not start dragging: {e}"));
                }
            };
            let hwnd = GetActiveWindow();
            let hwnd = if hwnd.0.is_null() { None } else { Some(hwnd) };
            let effect = SHDoDragDrop(hwnd, &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK);
            ILFree(Some(folder as *const _));
            if let Some(h) = hwnd {
                release_mouse_button(h);
            }
            let effect = effect.map_err(|e| anyhow!("drag and drop failed: {e}"))?;
            Ok(if effect.0 & DROPEFFECT_MOVE.0 != 0 {
                DragOutcome::Moved
            } else if effect.0 & DROPEFFECT_COPY.0 != 0 {
                DragOutcome::Copied
            } else if effect.0 & DROPEFFECT_LINK.0 != 0 {
                DragOutcome::Linked
            } else {
                DragOutcome::Cancelled
            })
        })();
        for p in full {
            ILFree(Some(p as *const _));
        }
        // DRAGDROP_S_CANCEL / DRAGDROP_S_DROP are success codes; only real failures are Err.
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn true_case_restores_the_spelling_on_disk() {
        let base = std::env::temp_dir().join(format!("lumen-true-case-{}", std::process::id()));
        let dir = base.join("MixedCase Folder");
        std::fs::create_dir_all(&dir).unwrap();
        let typed = PathBuf::from(base.to_string_lossy().to_lowercase()).join("mixedcase folder");
        let fixed = true_case(&typed);
        assert_eq!(fixed.file_name().unwrap(), "MixedCase Folder");
        assert!(fixed.to_string_lossy().starts_with(|c: char| c.is_ascii_uppercase()));
        // Missing components are kept as typed.
        assert_eq!(true_case(&typed.join("not there")).file_name().unwrap(), "not there");
        let _ = std::fs::remove_dir_all(&base);
    }
}
