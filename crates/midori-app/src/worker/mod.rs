//! Worker thread: source ownership, DSP step loop, and plugin plumbing.

use std::any::Any;
use std::panic;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use midori_core::{Channel, DemodMode, IqSource, Session, Sink, SourceConfig, VfoConfig};
use midori_engine::{CwStartGateSink, LiveSession, PluginHandle, RecorderSink, VfoChannel};
use midori_orbit::{GridSquare, PassPredictor, Sgp4Doppler, Tle};
use midori_source_file::WavIqSource;
use midori_source_libiio::LibiioSource;

use crate::app::format::format_demod;
use crate::audio::{AudioOutput, AudioSink};
use crate::sinks::{AutoTuneSink, AutoTuneState, SpectrumSink};
use crate::worker::commands::WorkerCommand;
use crate::worker::events::{EventSender, PluginSource, WorkerEvent};

pub(crate) mod commands;
pub(crate) mod events;
mod plugins;

use plugins::{start_cw_plugin, start_ssdv_plugin, start_sstv_plugin};

// ---------------------------------------------------------------------------
// Worker messages
// ---------------------------------------------------------------------------

pub(crate) struct SpectrumFrame {
    pub(crate) spectrum_db: Vec<f32>,
    pub(crate) frequencies_mhz: Vec<f32>,
    pub(crate) center_freq_hz: f64,
    pub(crate) sample_rate_hz: f64,
}

pub(crate) struct WorkerOptions {
    pub(crate) source: WorkerSource,
    pub(crate) cw_enabled: bool,
    pub(crate) cw_afc: bool,
    pub(crate) cw_tone_hz: f64,
    pub(crate) sstv_enabled: bool,
    pub(crate) sstv_output_dir: PathBuf,
    pub(crate) ssdv_enabled: bool,
    pub(crate) ssdv_crc_inits: Vec<String>,
    pub(crate) ssdv_frame_header_len: usize,
    pub(crate) ssdv_audio_if: bool,
    pub(crate) fft_size: usize,
    pub(crate) doppler: Option<DopplerOptions>,
    pub(crate) initial_vfo_offset_hz: f64,
}

pub(crate) enum WorkerSource {
    Pluto { uri: String },
    File { path: PathBuf, loop_file: bool },
}

pub(crate) struct DopplerOptions {
    pub(crate) tle_path: PathBuf,
    pub(crate) grid: String,
    pub(crate) downlink_hz: f64,
}

fn build_doppler(
    options: &DopplerOptions,
    event_tx: &EventSender,
) -> Option<Box<dyn midori_core::DopplerSource>> {
    let tle = match Tle::from_file(&options.tle_path) {
        Ok(tle) => tle,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "Doppler: could not load TLE {}: {error}",
                options.tle_path.display()
            )));
            return None;
        }
    };
    let grid = match GridSquare::parse(&options.grid) {
        Ok(grid) => grid,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "Doppler: invalid grid '{}': {error}",
                options.grid
            )));
            return None;
        }
    };
    let predictor = match PassPredictor::new(&tle, grid.observer(0.0)) {
        Ok(predictor) => predictor,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "Doppler: SGP4 setup failed: {error}"
            )));
            return None;
        }
    };
    event_tx.best_effort(WorkerEvent::Info(format!(
        "Doppler: tracking {} at {:.6} MHz",
        tle.name
            .clone()
            .unwrap_or_else(|| format!("NORAD {}", tle.norad_id)),
        options.downlink_hz / 1e6
    )));
    Some(Box::new(Sgp4Doppler::new(predictor, options.downlink_hz)))
}

const MAX_PLUGIN_EVENTS_PER_STEP: usize = 64;

