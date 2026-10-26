//! Tuning, recording, configuration, and command-sender helpers.

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use eframe::egui;

use midori_config::AppConfig;

use crate::app::MidoriApp;
use crate::app::format::{apply_theme, split_list};
use crate::worker::commands::{DemodUpdate, WorkerCommand};

impl MidoriApp {
    pub(crate) fn set_frequency_mhz(&mut self, mhz: f64) {
        if self.is_recording {
            self.error_msg = Some("stop recording before retuning".to_string());
            return;
        }
        let mhz = mhz.clamp(0.0, 6_000.0);
        self.freq_input = format!("{mhz:.6}");
        self.send_frequency();
    }

    pub(crate) fn send_frequency(&mut self) {
        if self.is_recording {
            // Both the Enter key in the frequency field and the tuning
            // buttons end up here, so the recording guard cannot be bypassed.
            self.freq_input = format!("{:.6}", self.tuned_freq_hz() / 1e6);
            self.error_msg = Some("stop recording before retuning".to_string());
            return;
        }
        let Ok(freq_mhz) = self.freq_input.parse::<f64>() else {
            self.error_msg = Some("invalid centre frequency".to_string());
            return;
        };
        let hz = freq_mhz * 1e6;

        if self.is_file_source() && (self.is_connected || self.is_connecting) {
            let max_offset = (self.sample_rate_hz / 2.0 - self.demod_bandwidth_hz() / 2.0).max(0.0);
            let offset = (hz - self.center_freq_hz).clamp(-max_offset, max_offset);
            let previous_offset = self.vfo_offset_hz;
            self.vfo_offset_hz = offset;
            self.freq_input = format!("{:.6}", (self.center_freq_hz + offset) / 1e6);
            if !self.send_command(WorkerCommand::SetTuning {
                frequency_hz: None,
                vfo_offset_hz: offset,
            }) {
                self.vfo_offset_hz = previous_offset;
                self.freq_input = format!("{:.6}", self.tuned_freq_hz() / 1e6);
            }
            return;
        }

        let previous_center = self.center_freq_hz;
        let previous_offset = self.vfo_offset_hz;
        self.center_freq_hz = hz;
        // A hardware retune starts with a fresh digital VFO. This also clears
        // any AFC correction from the previous frequency.
        self.vfo_offset_hz = 0.0;
        if self.is_connected || self.is_connecting {
            // Offset reset and hardware retune travel as one atomic command so
            // the worker cannot apply only half of the change.
            if !self.send_command(WorkerCommand::SetTuning {
                frequency_hz: Some(hz),
                vfo_offset_hz: 0.0,
            }) {
                self.center_freq_hz = previous_center;
                self.vfo_offset_hz = previous_offset;
                self.freq_input = format!("{:.6}", self.tuned_freq_hz() / 1e6);
            }
        }
    }

    pub(crate) fn send_gain(&mut self) {
        let Ok(gain_db) = self.gain_input.parse::<f64>() else {
            self.error_msg = Some("invalid gain".to_string());
            return;
        };
        self.send_command(WorkerCommand::SetGain(gain_db));
    }

    pub(crate) fn send_demod_update(&mut self) {
        self.config.demod.mode = self.demod_mode;
        let Some(params) = self.config.demod.params(self.demod_mode) else {
            self.error_msg = Some(format!(
                "{} demod is not implemented yet",
                self.demod_mode.label()
            ));
            return;
        };
        let update = DemodUpdate {
            mode: self.demod_mode,
            bandwidth_hz: params.bandwidth_hz,
            audio_bandwidth_hz: params.audio_bandwidth_hz,
            deemphasis_us: params.deemphasis_us,
            bfo_hz: params.bfo_hz,
            channel_rate_hz: params.channel_rate_hz,
        };
        self.send_command(WorkerCommand::SetDemod(update));
    }

    pub(crate) fn mark_demod_dirty(&mut self) {
        self.demod_dirty = true;
        self.last_demod_change = Instant::now();
    }

    pub(crate) fn send_cw_afc(&mut self) {
        self.send_command(WorkerCommand::SetCwAfc(self.cw_afc));
    }

    pub(crate) fn start_auto_tune(&mut self) {
        if !self.is_connected || self.is_recording {
            self.auto_tune_status = "connect and stop recording first".to_string();
            return;
        }
        self.auto_tune_status = format!(
            "scanning 4 s (±{:.0} kHz) for a CW carrier...",
            self.auto_tune_span_khz()
        );
        self.auto_tune_failed = false;
        if !self.send_command(WorkerCommand::AutoTune) {
            self.auto_tune_status = "could not start the scan".to_string();
        }
    }

    pub(crate) fn adjust_freq(&mut self, delta_mhz: f64) {
        if self.is_recording {
            self.error_msg = Some("stop recording before retuning".to_string());
            return;
        }
        let current = self.tuned_freq_hz() / 1e6;
        self.set_frequency_mhz((current + delta_mhz).max(0.0));
    }

    pub(crate) fn adjust_gain(&mut self, delta_db: f64) {
        let current = self.gain_input.parse::<f64>().unwrap_or(40.0);
        self.gain_input = format!("{:.1}", (current + delta_db).clamp(-3.0, 71.0));
        self.send_gain();
    }

