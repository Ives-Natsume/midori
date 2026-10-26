//! Receiver strip, status bar, and radio side panel.

use chrono::{Local, Utc};
use eframe::egui;

use midori_config::{DecoderTab, SourceKind};
use midori_core::DemodMode;

use crate::app::MidoriApp;
use crate::app::format::{format_bandwidth, frequency_from_path};

impl MidoriApp {
    pub(crate) fn draw_receiver_strip(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("receiver_strip").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("MIDORI").strong().size(14.0));
                ui.separator();

                let freq_response = ui.add(
                    egui::TextEdit::singleline(&mut self.freq_input)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(120.0),
                );
                if freq_response.lost_focus()
                    || (freq_response.has_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                {
                    self.send_frequency();
                }
                ui.label(if self.is_file_source() && self.is_connected {
                    "MHz (VFO)"
                } else {
                    "MHz"
                });
                if ui.small_button("-").on_hover_text("-10 kHz").clicked() {
                    self.adjust_freq(-0.01);
                }
                if ui.small_button("+").on_hover_text("+10 kHz").clicked() {
                    self.adjust_freq(0.01);
                }
                ui.separator();

                let previous_demod = self.demod_mode;
                for mode in DemodMode::implemented() {
                    let response = ui.selectable_value(&mut self.demod_mode, mode, mode.label());
                    if mode == DemodMode::BpskIf {
                        response.on_hover_text("mono 48 kHz, 12 kHz real-IF recording");
                    }
                }
                if self.demod_mode != previous_demod {
                    self.config.demod.mode = self.demod_mode;
                    self.mark_demod_dirty();
                    self.auto_tune_status.clear();
                    self.auto_tune_failed = false;
                }

                ui.separator();
                let record_text = if self.is_recording {
                    egui::RichText::new("■ REC").color(egui::Color32::LIGHT_RED)
                } else {
                    egui::RichText::new("● Record IQ")
                };
                if ui.button(record_text).clicked() {
                    if self.is_recording {
                        self.stop_recording();
                    } else {
                        self.start_recording();
                    }
                }
                if self.doppler_tracking {
                    let text = if self.is_connected || self.is_connecting {
                        format!(
                            "Doppler {:+.0} Hz ({:+.1} Hz/s)",
                            self.doppler_hz, self.doppler_rate_hz_s
                        )
                    } else {
                        "Doppler: armed".to_string()
                    };
                    ui.label(
                        egui::RichText::new(text).color(egui::Color32::from_rgb(120, 220, 180)),
                    );
                } else {
                    ui.label(egui::RichText::new("Track: off").weak());
                }
            });

            ui.horizontal(|ui| {
                let signal = if self.signal_dbfs.is_finite() {
                    format!("{:.1}", self.signal_dbfs)
                } else {
                    "--".to_string()
                };
                let snr = if self.signal_dbfs.is_finite() && self.noise_dbfs.is_finite() {
                    format!("{:.1}", self.signal_dbfs - self.noise_dbfs)
                } else {
                    "--".to_string()
                };
                ui.label(egui::RichText::new(format!("S {signal} dBFS  SNR {snr} dB")).weak());
                ui.separator();
                ui.label(
                    egui::RichText::new(format!(
                        "{}  |  BW {}  |  {:.3} MSPS  |  RX1A",
                        self.demod_label(),
                        format_bandwidth(self.demod_bandwidth_hz()),
                        self.sample_rate_hz / 1e6
                    ))
                    .weak(),
                );
                if self.is_recording
                    && let Some(path) = &self.recording_path
                {
                    ui.label(
                        egui::RichText::new(format!("REC {}", path.display()))
                            .color(egui::Color32::LIGHT_RED),
                    );
                }
                if let Some(pass) = self.next_pass() {
                    ui.label(
                        egui::RichText::new(format!(
                            "next {} {:.1}° at {}",
                            self.satellite_name,
                            pass.max_elevation_deg,
                            pass.aos.format("%H:%M:%S")
                        ))
                        .weak(),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(error) = &self.error_msg {
                        ui.label(egui::RichText::new(error).color(egui::Color32::LIGHT_RED));
                    } else {
                        ui.label(egui::RichText::new(&self.status_msg).weak());
                    }
                });
            });
        });
    }

    pub(crate) fn draw_bottom_bar(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let utc = Utc::now();
                let local = Local::now();
                ui.label(
                    egui::RichText::new(format!(
                        "UTC {}   Local {}",
                        utc.format("%H:%M:%S"),
                        local.format("%H:%M:%S")
                    ))
                    .monospace(),
                );
                ui.separator();
                let centre_label = if self.is_file_source() { "CTR" } else { "LO" };
                ui.label(
                    egui::RichText::new(format!(
                        "{centre_label} {:.3} MHz   {:.3} MSPS   FFT {}",
                        self.center_freq_hz / 1e6,
                        self.sample_rate_hz / 1e6,
                        self.config.ui.fft_size
                    ))
                    .weak(),
                );
                if self.is_file_source() || self.vfo_offset_hz.abs() >= 1.0 {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!("VFO {:+.3} kHz", self.vfo_offset_hz / 1e3))
                            .weak(),
                    );
                }
                if self.cw_afc && self.demod_mode == DemodMode::Cw {
                    ui.separator();
                    ui.label(
                        egui::RichText::new("AFC").color(egui::Color32::from_rgb(120, 220, 180)),
                    );
                }
                ui.separator();
                ui.label(
                    egui::RichText::new(format!("GRID {}", self.grid_input.to_uppercase())).weak(),
                );

                if let Some(pass) = self.next_pass() {
                    ui.separator();
                    let now = Utc::now();
                    let label = if pass.aos > now {
                        format!(
                            "NEXT {} AOS {} ({} min)",
                            self.satellite_name,
                            pass.aos.format("%H:%M:%S"),
                            (pass.aos - now).num_seconds() / 60
                        )
                    } else {
                        format!(
                            "IN PASS {}  max {:.1}°  LOS {}",
                            self.satellite_name,
                            pass.max_elevation_deg,
                            pass.los.format("%H:%M:%S")
                        )
                    };
                    ui.label(label);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.is_recording {
                        ui.label(
                            egui::RichText::new("REC")
                                .color(egui::Color32::LIGHT_RED)
                                .strong(),
                        );
                    }
                    ui.label(
                        egui::RichText::new(if self.is_connected {
                            "connected"
                        } else {
                            "idle"
                        })
                        .weak(),
                    );
                });
            });
        });
    }
    pub(crate) fn draw_radio_panel(&mut self, ctx: &egui::Context) {
        let panel = egui::SidePanel::left("radio_panel")
            .resizable(true)
            .default_width(self.config.ui.left_panel_width)
            .min_width(230.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Radio");
                ui.separator();

                if self.worker_stopping {
                    ui.add_enabled(
                        false,
                        egui::Button::new("Stopping worker...")
                            .min_size(egui::vec2(ui.available_width(), 30.0)),
                    );
                } else if self.is_connected || self.is_connecting {
                    if ui
                        .add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::new("Disconnect"),
                        )
                        .clicked()
                    {
                        self.disconnect();
                    }
                } else {
                    let start_label = if self.is_file_source() {
                        "Start file"
                    } else {
                        "Connect Pluto"
                    };
                    if ui
                        .add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::new(start_label),
                        )
                        .clicked()
                    {
                        self.connect();
                    }
                }

                egui::CollapsingHeader::new("Source")
                    .default_open(true)
                    .show(ui, |ui| {
                        let previous = self.source_kind;
                        ui.horizontal(|ui| {
                            ui.label("Input");
                            ui.selectable_value(&mut self.source_kind, SourceKind::Pluto, "Pluto");
                            ui.selectable_value(&mut self.source_kind, SourceKind::File, "I/Q file");
                        });
                        if self.source_kind != previous && (self.is_connected || self.is_connecting)
                        {
                            self.status_msg = "Reconnect to change source".to_string();
                        }

                        if self.is_file_source() {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.file_path)
                                    .hint_text("data/recording_437393500Hz.wav")
                                    .desired_width(ui.available_width()),
                            );
                            ui.horizontal(|ui| {
                                let from_name = frequency_from_path(&self.file_path);
                                if ui
                                    .add_enabled(from_name.is_some(), egui::Button::new("From filename"))
                                    .clicked()
                                    && let Some(mhz) = from_name
                                {
                                    self.freq_input = format!("{mhz:.6}");
                                    self.center_freq_hz = mhz * 1e6;
                                }
                                ui.checkbox(&mut self.file_loop, "Loop");
                            });
                            if let Some(mhz) = frequency_from_path(&self.file_path) {
                                ui.label(
                                    egui::RichText::new(format!("filename centre: {mhz:.6} MHz"))
                                        .weak(),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new(
                                        "set the centre from the filename or the frequency box",
                                    )
                                    .weak(),
                                );
                            }
                            ui.label(
                                egui::RichText::new(
                                    "RIFF/RF64 WAV, I/Q. Playback is real time; tune with the VFO after start.",
                                )
                                .weak(),
                            );
                        } else {
                            ui.horizontal(|ui| {
                                ui.label("Gain (dB)");
                                let response = ui.add(
                                    egui::TextEdit::singleline(&mut self.gain_input)
                                        .hint_text("60")
                                        .desired_width(60.0),
                                );
                                if response.lost_focus()
                                    || (response.has_focus()
                                        && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                                {
                                    self.send_gain();
                                }
                            });
                            ui.horizontal(|ui| {
                                if ui.small_button("-5").clicked() {
                                    self.adjust_gain(-5.0);
                                }
                                if ui.small_button("-1").clicked() {
                                    self.adjust_gain(-1.0);
                                }
                                if ui.small_button("+1").clicked() {
                                    self.adjust_gain(1.0);
                                }
                                if ui.small_button("+5").clicked() {
                                    self.adjust_gain(5.0);
                                }
                                if ui.small_button("Set").clicked() {
                                    self.send_gain();
                                }
                            });
                            ui.label(egui::RichText::new("Port: RX1A / A_BALANCED").weak());
                        }

                        ui.label(
                            egui::RichText::new(format!(
                                "Sample rate: {:.3} MSPS",
                                self.sample_rate_hz / 1e6
                            ))
                            .weak(),
                        );
                        let centre_label = if self.is_file_source() {
                            "File centre"
                        } else {
                            "LO"
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{centre_label}: {:.3} MHz",
                                self.center_freq_hz / 1e6
                            ))
                            .weak(),
                        );
                        if self.is_file_source() || self.vfo_offset_hz.abs() >= 1.0 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "VFO offset: {:+.3} kHz{}",
                                    self.vfo_offset_hz / 1e3,
                                    if self.cw_afc && self.demod_mode == DemodMode::Cw {
                                        " (AFC)"
                                    } else {
                                        ""
                                    }
                                ))
                                .weak(),
                            );
                        }
                    });

                egui::CollapsingHeader::new("Demod")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(format!(
                            "Mode: {}   BW: {}",
                            self.demod_label(),
                            format_bandwidth(self.demod_bandwidth_hz())
                        ));

                        let mut changed = false;
                        if let Some(params) = self.config.demod.params_mut(self.demod_mode) {
                            ui.horizontal(|ui| {
                                ui.label("RF bandwidth");
                                let mut bandwidth_khz = params.bandwidth_hz / 1_000.0;
                                let response = ui.add(
                                    egui::DragValue::new(&mut bandwidth_khz)
                                        .range(0.1..=2_000.0)
                                        .speed(0.5)
                                        .suffix(" kHz"),
                                );
                                if response.changed() {
                                    params.bandwidth_hz =
                                        (bandwidth_khz * 1_000.0).clamp(100.0, 2_000_000.0);
                                    changed = true;
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("Audio bandwidth");
                                let mut audio_hz = params.audio_bandwidth_hz;
                                let response = ui.add(
                                    egui::DragValue::new(&mut audio_hz)
                                        .range(100.0..=24_000.0)
                                        .speed(50.0)
                                        .suffix(" Hz"),
                                );
                                if response.changed() {
                                    params.audio_bandwidth_hz =
                                        audio_hz.clamp(100.0, params.audio_rate_hz * 0.45);
                                    changed = true;
                                }
                            });
                            match self.demod_mode {
                                DemodMode::Fm => {
                                    ui.horizontal(|ui| {
                                        ui.label("De-emphasis");
                                        let mut deemph = params.deemphasis_us;
                                        let response = ui.add(
                                            egui::DragValue::new(&mut deemph)
                                                .range(0.0..=500.0)
                                                .speed(1.0)
                                                .suffix(" µs"),
                                        );
                                        if response.changed() {
                                            params.deemphasis_us = deemph;
                                            changed = true;
                                        }
                                    });
                                }
                                DemodMode::Cw => {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "BFO {:.0} Hz (set demod.cw.bfo_hz in config)",
                                            params.bfo_hz
                                        ))
                                        .weak(),
                                    );
                                }
                                DemodMode::Am => {
                                    ui.label(
                                        egui::RichText::new("envelope detector").weak(),
                                    );
                                }
                                DemodMode::Sstv => {
                                    ui.label(
                                        egui::RichText::new(
                                            "FM · 3 kHz audio · no de-emphasis",
                                        )
                                        .weak(),
                                    );
                                }
                                DemodMode::Bpsk => {
                                    ui.label(
                                        egui::RichText::new(
                                            "complex output · 9k6 BPSK data/SSDV",
                                        )
                                        .weak(),
                                    );
                                }
                                DemodMode::BpskIf => {
                                    ui.label(
                                        egui::RichText::new(
                                            "mono 48 kHz real IF · 12 kHz center · pass-through",
                                        )
                                        .weak(),
                                    );
                                }
                                _ => {}
                            }
                        }
                        if changed {
                            self.mark_demod_dirty();
                        }
                        if self.demod_dirty && (self.is_connected || self.is_connecting) {
                            ui.label(
                                egui::RichText::new("changes apply automatically")
                                    .weak(),
                            );
                        }
                        let mut agc = false;
                        let mut squelch = false;
                        ui.add_enabled(false, egui::Checkbox::new(&mut agc, "AGC"));
                        ui.add_enabled(false, egui::Checkbox::new(&mut squelch, "Squelch"));
                    });

                egui::CollapsingHeader::new("FFT / Waterfall")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!("FFT {} · Hann", self.config.ui.fft_size))
                                .weak(),
                        );
                        ui.horizontal(|ui| {
                            ui.label("dB min");
                            ui.add(
                                egui::DragValue::new(&mut self.db_min)
                                    .range(-200.0..=-10.0)
                                    .speed(1.0),
                            );
                            ui.label("max");
                            ui.add(
                                egui::DragValue::new(&mut self.db_max)
                                    .range(-100.0..=0.0)
                                    .speed(1.0),
                            );
                        });
                        if self.db_max <= self.db_min + 10.0 {
                            self.db_max = self.db_min + 10.0;
                        }
                        ui.checkbox(
                            &mut self.waterfall_follow,
                            "Waterfall follows spectrum zoom",
                        );
                        if !self.waterfall_follow {
                            ui.label(
                                egui::RichText::new(
                                    "yellow lines mark the spectrum view on the full-band waterfall",
                                )
                                .weak(),
                            );
                        }
                        if ui.button("Reset spectrum view").clicked() {
                            self.reset_spectrum_view = true;
                        }
                        ui.label(
                            egui::RichText::new(format!("colormap {:?}", self.colormap)).weak(),
                        );
                    });

                egui::CollapsingHeader::new("Recording")
                    .default_open(true)
                    .show(ui, |ui| {
                        if self.is_recording {
                            ui.label(
                                egui::RichText::new("recording IQ")
                                    .color(egui::Color32::LIGHT_RED)
                                    .strong(),
                            );
                            if let Some(path) = &self.recording_path {
                                ui.label(egui::RichText::new(path.display().to_string()).weak());
                            }
                            if ui.button("Stop recording").clicked() {
                                self.stop_recording();
                            }
                        } else if ui.button("Record IQ").clicked() {
                            self.start_recording();
                        }
                        ui.label(egui::RichText::new("writes SigMF into captures/").weak());
                    });

                egui::CollapsingHeader::new("Satellite")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(format!("TLE: {}", self.satellite_name));
                        ui.label(format!("Grid: {}", self.grid_input.to_uppercase()));
                        if let Some(pass) = self.next_pass() {
                            ui.label(format!(
                                "Next: {}  max {:.1}°  AOS {}",
                                pass.aos.format("%m-%d %H:%M:%S"),
                                pass.max_elevation_deg,
                                pass.aos.format("%H:%M:%S")
                            ));
                        } else {
                            ui.label(egui::RichText::new("no pass loaded").weak());
                        }
                        if ui.button("Open pass planner").clicked() {
                            self.decoder_tab = DecoderTab::Passes;
                            self.decoder_dock_open = true;
                        }
                        let previous = self.doppler_tracking;
                        ui.checkbox(&mut self.doppler_tracking, "Track Doppler (SGP4)");
                        if self.doppler_tracking != previous
                            && (self.is_connected || self.is_connecting)
                        {
                            self.status_msg = "Reconnect to apply Doppler tracking".to_string();
                        }
                        if self.doppler_tracking {
                            ui.label(
                                egui::RichText::new(format!(
                                    "shift {:+.0} Hz   rate {:+.1} Hz/s",
                                    self.doppler_hz, self.doppler_rate_hz_s
                                ))
                                .monospace(),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "keep the LO on the nominal downlink; the VFO corrects",
                                )
                                .weak(),
                            );
                        } else {
                            ui.label(egui::RichText::new("Doppler tracking off").weak());
                        }
                        if self.is_file_source() {
                            ui.label(
                                egui::RichText::new(
                                    "file playback: Doppler is already present in the recording",
                                )
                                .weak(),
                            );
                        }
                    });

                egui::CollapsingHeader::new("Keyboard shortcuts")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label("Left / Right     +/-10 kHz");
                        ui.label("Shift+Left/Right +/-100 kHz");
                        ui.label("PageUp / PageDown +/-1 MHz");
                        ui.label("G / H            gain +/-5 dB");
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Save settings").clicked() {
                        self.save_config();
                    }
                    if ui.button("Reload config").clicked() {
                        self.reload_config(ui.ctx());
                    }
                });
                ui.label(
                    egui::RichText::new(format!("config: {}", self.config_path.display())).weak(),
                );
            });
        self.config.ui.left_panel_width = panel.response.rect.width();
    }
}
