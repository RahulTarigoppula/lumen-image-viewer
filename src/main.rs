#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Lumen - a fast, private, modern image & video viewer for Windows.

mod app;
mod catalog;
mod bg;
mod decode;
mod devhook;
mod edit;
mod encode;
mod fileops;
mod formats;
mod imaging;
mod loader;
mod meta;
mod render;
mod settings;
mod shell;
mod tags;
mod thumbs;
mod ui;
mod video;

use std::path::PathBuf;

fn main() -> eframe::Result {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("{info}\n\n{}", std::backtrace::Backtrace::force_capture());
        let dir = settings::data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("crash.log"), &msg);
        eprintln!("{msg}");
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let initial: Option<PathBuf> = args.iter().find(|a| !a.starts_with("--")).map(PathBuf::from);

    let settings = settings::Settings::load();

    // Single instance: if Lumen is already running, hand it the file and exit.
    // Any failure falls through to a normal start (see shell.rs).
    let force_new = args.iter().any(|a| a == "--new-window") || std::env::var_os("LUMEN_DEV_DIR").is_some();
    if settings.reuse_window && !force_new {
        if let Some(p) = &initial {
            if shell::forward_to_running(std::slice::from_ref(p)) {
                return Ok(());
            }
        }
    }
    shell::start_instance_server();

    let icon = image::load_from_memory(include_bytes!("../assets/lumen-256.png"))
        .ok()
        .map(|img| {
            let rgba = img.into_rgba8();
            let (w, h) = rgba.dimensions();
            egui::IconData { rgba: rgba.into_raw(), width: w, height: h }
        });

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Lumen")
        .with_inner_size([1360.0, 860.0])
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
        ..Default::default()
    };
    eframe::run_native(
        "Lumen",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, settings, initial)))),
    )
}
