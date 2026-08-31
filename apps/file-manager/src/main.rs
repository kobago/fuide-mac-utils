#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod fs;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FUIDE File Manager")
            .with_app_id("fuide-file-manager")
            .with_decorations(false)
            .with_transparent(true)
            .with_has_shadow(false)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FUIDE File Manager",
        options,
        Box::new(|cc| Ok(Box::new(app::Explorer::new(cc)))),
    )
}
