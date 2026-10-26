//! Sidecar plugin SDK for Midori.
//!
//! A decoder sidecar implements [`Plugin`] and calls [`run`]. The SDK owns the
//! protocol loop: it sends `hello`, waits for `configure`, forwards `stream`
//! and data frames to the plugin, and flushes on `shutdown` or stdin EOF. A
//! plugin only deals with its own decoder state and emits events through an
//! [`Emitter`].
//!
//! The JSON and binary wire format is defined by [`midori_plugin`]; the SDK is
//! a convenience layer, not a protocol change. Plugins in other languages can
//! keep implementing the framing directly.
//!
//! # Semantics
//!
//! - `hello` is written before anything else.
//! - A successful [`Plugin::configure`] is answered with `ready`. If configure
//!   fails, the SDK writes `{"type":"error","message":"..."}` and keeps
//!   waiting for another `configure` (or EOF/shutdown) instead of exiting.
//! - Every `stream` message is passed to [`Plugin::on_stream`]. Plugins must
//!   treat repeated messages as metadata-only updates and reset decoder state
//!   only when `kind` or `sample_rate_hz` changed.
//! - Data frames that arrive before the first successful configure are ignored
//!   and reported once per frame kind with a non-fatal status warning.
//! - An error returned by a stream or frame handler is sent as a non-fatal
//!   `error` event and the loop continues.
//! - `shutdown` and stdin EOF both call [`Plugin::on_shutdown`] exactly once,
//!   then return `Ok(())`. Errors from the shutdown handler are reported as a
//!   non-fatal event; shutdown itself still completes successfully.
//! - [`Plugin::on_ready`] runs after `ready` is written, letting a plugin emit
//!   status text that depends on the accepted configuration.

use std::io::{BufReader, BufWriter, Read, Write};

use anyhow::Result;
use midori_plugin::wire::{WireMessage, read_message, write_json};
use serde_json::Value;

pub use midori_core::{FrameMeta, Sample, StreamKind};
pub use midori_plugin::{
    ControlMessage, DisplayUpdate, ImageFormat, PROTOCOL_VERSION, PluginEvent, StatusLevel,
    StreamInfo, stream_kind_name,
};

/// Timing metadata for one data frame.
///
/// `timestamp_ns` is the frame's exact UTC nanosecond timestamp, or `None`
/// when the host did not know it (the wire sentinel is stripped by the SDK).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameInfo {
    pub seq: u64,
    pub timestamp_ns: Option<u64>,
}

/// A decoder sidecar.
///
/// Only [`Plugin::name`], [`Plugin::version`] and [`Plugin::configure`] are
/// required. Override the `on_*` methods for the frame kinds declared by the
/// plugin's manifest.
pub trait Plugin {
    /// Manifest name, sent in `hello` (for example `"cw"`).
    fn name(&self) -> &str;

    /// Plugin version, sent in `hello` (normally `env!("CARGO_PKG_VERSION")`).
    fn version(&self) -> &str;

    /// Apply the host's configuration parameters.
    ///
    /// Returning `Err` rejects the configuration: the SDK sends an `error`
    /// control message and waits for the host to try again.
    fn configure(&mut self, params: &Value) -> Result<()>;

    /// Called after `ready` has been sent for a successful configuration.
    ///
    /// Useful for status messages that describe the accepted parameters.
    fn on_ready(&mut self, _out: &mut Emitter<'_>) -> Result<()> {
        Ok(())
    }

    /// Called for every `stream` control message, including repeated ones.
    fn on_stream(&mut self, _info: &StreamInfo, _out: &mut Emitter<'_>) -> Result<()> {
        Ok(())
    }

    /// Called for every mono audio frame (`kind = 2`).
    fn on_audio(
        &mut self,
        _meta: &FrameInfo,
        _samples: &[f32],
        _out: &mut Emitter<'_>,
    ) -> Result<()> {
        Ok(())
    }

