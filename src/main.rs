// No console window behind the app on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod decode;
mod engine;
mod export;
mod project;
mod update;

fn main() -> eframe::Result {
    // Absolute, so audio paths stay valid whatever the working directory.
    let open_arg = std::env::args_os().nth(1).map(|p| std::fs::canonicalize(&p).unwrap_or(p.into()));
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Cue Spark")
            .with_app_id("cuespark")
            .with_inner_size([1100.0, 780.0])
            .with_min_inner_size([900.0, 620.0])
            .with_drag_and_drop(true)
            .with_icon(eframe::egui::IconData {
                rgba: include_bytes!("../assets/icon-128.rgba").to_vec(),
                width: 128,
                height: 128,
            }),
        ..Default::default()
    };
    eframe::run_native("Cue Spark", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, open_arg)))))
}

/// Stop Windows from sleeping or blanking the screen in the middle of a show.
#[cfg(windows)]
pub fn set_keep_awake(on: bool) {
    use windows_sys::Win32::System::Power::*;
    let flags = if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED } else { ES_CONTINUOUS };
    unsafe { SetThreadExecutionState(flags) };
}

// No screen-saver inhibit on Linux yet (the idle-inhibit portal would do it).
#[cfg(not(windows))]
pub fn set_keep_awake(_on: bool) {}
