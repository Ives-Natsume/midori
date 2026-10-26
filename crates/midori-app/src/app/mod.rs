//! egui front end for the Midori core.
//!
//! The UI thread only renders and handles input. A worker thread owns the
//! hardware source, computes spectra, and optionally records SigMF. This keeps
//! a slow or disconnected device from freezing the window.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded};
use eframe::egui;

use midori_config::{AppConfig, Colormap, DecoderTab, SourceKind};
use midori_core::{DemodMode, SourceConfig, VfoConfig};
use midori_orbit::Pass;

use crate::app::format::{apply_theme, frequency_from_path, split_list};
use crate::worker::commands::{CommandSender, WorkerCommand};
use crate::worker::events::{EventReceiver, EventSender};
use crate::worker::{DopplerOptions, SpectrumFrame, WorkerOptions, WorkerSource, run_worker};

mod control;
mod events;
pub(crate) mod format;
mod images;
mod satellite;
mod ui;

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

pub struct MidoriApp {
    pub(crate) config: AppConfig,
    pub(crate) config_path: PathBuf,
    pub(crate) config_warning: Option<String>,

    pub(crate) data_rx: Option<Receiver<SpectrumFrame>>,
    pub(crate) event_rx: Option<EventReceiver>,
    pub(crate) cmd_tx: Option<CommandSender>,
    pub(crate) worker: Option<JoinHandle<()>>,
    pub(crate) worker_stopping: bool,
    pub(crate) stop_flag: Arc<AtomicBool>,

    pub(crate) current_spectrum: Vec<[f64; 2]>,
    pub(crate) center_freq_hz: f64,
    pub(crate) sample_rate_hz: f64,
    pub(crate) signal_dbfs: f32,
    pub(crate) noise_dbfs: f32,
    pub(crate) spectrum_ratio: f32,
    pub(crate) db_min: f32,
    pub(crate) db_max: f32,
    pub(crate) colormap: Colormap,
    pub(crate) reset_spectrum_view: bool,
    pub(crate) spectrum_view_x: Option<(f64, f64)>,

    pub(crate) waterfall_rows: VecDeque<Vec<f32>>,
    pub(crate) waterfall_texture: Option<egui::TextureHandle>,
    pub(crate) waterfall_follow: bool,

    pub(crate) freq_input: String,
    pub(crate) gain_input: String,
    pub(crate) demod_mode: DemodMode,
    pub(crate) source_kind: SourceKind,
    pub(crate) file_path: String,
    pub(crate) file_loop: bool,
    pub(crate) vfo_offset_hz: f64,

    pub(crate) decoder_tab: DecoderTab,
    pub(crate) decoder_dock_open: bool,
    pub(crate) cw_enabled: bool,
    pub(crate) cw_afc: bool,
    pub(crate) cw_text: String,
    pub(crate) cw_status: String,
    pub(crate) auto_tune_status: String,
    pub(crate) auto_tune_failed: bool,
    pub(crate) sstv_enabled: bool,
    pub(crate) sstv_status: String,
    pub(crate) sstv_progress: f32,
    pub(crate) sstv_image_title: String,
    pub(crate) sstv_image_png: Option<Vec<u8>>,
    pub(crate) sstv_texture: Option<egui::TextureHandle>,
    pub(crate) ssdv_enabled: bool,
    pub(crate) ssdv_crc_inits_input: String,
    pub(crate) ssdv_frame_header_len: u32,
    pub(crate) ssdv_status: String,
    pub(crate) ssdv_progress: f32,
    pub(crate) ssdv_image_title: String,
    pub(crate) ssdv_image_png: Option<Vec<u8>>,
    pub(crate) ssdv_texture: Option<egui::TextureHandle>,

    pub(crate) tle_path: String,
    pub(crate) grid_input: String,
    pub(crate) min_elev_input: String,
    pub(crate) satellite_name: String,
    pub(crate) passes: Vec<Pass>,
    pub(crate) pass_status: String,
    pub(crate) doppler_tracking: bool,
    pub(crate) doppler_hz: f64,
    pub(crate) doppler_rate_hz_s: f64,
    pub(crate) log: VecDeque<String>,

    pub(crate) is_connected: bool,
    pub(crate) is_connecting: bool,
    pub(crate) is_recording: bool,
    pub(crate) auto_connect: bool,
    pub(crate) recording_path: Option<PathBuf>,
    pub(crate) status_msg: String,
    pub(crate) error_msg: Option<String>,

    pub(crate) demod_dirty: bool,
    pub(crate) last_demod_change: Instant,
}

