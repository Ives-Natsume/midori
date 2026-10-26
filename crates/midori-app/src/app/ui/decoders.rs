//! CW, SSTV, and SSDV tabs, the decoder dock, and the decoder side panel.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;

use midori_config::DecoderTab;
use midori_core::DemodMode;

use crate::app::MidoriApp;

impl MidoriApp {
    pub(crate) fn draw_decoder_dock(&mut self, ctx: &egui::Context) {
        if !self.decoder_dock_open {
            return;
        }
        let dock = egui::TopBottomPanel::bottom("decoder_dock")
            .resizable(true)
            .default_height(self.config.ui.decoder_dock_height)
            .min_height(80.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.decoder_tab, DecoderTab::Cw, "CW");
                    ui.selectable_value(&mut self.decoder_tab, DecoderTab::Sstv, "SSTV");
                    ui.selectable_value(&mut self.decoder_tab, DecoderTab::Ssdv, "SSDV");
                    ui.selectable_value(&mut self.decoder_tab, DecoderTab::Passes, "Passes");
                    ui.selectable_value(&mut self.decoder_tab, DecoderTab::Log, "Log");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("hide").clicked() {
                            self.decoder_dock_open = false;
                        }
                        ui.label(egui::RichText::new("decoder dock").weak());
                    });
                });
                ui.separator();

                match self.decoder_tab {
                    DecoderTab::Cw => self.draw_cw_tab(ui),
                    DecoderTab::Sstv => self.draw_sstv_tab(ui),
                    DecoderTab::Ssdv => self.draw_ssdv_tab(ui),
                    DecoderTab::Passes => self.draw_passes_tab(ui),
                    DecoderTab::Log => self.draw_log_tab(ui),
                }
            });
        self.config.ui.decoder_dock_height = dock.response.rect.height();
    }

    pub(crate) fn draw_cw_tab(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("CW");
            ui.label(&self.cw_status);
            if self.cw_afc && self.demod_mode == DemodMode::Cw {
                ui.label(
                    egui::RichText::new(format!("AFC {:+.1} Hz", self.vfo_offset_hz))
                        .color(egui::Color32::from_rgb(120, 220, 180)),
                );
            }
        });
        if !self.auto_tune_status.is_empty() {
            ui.label(egui::RichText::new(&self.auto_tune_status).color(self.auto_tune_color()));
        }
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.cw_text.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Enable the CW decoder, select CW demod, and tune a beacon.",
                        )
                        .weak(),
                    );
                } else {
                    ui.monospace(&self.cw_text);
                }
            });
    }

    pub(crate) fn draw_sstv_tab(&mut self, ui: &mut egui::Ui) {
        let mut save = false;
        let mut clear = false;
        ui.columns(2, |columns| {
            let info = &mut columns[0];
            info.heading("SSTV");
            info.label(&self.sstv_status);
            info.horizontal(|ui| {
                if ui
                    .add_enabled(self.sstv_image_png.is_some(), egui::Button::new("Save PNG"))
                    .clicked()
                {
                    save = true;
                }
                if self.sstv_texture.is_some() && ui.small_button("Clear").clicked() {
                    clear = true;
                }
            });
            if self.sstv_progress > 0.0 && self.sstv_progress < 1.0 {
                info.add(
                    egui::ProgressBar::new(self.sstv_progress)
                        .desired_height(8.0)
                        .show_percentage(),
                );
            }
            info.separator();
            info.label(
                egui::RichText::new(
                    "Select the SSTV demod and tune an FM SSTV signal.\n\
                     VIS is detected automatically; PD-120 and Robot 36 are the primary modes.\n\
                     Images are saved to captures/.",
                )
                .weak(),
            );

            draw_decoder_image(
                &mut columns[1],
                self.sstv_texture.as_ref(),
                &self.sstv_image_title,
                "SSTV image",
            );
        });
        if save {
            self.save_sstv_png();
        }
        if clear {
            self.sstv_texture = None;
            self.sstv_image_png = None;
            self.sstv_image_title.clear();
            self.sstv_progress = 0.0;
        }
    }

    pub(crate) fn save_sstv_png(&mut self) {
        let Some(bytes) = &self.sstv_image_png else {
            return;
        };
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("captures")
            .join(format!("sstv_manual_{stamp}.png"));
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            self.sstv_status = format!("creating {}: {error}", parent.display());
            return;
        }
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.sstv_status = format!("saved {}", path.display());
                self.push_log(self.sstv_status.clone());
            }
            Err(error) => {
                let message = format!("saving {}: {error}", path.display());
                self.sstv_status = message.clone();
                self.push_log(message.clone());
                self.error_msg = Some(message);
            }
        }
    }

    pub(crate) fn draw_ssdv_tab(&mut self, ui: &mut egui::Ui) {
        let mut save = false;
        let mut clear = false;
        ui.columns(2, |columns| {
            let info = &mut columns[0];
            info.heading("SSDV");
            info.label(&self.ssdv_status);
            info.horizontal(|ui| {
                if ui
                    .add_enabled(self.ssdv_image_png.is_some(), egui::Button::new("Save PNG"))
                    .clicked()
                {
                    save = true;
                }
                if self.ssdv_texture.is_some() && ui.small_button("Clear").clicked() {
                    clear = true;
                }
            });
            if self.ssdv_progress > 0.0 && self.ssdv_progress < 1.0 {
                info.add(
                    egui::ProgressBar::new(self.ssdv_progress)
                        .desired_height(8.0)
                        .show_percentage(),
                );
            }
            info.separator();
            info.label(
                egui::RichText::new(
                    "Select the BPSK 9k6 demod and tune the image downlink.\n\
                     The decoder accepts 218-byte DSLWP packets and classic 256-byte SSDV.\n\
                     JPEG/PNG and packet dumps are saved to captures/.",
                )
                .weak(),
            );

            draw_decoder_image(
                &mut columns[1],
                self.ssdv_texture.as_ref(),
                &self.ssdv_image_title,
                "SSDV image",
            );
        });
        if save {
            self.save_ssdv_png();
        }
        if clear {
            self.ssdv_texture = None;
            self.ssdv_image_png = None;
            self.ssdv_image_title.clear();
            self.ssdv_progress = 0.0;
        }
    }

    pub(crate) fn save_ssdv_png(&mut self) {
        let Some(bytes) = &self.ssdv_image_png else {
            return;
        };
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("captures")
            .join(format!("ssdv_manual_{stamp}.png"));
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            self.ssdv_status = format!("creating {}: {error}", parent.display());
            return;
        }
        match std::fs::write(&path, bytes) {
            Ok(()) => {
                self.ssdv_status = format!("saved {}", path.display());
                self.push_log(self.ssdv_status.clone());
            }
            Err(error) => {
                let message = format!("saving {}: {error}", path.display());
                self.ssdv_status = message.clone();
                self.push_log(message.clone());
                self.error_msg = Some(message);
            }
        }
    }
    pub(crate) fn draw_decoder_panel(&mut self, ctx: &egui::Context) {
        let panel = egui::SidePanel::right("decoder_panel")
            .resizable(true)
            .default_width(self.config.ui.right_panel_width)
            .min_width(260.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Decoders");
                ui.separator();

                egui::CollapsingHeader::new("CW")
                    .default_open(true)
                    .show(ui, |ui| {
                        let previous = self.cw_enabled;
                        ui.checkbox(&mut self.cw_enabled, "CW decoder (700 Hz)");
                        if self.cw_enabled != previous && (self.is_connected || self.is_connecting)
                        {
                            self.status_msg = "Reconnect to apply CW decoder".to_string();
                        }
                        let previous_afc = self.cw_afc;
                        ui.checkbox(&mut self.cw_afc, "AFC (track carrier drift)");
                        if self.cw_afc != previous_afc && (self.is_connected || self.is_connecting)
                        {
                            self.send_cw_afc();
                        }
                        if ui
                            .add_enabled(
                                self.is_connected && !self.is_recording,
                                egui::Button::new("Auto tune CW carrier"),
                            )
                            .on_hover_text(
                                "Scan a 4 s window within ±100 kHz of the LO and move the digital VFO to the strongest narrowband peak",
                            )
                            .clicked()
                        {
                            self.start_auto_tune();
                        }
                        ui.label(format!("Status: {}", self.cw_status));
                        if !self.auto_tune_status.is_empty() {
                            ui.label(
                                egui::RichText::new(&self.auto_tune_status)
                                    .color(self.auto_tune_color()),
                            );
                        }
                        if self.demod_mode != DemodMode::Cw {
                            ui.label(
                                egui::RichText::new("select the CW demod to decode")
                                    .color(egui::Color32::from_rgb(220, 180, 90)),
                            );
                        }
                        ui.label(egui::RichText::new("output in the bottom CW tab").weak());
                    });

                egui::CollapsingHeader::new("SSTV")
                    .default_open(true)
                    .show(ui, |ui| {
                        let previous = self.sstv_enabled;
                        ui.checkbox(&mut self.sstv_enabled, "SSTV decoder (PD-120 / Robot 36)");
                        if self.sstv_enabled != previous
                            && (self.is_connected || self.is_connecting)
                        {
                            self.status_msg = "Reconnect to apply SSTV decoder".to_string();
                        }
                        ui.label(format!("Status: {}", self.sstv_status));
                        if self.sstv_enabled && self.demod_mode != DemodMode::Sstv {
                            ui.label(
                                egui::RichText::new("select the SSTV demod for 3 kHz audio")
                                    .color(egui::Color32::from_rgb(220, 180, 90)),
                            );
                        }
                        ui.label(
                            egui::RichText::new("slowrx sidecar · images saved to captures/")
                                .weak(),
                        );
                    });

                egui::CollapsingHeader::new("SSDV")
                    .default_open(true)
                    .show(ui, |ui| {
                        let previous_enabled = self.ssdv_enabled;
                        let previous_crc = self.ssdv_crc_inits_input.clone();
                        let previous_header = self.ssdv_frame_header_len;
                        ui.checkbox(&mut self.ssdv_enabled, "SSDV decoder (DSLWP / classic)");
                        if self.ssdv_enabled != previous_enabled
                            && (self.is_connected || self.is_connecting)
                        {
                            self.status_msg = "Reconnect to apply SSDV decoder".to_string();
                        }
                        ui.label(format!("Status: {}", self.ssdv_status));
                        ui.horizontal(|ui| {
                            ui.label("CRC init");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.ssdv_crc_inits_input)
                                    .hint_text("0xFFFFFFFF, 0x4EE4FDE1")
                                    .desired_width(170.0),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Frame header");
                            ui.add(
                                egui::DragValue::new(&mut self.ssdv_frame_header_len)
                                    .range(0..=5)
                                    .suffix(" B"),
                            );
                        });
                        if (self.ssdv_crc_inits_input != previous_crc
                            || self.ssdv_frame_header_len != previous_header)
                            && (self.is_connected || self.is_connecting)
                        {
                            self.status_msg =
                                "Reconnect to apply SSDV CRC/header parameters".to_string();
                        }
                        ui.label(
                            egui::RichText::new(
                                "CRC polynomial is fixed; the initial value is stream-specific.\n\
                                 223-byte CCSDS frames default to a 5-byte header.\n\
                                 Images are saved to captures/.",
                            )
                            .weak(),
                        );
                        if self.ssdv_enabled
                            && !matches!(
                                self.demod_mode,
                                DemodMode::Bpsk | DemodMode::BpskIf
                            )
                        {
                            ui.label(
                                egui::RichText::new(
                                    "select BPSK (IQ) or BPSK IF (real 12 kHz IF)",
                                )
                                .color(egui::Color32::from_rgb(220, 180, 90)),
                            );
                        }
                    });

                egui::CollapsingHeader::new("Plugins")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(format!(
                            "CW: {}",
                            if self.cw_enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        ));
                        ui.label(format!(
                            "SSTV: {}",
                            if self.sstv_enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        ));
                        ui.label(format!(
                            "SSDV: {}",
                            if self.ssdv_enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        ));
                        ui.label(egui::RichText::new("more: APT · FT4").weak());
                    });

                ui.add_space(6.0);
                ui.checkbox(&mut self.decoder_dock_open, "Show decoder dock");
                ui.separator();
                ui.label(egui::RichText::new("plugins run as separate processes").weak());
            });
        self.config.ui.right_panel_width = panel.response.rect.width();
    }
}
pub(crate) fn draw_decoder_image(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    title: &str,
    placeholder: &str,
) {
    ui.separator();
    let Some(texture) = texture else {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new(placeholder).weak());
        });
        return;
    };

    let available = ui.available_size();
    let original = texture.size_vec2();
    let scale = (available.x / original.x)
        .min(available.y / original.y)
        .clamp(0.05, 1.0);
    let size = original * scale;
    ui.centered_and_justified(|ui| {
        let image = egui::Image::new(egui::load::SizedTexture::new(texture.id(), size));
        ui.add(image).on_hover_text(title);
    });
}
