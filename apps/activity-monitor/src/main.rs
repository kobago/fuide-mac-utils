#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    // `fuide-activity-monitor --mcp`: stdio MCP bridge to the running app (see `fuide::agent::bridge`)
    if std::env::args().nth(1).as_deref() == Some("--mcp") {
        std::process::exit(fuide::agent::bridge::run(
            "activity-monitor",
            "FUIDE Activity Monitor",
        ));
    }
    fuide::devshot::install_trace_logger();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FUIDE Activity Monitor")
            .with_app_id("fuide-activity-monitor")
            .with_decorations(false)
            .with_transparent(true)
            .with_has_shadow(false)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1024.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FUIDE Activity Monitor",
        options,
        Box::new(|cc| Ok(Box::new(fuide_activity_monitor::app::MonitorApp::new(cc)))),
    )
}
