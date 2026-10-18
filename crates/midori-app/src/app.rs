//! egui front end for the Midori core.
//!
//! The UI thread only renders and handles input. A worker thread owns the
//! hardware source, computes spectra, and optionally records SigMF. This keeps
//! a slow or disconnected device from freezing the window.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Local, Utc};
use crossbeam_channel::{Receiver, Sender, bounded};
use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use midori_config::{AppConfig, Colormap, DecoderTab, Theme};
use midori_core::{DemodMode, Session, Sink, SourceConfig, StreamSpec, VfoConfig};
use midori_dsp::median;
use midori_engine::{LiveSession, PluginHandle, PluginSink, RecorderSink, VfoChannel, plugin_sink};
use midori_orbit::{GridSquare, Pass, PassPredictor, Sgp4Doppler, Tle};
use midori_plugin::{
    DisplayUpdate, InputSpec, Manifest, OutputKind, PROTOCOL_VERSION, PluginEvent, PluginHost,
    SidecarPlugin,
};
use midori_source_libiio::LibiioSource;

use crate::audio::{AudioOutput, AudioSink};
use crate::sinks::SpectrumSink;

// ---------------------------------------------------------------------------
// Worker messages
// ---------------------------------------------------------------------------

pub(crate) struct SpectrumFrame {
    pub(crate) spectrum_db: Vec<f32>,
    pub(crate) frequencies_mhz: Vec<f32>,
    pub(crate) center_freq_hz: f64,
    pub(crate) sample_rate_hz: f64,
}

enum WorkerCommand {
    SetFrequency(f64),
    SetGain(f64),
    StartRecording(PathBuf),
    StopRecording,
    Stop,
}

struct WorkerOptions {
    cw_enabled: bool,
    cw_tone_hz: f64,
    source_uri: String,
    fft_size: usize,
    doppler: Option<DopplerOptions>,
}

struct DopplerOptions {
    tle_path: PathBuf,
    grid: String,
    downlink_hz: f64,
}

enum WorkerEvent {
    Connected(StreamSpec),
    Info(String),
    Error(String),
    Fatal(String),
    Doppler {
        offset_hz: f64,
        rate_hz_s: f64,
    },
    RecordingStarted(PathBuf),
    RecordingError(String),
    RecordingStopped {
        path: PathBuf,
        samples: u64,
        seconds: f64,
    },
    Plugin(PluginEvent),
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

pub struct MidoriApp {
    config: AppConfig,
    config_path: PathBuf,
    config_warning: Option<String>,

    data_rx: Option<Receiver<SpectrumFrame>>,
    event_rx: Option<Receiver<WorkerEvent>>,
    cmd_tx: Option<Sender<WorkerCommand>>,
    worker: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,

    current_spectrum: Vec<[f64; 2]>,
    center_freq_hz: f64,
    sample_rate_hz: f64,
    signal_dbfs: f32,
    noise_dbfs: f32,
    spectrum_ratio: f32,
    db_min: f32,
    db_max: f32,
    colormap: Colormap,

    waterfall_rows: VecDeque<Vec<f32>>,
    waterfall_texture: Option<egui::TextureHandle>,

    freq_input: String,
    gain_input: String,
    demod_mode: DemodMode,

    decoder_tab: DecoderTab,
    decoder_dock_open: bool,
    cw_enabled: bool,
    cw_text: String,
    cw_status: String,

    tle_path: String,
    grid_input: String,
    min_elev_input: String,
    satellite_name: String,
    passes: Vec<Pass>,
    pass_status: String,
    doppler_tracking: bool,
    doppler_hz: f64,
    doppler_rate_hz_s: f64,
    log: VecDeque<String>,

    is_connected: bool,
    is_connecting: bool,
    is_recording: bool,
    auto_connect: bool,
    recording_path: Option<PathBuf>,
    status_msg: String,
    error_msg: Option<String>,
}

impl Default for MidoriApp {
    fn default() -> Self {
        let config_path = PathBuf::from(midori_config::DEFAULT_FILE);
        let loaded = AppConfig::load(&config_path);
        let warning = loaded.warning.clone();
        let config = loaded.config;

        let center_freq_hz = config.source.center_freq_hz;
        let sample_rate_hz = config.source.sample_rate_hz;
        let gain_db = config.source.gain_db;
        let demod_mode = config.demod.mode;
        let spectrum_ratio = config.ui.spectrum_ratio;
        let decoder_tab = config.ui.decoder_tab;
        let decoder_dock_open = config.ui.decoder_dock_open;
        let db_min = config.ui.db_min;
        let db_max = config.ui.db_max;
        let colormap = config.ui.colormap;
        let waterfall_capacity = config.ui.waterfall_rows + 4;
        let tle_path = config.satellite.tle_path.clone();
        let grid_input = config.satellite.grid.clone();
        let min_elev_input = format!("{:.1}", config.satellite.min_elevation_deg);
        let cw_enabled = config.decoder.cw_enabled;
        let doppler_tracking =
            std::env::var_os("MIDORI_DOPPLER").is_some() || config.satellite.doppler_tracking;

        Self {
            config,
            config_path,
            config_warning: warning,
            data_rx: None,
            event_rx: None,
            cmd_tx: None,
            worker: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            current_spectrum: Vec::new(),
            center_freq_hz,
            sample_rate_hz,
            signal_dbfs: f32::NEG_INFINITY,
            noise_dbfs: f32::NEG_INFINITY,
            spectrum_ratio,
            db_min,
            db_max,
            colormap,
            waterfall_rows: VecDeque::with_capacity(waterfall_capacity),
            waterfall_texture: None,
            freq_input: format!("{:.6}", center_freq_hz / 1e6),
            gain_input: format!("{gain_db:.1}"),
            demod_mode,
            decoder_tab,
            decoder_dock_open,
            cw_enabled: std::env::var_os("MIDORI_CW").is_some() || cw_enabled,
            cw_text: String::new(),
            cw_status: "idle".to_string(),
            tle_path,
            grid_input,
            min_elev_input,
            satellite_name: "No TLE loaded".to_string(),
            passes: Vec::new(),
            pass_status: "Load a TLE and predict passes".to_string(),
            doppler_tracking,
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
            log: VecDeque::with_capacity(200),
            is_connected: false,
            is_connecting: false,
            is_recording: false,
            auto_connect: std::env::var_os("MIDORI_AUTOCONNECT").is_some(),
            recording_path: None,
            status_msg: "Not connected".to_string(),
            error_msg: None,
        }
    }
}

impl MidoriApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        apply_theme(&cc.egui_ctx, app.config.ui.theme);

