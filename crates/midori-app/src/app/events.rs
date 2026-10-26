//! Worker event and spectrum-frame polling.

use eframe::egui;

use midori_config::DecoderTab;
use midori_dsp::median;
use midori_plugin::{DisplayUpdate, ImageFormat, PluginEvent};

use crate::app::MidoriApp;
use crate::app::format::downsample;
use crate::app::images::decode_image_update;
use crate::worker::SpectrumFrame;
use crate::worker::events::{PluginSource, WorkerEvent, accept_worker_event};

impl MidoriApp {
    pub(crate) fn poll_events(&mut self, ctx: &egui::Context) {
        let events: Vec<WorkerEvent> = {
            let Some(rx) = &self.event_rx else { return };
            let mut events = Vec::new();
            while let Some(event) = rx.try_recv() {
                events.push(event);
            }
            events
        };

        for event in events {
            if !accept_worker_event(self.worker_stopping, &event) {
                self.push_log("ignored a late Connected event while stopping");
                continue;
            }
            match event {
                WorkerEvent::Connected(spec) => {
                    self.is_connecting = false;
                    self.is_connected = true;
                    self.center_freq_hz = spec.center_freq_hz;
                    self.sample_rate_hz = spec.sample_rate_hz;
                    if self.is_file_source() {
                        self.freq_input =
                            format!("{:.6}", (spec.center_freq_hz + self.vfo_offset_hz) / 1e6);
                    } else {
                        self.vfo_offset_hz = 0.0;
                    }
                    self.status_msg = if self.is_file_source() {
                        format!(
                            "File | {:.3} MHz | {:.3} MSPS",
                            spec.center_freq_hz / 1e6,
                            spec.sample_rate_hz / 1e6
                        )
                    } else {
                        format!(
                            "Connected | {:.3} MHz | {:.3} MSPS",
                            spec.center_freq_hz / 1e6,
                            spec.sample_rate_hz / 1e6
                        )
                    };
                    self.push_log(self.status_msg.clone());
                }
                WorkerEvent::Doppler {
                    offset_hz,
                    rate_hz_s,
                } => {
                    self.doppler_hz = offset_hz;
                    self.doppler_rate_hz_s = rate_hz_s;
                }
                WorkerEvent::Info(message) => {
                    self.push_log(message);
                }
                WorkerEvent::Error(message) => {
                    self.push_log(format!("error: {message}"));
                    self.error_msg = Some(message);
                }
                WorkerEvent::Fatal(message) => {
                    self.is_connecting = false;
                    self.is_connected = false;
                    self.is_recording = false;
                    self.push_log(format!("fatal: {message}"));
                    self.error_msg = Some(message);
                    self.status_msg = "Device error".to_string();
                    // The worker is unwinding. Let `poll_worker_state` join it
                    // instead of blocking the UI on plugin/source teardown.
                    self.worker_stopping = true;
                }
                WorkerEvent::FileEnded => {
                    self.is_connected = false;
                    self.is_connecting = false;
                    self.is_recording = false;
                    self.vfo_offset_hz = 0.0;
                    if self.is_file_source() {
                        self.freq_input = format!("{:.6}", self.center_freq_hz / 1e6);
                    }
                    self.status_msg = "file playback finished".to_string();
                    self.push_log(self.status_msg.clone());
                }
                WorkerEvent::RecordingStarted(path) => {
                    self.is_recording = true;
                    self.recording_path = Some(path.clone());
                    self.status_msg = format!("Recording {}", path.display());
                    self.push_log(self.status_msg.clone());
                }
                WorkerEvent::RecordingError(message) => {
                    self.is_recording = false;
                    self.push_log(format!("recording: {message}"));
                    self.error_msg = Some(message);
                    self.status_msg = "Recording stopped".to_string();
                }
                WorkerEvent::RecordingStopped {
                    path,
                    samples,
                    seconds,
                } => {
                    self.is_recording = false;
                    self.recording_path = Some(path.clone());
                    self.status_msg = format!(
                        "Saved {} ({:.1} s, {} samples)",
                        path.display(),
                        seconds,
                        samples
                    );
                    self.push_log(self.status_msg.clone());
                }
                WorkerEvent::Plugin { source, event } => {
                    self.handle_plugin_event(source, event, ctx)
                }
                WorkerEvent::VfoOffset { offset_hz } => {
                    self.vfo_offset_hz = offset_hz;
                    if self.is_file_source() && ctx.memory(|memory| memory.focused().is_none()) {
                        self.freq_input = format!("{:.6}", (self.center_freq_hz + offset_hz) / 1e6);
                    }
                }
                WorkerEvent::AutoTuneDone { offset_hz } => match offset_hz {
                    Some(offset) => {
                        self.vfo_offset_hz = offset;
                        self.auto_tune_failed = false;
                        self.auto_tune_status =
                            format!("carrier found: {:+.1} Hz from centre", offset);
                        if self.is_file_source() && ctx.memory(|memory| memory.focused().is_none())
                        {
                            self.freq_input =
                                format!("{:.6}", (self.center_freq_hz + offset) / 1e6);
                        }
                        self.push_log(self.auto_tune_status.clone());
                    }
                    None => {
                        self.auto_tune_failed = true;
                        self.auto_tune_status = format!(
                            "no CW signal found within ±{:.0} kHz; check the band/antenna or retune the LO",
                            self.auto_tune_span_khz()
                        );
                        self.push_log(self.auto_tune_status.clone());
                    }
                },
            }
        }
    }

