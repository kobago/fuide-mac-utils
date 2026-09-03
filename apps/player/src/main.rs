#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    // `fuide-player --mcp`: stdio MCP bridge to the running app (see `fuide::agent::bridge`)
    if std::env::args().nth(1).as_deref() == Some("--mcp") {
        std::process::exit(fuide::agent::bridge::run("player", "FUIDE Player"));
    }
    fuide::devshot::install_trace_logger();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FUIDE Player")
            .with_app_id("fuide-player")
            .with_decorations(false)
            .with_transparent(true)
            .with_has_shadow(false)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FUIDE Player",
        options,
        Box::new(|cc| Ok(Box::new(fuide_player::app::PlayerApp::new(cc)))),
    )
}
