//! Log tab.

use eframe::egui;

use crate::app::MidoriApp;

impl MidoriApp {
    pub(crate) fn draw_log_tab(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.log.is_empty() {
                    ui.label(egui::RichText::new("no log events yet").weak());
                } else {
                    for line in &self.log {
                        ui.monospace(line);
                    }
                }
            });
    }
}