    pub(crate) fn start_recording(&mut self) {
        if !self.is_connected || self.is_recording {
            return;
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("captures")
            .join(format!("gui_{stamp}.sigmf"));
        self.is_recording = true;
        self.recording_path = Some(path.clone());
        self.status_msg = "Starting recording...".to_string();
        if !self.send_command(WorkerCommand::StartRecording(path)) {
            self.is_recording = false;
            self.recording_path = None;
        }
    }

    pub(crate) fn stop_recording(&mut self) {
        self.send_command(WorkerCommand::StopRecording);
    }

    pub(crate) fn save_config(&mut self) {
        if self.is_file_source() {
            self.config.source.center_freq_hz = self.center_freq_hz;
        } else if let Ok(mhz) = self.freq_input.parse::<f64>() {
            self.config.source.center_freq_hz = mhz * 1e6;
        }
        if let Ok(gain) = self.gain_input.parse::<f64>() {
            self.config.source.gain_db = gain;
        }
        if let Ok(elevation) = self.min_elev_input.parse::<f64>() {
            self.config.satellite.min_elevation_deg = elevation;
        }

        self.config.source.kind = self.source_kind;
        self.config.source.file_path = self.file_path.clone();
        self.config.source.file_loop = self.file_loop;
        self.config.demod.mode = self.demod_mode;
        self.config.decoder.cw_enabled = self.cw_enabled;
        self.config.decoder.cw_afc = self.cw_afc;
        self.config.decoder.sstv_enabled = self.sstv_enabled;
        self.config.decoder.ssdv_enabled = self.ssdv_enabled;
        self.config.decoder.ssdv_crc_inits = split_list(&self.ssdv_crc_inits_input);
        self.config.decoder.ssdv_frame_header_len = self.ssdv_frame_header_len as usize;
        self.config.ui.spectrum_ratio = self.spectrum_ratio;
        self.config.ui.decoder_tab = self.decoder_tab;
        self.config.ui.decoder_dock_open = self.decoder_dock_open;
        self.config.ui.db_min = self.db_min;
        self.config.ui.db_max = self.db_max;
        self.config.ui.colormap = self.colormap;
        self.config.ui.waterfall_follow_spectrum = self.waterfall_follow;
        self.config.satellite.tle_path = self.tle_path.clone();
        self.config.satellite.grid = self.grid_input.clone();
        self.config.satellite.doppler_tracking = self.doppler_tracking;
        self.config.sanitize();

        match self.config.save(&self.config_path) {
            Ok(()) => {
                self.status_msg = format!("settings saved to {}", self.config_path.display());
                self.push_log(self.status_msg.clone());
            }
            Err(error) => {
                let message = format!("saving config: {error}");
                self.error_msg = Some(message.clone());
                self.push_log(message);
            }
        }
    }

    pub(crate) fn reload_config(&mut self, ctx: &egui::Context) {
        let loaded = AppConfig::load(&self.config_path);
        self.config = loaded.config;
        self.config_warning = loaded.warning.clone();
        if let Some(warning) = loaded.warning {
            self.push_log(format!("config: {warning}"));
            self.error_msg = Some(warning);
        }

        let config = &self.config;
        self.source_kind = config.source.kind;
        self.file_path = config.source.file_path.clone();
        self.file_loop = config.source.file_loop;
        self.freq_input = format!("{:.6}", config.source.center_freq_hz / 1e6);
        self.gain_input = format!("{:.1}", config.source.gain_db);
        self.center_freq_hz = config.source.center_freq_hz;
        self.sample_rate_hz = config.source.sample_rate_hz;
        self.vfo_offset_hz = 0.0;
        self.demod_mode = config.demod.mode;
        self.cw_enabled = config.decoder.cw_enabled;
        self.cw_afc = config.decoder.cw_afc;
        self.sstv_enabled = config.decoder.sstv_enabled;
        self.ssdv_enabled = config.decoder.ssdv_enabled;
        self.ssdv_crc_inits_input = if config.decoder.ssdv_crc_inits.is_empty() {
            "0xFFFFFFFF".to_string()
        } else {
            config.decoder.ssdv_crc_inits.join(", ")
        };
        self.ssdv_frame_header_len = config.decoder.ssdv_frame_header_len as u32;
        self.spectrum_ratio = config.ui.spectrum_ratio;
        self.waterfall_follow = config.ui.waterfall_follow_spectrum;
        self.decoder_tab = config.ui.decoder_tab;
        self.decoder_dock_open = config.ui.decoder_dock_open;
        self.db_min = config.ui.db_min;
        self.db_max = config.ui.db_max;
        self.colormap = config.ui.colormap;
        self.spectrum_view_x = None;
        self.demod_dirty = false;
        self.tle_path = config.satellite.tle_path.clone();
        self.grid_input = config.satellite.grid.clone();
        self.min_elev_input = format!("{:.1}", config.satellite.min_elevation_deg);
        self.doppler_tracking = config.satellite.doppler_tracking;

        apply_theme(ctx, config.ui.theme);
        self.status_msg = if self.is_connected || self.is_connecting {
            "config reloaded; reconnect to apply source/demod/decoder".to_string()
        } else {
            "config reloaded".to_string()
        };
        self.push_log(self.status_msg.clone());
    }
    pub(crate) fn push_log(&mut self, message: impl Into<String>) {
        if self.log.len() >= 200 {
            self.log.pop_front();
        }
        self.log.push_back(format!(
            "{}  {}",
            Utc::now().format("%H:%M:%S"),
            message.into()
        ));
    }
    pub(crate) fn demod_bandwidth_hz(&self) -> f64 {
        self.config
            .demod
            .params(self.demod_mode)
            .map_or(0.0, |params| params.bandwidth_hz)
    }

    pub(crate) fn demod_label(&self) -> &'static str {
        self.demod_mode.label()
    }

    pub(crate) fn auto_tune_color(&self) -> egui::Color32 {
        if self.auto_tune_failed {
            egui::Color32::from_rgb(235, 175, 80)
        } else {
            egui::Color32::from_rgb(120, 220, 180)
        }
    }

    pub(crate) fn auto_tune_span_khz(&self) -> f64 {
        (self.sample_rate_hz * 0.45).min(100_000.0) / 1e3
    }
}
