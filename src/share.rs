//! Share... : the Windows share sheet for files (mail, nearby sharing, apps).
//!
//! The share window is opened for Lumen's main window through
//! `IDataTransferManagerInterop`. Its `DataRequested` handler, registered once
//! per window, hands over the files waiting in [`PENDING`]; they are turned
//! into storage items on a short-lived thread (WinRT file objects are made
//! asynchronously), under a deferral, so the UI thread never waits. When the
//! share window cannot be opened, Explorer's own Share command on the files
//! is tried instead.

use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};

/// What the next share request hands over.
#[derive(Clone)]
struct Payload {
    title: String,
    files: Vec<PathBuf>,
}

static PENDING: Mutex<Option<Payload>> = Mutex::new(None);
/// The window whose `DataRequested` handler is registered (0: none yet).
static REGISTERED: AtomicIsize = AtomicIsize::new(0);

/// The title the share window shows: the file name, or "3 files".
pub fn share_title(files: &[PathBuf]) -> String {
    match files {
        [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "1 file".into()),
        _ => format!("{} files", files.len()),
    }
}

/// Open the Windows share window for `files` over the window `hwnd` (Lumen's
/// main window; the active window when unknown). Returns, for a message, why
/// sharing is not possible.
pub fn share_files(hwnd: Option<isize>, files: &[PathBuf]) -> Result<(), String> {
    if files.is_empty() {
        return Err("Select the files to share".into());
    }
    let hwnd = hwnd.filter(|h| *h != 0).or_else(active_window).ok_or_else(|| "Lumen's window was not found".to_string())?;
    let first = match share_sheet(hwnd, files) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    log::info!("share window not available ({first}); trying Explorer's Share command");
    match share_verb(hwnd, files) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("Explorer's Share command failed too: {e}");
            Err(format!("Windows could not open the share window ({first})"))
        }
    }
}

fn active_window() -> Option<isize> {
    // SAFETY: no arguments; returns the calling thread's active window or null.
    let h = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow() };
    (!h.0.is_null()).then_some(h.0 as isize)
}

/// The share window of `IDataTransferManagerInterop` (Windows 10 and later).
fn share_sheet(hwnd: isize, files: &[PathBuf]) -> Result<(), String> {
    use windows::ApplicationModel::DataTransfer::DataTransferManager;
    use windows::Foundation::TypedEventHandler;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::IDataTransferManagerInterop;
    let msg = |e: windows::core::Error| e.message();
    if !DataTransferManager::IsSupported().unwrap_or(true) {
        return Err("sharing is not supported on this PC".into());
    }
    let window = HWND(hwnd as *mut core::ffi::c_void);
    let interop = windows::core::factory::<DataTransferManager, IDataTransferManagerInterop>().map_err(msg)?;
    *PENDING.lock() = Some(Payload { title: share_title(files), files: files.to_vec() });
    if REGISTERED.load(Ordering::Relaxed) != hwnd {
        // SAFETY: `window` is Lumen's own top-level window, alive while it runs.
        let manager: DataTransferManager = unsafe { interop.GetForWindow(window) }.map_err(msg)?;
        let handler = TypedEventHandler::<DataTransferManager, windows::ApplicationModel::DataTransfer::DataRequestedEventArgs>::new(on_data_requested);
        manager.DataRequested(&handler).map_err(msg)?;
        REGISTERED.store(hwnd, Ordering::Relaxed);
        // The handler stays registered for the window's whole life: keep the
        // manager alive too, and never release it after COM has shut down.
        std::mem::forget(manager);
    }
    // SAFETY: as above; the call returns at once and the window opens on its own.
    unsafe { interop.ShowShareUIForWindow(window) }.map_err(msg)
}

