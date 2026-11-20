#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
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

    // Размер и позицию окна берём из прошлого запуска.
    let saved = config::Settings::load();
    let (width, height) = saved
        .window
        .map(|(_, _, width, height)| (width, height))
        .unwrap_or((1320.0, 860.0));
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_inner_size([width.max(1040.0), height.max(640.0)])
        .with_min_inner_size([1040.0, 640.0])
        .with_icon(icon)
        .with_title("TrackCut");
    if let Some((x, y, _, _)) = saved.window {
        viewport = viewport.with_position([x, y]);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "TrackCut",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
