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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, bounded};
use eframe::egui;
use egui_plot::{Line, Plot, PlotPoints};

use midori_core::{GainMode, IqSource, SourceConfig, StreamSpec};
use midori_dsp::{SpectrumAnalyzer, fftshift, frequency_axis};
use midori_sigmf::SigMfWriter;
use midori_source_libiio::LibiioSource;

const SAMPLE_RATE_HZ: f64 = 2_400_000.0;
const BANDWIDTH_HZ: f64 = 2_400_000.0;
const BUFFER_SAMPLES: usize = 32_768;
const FFT_SIZE: usize = 4_096;
const WATERFALL_ROWS: usize = 200;
const WATERFALL_WIDTH: usize = 1_024;
const DB_MIN: f32 = -110.0;
const DB_MAX: f32 = -20.0;

// ---------------------------------------------------------------------------
// Worker messages
// ---------------------------------------------------------------------------

struct SpectrumFrame {
    spectrum_db: Vec<f32>,
    frequencies_mhz: Vec<f32>,
    center_freq_hz: f64,
    sample_rate_hz: f64,
}

enum WorkerCommand {
    SetFrequency(f64),
    SetGain(f64),
    StartRecording(PathBuf),
    StopRecording,
    Stop,
}

enum WorkerEvent {
    Connected(StreamSpec),
    Error(String),
    Fatal(String),
    RecordingStarted(PathBuf),
    RecordingError(String),
    RecordingStopped {
        path: PathBuf,
        samples: u64,
        seconds: f64,
    },
}

// ---------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum ViewMode {
    Spectrum,
    Waterfall,
}

pub struct MidoriApp {
    view_mode: ViewMode,

    data_rx: Option<Receiver<SpectrumFrame>>,
    event_rx: Option<Receiver<WorkerEvent>>,
    cmd_tx: Option<Sender<WorkerCommand>>,
    worker: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,

    current_spectrum: Vec<[f64; 2]>,
    center_freq_hz: f64,
    sample_rate_hz: f64,

    waterfall_rows: VecDeque<Vec<f32>>,
    waterfall_texture: Option<egui::TextureHandle>,

    freq_input: String,
    gain_input: String,

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
        Self {
            view_mode: ViewMode::Spectrum,
            data_rx: None,
            event_rx: None,
            cmd_tx: None,
            worker: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            current_spectrum: Vec::new(),
            center_freq_hz: 120_000_000.0,
            sample_rate_hz: SAMPLE_RATE_HZ,
            waterfall_rows: VecDeque::with_capacity(WATERFALL_ROWS + 4),
            waterfall_texture: None,
            freq_input: "120.000".to_string(),
            gain_input: "40".to_string(),
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
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self::default()
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

        let config = SourceConfig {
            center_freq_hz: freq_mhz * 1e6,
            sample_rate_hz: SAMPLE_RATE_HZ,
            bandwidth_hz: BANDWIDTH_HZ,
            gain_db,
            gain_mode: GainMode::Manual,
            rf_port: Some("A_BALANCED".to_string()),
            buffer_samples: BUFFER_SAMPLES,
        };

        let (data_tx, data_rx) = bounded(4);
        let (event_tx, event_rx) = bounded(64);
        let (cmd_tx, cmd_rx) = bounded(16);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop_flag);