        if let Some(warning) = app.config_warning.clone() {
            app.push_log(format!("config: {warning}"));
            app.status_msg = "config warning; using defaults".to_string();
        }

        app
    }

    // -----------------------------------------------------------------------
    // Connection lifecycle
    // -----------------------------------------------------------------------

    fn connect(&mut self) {
        if self.is_connected || self.is_connecting {
            return;
        }
        self.disconnect();

        let Ok(freq_mhz) = self.freq_input.parse::<f64>() else {
            self.error_msg = Some("invalid centre frequency".to_string());
            return;
        };
        let Ok(gain_db) = self.gain_input.parse::<f64>() else {
            self.error_msg = Some("invalid gain".to_string());
            return;
        };

        let source = &self.config.source;
        let config = SourceConfig {
            center_freq_hz: freq_mhz * 1e6,
            sample_rate_hz: source.sample_rate_hz,
            bandwidth_hz: source.bandwidth_hz,
            gain_db,
            gain_mode: source.gain_mode,
            rf_port: Some(source.rf_port.clone()),
            buffer_samples: source.buffer_samples,
        };

        let demod = self.config.demod.params(self.demod_mode);
        let vfo = VfoConfig {
            id: 0,
            offset_hz: 0.0,
            bandwidth_hz: demod.bandwidth_hz,
            demod: self.demod_mode,
            audio_rate_hz: demod.audio_rate_hz,
            channel_rate_hz: demod.channel_rate_hz,
            audio_bandwidth_hz: demod.audio_bandwidth_hz,
            deemphasis_us: demod.deemphasis_us,
            bfo_hz: demod.bfo_hz,
        };
        let cw_tone_hz = self.config.demod.cw.bfo_hz;
        let source_uri = source.uri.clone();
        let fft_size = self.config.ui.fft_size;
        let cw_enabled = self.cw_enabled;
        let doppler = self.doppler_tracking.then(|| DopplerOptions {
            tle_path: PathBuf::from(&self.tle_path),
            grid: self.grid_input.clone(),
            downlink_hz: freq_mhz * 1e6,
        });

        let (data_tx, data_rx) = bounded(4);
        let (event_tx, event_rx) = bounded(64);
        let (cmd_tx, cmd_rx) = bounded(16);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop_flag);
        let options = WorkerOptions {
            cw_enabled,
            cw_tone_hz,
            source_uri,
            fft_size,
            doppler,
        };

        match thread::Builder::new()
            .name("sdr-worker".into())
            .spawn(move || run_worker(config, vfo, options, data_tx, event_tx, cmd_rx, worker_stop))
        {
            Ok(handle) => {
                self.worker = Some(handle);
                self.data_rx = Some(data_rx);
                self.event_rx = Some(event_rx);
                self.cmd_tx = Some(cmd_tx);
                self.stop_flag = stop_flag;
                self.is_connecting = true;
                self.is_connected = false;
                self.error_msg = None;
                self.status_msg = "Connecting to Pluto SDR...".to_string();
            }
            Err(error) => {
                self.error_msg = Some(format!("could not spawn worker: {error}"));
            }
        }
    }

    fn disconnect(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(WorkerCommand::Stop);
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        self.data_rx = None;
        self.event_rx = None;
        self.cmd_tx = None;
        self.is_connected = false;
        self.is_connecting = false;
        self.is_recording = false;
        self.recording_path = None;
        self.doppler_hz = 0.0;
        self.doppler_rate_hz_s = 0.0;
        self.current_spectrum.clear();
        self.waterfall_rows.clear();
        self.waterfall_texture = None;
        self.error_msg = None;
        self.status_msg = "Disconnected".to_string();
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    fn set_frequency_mhz(&mut self, mhz: f64) {
        if self.is_recording {
            self.error_msg = Some("stop recording before retuning".to_string());
            return;
        }
        let mhz = mhz.clamp(0.0, 6_000.0);
        self.freq_input = format!("{mhz:.6}");
        self.send_frequency();
    }

    fn send_frequency(&mut self) {
        let Ok(freq_mhz) = self.freq_input.parse::<f64>() else {
            self.error_msg = Some("invalid centre frequency".to_string());
            return;
        };
        let hz = freq_mhz * 1e6;
        self.center_freq_hz = hz;
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(WorkerCommand::SetFrequency(hz));
        }
    }

    fn send_gain(&mut self) {
        let Ok(gain_db) = self.gain_input.parse::<f64>() else {
            self.error_msg = Some("invalid gain".to_string());
            return;
        };
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(WorkerCommand::SetGain(gain_db));
        }
    }

    fn adjust_freq(&mut self, delta_mhz: f64) {
        if self.is_recording {
            self.error_msg = Some("stop recording before retuning".to_string());
            return;
        }
        let current = self.freq_input.parse::<f64>().unwrap_or(120.0);
        self.freq_input = format!("{:.3}", (current + delta_mhz).max(0.0));
        self.send_frequency();
    }

    fn adjust_gain(&mut self, delta_db: f64) {
        let current = self.gain_input.parse::<f64>().unwrap_or(40.0);
        self.gain_input = format!("{:.1}", (current + delta_db).clamp(-3.0, 71.0));
        self.send_gain();
    }

    fn start_recording(&mut self) {
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
        if let Some(tx) = &self.cmd_tx
            && tx.try_send(WorkerCommand::StartRecording(path)).is_err()
        {
            self.is_recording = false;
            self.recording_path = None;
            self.error_msg = Some("could not send recording command".to_string());
        }
    }

    fn stop_recording(&mut self) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(WorkerCommand::StopRecording);
        }
    }

    fn save_config(&mut self) {
        if let Ok(mhz) = self.freq_input.parse::<f64>() {
            self.config.source.center_freq_hz = mhz * 1e6;
        }
        if let Ok(gain) = self.gain_input.parse::<f64>() {
            self.config.source.gain_db = gain;
        }
        if let Ok(elevation) = self.min_elev_input.parse::<f64>() {
            self.config.satellite.min_elevation_deg = elevation;
        }

        self.config.demod.mode = self.demod_mode;
        self.config.decoder.cw_enabled = self.cw_enabled;
        self.config.ui.spectrum_ratio = self.spectrum_ratio;
        self.config.ui.decoder_tab = self.decoder_tab;
        self.config.ui.decoder_dock_open = self.decoder_dock_open;
        self.config.ui.db_min = self.db_min;
        self.config.ui.db_max = self.db_max;
        self.config.ui.colormap = self.colormap;
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

    fn reload_config(&mut self, ctx: &egui::Context) {
        let loaded = AppConfig::load(&self.config_path);
        self.config = loaded.config;
        self.config_warning = loaded.warning.clone();
        if let Some(warning) = loaded.warning {
            self.push_log(format!("config: {warning}"));
            self.error_msg = Some(warning);
        }

        let config = &self.config;
        self.freq_input = format!("{:.6}", config.source.center_freq_hz / 1e6);
        self.gain_input = format!("{:.1}", config.source.gain_db);
        self.center_freq_hz = config.source.center_freq_hz;
        self.sample_rate_hz = config.source.sample_rate_hz;
        self.demod_mode = config.demod.mode;
        self.cw_enabled = config.decoder.cw_enabled;
        self.spectrum_ratio = config.ui.spectrum_ratio;
        self.decoder_tab = config.ui.decoder_tab;
        self.decoder_dock_open = config.ui.decoder_dock_open;
        self.db_min = config.ui.db_min;
        self.db_max = config.ui.db_max;
        self.colormap = config.ui.colormap;
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

    // -----------------------------------------------------------------------
    // Satellite / pass planning
    // -----------------------------------------------------------------------

    fn push_log(&mut self, message: impl Into<String>) {
        if self.log.len() >= 200 {
            self.log.pop_front();
        }
        self.log.push_back(format!(
            "{}  {}",
            Utc::now().format("%H:%M:%S"),
            message.into()
        ));
    }

    fn predict_passes(&mut self) {
        let tle = match Tle::from_file(Path::new(&self.tle_path)) {
            Ok(tle) => tle,
            Err(error) => {
                self.pass_status = format!("TLE error: {error}");
                self.push_log(self.pass_status.clone());
                return;
            }
        };
        let grid = match GridSquare::parse(&self.grid_input) {
            Ok(grid) => grid,
            Err(error) => {
                self.pass_status = format!("grid error: {error}");
                self.push_log(self.pass_status.clone());
                return;
            }
        };
        let min_elevation = match self.min_elev_input.parse::<f64>() {
            Ok(value) if (0.0..=90.0).contains(&value) => value,
            _ => {
                self.pass_status = "minimum elevation must be 0..90".to_string();
                return;
            }
        };

        let observer = grid.observer(0.0);
        let predictor = match PassPredictor::new(&tle, observer) {
            Ok(predictor) => predictor,
            Err(error) => {
                self.pass_status = format!("SGP4 error: {error}");
                self.push_log(self.pass_status.clone());
                return;
            }
        };

        let now = Utc::now();
        match predictor.predict(now, now + chrono::Duration::hours(24), min_elevation) {
            Ok(passes) => {
                self.satellite_name = tle
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("NORAD {}", tle.norad_id));
                self.pass_status = format!(
                    "{} passes in the next 24 h | grid {} | min el {:.1}°",
                    passes.len(),
                    self.grid_input.to_uppercase(),
                    min_elevation
                );
                self.passes = passes;
                self.decoder_tab = DecoderTab::Passes;
                self.decoder_dock_open = true;
                self.push_log(format!("predicted {} passes", self.passes.len()));
            }
            Err(error) => {
                self.pass_status = format!("prediction error: {error}");
                self.push_log(self.pass_status.clone());
            }
        }
    }

    fn next_pass(&self) -> Option<&Pass> {
        let now = Utc::now();
        self.passes.iter().find(|pass| pass.los > now)
    }

    // -----------------------------------------------------------------------
    // Data polling
    // -----------------------------------------------------------------------

    fn poll_events(&mut self) {
        let events: Vec<WorkerEvent> = {
            let Some(rx) = &self.event_rx else { return };
            let mut events = Vec::new();
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
            events
        };

        for event in events {
            match event {
                WorkerEvent::Connected(spec) => {
                    self.is_connecting = false;
                    self.is_connected = true;
                    self.center_freq_hz = spec.center_freq_hz;
                    self.sample_rate_hz = spec.sample_rate_hz;
                    self.status_msg = format!(
                        "Connected | {:.3} MHz | {:.3} MSPS",
                        spec.center_freq_hz / 1e6,
                        spec.sample_rate_hz / 1e6
                    );
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
                    if let Some(handle) = self.worker.take() {
                        let _ = handle.join();
                    }
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
                WorkerEvent::Plugin(event) => self.handle_plugin_event(event),
            }
        }
    }

    fn handle_plugin_event(&mut self, event: PluginEvent) {
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

    fn poll_data(&mut self, ctx: &egui::Context) {
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

    fn rebuild_waterfall_texture(&mut self, ctx: &egui::Context) {
        let width = self.config.ui.waterfall_width;
        let height = self.config.ui.waterfall_rows;
        let mut pixels = vec![egui::Color32::BLACK; width * height];

        for (row_index, row) in self.waterfall_rows.iter().rev().enumerate() {
            if row_index >= height {
                break;
            }
            for (column, &db) in row.iter().enumerate().take(width) {
                pixels[row_index * width + column] =
                    db_to_color(db, self.db_min, self.db_max, self.colormap);
            }
        }

        let image = egui::ColorImage {
            size: [width, height],
            pixels,
            source_size: egui::Vec2::new(width as f32, height as f32),
        };

        match &mut self.waterfall_texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.waterfall_texture =
                    Some(ctx.load_texture("waterfall", image, egui::TextureOptions::LINEAR));
            }
        }
    }

    // -----------------------------------------------------------------------
    // UI drawing
    // -----------------------------------------------------------------------

    fn demod_bandwidth_hz(&self) -> f64 {
        match self.demod_mode {
            DemodMode::Fm => 200_000.0,
            DemodMode::Am => 6_000.0,
            DemodMode::Cw => 500.0,
            _ => 15_000.0,
        }
    }

    fn demod_label(&self) -> &'static str {
        match self.demod_mode {
            DemodMode::Fm => "FM",
            DemodMode::Am => "AM",
            DemodMode::Cw => "CW",
            _ => "?",
        }
    }

    fn draw_receiver_strip(&mut self, ctx: &egui::Context) {
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
                ui.label("MHz");
                if ui.small_button("-").on_hover_text("-10 kHz").clicked() {
                    self.adjust_freq(-0.01);
                }
                if ui.small_button("+").on_hover_text("+10 kHz").clicked() {
                    self.adjust_freq(0.01);
                }
                ui.separator();

                let previous_demod = self.demod_mode;
                ui.selectable_value(&mut self.demod_mode, DemodMode::Fm, "FM");
                ui.selectable_value(&mut self.demod_mode, DemodMode::Am, "AM");
                ui.selectable_value(&mut self.demod_mode, DemodMode::Cw, "CW");
                if self.demod_mode != previous_demod && (self.is_connected || self.is_connecting) {
                    self.status_msg = "Reconnect to apply demod mode".to_string();
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

    fn draw_bottom_bar(&self, ctx: &egui::Context) {
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
                ui.label(
                    egui::RichText::new(format!(
                        "LO {:.3} MHz   {:.3} MSPS   FFT {}",
                        self.center_freq_hz / 1e6,
                        self.sample_rate_hz / 1e6,
                        self.config.ui.fft_size
                    ))
                    .weak(),
                );
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

    fn draw_decoder_dock(&mut self, ctx: &egui::Context) {
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
                    DecoderTab::Passes => self.draw_passes_tab(ui),
                    DecoderTab::Log => self.draw_log_tab(ui),
                }
            });
        self.config.ui.decoder_dock_height = dock.response.rect.height();
    }

    fn draw_cw_tab(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("CW");
            ui.label(&self.cw_status);
        });
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

    fn draw_passes_tab(&mut self, ui: &mut egui::Ui) {
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

    fn draw_log_tab(&self, ui: &mut egui::Ui) {
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

    fn draw_radio_panel(&mut self, ctx: &egui::Context) {
        let panel = egui::SidePanel::left("radio_panel")
            .resizable(true)
            .default_width(self.config.ui.left_panel_width)
            .min_width(230.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Radio");
                ui.separator();

                if self.is_connected || self.is_connecting {
                    if ui
                        .add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::new("Disconnect"),
                        )
                        .clicked()
                    {
                        self.disconnect();
                    }
                } else if ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::new("Connect Pluto"),
                    )
                    .clicked()
                {
                    self.connect();
                }

                egui::CollapsingHeader::new("Source")
                    .default_open(true)
                    .show(ui, |ui| {
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
                        ui.label(
                            egui::RichText::new(format!(
                                "Sample rate: {:.3} MSPS",
                                self.sample_rate_hz / 1e6
                            ))
                            .weak(),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "LO: {:.3} MHz",
                                self.center_freq_hz / 1e6
                            ))
                            .weak(),
                        );
                    });

                egui::CollapsingHeader::new("Demod")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(format!(
                            "Mode: {}   BW: {}",
                            self.demod_label(),
                            format_bandwidth(self.demod_bandwidth_hz())
                        ));
                        ui.label(
                            egui::RichText::new(match self.demod_mode {
                                DemodMode::Fm => {
                                    format!(
                                        "de-emphasis {:.0} µs",
                                        self.config.demod.params(self.demod_mode).deemphasis_us
                                    )
                                }
                                DemodMode::Cw => format!(
                                    "BFO {:.0} Hz",
                                    self.config.demod.params(self.demod_mode).bfo_hz
                                ),
                                DemodMode::Am => "envelope detector".to_string(),
                                _ => String::new(),
                            })
                            .weak(),
                        );
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
                        ui.label(
                            egui::RichText::new(format!(
                                "range {:.0} .. {:.0} dBFS",
                                self.db_min, self.db_max
                            ))
                            .weak(),
                        );
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

    fn draw_decoder_panel(&mut self, ctx: &egui::Context) {
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
                        ui.label(format!("Status: {}", self.cw_status));
                        ui.label(egui::RichText::new("output in the bottom CW tab").weak());
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
                        ui.label(egui::RichText::new("more: APT · SSTV · SSDV · FT4").weak());
                    });

                ui.add_space(6.0);
                ui.checkbox(&mut self.decoder_dock_open, "Show decoder dock");
                ui.separator();
                ui.label(egui::RichText::new("plugins run as separate processes").weak());
            });
        self.config.ui.right_panel_width = panel.response.rect.width();
    }

    fn draw_central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let total = ui.available_height().max(120.0);
            let max_split = (total - 70.0).max(60.0);
            let split = (total * self.spectrum_ratio).clamp(60.0, max_split);

            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), split),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    self.draw_spectrum(ui);
                },
            );

            let separator = ui
                .allocate_response(egui::vec2(ui.available_width(), 6.0), egui::Sense::drag())
                .on_hover_cursor(egui::CursorIcon::ResizeVertical);
            ui.painter()
                .rect_filled(separator.rect, 1.0, egui::Color32::from_rgb(45, 55, 68));
            if separator.dragged() {
                let delta = ui.input(|input| input.pointer.delta().y);
                self.spectrum_ratio = ((split + delta) / total).clamp(0.2, 0.85);
            }

            self.draw_waterfall(ui);
        });
    }

    fn draw_spectrum(&mut self, ui: &mut egui::Ui) {
        let mut tune_to = None;
        let plot = Plot::new("spectrum_plot")
            .x_axis_label("Frequency (MHz)")
            .y_axis_label("Power (dBFS)")
            .include_y(self.db_min as f64)
            .include_y(self.db_max as f64)
            .allow_drag(true)
            .allow_zoom(true)
            .show_axes([true, true])
            .label_formatter(|_name, value| format!("{:.4} MHz\n{:.1} dBFS", value.x, value.y));

        plot.show(ui, |plot_ui| {
            if !self.current_spectrum.is_empty() {
                let points = PlotPoints::new(self.current_spectrum.clone());
                let line = Line::new("Power", points)
                    .color(egui::Color32::from_rgb(30, 160, 255))
                    .width(1.2);
                plot_ui.line(line);
            }

            let center_mhz = self.center_freq_hz / 1e6;
            let half_bw_mhz = self.demod_bandwidth_hz() / 2.0 / 1e6;
            for (name, x) in [
                ("VFO low", center_mhz - half_bw_mhz),
                ("VFO high", center_mhz + half_bw_mhz),
            ] {
                let edges = Line::new(
                    name,
                    PlotPoints::new(vec![[x, self.db_min as f64], [x, self.db_max as f64]]),
                )
                .color(egui::Color32::from_rgb(90, 110, 130))
                .width(0.8);
                plot_ui.line(edges);
            }
            let vfo = Line::new(
                "VFO",
                PlotPoints::new(vec![
                    [center_mhz, self.db_min as f64],
                    [center_mhz, self.db_max as f64],
                ]),
            )
            .color(egui::Color32::from_rgb(80, 220, 255))
            .width(1.0);
            plot_ui.line(vfo);

            if plot_ui.response().clicked()
                && let Some(point) = plot_ui.pointer_coordinate()
            {
                tune_to = Some(point.x);
            }
        });

        if let Some(mhz) = tune_to {
            self.set_frequency_mhz(mhz);
        }
    }

    fn draw_waterfall(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let Some(texture_id) = self.waterfall_texture.as_ref().map(|texture| texture.id()) else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("Waiting for IQ\nConnect the Pluto first")
                        .size(18.0)
                        .color(egui::Color32::GRAY),
                );
            });
            return;
        };

        let response = ui
            .allocate_response(available, egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::Crosshair);
        let rect = response.rect;
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        ui.painter()
            .image(texture_id, rect, uv, egui::Color32::WHITE);

        let painter = ui.painter();
        let half = (self.demod_bandwidth_hz() / 2.0 / self.sample_rate_hz) as f32;
        let x_center = rect.center().x;
        let x_low = rect.min.x + (0.5 - half) * rect.width();
        let x_high = rect.min.x + (0.5 + half) * rect.width();
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x_low, rect.min.y),
                egui::pos2(x_high, rect.max.y),
            ),
            0.0,
            egui::Color32::from_rgba_unmultiplied(30, 200, 255, 18),
        );
        painter.line_segment(
            [
                egui::pos2(x_center, rect.min.y),
                egui::pos2(x_center, rect.max.y),
            ],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 220, 255)),
        );

        let steps = 5;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let db = self.db_min + t * (self.db_max - self.db_min);
            let y = rect.min.y + (1.0 - t) * rect.height();
            painter.text(
                egui::pos2(rect.max.x - 6.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{db:.0} dB"),
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
        }

        for i in 0..=4 {
            let t = i as f32 / 4.0;
            let frequency =
                self.center_freq_hz / 1e6 + (t as f64 - 0.5) * self.sample_rate_hz / 1e6;
            let x = rect.min.x + t * rect.width();
            painter.text(
                egui::pos2(x, rect.max.y - 2.0),
                egui::Align2::CENTER_BOTTOM,
                format!("{frequency:.3}"),
                egui::FontId::proportional(10.0),
                egui::Color32::from_gray(200),
            );
        }

        if response.clicked()
            && let Some(position) = response.interact_pointer_pos()
        {
            let fraction = ((position.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            let frequency =
                self.center_freq_hz / 1e6 + (fraction as f64 - 0.5) * self.sample_rate_hz / 1e6;
            self.set_frequency_mhz(frequency);
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
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

impl eframe::App for MidoriApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.auto_connect {
            self.auto_connect = false;
            self.connect();
        }

        self.poll_events();
        self.poll_data(ctx);
        self.handle_keyboard(ctx);

        if self.is_connected || self.is_connecting {
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        self.draw_receiver_strip(ctx);
        self.draw_bottom_bar(ctx);
        self.draw_decoder_dock(ctx);
        self.draw_radio_panel(ctx);
        self.draw_decoder_panel(ctx);
        self.draw_central(ctx);
    }
}

impl Drop for MidoriApp {
    fn drop(&mut self) {
        self.disconnect();
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn cw_plugin_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MIDORI_CW_PLUGIN") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) {
        "midori-plugin-cw.exe"
    } else {
        "midori-plugin-cw"
    };
    let path = dir.join(name);
    path.exists().then_some(path)
}

fn start_cw_plugin(
    event_tx: &Sender<WorkerEvent>,
    tone_hz: f64,
) -> Option<(PluginSink, PluginHandle)> {
    let Some(path) = cw_plugin_path() else {
        let _ = event_tx.try_send(WorkerEvent::Error(
            "CW plugin binary not found; build the workspace first".to_string(),
        ));
        return None;
    };

    let manifest = Manifest {
        name: "cw".to_string(),
        version: "0.1.0".to_string(),
        protocol: PROTOCOL_VERSION,
        command: vec![path.display().to_string()],
        input: InputSpec::Audio {
            sample_rate_hz: 48_000.0,
            channels: 1,
        },
        outputs: vec![OutputKind::Display, OutputKind::Packets],
        params: vec![],
    };

    let mut plugin = match SidecarPlugin::new(manifest) {
        Ok(plugin) => plugin,
        Err(error) => {
            let _ = event_tx.try_send(WorkerEvent::Error(format!("CW plugin manifest: {error}")));
            return None;
        }
    };
    if let Err(error) = plugin.start() {
        let _ = event_tx.try_send(WorkerEvent::Error(format!("CW plugin start: {error}")));
        return None;
    }
    if let Err(error) = plugin.configure(&serde_json::json!({"tone_hz": tone_hz})) {
        let _ = plugin.stop();
        let _ = event_tx.try_send(WorkerEvent::Error(format!("CW plugin configure: {error}")));
        return None;
    }

    let (sink, handle) = plugin_sink(Box::new(plugin));
    Some((sink, handle))
}

fn build_doppler(
    options: &DopplerOptions,
    event_tx: &Sender<WorkerEvent>,
) -> Option<Box<dyn midori_core::DopplerSource>> {
    let tle = match Tle::from_file(&options.tle_path) {
        Ok(tle) => tle,
        Err(error) => {
            let _ = event_tx.try_send(WorkerEvent::Error(format!(
                "Doppler: could not load TLE {}: {error}",
                options.tle_path.display()
            )));
            return None;
        }
    };
    let grid = match GridSquare::parse(&options.grid) {
        Ok(grid) => grid,
        Err(error) => {
            let _ = event_tx.try_send(WorkerEvent::Error(format!(
                "Doppler: invalid grid '{}': {error}",
                options.grid
            )));
            return None;
        }
    };
    let predictor = match PassPredictor::new(&tle, grid.observer(0.0)) {
        Ok(predictor) => predictor,
        Err(error) => {
            let _ = event_tx.try_send(WorkerEvent::Error(format!(
                "Doppler: SGP4 setup failed: {error}"
            )));
            return None;
        }
    };
    let _ = event_tx.try_send(WorkerEvent::Info(format!(
        "Doppler: tracking {} at {:.6} MHz",
        tle.name
            .clone()
            .unwrap_or_else(|| format!("NORAD {}", tle.norad_id)),
        options.downlink_hz / 1e6
    )));
    Some(Box::new(Sgp4Doppler::new(predictor, options.downlink_hz)))
}

fn run_worker(
    config: SourceConfig,
    mut vfo: VfoConfig,
    options: WorkerOptions,
    data_tx: Sender<SpectrumFrame>,
    event_tx: Sender<WorkerEvent>,
    cmd_rx: Receiver<WorkerCommand>,
    stop: Arc<AtomicBool>,
) {
    let source = match LibiioSource::open(Some(&options.source_uri)) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("[worker] connect failed: {error}");
            let _ = event_tx.try_send(WorkerEvent::Fatal(format!("connect failed: {error}")));
            return;
        }
    };

    let doppler = options
        .doppler
        .as_ref()
        .and_then(|doppler| build_doppler(doppler, &event_tx));
    let doppler_active = doppler.is_some();

    let (audio_output, audio_producer) = match AudioOutput::open() {
        Ok((output, producer)) => (Some(output), Some(producer)),
        Err(error) => {
            eprintln!("[worker] audio unavailable: {error}");
            let _ = event_tx.try_send(WorkerEvent::Error(format!("audio unavailable: {error}")));
            (None, None)
        }
    };

    if let Some(producer) = &audio_producer {
        vfo.audio_rate_hz = producer.sample_rate_hz() as f64;
    }
    if let Some(output) = &audio_output {
        eprintln!(
            "[worker] audio output: {} Hz, {} channel(s)",
            output.sample_rate_hz(),
            output.channels()
        );
    }

    let (plugin_sink, plugin_handle) = if options.cw_enabled {
        match start_cw_plugin(&event_tx, options.cw_tone_hz) {
            Some((sink, handle)) => (Some(sink), Some(handle)),
            None => (None, None),
        }
    } else {
        (None, None)
    };

    let (recorder_sink, recorder_handle) = RecorderSink::new();
    let mut sinks: Vec<Box<dyn Sink>> = vec![
        Box::new(SpectrumSink::new(options.fft_size, data_tx)),
        Box::new(recorder_sink),
    ];
    if let Some(sink) = plugin_sink {
        sinks.push(Box::new(sink));
    }
    if let Some(producer) = audio_producer {
        sinks.push(Box::new(AudioSink::new(producer)));
    }

    let channel = VfoChannel::new(vfo);
    let mut session = LiveSession::new(config, Box::new(source), vec![Box::new(channel)], sinks);
    session.set_doppler_source(doppler);
    let _audio_output = audio_output;

    if let Err(error) = session.start() {
        eprintln!("[worker] start failed: {error}");
        let _ = event_tx.try_send(WorkerEvent::Fatal(format!("start failed: {error}")));
        return;
    }

    let Some(spec) = session.current_spec() else {
        let _ = event_tx.try_send(WorkerEvent::Fatal("session has no stream spec".to_string()));
        return;
    };
    eprintln!(
        "[worker] connected: {:.3} MHz, {:.3} MSPS, {} audio",
        spec.center_freq_hz / 1e6,
        spec.sample_rate_hz / 1e6,
        if _audio_output.is_some() {
            "with"
        } else {
            "without"
        }
    );
    let _ = event_tx.try_send(WorkerEvent::Connected(spec));

    let mut plugin_events = 0u64;
    let track_doppler = doppler_active;
    let mut last_doppler_event: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        while let Ok(command) = cmd_rx.try_recv() {
            match command {
                WorkerCommand::SetFrequency(hz) => {
                    if recorder_handle.is_recording() {
                        let _ = event_tx.try_send(WorkerEvent::Error(
                            "stop recording before retuning".to_string(),
                        ));
                        continue;
                    }
                    if let Err(error) = session.set_frequency(hz) {
                        let _ =
                            event_tx.try_send(WorkerEvent::Error(format!("tune failed: {error}")));
                    }
                }
                WorkerCommand::SetGain(db) => {
                    if let Err(error) = session.set_gain(db) {
                        let _ =
                            event_tx.try_send(WorkerEvent::Error(format!("gain failed: {error}")));
                    }
                }
                WorkerCommand::StartRecording(path) => {
                    let Some(spec) = session.current_spec() else {
                        let _ = event_tx.try_send(WorkerEvent::RecordingError(
                            "session has no stream spec".to_string(),
                        ));
                        continue;
                    };
                    match recorder_handle.start(path.clone(), spec) {
                        Ok(()) => {
                            let _ = event_tx.try_send(WorkerEvent::RecordingStarted(path));
                        }
                        Err(error) => {
                            let _ = event_tx.try_send(WorkerEvent::RecordingError(format!(
                                "could not start recording: {error}"
                            )));
                        }
                    }
                }
                WorkerCommand::StopRecording => match recorder_handle.stop() {
                    Ok(Some(info)) => {
                        let _ = event_tx.try_send(WorkerEvent::RecordingStopped {
                            path: info.path,
                            samples: info.samples,
                            seconds: info.seconds,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = event_tx.try_send(WorkerEvent::RecordingError(format!(
                            "recording finalise failed: {error}"
                        )));
                    }
                },
                WorkerCommand::Stop => {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }

        if stop.load(Ordering::SeqCst) {
            break;
        }

        if let Err(error) = session.step() {
            eprintln!("[worker] receive failed: {error}");
            let _ = event_tx.try_send(WorkerEvent::Fatal(format!("receive failed: {error}")));
            break;
        }

        if track_doppler {
            let now = Instant::now();
            let due = last_doppler_event
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500));
            if due {
                last_doppler_event = Some(now);
                let _ = event_tx.try_send(WorkerEvent::Doppler {
                    offset_hz: session.doppler_hz(),
                    rate_hz_s: session.doppler_rate_hz_s(),
                });
            }
        }

        if let Some(error) = recorder_handle.take_error() {
            let _ = event_tx.try_send(WorkerEvent::RecordingError(error));
        }

        if let Some(handle) = &plugin_handle {
            loop {
                match handle.try_recv() {
                    Ok(Some(event)) => {
                        if plugin_events == 0 {
                            eprintln!("[plugin] first event");
                        }
                        plugin_events += 1;
                        let _ = event_tx.try_send(WorkerEvent::Plugin(event));
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = event_tx.try_send(WorkerEvent::Error(format!("plugin: {error}")));
                        break;
                    }
                }
            }
        }
    }

    if let Some(handle) = &plugin_handle {
        let _ = handle.stop();
    }
    if let Ok(Some(info)) = recorder_handle.stop() {
        let _ = event_tx.try_send(WorkerEvent::RecordingStopped {
            path: info.path,
            samples: info.samples,
            seconds: info.seconds,
        });
    }
    let _ = session.stop();
    eprintln!("[worker] stopped");
}

// ---------------------------------------------------------------------------
// Presentation helpers
// ---------------------------------------------------------------------------

fn apply_theme(ctx: &egui::Context, theme: Theme) {
    let mut visuals = match theme {
        Theme::Dark => {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::from_rgb(16, 20, 26);
            visuals.window_fill = egui::Color32::from_rgb(20, 25, 32);
            visuals.extreme_bg_color = egui::Color32::from_rgb(10, 13, 18);
            visuals.faint_bg_color = egui::Color32::from_rgb(24, 30, 38);
            visuals.selection.bg_fill = egui::Color32::from_rgb(30, 120, 180);
            visuals
        }
        Theme::Light => egui::Visuals::light(),
    };
    if theme == Theme::Light {
        visuals.selection.bg_fill = egui::Color32::from_rgb(30, 120, 180);
    }
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    ctx.set_style(style);
}

fn format_bandwidth(hz: f64) -> String {
    if hz >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

fn downsample(input: &[f32], width: usize) -> Vec<f32> {
    if input.len() == width || input.is_empty() {
        return input.to_vec();
    }
    (0..width)
        .map(|index| {
            let start = index * input.len() / width;
            let end = (((index + 1) * input.len()) / width)
                .max(start + 1)
                .min(input.len());
            let sum: f32 = input[start..end].iter().sum();
            sum / (end - start) as f32
        })
        .collect()
}

/// Map a dBFS value to a viridis-like colour.
#[inline]
fn db_to_color(db: f32, min: f32, max: f32, colormap: Colormap) -> egui::Color32 {
    let t = ((db - min) / (max - min)).clamp(0.0, 1.0);
    match colormap {
        Colormap::Grayscale => {
            let value = (t * 255.0) as u8;
            egui::Color32::from_rgb(value, value, value)
        }
        Colormap::Turbo => turbo_color(t),
        Colormap::Viridis => viridis_color(t),
    }
}

fn viridis_color(t: f32) -> egui::Color32 {
    let (r, g, b) = if t < 0.25 {
        let s = t / 0.25;
        (
            lerp(68.0, 59.0, s),
            lerp(1.0, 82.0, s),
            lerp(84.0, 139.0, s),
        )
    } else if t < 0.5 {
        let s = (t - 0.25) / 0.25;
        (
            lerp(59.0, 33.0, s),
            lerp(82.0, 145.0, s),
            lerp(139.0, 140.0, s),
        )
    } else if t < 0.75 {
        let s = (t - 0.5) / 0.25;
        (
            lerp(33.0, 94.0, s),
            lerp(145.0, 201.0, s),
            lerp(140.0, 97.0, s),
        )
    } else {
        let s = (t - 0.75) / 0.25;
        (
            lerp(94.0, 253.0, s),
            lerp(201.0, 231.0, s),
            lerp(97.0, 37.0, s),
        )
    };
    egui::Color32::from_rgb(r as u8, g as u8, b as u8)
}

fn turbo_color(t: f32) -> egui::Color32 {
    const STOPS: [(f32, [f32; 3]); 5] = [
        (0.0, [48.0, 18.0, 59.0]),
        (0.25, [70.0, 107.0, 227.0]),
        (0.5, [62.0, 188.0, 115.0]),
        (0.75, [245.0, 187.0, 68.0]),
        (1.0, [180.0, 4.0, 38.0]),
    ];

    for pair in STOPS.windows(2) {
        let (start, start_color) = pair[0];
        let (end, end_color) = pair[1];
        if t <= end {
            let local = ((t - start) / (end - start)).clamp(0.0, 1.0);
            let red = lerp(start_color[0], end_color[0], local);
            let green = lerp(start_color[1], end_color[1], local);
            let blue = lerp(start_color[2], end_color[2], local);
            return egui::Color32::from_rgb(red as u8, green as u8, blue as u8);
        }
    }
    egui::Color32::from_rgb(180, 4, 38)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
