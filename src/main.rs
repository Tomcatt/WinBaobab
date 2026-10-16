#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use eframe::egui;
use winbaobab::app::WinBaobabApp;
use winbaobab::app::locations::clean_path_input;

fn main() -> eframe::Result {
    let initial: Option<PathBuf> = std::env::args_os()
        .nth(1)
        .map(|a| clean_path_input(&a.to_string_lossy()));
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/winbaobab.png"))
        .expect("bundled icon is a valid PNG");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WinBaobab")
            .with_icon(icon)
            .with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "WinBaobab",
        options,
        Box::new(move |cc| Ok(Box::new(WinBaobabApp::new(cc, initial)))),
    )
}
