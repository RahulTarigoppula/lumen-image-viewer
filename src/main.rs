#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Lumen - a fast, private, modern image & video viewer for Windows.

mod analysis_ui;
mod app;
mod bg;
mod devhook;
mod edit;
mod fileops;
mod loader;
mod logging;
mod recycle;
mod render;
mod share;
mod shell;
mod thumbs;
mod ui;
mod video;

// UI-free modules live in the lumen-core crate; importing them at the crate
// root keeps `crate::decode`, `crate::settings`, ... paths working.
use lumen_core::{catalog, decode, encode, formats, imaging, meta, metaedit, settings, tags};

use std::path::PathBuf;

fn main() -> eframe::Result {
    devhook::mark("main");
    #[cfg(feature = "video")]
    lumen_core::video::set_thumbnailer(video::engine::thumbnail);
    logging::init();
    // Rayon runs decoders, resizing and histograms: below the UI thread. A
    // panic in a spawned job is logged; without a handler rayon aborts the app.
    let _ = rayon::ThreadPoolBuilder::new().thread_name(|i| format!("lumen-rayon-{i}")).start_handler(|_| bg::background_priority()).panic_handler(bg::rayon_panic).build_global();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let initial: Option<PathBuf> = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from);

    let settings = settings::Settings::load();
    devhook::mark("settings");

    // Single instance: if Lumen is already running, hand it the file and exit.
    // Any failure falls through to a normal start (see shell.rs).
    // The first process owns a per-user mutex; launches that start at the same
    // moment (Explorer "Open" on many files) see it and wait for its pipe.
    // Dev hook instances (LUMEN_DEV_DIR) are automation targets: they never
    // forward to, nor act as, the running instance, so files opened from
    // Explorer always reach the user's real window.
    let dev_instance = std::env::var_os("LUMEN_DEV_DIR").is_some();
    let force_new = args.iter().any(|a| a == "--new-window") || dev_instance;
    if settings.reuse_window && !force_new {
        let primary = shell::claim_primary();
        if let Some(p) = &initial {
            if shell::forward_to_running(std::slice::from_ref(p), !primary) {
                return Ok(());
            }
        }
    }
    if !dev_instance {
        shell::start_instance_server();
    }

    let icon = image::load_from_memory(include_bytes!("../assets/lumen-256.png"))
        .ok()
        .map(|img| {
            let rgba = img.into_rgba8();
            let (w, h) = rgba.dimensions();
            egui::IconData { rgba: rgba.into_raw(), width: w, height: h }
        });

    // The last un-maximized size, within sane bounds (a corrupt value must not
    // open a 1x1 or 100,000-wide window).
    let size = settings.window_size.filter(|[w, h]| (640.0..=16384.0).contains(w) && (420.0..=16384.0).contains(h)).unwrap_or([1360.0, 860.0]);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Lumen")
        .with_inner_size(size)
        .with_min_inner_size([640.0, 420.0])
        .with_drag_and_drop(true)
        .with_maximized(settings.window_maximized);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        persist_window: false,
        // Ctrl+C / Ctrl+V / Ctrl+X must reach the app's keymap (copy files, paste
        // images); text boxes get them re-injected by the app (see App::clipboard_keys).
        clipboard_shortcuts: false,
        ..Default::default()
    };
    devhook::mark("run_native");
    let res = eframe::run_native(
        "Lumen",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, settings, initial)))),
    );
    if let Err(e) = &res {
        // Usually no usable OpenGL driver: keep the reason for troubleshooting.
        log::error!("could not start the window: {e}");
        logging::write_crash(&format!("{} could not start the window: {e}

", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    }
    res
}
