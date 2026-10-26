//! Panel drawing and keyboard input.

use eframe::egui;

use crate::app::MidoriApp;

mod decoders;
mod log;
mod radio;
mod satellite;
mod spectrum;

impl MidoriApp {
    pub(crate) fn handle_keyboard(&mut self, ctx: &egui::Context) {
        if !self.is_connected || self.is_recording {
            return;
        }
        if ctx.memory(|memory| memory.focused().is_some()) {
            return;
        }
        ctx.input(|input| {
            let step = if input.modifiers.shift { 0.1 } else { 0.01 };
            if input.key_pressed(egui::Key::ArrowRight) {
                self.adjust_freq(step);
            }
            if input.key_pressed(egui::Key::ArrowLeft) {
                self.adjust_freq(-step);
            }
            if input.key_pressed(egui::Key::PageUp) {
                self.adjust_freq(1.0);
            }
            if input.key_pressed(egui::Key::PageDown) {
                self.adjust_freq(-1.0);
            }
            if input.key_pressed(egui::Key::G) {
                self.adjust_gain(5.0);
            }
            if input.key_pressed(egui::Key::H) {
                self.adjust_gain(-5.0);
            }
        });
    }
}
