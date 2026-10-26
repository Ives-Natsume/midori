//! Satellite pass-planner tab.

use eframe::egui;

use crate::app::MidoriApp;

impl MidoriApp {
    pub(crate) fn draw_passes_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("TLE");
            ui.add(
                egui::TextEdit::singleline(&mut self.tle_path)
                    .hint_text("path/to/iss.tle")
                    .desired_width(220.0),
            );
            ui.label("Grid");
            ui.add(egui::TextEdit::singleline(&mut self.grid_input).desired_width(64.0));
            ui.label("Min el");
            ui.add(egui::TextEdit::singleline(&mut self.min_elev_input).desired_width(40.0));
            if ui.button("Predict 24 h").clicked() {
                self.predict_passes();
            }
            ui.label(egui::RichText::new(&self.pass_status).weak());
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("Satellite: {}", self.satellite_name));
            ui.label(egui::RichText::new("tracking: SGP4 next").weak());
            ui.label(egui::RichText::new("times UTC").weak());
        });

        if self.passes.is_empty() {
            ui.label(
                egui::RichText::new("No passes loaded. Choose a TLE file and press Predict.")
                    .weak(),
            );
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("pass_table")
                .striped(true)
                .num_columns(7)
                .show(ui, |ui| {
                    ui.strong("#");
                    ui.strong("AOS");
                    ui.strong("TCA");
                    ui.strong("LOS");
                    ui.strong("Max el");
                    ui.strong("AOS az");
                    ui.strong("LOS az");
                    ui.end_row();

                    for (index, pass) in self.passes.iter().enumerate() {
                        ui.label((index + 1).to_string());
                        ui.monospace(pass.aos.format("%m-%d %H:%M:%S").to_string());
                        ui.monospace(pass.tca.format("%m-%d %H:%M:%S").to_string());
                        ui.monospace(pass.los.format("%m-%d %H:%M:%S").to_string());
                        ui.label(format!("{:.1}°", pass.max_elevation_deg));
                        ui.label(format!("{:.1}°", pass.aos_azimuth_deg));
                        ui.label(format!("{:.1}°", pass.los_azimuth_deg));
                        ui.end_row();
                    }
                });
        });
    }
}
