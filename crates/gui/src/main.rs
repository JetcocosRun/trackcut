#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod i18n;
mod preview;

use app::App;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "trackcut=info,trackcut_core=info".into()),
        )
        .with_target(false)
        .init();

    let icon = eframe::egui::IconData {
        rgba: include_bytes!("../assets/icon_128.rgba").to_vec(),
        width: 128,
        height: 128,
    };

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([1040.0, 640.0])
            .with_icon(icon)
            .with_title("TrackCut"),
        ..Default::default()
    };

    eframe::run_native(
        "TrackCut",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
