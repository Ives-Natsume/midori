mod app;

use app::MidoriApp;
use eframe::egui;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Midori SDR")
            .with_inner_size([1200.0, 700.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Midori SDR",
        native_options,
        Box::new(|cc| Ok(Box::new(MidoriApp::new(cc)))),
    )
}