impl Default for MidoriApp {
    fn default() -> Self {
        let config_path = PathBuf::from(midori_config::DEFAULT_FILE);
        let loaded = AppConfig::load(&config_path);
        let warning = loaded.warning.clone();
        let config = loaded.config;

        let mut center_freq_hz = config.source.center_freq_hz;
        let sample_rate_hz = config.source.sample_rate_hz;
        let gain_db = config.source.gain_db;
        let demod_mode = config.demod.mode;
        let mut source_kind = config.source.kind;
        let mut file_path = config.source.file_path.clone();
        let file_loop = config.source.file_loop;
        if let Some(path) = std::env::var_os("MIDORI_FILE") {
            file_path = path.to_string_lossy().into_owned();
            source_kind = SourceKind::File;
        }
        if source_kind == SourceKind::File
            && let Some(mhz) = frequency_from_path(&file_path)
        {
            center_freq_hz = mhz * 1e6;
        }
        let mut vfo_offset_hz = 0.0;
        if source_kind == SourceKind::File
            && let Ok(value) = std::env::var("MIDORI_VFO")
        {
            vfo_offset_hz = value.parse::<f64>().unwrap_or(0.0);
        }
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
        let cw_afc = config.decoder.cw_afc;
        let sstv_enabled = config.decoder.sstv_enabled;
        let ssdv_enabled = config.decoder.ssdv_enabled;
        let ssdv_crc_inits = config.decoder.ssdv_crc_inits.clone();
        let ssdv_frame_header_len = config.decoder.ssdv_frame_header_len as u32;
        let waterfall_follow = config.ui.waterfall_follow_spectrum;
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
            worker_stopping: false,
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
            reset_spectrum_view: false,
            spectrum_view_x: None,
            waterfall_rows: VecDeque::with_capacity(waterfall_capacity),
            waterfall_texture: None,
            waterfall_follow,
            freq_input: format!("{:.6}", center_freq_hz / 1e6),
            gain_input: format!("{gain_db:.1}"),
            demod_mode,
            source_kind,
            file_path,
            file_loop,
            vfo_offset_hz,
            decoder_tab,
            decoder_dock_open,
            cw_enabled: std::env::var_os("MIDORI_CW").is_some() || cw_enabled,
            cw_afc,
            cw_text: String::new(),
            cw_status: "idle".to_string(),
            auto_tune_status: String::new(),
            auto_tune_failed: false,
            sstv_enabled: std::env::var_os("MIDORI_SSTV").is_some() || sstv_enabled,
            sstv_status: "idle".to_string(),
            sstv_progress: 0.0,
            sstv_image_title: String::new(),
            sstv_image_png: None,
            sstv_texture: None,
            ssdv_enabled: std::env::var_os("MIDORI_SSDV").is_some() || ssdv_enabled,
            ssdv_crc_inits_input: if ssdv_crc_inits.is_empty() {
                "0xFFFFFFFF".to_string()
            } else {
                ssdv_crc_inits.join(", ")
            },
            ssdv_frame_header_len,
            ssdv_status: "idle".to_string(),
            ssdv_progress: 0.0,
            ssdv_image_title: String::new(),
            ssdv_image_png: None,
            ssdv_texture: None,
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
            demod_dirty: false,
            last_demod_change: Instant::now() - Duration::from_secs(1),
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

    pub(crate) fn connect(&mut self) {
        if self.is_connected || self.is_connecting {
            return;
        }
        if self.worker_stopping || self.worker.is_some() {
            // A previous worker is still releasing the SDR, or has exited but
            // has not had its final events drained yet. Starting another one
            // now would make two threads touch the same hardware and could
            // lose the shutdown output.
            let message = if self.worker_stopping {
                "previous worker is still stopping; try again"
            } else {
                "previous worker has not shut down yet; try again"
            };
            self.push_log(message);
            self.error_msg = Some(message.to_string());
            return;
        }
        // `disconnect` clears the transient state, including the VFO offset.
        // Keep a file replay's initial offset across the implicit disconnect
        // so MIDORI_VFO / a previous file tuning still starts where expected.
        let initial_vfo_offset_hz = self.vfo_offset_hz;
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
        self.center_freq_hz = freq_mhz * 1e6;
        self.vfo_offset_hz = if self.is_file_source() {
            initial_vfo_offset_hz
        } else {
            0.0
        };

        let worker_source = if self.is_file_source() {
            if self.file_path.trim().is_empty() {
                self.error_msg = Some("choose an I/Q file first".to_string());
                return;
            }
            WorkerSource::File {
                path: PathBuf::from(self.file_path.trim()),
                loop_file: self.file_loop,
            }
        } else {
            WorkerSource::Pluto {
                uri: source.uri.clone(),
            }
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
        let fft_size = self.config.ui.fft_size;
        let cw_enabled = self.cw_enabled;
        let cw_afc = self.cw_afc;
        let sstv_enabled = self.sstv_enabled;
        let ssdv_enabled = self.ssdv_enabled;
        let ssdv_crc_inits = split_list(&self.ssdv_crc_inits_input);
        let ssdv_frame_header_len = self.ssdv_frame_header_len as usize;
        let ssdv_audio_if = self.demod_mode == DemodMode::BpskIf;
        let sstv_output_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("captures");
        let doppler = (self.doppler_tracking && !self.is_file_source()).then(|| DopplerOptions {
            tle_path: PathBuf::from(&self.tle_path),
            grid: self.grid_input.clone(),
            downlink_hz: freq_mhz * 1e6,
        });

        let (data_tx, data_rx) = bounded(4);
        let (event_tx, event_rx) = EventSender::new();
        let (cmd_tx, cmd_rx) = CommandSender::new();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop_flag);
        let options = WorkerOptions {
            source: worker_source,
            cw_enabled,
            cw_afc,
            cw_tone_hz,
            sstv_enabled,
            sstv_output_dir,
            ssdv_enabled,
            ssdv_crc_inits,
            ssdv_frame_header_len,
            ssdv_audio_if,
            fft_size,
            doppler,
            initial_vfo_offset_hz: if self.is_file_source() {
                self.vfo_offset_hz
            } else {
                0.0
            },
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
                self.demod_dirty = false;
                self.auto_tune_status.clear();
                self.auto_tune_failed = false;
                self.status_msg = if self.is_file_source() {
                    "Opening I/Q file...".to_string()
                } else {
                    "Connecting to Pluto SDR...".to_string()
                };
            }
            Err(error) => {
                self.error_msg = Some(format!("could not spawn worker: {error}"));
            }
        }
    }