    /// Called for every complex IQ frame (`kind = 5`).
    fn on_iq(
        &mut self,
        _meta: &FrameInfo,
        _samples: &[Sample],
        _out: &mut Emitter<'_>,
    ) -> Result<()> {
        Ok(())
    }

    /// Called for every raw/framed byte frame (`kind = 4`).
    fn on_bytes(&mut self, _meta: &FrameInfo, _bytes: &[u8], _out: &mut Emitter<'_>) -> Result<()> {
        Ok(())
    }

    /// Called for every soft-bit / LLR frame (`kind = 3`).
    fn on_soft_bits(
        &mut self,
        _meta: &FrameInfo,
        _bits: &[u8],
        _out: &mut Emitter<'_>,
    ) -> Result<()> {
        Ok(())
    }

    /// Flush buffered results on `shutdown` or stdin EOF. Called exactly once.
    fn on_shutdown(&mut self, _out: &mut Emitter<'_>) -> Result<()> {
        Ok(())
    }
}

/// Event sink handed to plugin callbacks.
///
/// All methods serialise one JSON frame and flush the writer.
pub struct Emitter<'a> {
    writer: &'a mut dyn Write,
}

impl<'a> Emitter<'a> {
    pub fn new(writer: &'a mut dyn Write) -> Self {
        Self { writer }
    }

    /// Emit an arbitrary [`PluginEvent`].
    pub fn event(&mut self, event: PluginEvent) -> Result<()> {
        let value = serde_json::to_value(event)?;
        write_json(&mut self.writer, &value)
    }

    /// Emit a status line.
    pub fn status(&mut self, level: StatusLevel, message: impl Into<String>) -> Result<()> {
        self.event(PluginEvent::Status {
            level,
            message: message.into(),
        })
    }

    /// Emit an error event. Set `fatal` when the plugin cannot continue.
    pub fn error(&mut self, message: impl Into<String>, fatal: bool) -> Result<()> {
        self.event(PluginEvent::Error {
            message: message.into(),
            fatal,
        })
    }

    /// Emit a text display update.
    pub fn text(&mut self, title: impl Into<String>, lines: Vec<String>) -> Result<()> {
        self.event(PluginEvent::Display {
            update: DisplayUpdate::Text {
                title: title.into(),
                lines,
            },
        })
    }

    /// Emit an image display update. `format` is normally [`ImageFormat::Png`];
    /// `width`/`height` are required for RGBA payloads.
    pub fn image(
        &mut self,
        title: impl Into<String>,
        format: ImageFormat,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    ) -> Result<()> {
        self.event(PluginEvent::Display {
            update: DisplayUpdate::Image {
                title: title.into(),
                format,
                width,
                height,
                bytes,
            },
        })
    }

    /// Emit a decoded packet.
    pub fn packet(&mut self, meta: FrameMeta, data: Vec<u8>) -> Result<()> {
        self.event(PluginEvent::Packet { meta, data })
    }

    /// Emit a display progress update.
    pub fn progress(&mut self, title: impl Into<String>, fraction: f32) -> Result<()> {
        self.event(PluginEvent::Display {
            update: DisplayUpdate::Progress {
                title: title.into(),
                fraction,
            },
        })
    }
}

/// Run a plugin on the process's stdin/stdout.
pub fn run<P: Plugin>(plugin: P) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());
    let writer = BufWriter::new(stdout.lock());
    run_with(plugin, reader, writer)
}