    pub(crate) fn handle_plugin_event(
        &mut self,
        source: PluginSource,
        event: PluginEvent,
        ctx: &egui::Context,
    ) {
        match source {
            PluginSource::Cw => self.handle_cw_plugin_event(event),
            PluginSource::Sstv => self.handle_sstv_plugin_event(event, ctx),
            PluginSource::Ssdv => self.handle_ssdv_plugin_event(event, ctx),
        }
    }

    pub(crate) fn handle_cw_plugin_event(&mut self, event: PluginEvent) {
        match event {
            PluginEvent::Display { update } => match update {
                DisplayUpdate::Text { lines, .. } => {
                    self.cw_text = lines.join("\n");
                }
                DisplayUpdate::Table { columns, rows, .. } => {
                    let mut text = columns.join(" | ");
                    for row in rows {
                        text.push('\n');
                        text.push_str(&row.join(" | "));
                    }
                    self.cw_text = text;
                }
                DisplayUpdate::Progress { title, fraction } => {
                    self.cw_status = format!("{title}: {:.0}%", fraction * 100.0);
                }
                DisplayUpdate::Image { .. } => {}
            },
            PluginEvent::Status { message, .. } => {
                self.cw_status = message;
            }
            PluginEvent::Error { message, fatal } => {
                self.cw_status = if fatal {
                    format!("fatal: {message}")
                } else {
                    message.clone()
                };
                self.push_log(format!("plugin: {message}"));
                self.error_msg = Some(message);
            }
            PluginEvent::Packet { .. } => {}
        }
    }