/// Worker thread entry point.
///
/// The real loop lives in [`run_worker_inner`]; catching panics here guarantees
/// that the UI receives a terminal `Fatal` event instead of remaining
/// "connected" to a thread that has already died.
pub(crate) fn run_worker(
    config: SourceConfig,
    vfo: VfoConfig,
    options: WorkerOptions,
    data_tx: Sender<SpectrumFrame>,
    event_tx: EventSender,
    cmd_rx: Receiver<WorkerCommand>,
    stop: Arc<AtomicBool>,
) {
    let panic_tx = event_tx.clone();
    let result = panic::catch_unwind(panic::AssertUnwindSafe(move || {
        run_worker_inner(config, vfo, options, data_tx, event_tx, cmd_rx, stop);
    }));
    if let Err(payload) = result {
        let message = panic_message(payload.as_ref());
        eprintln!("[worker] panicked: {message}");
        panic_tx.critical(WorkerEvent::Fatal(format!("worker panicked: {message}")));
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn run_worker_inner(
    config: SourceConfig,
    mut vfo: VfoConfig,
    options: WorkerOptions,
    data_tx: Sender<SpectrumFrame>,
    event_tx: EventSender,
    cmd_rx: Receiver<WorkerCommand>,
    stop: Arc<AtomicBool>,
) {
    let source: Box<dyn IqSource> = match &options.source {
        WorkerSource::Pluto { uri } => match LibiioSource::open(Some(uri)) {
            Ok(source) => Box::new(source),
            Err(error) => {
                eprintln!("[worker] connect failed: {error}");
                event_tx.critical(WorkerEvent::Fatal(format!("connect failed: {error}")));
                return;
            }
        },
        WorkerSource::File { path, loop_file } => match WavIqSource::open(path, *loop_file) {
            Ok(source) => {
                event_tx.best_effort(WorkerEvent::Info(format!(
                    "file: {} | {} ch | {:.3} MSPS | {:.1} s{}",
                    path.display(),
                    source.file_spec().channels,
                    source.file_spec().sample_rate_hz / 1e6,
                    source.file_spec().frames as f64 / source.file_spec().sample_rate_hz,
                    if *loop_file { " | loop" } else { "" }
                )));
                Box::new(source)
            }
            Err(error) => {
                eprintln!("[worker] file open failed: {error}");
                event_tx.critical(WorkerEvent::Fatal(format!("file open failed: {error}")));
                return;
            }
        },
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
            event_tx.best_effort(WorkerEvent::Error(format!("audio unavailable: {error}")));
            (None, None)
        }
    };

    if let Some(producer) = &audio_producer {
        // Audio modes follow the sound card rate. The BPSK channels are data
        // paths: they keep the configured 48 kHz rate regardless of the
        // default output device.
        if !matches!(vfo.demod, DemodMode::Bpsk | DemodMode::BpskIf) {
            vfo.audio_rate_hz = producer.sample_rate_hz() as f64;
        }
    }
    if let Some(output) = &audio_output {
        eprintln!(
            "[worker] audio output: {} Hz, {} channel(s)",
            output.sample_rate_hz(),
            output.channels()
        );
    }

    let mut plugin_handles: Vec<(PluginSource, PluginHandle)> = Vec::new();
    let mut plugin_sinks: Vec<Box<dyn Sink>> = Vec::new();
    if options.cw_enabled
        && let Some((sink, handle)) = start_cw_plugin(&event_tx, options.cw_tone_hz)
    {
        plugin_sinks.push(Box::new(CwStartGateSink::new(sink)));
        plugin_handles.push((PluginSource::Cw, handle));
    }
    if options.sstv_enabled
        && let Some((sink, handle)) =
            start_sstv_plugin(&event_tx, options.sstv_output_dir.as_path())
    {
        plugin_sinks.push(Box::new(sink));
        plugin_handles.push((PluginSource::Sstv, handle));
    }
    if options.ssdv_enabled
        && let Some((sink, handle)) = start_ssdv_plugin(
            &event_tx,
            options.sstv_output_dir.as_path(),
            &options.ssdv_crc_inits,
            options.ssdv_frame_header_len,
            options.ssdv_audio_if,
        )
    {
        plugin_sinks.push(Box::new(sink));
        plugin_handles.push((PluginSource::Ssdv, handle));
    }

    let (recorder_sink, recorder_handle) = RecorderSink::new();
    let auto_tune_state = Arc::new(Mutex::new(AutoTuneState::Idle));
    let mut sinks: Vec<Box<dyn Sink>> = vec![
        Box::new(SpectrumSink::new(options.fft_size, data_tx)),
        Box::new(recorder_sink),
        Box::new(AutoTuneSink::new(Arc::clone(&auto_tune_state))),
    ];
    sinks.extend(plugin_sinks);
    if let Some(producer) = audio_producer {
        // A real-IF data stream is not listening audio.
        if !options.ssdv_audio_if {
            sinks.push(Box::new(AudioSink::new(producer)));
        }
    }

    let mut channel = VfoChannel::new(vfo.clone());
    channel.set_afc(options.cw_afc);
    let mut session = LiveSession::new(config, source, vec![Box::new(channel)], sinks);
    session.set_doppler_source(doppler);
    if options.initial_vfo_offset_hz != 0.0 {
        session.set_vfo_offset(options.initial_vfo_offset_hz);
    }
    let _audio_output = audio_output;

    if let Err(error) = session.start() {
        eprintln!("[worker] start failed: {error}");
        event_tx.critical(WorkerEvent::Fatal(format!("start failed: {error}")));
        return;
    }

    let Some(spec) = session.current_spec() else {
        event_tx.critical(WorkerEvent::Fatal("session has no stream spec".to_string()));
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
    event_tx.critical(WorkerEvent::Connected(spec));

    let mut plugin_events = 0u64;
    let track_doppler = doppler_active;
    let mut last_doppler_event: Option<Instant> = None;
    let mut cw_afc_enabled = options.cw_afc;
    let mut last_vfo_offset = session.vfo_offset_hz();
    let mut last_vfo_event: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        while let Ok(command) = cmd_rx.try_recv() {
            match command {
                WorkerCommand::SetTuning {
                    frequency_hz,
                    vfo_offset_hz,
                } => {
                    if recorder_handle.is_recording() {
                        event_tx.best_effort(WorkerEvent::Error(
                            "stop recording before retuning".to_string(),
                        ));
                        continue;
                    }
                    session.set_vfo_offset(vfo_offset_hz);
                    vfo.offset_hz = vfo_offset_hz;
                    last_vfo_offset = vfo_offset_hz;
                    if let Some(hz) = frequency_hz
                        && let Err(error) = session.set_frequency(hz)
                    {
                        event_tx.best_effort(WorkerEvent::Error(format!("tune failed: {error}")));
                    }
                }
                WorkerCommand::SetGain(db) => {
                    if let Err(error) = session.set_gain(db) {
                        event_tx.best_effort(WorkerEvent::Error(format!("gain failed: {error}")));
                    }
                }
                WorkerCommand::SetDemod(update) => {
                    vfo.demod = update.mode;
                    vfo.bandwidth_hz = update.bandwidth_hz;
                    vfo.audio_bandwidth_hz = update.audio_bandwidth_hz;
                    vfo.deemphasis_us = update.deemphasis_us;
                    vfo.bfo_hz = update.bfo_hz;
                    vfo.channel_rate_hz = update.channel_rate_hz;
                    vfo.offset_hz = session.vfo_offset_hz();
                    match session.reconfigure_vfo(vfo.clone()) {
                        Ok(()) => {
                            event_tx.best_effort(WorkerEvent::Info(format!(
                                "demod {} · RF BW {:.1} kHz",
                                format_demod(update.mode),
                                update.bandwidth_hz / 1e3
                            )));
                        }
                        Err(error) => {
                            event_tx.best_effort(WorkerEvent::Error(format!(
                                "demod update failed: {error}"
                            )));
                        }
                    }
                }
                WorkerCommand::SetCwAfc(enabled) => {
                    cw_afc_enabled = enabled;
                    session.set_afc(enabled);
                    event_tx.best_effort(WorkerEvent::Info(format!(
                        "CW AFC {}",
                        if enabled { "enabled" } else { "disabled" }
                    )));
                }
                WorkerCommand::AutoTune => {
                    if recorder_handle.is_recording() {
                        event_tx.best_effort(WorkerEvent::Error(
                            "stop recording before auto tuning".to_string(),
                        ));
                    } else {
                        *auto_tune_state.lock().unwrap() = AutoTuneState::Scanning;
                        event_tx.best_effort(WorkerEvent::Info(
                            "auto tune: scanning 4 s for a narrowband carrier".to_string(),
                        ));
                    }
                }
                WorkerCommand::StartRecording(path) => {
                    let Some(spec) = session.current_spec() else {
                        event_tx.critical(WorkerEvent::RecordingError(
                            "session has no stream spec".to_string(),
                        ));
                        continue;
                    };
                    match recorder_handle.start(path.clone(), spec) {
                        Ok(()) => {
                            event_tx.critical(WorkerEvent::RecordingStarted(path));
                        }
                        Err(error) => {
                            event_tx.critical(WorkerEvent::RecordingError(format!(
                                "could not start recording: {error}"
                            )));
                        }
                    }
                }
                WorkerCommand::StopRecording => match recorder_handle.stop() {
                    Ok(Some(info)) => {
                        event_tx.critical(WorkerEvent::RecordingStopped {
                            path: info.path,
                            samples: info.samples,
                            seconds: info.seconds,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        event_tx.critical(WorkerEvent::RecordingError(format!(
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

        if session.source_finished() {
            event_tx.critical(WorkerEvent::FileEnded);
            break;
        }

        if let Err(error) = session.step() {
            eprintln!("[worker] receive failed: {error}");
            event_tx.critical(WorkerEvent::Fatal(format!("receive failed: {error}")));
            break;
        }
        for error in session.take_sink_errors() {
            eprintln!("[worker] {error}");
            event_tx.critical(WorkerEvent::Error(error));
        }

        let tune_result = {
            let mut state = auto_tune_state.lock().unwrap();
            if let AutoTuneState::Done(result) = *state {
                *state = AutoTuneState::Idle;
                Some(result)
            } else {
                None
            }
        };
        if let Some(result) = tune_result {
            if let Some(offset) = result
                && !recorder_handle.is_recording()
            {
                session.set_vfo_offset(offset);
                vfo.offset_hz = offset;
                last_vfo_offset = offset;
                last_vfo_event = Some(Instant::now());
                event_tx.best_effort(WorkerEvent::VfoOffset { offset_hz: offset });
            }
            event_tx.critical(WorkerEvent::AutoTuneDone { offset_hz: result });
        }

        let offset = session.vfo_offset_hz();
        let due = (offset - last_vfo_offset).abs() > 0.5
            || (cw_afc_enabled
                && last_vfo_event.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)));
        if due {
            last_vfo_offset = offset;
            last_vfo_event = Some(Instant::now());
            vfo.offset_hz = offset;
            event_tx.best_effort(WorkerEvent::VfoOffset { offset_hz: offset });
        }

        if track_doppler {
            let now = Instant::now();
            let due = last_doppler_event
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500));
            if due {
                last_doppler_event = Some(now);
                event_tx.best_effort(WorkerEvent::Doppler {
                    offset_hz: session.doppler_hz(),
                    rate_hz_s: session.doppler_rate_hz_s(),
                });
            }
        }

        if let Some(error) = recorder_handle.take_error() {
            event_tx.critical(WorkerEvent::RecordingError(error));
        }

        event_tx.report_drops();

        let mut forwarded_events = 0usize;
        'plugins: for (source, handle) in &plugin_handles {
            loop {
                if forwarded_events >= MAX_PLUGIN_EVENTS_PER_STEP {
                    // Leave the remaining events queued so this step can still
                    // run the SDR session; the next iteration picks them up.
                    break 'plugins;
                }
                match handle.try_recv() {
                    Ok(Some(event)) => {
                        if plugin_events == 0 {
                            eprintln!("[plugin] first event");
                        }
                        plugin_events += 1;
                        forwarded_events += 1;
                        event_tx.best_effort(WorkerEvent::Plugin {
                            source: *source,
                            event,
                        });
                    }
                    Ok(None) => break,
                    Err(error) => {
                        event_tx
                            .best_effort(WorkerEvent::Error(format!("plugin {source:?}: {error}")));
                        break;
                    }
                }
            }
        }
    }

    for (source, handle) in &plugin_handles {
        match handle.stop() {
            Ok(events) => {
                for event in events {
                    // Final decoder output (last CW text, completed images) is
                    // shown while the user is disconnecting, so it must use the
                    // non-lossy control path.
                    event_tx.critical(WorkerEvent::Plugin {
                        source: *source,
                        event,
                    });
                }
            }
            Err(error) => {
                event_tx.critical(WorkerEvent::Error(format!("plugin {source:?}: {error}")));
            }
        }
    }
    if let Ok(Some(info)) = recorder_handle.stop() {
        event_tx.critical(WorkerEvent::RecordingStopped {
            path: info.path,
            samples: info.samples,
            seconds: info.seconds,
        });
    }
    if let Err(error) = session.stop() {
        event_tx.critical(WorkerEvent::Error(format!("session stop: {error:#}")));
    }
    eprintln!("[worker] stopped");
}