/// The share window asks for the data: hand over the pending files.
fn on_data_requested(
    _: windows::core::Ref<windows::ApplicationModel::DataTransfer::DataTransferManager>,
    args: windows::core::Ref<windows::ApplicationModel::DataTransfer::DataRequestedEventArgs>,
) -> windows::core::Result<()> {
    use windows::core::HSTRING;
    let request = args.ok()?.Request()?;
    let Some(p) = PENDING.lock().clone() else {
        return request.FailWithDisplayText(&HSTRING::from("Nothing to share"));
    };
    let data = request.Data()?;
    let props = data.Properties()?;
    props.SetTitle(&HSTRING::from(p.title.as_str()))?;
    props.SetApplicationName(&HSTRING::from("Lumen"))?;
    // File objects are made asynchronously: on another thread, while the
    // share window waits for the deferral.
    let deferral = request.GetDeferral()?;
    let req = request.clone();
    let spawned = std::thread::Builder::new().name("lumen-share".into()).spawn(move || {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        let request = req;
        // SAFETY: COM for this thread only, released after the last object below.
        let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        match storage_items(&p.files) {
            Ok(items) => {
                if let Err(e) = data.SetStorageItemsReadOnly(&items) {
                    let _ = request.FailWithDisplayText(&HSTRING::from(format!("Could not share the files: {}", e.message())));
                }
            }
            Err(e) => {
                let _ = request.FailWithDisplayText(&HSTRING::from(format!("Could not share the files: {}", e.message())));
            }
        }
        let _ = deferral.Complete();
        drop((data, request, deferral));
        if init.is_ok() {
            unsafe { CoUninitialize() };
        }
    });
    if let Err(e) = spawned {
        log::warn!("cannot start the share thread: {e}");
        request.FailWithDisplayText(&HSTRING::from("Could not share the files"))?;
    }
    Ok(())
}

/// `files` as WinRT storage items (blocks until each file object is made).
fn storage_items(files: &[PathBuf]) -> windows::core::Result<windows_collections::IIterable<windows::Storage::IStorageItem>> {
    use windows::core::{Interface, HSTRING};
    use windows::Storage::{IStorageItem, StorageFile};
    let mut items: Vec<Option<IStorageItem>> = Vec::with_capacity(files.len());
    for f in files {
        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(f.as_os_str()))?.join()?;
        items.push(Some(file.cast::<IStorageItem>()?));
    }
    Ok(windows_collections::IIterable::<IStorageItem>::from(items))
}

/// Explorer's Share command ("Windows.ModernShare") on the files, through
/// their shell context menu. All files must be in one folder.
fn share_verb(hwnd: isize, files: &[PathBuf]) -> Result<(), String> {
    use windows::core::{s, w, HSTRING};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        IContextMenu, ILFree, SHCreateShellItemArrayFromIDLists, SHParseDisplayName, BHID_SFUIObject, CMF_NORMAL, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX,
    };
    use windows::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu, SW_SHOWNORMAL};
    // CMIC_MASK_UNICODE and CMIC_MASK_FLAG_NO_UI (ShObjIdl_core.h).
    const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
    const CMIC_MASK_FLAG_NO_UI: u32 = 0x0000_0400;
    let msg = |e: windows::core::Error| e.message();
    let Some(folder) = files[0].parent() else { return Err("no folder".into()) };
    let files: Vec<&PathBuf> = files.iter().filter(|f| f.parent() == Some(folder)).collect();
    let window = HWND(hwnd as *mut core::ffi::c_void);
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(files.len());
    // SAFETY: every PIDL comes from SHParseDisplayName and is freed below; the
    // menu is destroyed below; the verb strings are static.
    let res = unsafe {
        (|| -> Result<(), String> {
            for f in &files {
                let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
                SHParseDisplayName(&HSTRING::from(f.as_os_str()), None, &mut pidl, 0, None).map_err(msg)?;
                pidls.push(pidl);
            }
            let list: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
            let items = SHCreateShellItemArrayFromIDLists(&list).map_err(msg)?;
            let menu: IContextMenu = items.BindToHandler(None, &BHID_SFUIObject).map_err(msg)?;
            let popup = CreatePopupMenu().map_err(msg)?;
            let filled = menu.QueryContextMenu(popup, 0, 1, 0x7FFF, CMF_NORMAL);
            let res = if filled.is_err() {
                Err(windows::core::Error::from(filled).message())
            } else {
                let info = CMINVOKECOMMANDINFOEX {
                    cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
                    fMask: CMIC_MASK_UNICODE | CMIC_MASK_FLAG_NO_UI,
                    hwnd: window,
                    lpVerb: s!("Windows.ModernShare"),
                    lpVerbW: w!("Windows.ModernShare"),
                    nShow: SW_SHOWNORMAL.0,
                    ..Default::default()
                };
                menu.InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO).map_err(msg)
            };
            let _ = DestroyMenu(popup);
            res
        })()
    };
    for p in pidls {
        // SAFETY: each PIDL was allocated by SHParseDisplayName and is freed once.
        unsafe { ILFree(Some(p as *const ITEMIDLIST)) };
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(share_title(&[PathBuf::from(r"C:\Photos\beach.jpg")]), "beach.jpg");
        assert_eq!(share_title(&[PathBuf::from(r"C:\a.jpg"), PathBuf::from(r"C:\b.jpg")]), "2 files");
        assert!(share_files(None, &[]).is_err());
    }
}