        match thread::Builder::new()
            .name("sdr-worker".into())
            .spawn(move || run_worker(config, data_tx, event_tx, cmd_rx, worker_stop))
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
        self.current_spectrum.clear();
        self.waterfall_rows.clear();
        self.waterfall_texture = None;
        self.error_msg = None;
        self.status_msg = "Disconnected".to_string();
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

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
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(WorkerCommand::StartRecording(path));
        }
    }

    fn stop_recording(&mut self) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.try_send(WorkerCommand::StopRecording);
        }
    }

    // -----------------------------------------------------------------------
    // Data polling
    // -----------------------------------------------------------------------

    fn poll_events(&mut self) {
        let Some(rx) = &self.event_rx else { return };
        while let Ok(event) = rx.try_recv() {
            match event {
                WorkerEvent::Connected(spec) => {
                    self.is_connecting = false;
                    self.is_connected = true;
                    self.center_freq_hz = spec.center_freq_hz;
                    self.sample_rate_hz = spec.sample_rate_hz;
                    self.error_msg = None;
                    self.status_msg = format!(
                        "Connected | {:.3} MHz | {:.3} MSPS",
                        spec.center_freq_hz / 1e6,
                        spec.sample_rate_hz / 1e6
                    );
                }
                WorkerEvent::Error(message) => {
                    self.error_msg = Some(message);
                }
                WorkerEvent::Fatal(message) => {
                    self.is_connecting = false;
                    self.is_connected = false;
                    self.is_recording = false;
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
                }
                WorkerEvent::RecordingError(message) => {
                    self.is_recording = false;
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
                }
            }
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

        let row = downsample(&frame.spectrum_db, WATERFALL_WIDTH);
        if self.waterfall_rows.len() >= WATERFALL_ROWS {
            self.waterfall_rows.pop_front();
        }
        self.waterfall_rows.push_back(row);

        self.rebuild_waterfall_texture(ctx);
        ctx.request_repaint();
    }

    fn rebuild_waterfall_texture(&mut self, ctx: &egui::Context) {
        let width = WATERFALL_WIDTH;
        let height = WATERFALL_ROWS;
        let mut pixels = vec![egui::Color32::BLACK; width * height];

        for (row_index, row) in self.waterfall_rows.iter().rev().enumerate() {
            if row_index >= height {
                break;
            }
            for (column, &db) in row.iter().enumerate().take(width) {
                pixels[row_index * width + column] = db_to_color(db);
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

    fn draw_top_panel(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Midori SDR");
                ui.separator();
                ui.label(&self.status_msg);
                if let Some(error) = &self.error_msg {
                    ui.separator();
                    ui.label(egui::RichText::new(error).color(egui::Color32::LIGHT_RED));
                }
            });
        });
    }

    fn draw_bottom_bar(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("bottom_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "LO {:.3} MHz   {:.3} MSPS   FFT {}",
                    self.center_freq_hz / 1e6,
                    self.sample_rate_hz / 1e6,
                    FFT_SIZE
                ));
                if self.is_recording {
                    ui.separator();
                    ui.label(egui::RichText::new("REC").color(egui::Color32::LIGHT_RED));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("Pluto RX1A  |  cf32 core");
                });
            });
        });
    }

    fn draw_control_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("control_panel")
            .min_width(230.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.heading("Control");
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

                ui.add_space(8.0);
                ui.separator();

                ui.label("Display");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.view_mode, ViewMode::Spectrum, "Spectrum");
                    ui.selectable_value(&mut self.view_mode, ViewMode::Waterfall, "Waterfall");
                });

                ui.add_space(8.0);
                ui.separator();

                ui.label(egui::RichText::new("Centre frequency (MHz)").strong());
                let freq_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.freq_input)
                        .hint_text("120.000")
                        .desired_width(ui.available_width()),
                );
                if freq_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.send_frequency();
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("<<").on_hover_text("-100 kHz").clicked() {
                        self.adjust_freq(-0.1);
                    }
                    if ui.button("<").on_hover_text("-10 kHz").clicked() {
                        self.adjust_freq(-0.01);
                    }
                    if ui.button(">").on_hover_text("+10 kHz").clicked() {
                        self.adjust_freq(0.01);
                    }
                    if ui.button(">>").on_hover_text("+100 kHz").clicked() {
                        self.adjust_freq(0.1);
                    }
                });
                if ui
                    .add_sized(
                        [ui.available_width(), 28.0],
                        egui::Button::new("Set frequency"),
                    )
                    .clicked()
                {
                    self.send_frequency();
                }

                ui.add_space(8.0);
                ui.separator();

                ui.label(egui::RichText::new("Manual gain (dB)").strong());
                let gain_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.gain_input)
                        .hint_text("40")
                        .desired_width(ui.available_width()),
                );
                if gain_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.send_gain();
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("-5").clicked() {
                        self.adjust_gain(-5.0);
                    }
                    if ui.button("-1").clicked() {
                        self.adjust_gain(-1.0);
                    }
                    if ui.button("+1").clicked() {
                        self.adjust_gain(1.0);
                    }
                    if ui.button("+5").clicked() {
                        self.adjust_gain(5.0);
                    }
                });
                if ui
                    .add_sized([ui.available_width(), 28.0], egui::Button::new("Set gain"))
                    .clicked()
                {
                    self.send_gain();
                }

                ui.add_space(8.0);
                ui.separator();

                if self.is_recording {
                    if ui
                        .add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::new("Stop recording"),
                        )
                        .clicked()
                    {
                        self.stop_recording();
                    }
                    if let Some(path) = &self.recording_path {
                        ui.label(
                            egui::RichText::new(format!("REC {}", path.display()))
                                .color(egui::Color32::LIGHT_RED),
                        );
                    }
                } else if ui
                    .add_sized([ui.available_width(), 30.0], egui::Button::new("Record IQ"))
                    .clicked()
                {
                    self.start_recording();
                }

                ui.add_space(8.0);
                ui.separator();

                ui.collapsing("Keyboard shortcuts", |ui| {
                    ui.label("Left / Right     +/-10 kHz");
                    ui.label("Shift+Left/Right +/-100 kHz");
                    ui.label("PageUp / PageDown +/-1 MHz");
                    ui.label("G / H            gain +/-5 dB");
                });
            });
    }

    fn draw_spectrum(&self, ui: &mut egui::Ui) {
        let plot = Plot::new("spectrum_plot")
            .x_axis_label("Frequency (MHz)")
            .y_axis_label("Power (dBFS)")
            .include_y(DB_MIN as f64)
            .include_y(DB_MAX as f64)
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
        });
    }

    fn draw_waterfall(&self, ui: &mut egui::Ui) {
        let available = ui.available_size();

        if let Some(texture) = &self.waterfall_texture {
            let (_, rect) = ui.allocate_space(available);
            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            ui.painter()
                .image(texture.id(), rect, uv, egui::Color32::WHITE);

            let painter = ui.painter();
            let steps = 5;
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                let db = DB_MIN + t * (DB_MAX - DB_MIN);
                let y = rect.min.y + (1.0 - t) * rect.height();
                painter.text(
                    egui::pos2(rect.max.x - 6.0, y),
                    egui::Align2::RIGHT_CENTER,
                    format!("{db:.0} dB"),
                    egui::FontId::proportional(11.0),
                    egui::Color32::WHITE,
                );
            }
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("Waiting for IQ\nConnect the Pluto first")
                        .size(18.0)
                        .color(egui::Color32::GRAY),
                );
            });
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        if !self.is_connected || self.is_recording {
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

        self.draw_top_panel(ctx);
        self.draw_bottom_bar(ctx);
        self.draw_control_panel(ctx);

        egui::CentralPanel::default().show(ctx, |ui| match self.view_mode {
            ViewMode::Spectrum => self.draw_spectrum(ui),
            ViewMode::Waterfall => self.draw_waterfall(ui),
        });
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

fn run_worker(
    config: SourceConfig,
    data_tx: Sender<SpectrumFrame>,
    event_tx: Sender<WorkerEvent>,
    cmd_rx: Receiver<WorkerCommand>,
    stop: Arc<AtomicBool>,
) {
    let mut source = match LibiioSource::open(None) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("[worker] connect failed: {error}");
            let _ = event_tx.try_send(WorkerEvent::Fatal(format!("connect failed: {error}")));
            return;
        }
    };

    let mut current_spec = match source.configure(config) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("[worker] configure failed: {error}");
            let _ = event_tx.try_send(WorkerEvent::Fatal(format!("configure failed: {error}")));
            return;
        }
    };

    if let Err(error) = source.start() {
        eprintln!("[worker] start failed: {error}");
        let _ = event_tx.try_send(WorkerEvent::Fatal(format!("start failed: {error}")));
        return;
    }

    eprintln!(
        "[worker] connected: {:.3} MHz, {:.3} MSPS",
        current_spec.center_freq_hz / 1e6,
        current_spec.sample_rate_hz / 1e6
    );
    let _ = event_tx.try_send(WorkerEvent::Connected(current_spec));

    let mut analyzer = SpectrumAnalyzer::new(FFT_SIZE);
    let mut recorder: Option<(PathBuf, SigMfWriter)> = None;

    while !stop.load(Ordering::SeqCst) {
        while let Ok(command) = cmd_rx.try_recv() {
            match command {
                WorkerCommand::SetFrequency(hz) => {
                    if recorder.is_some() {
                        let _ = event_tx.try_send(WorkerEvent::Error(
                            "stop recording before retuning".to_string(),
                        ));
                        continue;
                    }
                    match source.set_frequency(hz) {
                        Ok(()) => {
                            current_spec.center_freq_hz = hz;
                        }
                        Err(error) => {
                            let _ = event_tx
                                .try_send(WorkerEvent::Error(format!("tune failed: {error}")));
                        }
                    }
                }
                WorkerCommand::SetGain(db) => {
                    if let Err(error) = source.set_gain(db) {
                        let _ =
                            event_tx.try_send(WorkerEvent::Error(format!("gain failed: {error}")));
                    }
                }
                WorkerCommand::StartRecording(path) => {
                    if recorder.is_some() {
                        let _ = event_tx.try_send(WorkerEvent::Error(
                            "recording already in progress".to_string(),
                        ));
                        continue;
                    }
                    match SigMfWriter::create(
                        &path,
                        current_spec.sample_rate_hz,
                        current_spec.center_freq_hz,
                        current_spec.full_scale,
                        "Midori GUI capture",
                    ) {
                        Ok(writer) => {
                            recorder = Some((path.clone(), writer));
                            let _ = event_tx.try_send(WorkerEvent::RecordingStarted(path));
                        }
                        Err(error) => {
                            let _ = event_tx.try_send(WorkerEvent::RecordingError(format!(
                                "could not start recording: {error}"
                            )));
                        }
                    }
                }
                WorkerCommand::StopRecording => {
                    if let Some((path, writer)) = recorder.take() {
                        let samples = writer.samples_written();
                        let seconds = samples as f64 / current_spec.sample_rate_hz;
                        match writer.finish() {
                            Ok(()) => {
                                let _ = event_tx.try_send(WorkerEvent::RecordingStopped {
                                    path,
                                    samples,
                                    seconds,
                                });
                            }
                            Err(error) => {
                                let _ = event_tx.try_send(WorkerEvent::RecordingError(format!(
                                    "recording finalise failed: {error}"
                                )));
                            }
                        }
                    }
                }
                WorkerCommand::Stop => {
                    stop.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }

        if stop.load(Ordering::SeqCst) {
            break;
        }

        let block = match source.read_block() {
            Ok(block) => block,
            Err(error) => {
                eprintln!("[worker] receive failed: {error}");
                let _ = event_tx.try_send(WorkerEvent::Fatal(format!("receive failed: {error}")));
                break;
            }
        };
        current_spec = block.spec;

        let mut write_error = None;
        if let Some((_, writer)) = recorder.as_mut()
            && let Err(error) = writer.write_block(&block.samples)
        {
            write_error = Some(error);
        }
        if let Some(error) = write_error {
            let _ = event_tx.try_send(WorkerEvent::RecordingError(format!(
                "write failed: {error}"
            )));
            recorder = None;
        }

        analyzer.process(&block.samples);
        let spectrum_db = fftshift(&analyzer.spectrum_db());
        let frequencies_mhz = frequency_axis(
            block.spec.center_freq_hz,
            block.spec.sample_rate_hz,
            spectrum_db.len(),
        )
        .into_iter()
        .map(|hz| (hz / 1e6) as f32)
        .collect();

        let frame = SpectrumFrame {
            spectrum_db,
            frequencies_mhz,
            center_freq_hz: block.spec.center_freq_hz,
            sample_rate_hz: block.spec.sample_rate_hz,
        };
        let _ = data_tx.try_send(frame);
        analyzer.reset();
    }

    if let Some((_, writer)) = recorder.take() {
        let _ = writer.finish();
    }
    let _ = source.stop();
    eprintln!("[worker] stopped");
}

// ---------------------------------------------------------------------------
// Presentation helpers
// ---------------------------------------------------------------------------

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
fn db_to_color(db: f32) -> egui::Color32 {
    let t = ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0);

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

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