/// Run a plugin over arbitrary reader/writer handles.
///
/// This is the testable entry point used by [`run`].
pub fn run_with<P: Plugin, R: Read, W: Write>(
    mut plugin: P,
    mut reader: R,
    mut writer: W,
) -> Result<()> {
    write_control(
        &mut writer,
        &ControlMessage::Hello {
            protocol: PROTOCOL_VERSION,
            name: plugin.name().to_string(),
            version: plugin.version().to_string(),
        },
    )?;

    let mut configured = false;
    let mut warned_before_configure = [false; 4];

    loop {
        let message = match read_message(&mut reader) {
            // A closed pipe and a malformed frame both end the run; the
            // shutdown hook still gets a chance to flush.
            Ok(message) => message,
            Err(_) => break,
        };

        match message {
            WireMessage::Json(value) => {
                match serde_json::from_value::<ControlMessage>(value) {
                    Ok(ControlMessage::Configure { params }) => match plugin.configure(&params) {
                        Ok(()) => {
                            configured = true;
                            write_control(&mut writer, &ControlMessage::Ready)?;
                            let mut out = Emitter::new(&mut writer);
                            if let Err(error) = plugin.on_ready(&mut out) {
                                out.error(format!("{error:#}"), false)?;
                            }
                        }
                        Err(error) => {
                            write_control(
                                &mut writer,
                                &ControlMessage::Error {
                                    message: format!("{error:#}"),
                                },
                            )?;
                        }
                    },
                    Ok(ControlMessage::Stream(info)) => {
                        let mut out = Emitter::new(&mut writer);
                        if let Err(error) = plugin.on_stream(&info, &mut out) {
                            out.error(format!("{error:#}"), false)?;
                        }
                    }
                    Ok(ControlMessage::Shutdown) => break,
                    // Stray control messages (including plug-in events echoed
                    // back by a test harness) are not meaningful here.
                    Ok(_) | Err(_) => {}
                }
            }
            WireMessage::Audio {
                seq,
                timestamp_ns,
                samples,
            } => {
                if !configured {
                    warn_before_configure(
                        &mut writer,
                        &mut warned_before_configure,
                        StreamKind::Audio,
                    )?;
                    continue;
                }
                let meta = FrameInfo { seq, timestamp_ns };
                let mut out = Emitter::new(&mut writer);
                if let Err(error) = plugin.on_audio(&meta, &samples, &mut out) {
                    out.error(format!("{error:#}"), false)?;
                }
            }
            WireMessage::Iq {
                seq,
                timestamp_ns,
                samples,
            } => {
                if !configured {
                    warn_before_configure(
                        &mut writer,
                        &mut warned_before_configure,
                        StreamKind::Iq,
                    )?;
                    continue;
                }
                let meta = FrameInfo { seq, timestamp_ns };
                let mut out = Emitter::new(&mut writer);
                if let Err(error) = plugin.on_iq(&meta, &samples, &mut out) {
                    out.error(format!("{error:#}"), false)?;
                }
            }
            WireMessage::Bytes {
                seq,
                timestamp_ns,
                bytes,
            } => {
                if !configured {
                    warn_before_configure(
                        &mut writer,
                        &mut warned_before_configure,
                        StreamKind::Bytes,
                    )?;
                    continue;
                }
                let meta = FrameInfo { seq, timestamp_ns };
                let mut out = Emitter::new(&mut writer);
                if let Err(error) = plugin.on_bytes(&meta, &bytes, &mut out) {
                    out.error(format!("{error:#}"), false)?;
                }
            }
            WireMessage::SoftBits {
                seq,
                timestamp_ns,
                bits,
            } => {
                if !configured {
                    warn_before_configure(
                        &mut writer,
                        &mut warned_before_configure,
                        StreamKind::SoftBits,
                    )?;
                    continue;
                }
                let meta = FrameInfo { seq, timestamp_ns };
                let mut out = Emitter::new(&mut writer);
                if let Err(error) = plugin.on_soft_bits(&meta, &bits, &mut out) {
                    out.error(format!("{error:#}"), false)?;
                }
            }
        }
    }

    let mut out = Emitter::new(&mut writer);
    if let Err(error) = plugin.on_shutdown(&mut out) {
        out.error(format!("{error:#}"), false)?;
    }
    Ok(())
}

/// Default capture directory: `captures/` under the current working directory.
///
/// Shared by the reference SSTV and SSDV plugins. Returns `None` when the
/// current directory cannot be read.
pub fn default_output_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::current_dir().ok()?.join("captures");
    Some(dir)
}

