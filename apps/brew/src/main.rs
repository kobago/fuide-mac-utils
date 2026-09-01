#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod brew;

fn main() -> eframe::Result {
    fuide::devshot::install_trace_logger();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FUIDE Brew")
            .with_app_id("fuide-brew")
            .with_decorations(false)
            .with_transparent(true)
            .with_has_shadow(false)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FUIDE Brew",
        options,
        Box::new(|cc| Ok(Box::new(app::BrewApp::new(cc)))),
    )
}