    pub(crate) fn handle_sstv_plugin_event(&mut self, event: PluginEvent, ctx: &egui::Context) {
        match event {
            PluginEvent::Display { update } => match update {
                DisplayUpdate::Image {
                    title,
                    format,
                    width,
                    height,
                    bytes,
                } => match decode_image_update(format, width, height, &bytes) {
                    Ok(image) => {
                        self.sstv_texture = Some(ctx.load_texture(
                            format!("sstv-{title}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                        self.sstv_image_png = (format == ImageFormat::Png).then_some(bytes);
                        self.sstv_image_title = title.clone();
                        self.sstv_status = format!("{title}: image ready");
                        self.sstv_progress = 1.0;
                        self.decoder_tab = DecoderTab::Sstv;
                        self.decoder_dock_open = true;
                        self.push_log(format!("SSTV image decoded: {title}"));
                    }
                    Err(error) => {
                        self.sstv_status = format!("image decode failed: {error}");
                        self.push_log(self.sstv_status.clone());
                    }
                },
                DisplayUpdate::Progress { title, fraction } => {
                    self.sstv_progress = fraction.clamp(0.0, 1.0);
                    self.sstv_status = format!("{title} ({:.0}%)", fraction * 100.0);
                }
                DisplayUpdate::Text { lines, .. } => {
                    self.sstv_status = lines.join(" ");
                }
                DisplayUpdate::Table { columns, rows, .. } => {
                    self.sstv_status = format!("{} ({} rows)", columns.join(" | "), rows.len());
                }
            },
            PluginEvent::Status { message, .. } => {
                self.sstv_status = message.clone();
                if message.contains("VIS") {
                    self.sstv_progress = 0.0;
                    self.push_log(format!("SSTV: {message}"));
                } else if message.contains("audio rate") {
                    self.push_log(format!("SSTV: {message}"));
                }
            }
            PluginEvent::Error { message, fatal } => {
                self.sstv_status = if fatal {
                    format!("fatal: {message}")
                } else {
                    message.clone()
                };
                self.push_log(format!("SSTV plugin: {message}"));
                self.error_msg = Some(message);
            }
            PluginEvent::Packet { .. } => {}
        }
    }

    pub(crate) fn handle_ssdv_plugin_event(&mut self, event: PluginEvent, ctx: &egui::Context) {
        match event {
            PluginEvent::Display { update } => match update {
                DisplayUpdate::Image {
                    title,
                    format,
                    width,
                    height,
                    bytes,
                } => match decode_image_update(format, width, height, &bytes) {
                    Ok(image) => {
                        self.ssdv_texture = Some(ctx.load_texture(
                            format!("ssdv-{title}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                        self.ssdv_image_png = (format == ImageFormat::Png).then_some(bytes);
                        self.ssdv_image_title = title.clone();
                        self.ssdv_status = format!("{title}: image ready");
                        if title.contains("(complete)") {
                            self.ssdv_progress = 1.0;
                            self.push_log(format!("SSDV image decoded: {title}"));
                        }
                        self.decoder_tab = DecoderTab::Ssdv;
                        self.decoder_dock_open = true;
                    }
                    Err(error) => {
                        self.ssdv_status = format!("image decode failed: {error}");
                        self.push_log(self.ssdv_status.clone());
                    }
                },
                DisplayUpdate::Progress { title, fraction } => {
                    self.ssdv_progress = fraction.clamp(0.0, 1.0);
                    self.ssdv_status = format!("{title} ({:.0}%)", fraction * 100.0);
                }
                DisplayUpdate::Text { lines, .. } => {
                    self.ssdv_status = lines.join(" ");
                }
                DisplayUpdate::Table { columns, rows, .. } => {
                    self.ssdv_status = format!("{} ({} rows)", columns.join(" | "), rows.len());
                }
            },
            PluginEvent::Status { message, .. } => {
                self.ssdv_status = message.clone();
                self.push_log(format!("SSDV: {message}"));
            }
            PluginEvent::Error { message, fatal } => {
                self.ssdv_status = if fatal {
                    format!("fatal: {message}")
                } else {
                    message.clone()
                };
                self.push_log(format!("SSDV plugin: {message}"));
                self.error_msg = Some(message);
            }
            PluginEvent::Packet { .. } => {}
        }
    }

    pub(crate) fn poll_data(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.data_rx else { return };

        let mut last_frame: Option<SpectrumFrame> = None;
        while let Ok(frame) = rx.try_recv() {
            last_frame = Some(frame);
        }

        let Some(frame) = last_frame else { return };

        self.current_spectrum = frame
            .frequencies_mhz
            .iter()
            .zip(frame.spectrum_db.iter())
            .map(|(&freq, &db)| [freq as f64, db as f64])
            .collect();
        self.center_freq_hz = frame.center_freq_hz;
        self.sample_rate_hz = frame.sample_rate_hz;
        self.noise_dbfs = median(&frame.spectrum_db);
        self.signal_dbfs = frame
            .spectrum_db
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold(f32::NEG_INFINITY, f32::max);

        let row = downsample(&frame.spectrum_db, self.config.ui.waterfall_width);
        if self.waterfall_rows.len() >= self.config.ui.waterfall_rows {
            self.waterfall_rows.pop_front();
        }
        self.waterfall_rows.push_back(row);

        self.rebuild_waterfall_texture(ctx);
        ctx.request_repaint();
    }
}