fn write_control(mut writer: &mut dyn Write, message: &ControlMessage) -> Result<()> {
    let value = serde_json::to_value(message)?;
    write_json(&mut writer, &value)
}

fn warn_before_configure(
    writer: &mut dyn Write,
    warned: &mut [bool; 4],
    kind: StreamKind,
) -> Result<()> {
    let slot = match kind {
        StreamKind::Iq => 0,
        StreamKind::Audio => 1,
        StreamKind::SoftBits => 2,
        StreamKind::Bytes => 3,
    };
    if warned[slot] {
        return Ok(());
    }
    warned[slot] = true;
    let mut out = Emitter::new(writer);
    out.status(
        StatusLevel::Warning,
        format!("ignoring {} frame before configure", stream_kind_name(kind)),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    use midori_plugin::wire::{read_message, write_audio, write_json};
    use midori_plugin::{DisplayUpdate, ImageFormat, PluginEvent};

    use super::*;

    #[derive(Default)]
    struct State {
        configured: Vec<Value>,
        streams: Vec<StreamInfo>,
        audio: Vec<(u64, Option<u64>, Vec<f32>)>,
        shutdowns: usize,
        fail_audio_once: bool,
    }

    struct Recorder {
        state: Arc<Mutex<State>>,
    }

    impl Recorder {
        fn new(state: &Arc<Mutex<State>>) -> Self {
            Self {
                state: Arc::clone(state),
            }
        }
    }

    impl Plugin for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }

        fn version(&self) -> &str {
            "9.9.9"
        }

        fn configure(&mut self, params: &Value) -> Result<()> {
            if params.get("fail").and_then(Value::as_bool).unwrap_or(false) {
                anyhow::bail!("bad params");
            }
            self.state.lock().unwrap().configured.push(params.clone());
            Ok(())
        }

        fn on_stream(&mut self, info: &StreamInfo, _out: &mut Emitter<'_>) -> Result<()> {
            self.state.lock().unwrap().streams.push(*info);
            Ok(())
        }

        fn on_audio(
            &mut self,
            meta: &FrameInfo,
            samples: &[f32],
            _out: &mut Emitter<'_>,
        ) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state
                .audio
                .push((meta.seq, meta.timestamp_ns, samples.to_vec()));
            if state.fail_audio_once {
                state.fail_audio_once = false;
                anyhow::bail!("audio boom");
            }
            Ok(())
        }

        fn on_shutdown(&mut self, out: &mut Emitter<'_>) -> Result<()> {
            self.state.lock().unwrap().shutdowns += 1;
            out.status(StatusLevel::Info, "flushed")?;
            Ok(())
        }
    }

    fn push_control(buffer: &mut Vec<u8>, message: ControlMessage) {
        write_json(buffer, &serde_json::to_value(message).unwrap()).unwrap();
    }

    fn input(messages: &[ControlMessage]) -> Cursor<Vec<u8>> {
        let mut buffer = Vec::new();
        for message in messages {
            push_control(&mut buffer, message.clone());
        }
        Cursor::new(buffer)
    }

    fn output_controls(buffer: &[u8]) -> Vec<ControlMessage> {
        let mut reader = Cursor::new(buffer);
        let mut messages = Vec::new();
        while let Ok(WireMessage::Json(value)) = read_message(&mut reader) {
            if let Ok(message) = serde_json::from_value::<ControlMessage>(value.clone()) {
                messages.push(message);
            }
        }
        messages
    }

    fn output_events(buffer: &[u8]) -> Vec<PluginEvent> {
        let mut reader = Cursor::new(buffer);
        let mut events = Vec::new();
        while let Ok(WireMessage::Json(value)) = read_message(&mut reader) {
            if let Ok(event) = serde_json::from_value::<PluginEvent>(value) {
                events.push(event);
            }
        }
        events
    }

    fn stream_info() -> StreamInfo {
        StreamInfo {
            kind: StreamKind::Audio,
            sample_rate_hz: 48_000.0,
            channels: 1,
            center_freq_hz: 145_800_000.0,
            nominal_freq_hz: 145_800_000.0,
            bandwidth_hz: 3_000.0,
            doppler_hz: 1.5,
            doppler_rate_hz_s: 0.0,
            vfo_id: Some(7),
        }
    }

    #[test]
    fn handshake_sends_hello_then_flushes_on_eof() {
        let state = Arc::new(Mutex::new(State::default()));
        let mut output = Vec::new();
        run_with(Recorder::new(&state), Cursor::new(Vec::new()), &mut output).unwrap();

        let controls = output_controls(&output);
        assert_eq!(
            controls.first(),
            Some(&ControlMessage::Hello {
                protocol: PROTOCOL_VERSION,
                name: "recorder".to_string(),
                version: "9.9.9".to_string(),
            })
        );
        assert_eq!(state.lock().unwrap().shutdowns, 1);
        assert!(matches!(
            output_events(&output).as_slice(),
            [PluginEvent::Status { message, .. }] if message == "flushed"
        ));
    }

    #[test]
    fn configure_error_is_reported_and_a_retry_succeeds() {
        let state = Arc::new(Mutex::new(State::default()));
        let mut output = Vec::new();
        let mut input_buffer = Vec::new();
        push_control(
            &mut input_buffer,
            ControlMessage::Configure {
                params: serde_json::json!({"fail": true}),
            },
        );
        push_control(
            &mut input_buffer,
            ControlMessage::Configure {
                params: serde_json::json!({"tone_hz": 700.0}),
            },
        );
        push_control(&mut input_buffer, ControlMessage::Shutdown);

        run_with(
            Recorder::new(&state),
            Cursor::new(input_buffer),
            &mut output,
        )
        .unwrap();

        let controls = output_controls(&output);
        assert!(matches!(&controls[0], ControlMessage::Hello { .. }));
        assert_eq!(
            controls[1],
            ControlMessage::Error {
                message: "bad params".to_string()
            }
        );
        assert_eq!(controls[2], ControlMessage::Ready);
        assert_eq!(state.lock().unwrap().configured.len(), 1);
        assert_eq!(state.lock().unwrap().shutdowns, 1);
    }

    #[test]
    fn every_stream_message_is_delivered() {
        let state = Arc::new(Mutex::new(State::default()));
        let mut output = Vec::new();
        let info = stream_info();
        let input = input(&[
            ControlMessage::Configure {
                params: serde_json::json!({}),
            },
            ControlMessage::Stream(info),
            ControlMessage::Stream(info),
            ControlMessage::Shutdown,
        ]);

        run_with(Recorder::new(&state), input, &mut output).unwrap();
        assert_eq!(state.lock().unwrap().streams, vec![info, info]);
    }

    #[test]
    fn audio_frames_reach_the_handler_with_exact_timestamps() {
        let state = Arc::new(Mutex::new(State::default()));
        let mut input_buffer = Vec::new();
        push_control(
            &mut input_buffer,
            ControlMessage::Configure {
                params: serde_json::json!({}),
            },
        );
        write_audio(
            &mut input_buffer,
            11,
            Some(1_792_466_014_123_456_789),
            &[0.25, -0.5],
        )
        .unwrap();
        push_control(&mut input_buffer, ControlMessage::Shutdown);

        let mut output = Vec::new();
        run_with(
            Recorder::new(&state),
            Cursor::new(input_buffer),
            &mut output,
        )
        .unwrap();

        assert_eq!(
            state.lock().unwrap().audio,
            vec![(11, Some(1_792_466_014_123_456_789), vec![0.25, -0.5])]
        );
    }

    #[test]
    fn frames_before_configure_are_ignored_with_one_warning() {
        let state = Arc::new(Mutex::new(State::default()));
        let mut input_buffer = Vec::new();
        write_audio(&mut input_buffer, 1, None, &[0.1]).unwrap();
        write_audio(&mut input_buffer, 2, None, &[0.2]).unwrap();
        push_control(
            &mut input_buffer,
            ControlMessage::Configure {
                params: serde_json::json!({}),
            },
        );
        push_control(&mut input_buffer, ControlMessage::Shutdown);

        let mut output = Vec::new();
        run_with(
            Recorder::new(&state),
            Cursor::new(input_buffer),
            &mut output,
        )
        .unwrap();

        assert!(state.lock().unwrap().audio.is_empty());
        let events = output_events(&output);
        let warnings = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    PluginEvent::Status {
                        level: StatusLevel::Warning,
                        message,
                    } if message.contains("before configure")
                )
            })
            .count();
        assert_eq!(warnings, 1);
    }

    #[test]
    fn frame_handler_errors_are_non_fatal_and_the_loop_continues() {
        let state = Arc::new(Mutex::new(State::default()));
        state.lock().unwrap().fail_audio_once = true;
        let mut input_buffer = Vec::new();
        push_control(
            &mut input_buffer,
            ControlMessage::Configure {
                params: serde_json::json!({}),
            },
        );
        write_audio(&mut input_buffer, 1, None, &[0.1]).unwrap();
        write_audio(&mut input_buffer, 2, None, &[0.2]).unwrap();
        push_control(&mut input_buffer, ControlMessage::Shutdown);

        let mut output = Vec::new();
        run_with(
            Recorder::new(&state),
            Cursor::new(input_buffer),
            &mut output,
        )
        .unwrap();

        assert_eq!(state.lock().unwrap().audio.len(), 2);
        let events = output_events(&output);
        assert!(events.iter().any(|event| matches!(
            event,
            PluginEvent::Error {
                message,
                fatal: false
            } if message.contains("audio boom")
        )));
    }

    #[test]
    fn shutdown_flush_runs_exactly_once_on_eof_and_shutdown() {
        for use_shutdown in [false, true] {
            let state = Arc::new(Mutex::new(State::default()));
            let mut input_buffer = Vec::new();
            push_control(
                &mut input_buffer,
                ControlMessage::Configure {
                    params: serde_json::json!({}),
                },
            );
            if use_shutdown {
                push_control(&mut input_buffer, ControlMessage::Shutdown);
            }
            let mut output = Vec::new();
            run_with(
                Recorder::new(&state),
                Cursor::new(input_buffer),
                &mut output,
            )
            .unwrap();

            assert_eq!(
                state.lock().unwrap().shutdowns,
                1,
                "shutdown hook count (explicit shutdown: {use_shutdown})"
            );
            let flushes = output_events(&output)
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        PluginEvent::Status { message, .. } if message == "flushed"
                    )
                })
                .count();
            assert_eq!(flushes, 1);
        }
    }

    #[test]
    fn emitter_helpers_match_plugin_event_json() {
        let mut buffer = Vec::new();
        {
            let mut out = Emitter::new(&mut buffer);
            out.text("CW", vec!["CQ".to_string()]).unwrap();
            out.image("SSTV", ImageFormat::Png, 640, 496, vec![1, 2, 3])
                .unwrap();
            out.progress("image", 0.5).unwrap();
            out.error("bad", false).unwrap();
        }
        let events = output_events(&buffer);
        assert!(matches!(
            &events[0],
            PluginEvent::Display {
                update: DisplayUpdate::Text { title, lines }
            } if title == "CW" && lines == &vec!["CQ".to_string()]
        ));
        assert!(matches!(
            &events[1],
            PluginEvent::Display {
                update: DisplayUpdate::Image {
                    width: 640,
                    height: 496,
                    ..
                }
            }
        ));
        assert!(matches!(
            &events[2],
            PluginEvent::Display {
                update: DisplayUpdate::Progress { fraction, .. }
            } if (*fraction - 0.5).abs() < f32::EPSILON
        ));
        assert!(matches!(
            &events[3],
            PluginEvent::Error { fatal: false, .. }
        ));
    }
}