    pub(crate) fn disconnect(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let stop_result = self.cmd_tx.as_ref().map(|tx| tx.send(WorkerCommand::Stop));
        if let Some(Err(error)) = stop_result {
            self.push_log(format!("stop command not delivered: {error}"));
        }
        if self.worker.is_some() {
            // Do not join or drop the receivers here. The worker's shutdown
            // path still emits final plugin output and RecordingStopped; those
            // are drained by `poll_worker_state` before the channels close.
            self.worker_stopping = true;
        } else {
            self.worker_stopping = false;
            self.data_rx = None;
            self.event_rx = None;
            self.cmd_tx = None;
        }
        self.is_connected = false;
        self.is_connecting = false;
        self.is_recording = false;
        self.recording_path = None;
        self.vfo_offset_hz = 0.0;
        if self.is_file_source() {
            self.freq_input = format!("{:.6}", self.center_freq_hz / 1e6);
        }
        self.doppler_hz = 0.0;
        self.doppler_rate_hz_s = 0.0;
        self.current_spectrum.clear();
        self.waterfall_rows.clear();
        self.waterfall_texture = None;
        self.spectrum_view_x = None;
        self.auto_tune_status.clear();
        self.auto_tune_failed = false;
        self.error_msg = None;
        self.status_msg = "Disconnected".to_string();
    }

    /// Join a worker that has already exited, drain its final events, and
    /// surface a worker that died without telling the UI (for example if its
    /// panic message could not be delivered).
    pub(crate) fn poll_worker_state(&mut self, ctx: &egui::Context) {
        let Some(handle) = &self.worker else {
            self.worker_stopping = false;
            return;
        };
        if !handle.is_finished() {
            return;
        }

        let handle = self.worker.take().expect("worker handle checked");
        let _ = handle.join();

        // The thread has ended, so no more events can be produced. Drain the
        // shutdown output one last time while the receiver is still alive;
        // this is where final plugin output and RecordingStopped are applied.
        self.poll_events(ctx);

        let was_stopping = self.worker_stopping;
        self.worker_stopping = false;
        self.data_rx = None;
        self.event_rx = None;
        self.cmd_tx = None;
        self.is_recording = false;

        if !was_stopping && (self.is_connected || self.is_connecting) {
            let message = "worker stopped unexpectedly".to_string();
            self.push_log(format!("fatal: {message}"));
            self.error_msg = Some(message);
            self.status_msg = "Device error".to_string();
            self.is_connected = false;
            self.is_connecting = false;
        }
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Deliver a command to the worker, logging if the bounded queue is full
    /// or the worker has gone away. The UI is the only producer and commands
    /// are generated at human rates, so a short wait is preferable to silently
    /// losing a retune or recording request.
    pub(crate) fn send_command(&mut self, command: WorkerCommand) -> bool {
        let result = self.cmd_tx.as_ref().map(|tx| tx.send(command));
        match result {
            None => false,
            Some(Ok(())) => true,
            Some(Err(error)) => {
                let message = format!("command not delivered: {error}");
                self.push_log(message.clone());
                self.error_msg = Some(message);
                false
            }
        }
    }
    pub(crate) fn is_file_source(&self) -> bool {
        self.source_kind == SourceKind::File
    }

    pub(crate) fn tuned_freq_hz(&self) -> f64 {
        self.center_freq_hz + self.vfo_offset_hz
    }
}

impl eframe::App for MidoriApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.auto_connect {
            self.auto_connect = false;
            self.connect();
        }

        self.poll_events(ctx);
        self.poll_worker_state(ctx);
        self.poll_data(ctx);
        self.handle_keyboard(ctx);

        if self.demod_dirty
            && (self.is_connected || self.is_connecting)
            && self.last_demod_change.elapsed() >= Duration::from_millis(250)
        {
            self.demod_dirty = false;
            self.send_demod_update();
        }

        if self.is_connected || self.is_connecting || self.worker_stopping {
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
        // At exit it is fine to wait for the worker: Stop was already sent and
        // the stop flag is set, so this only covers plugin/source teardown.
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}
