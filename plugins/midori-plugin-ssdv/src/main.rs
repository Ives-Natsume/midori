//! SSDV sidecar: 9k6 BPSK IQ or raw packets in, PNG image events out.

use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use midori_plugin::wire::{WireMessage, read_message, write_json};
use midori_plugin::{DisplayUpdate, ImageFormat, PROTOCOL_VERSION, PluginEvent, StatusLevel};
use midori_plugin_ssdv::{
    Bpsk9600AudioReceiver, Bpsk9600Receiver, DEFAULT_CRC_INIT, SessionEvent, SsdvSession,
    parse_crc_inits,
};
use serde_json::{Value, json};

enum Pipeline {
    Iq(Bpsk9600Receiver),
    AudioIf(Bpsk9600AudioReceiver),
    Bytes(SsdvSession),
}

impl Pipeline {
    fn new(
        input: &str,
        output_dir: Option<PathBuf>,
        crc_inits: Vec<u32>,
        frame_header_len: usize,
    ) -> Result<Self> {
        if input == "bytes" || input == "frames" {
            Ok(Self::Bytes(SsdvSession::with_config(
                output_dir,
                crc_inits,
                frame_header_len,
            )))
        } else if input == "audio-if" || input == "audio" {
            Ok(Self::AudioIf(Bpsk9600AudioReceiver::with_config(
                output_dir,
                crc_inits,
                frame_header_len,
            )?))
        } else {
            Ok(Self::Iq(Bpsk9600Receiver::with_config(
                output_dir,
                crc_inits,
                frame_header_len,
            )?))
        }
    }

    fn process_iq(&mut self, samples: &[midori_core::Sample]) -> Result<Vec<SessionEvent>> {
        match self {
            Self::Iq(receiver) => Ok(receiver.process_iq(samples)),
            Self::AudioIf(_) | Self::Bytes(_) => Ok(Vec::new()),
        }
    }

    fn process_audio(&mut self, samples: &[f32]) -> Result<Vec<SessionEvent>> {
        match self {
            Self::AudioIf(receiver) => Ok(receiver.process_mono(samples)),
            Self::Iq(_) | Self::Bytes(_) => Ok(Vec::new()),
        }
    }

    fn process_bytes(&mut self, bytes: &[u8]) -> Result<Vec<SessionEvent>> {
        match self {
            Self::Iq(_) | Self::AudioIf(_) => Ok(Vec::new()),
            Self::Bytes(session) => session.process_bytes(bytes),
        }
    }

    fn flush(&mut self) -> Vec<SessionEvent> {
        match self {
            Self::Iq(receiver) => receiver.flush(),
            Self::AudioIf(receiver) => receiver.flush(),
            Self::Bytes(_) => Vec::new(),
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Iq(receiver) => format!(
                "{} CCSDS frames, {} RS bytes corrected",
                receiver.frame_count(),
                receiver.rs_corrected_bytes()
            ),
            Self::AudioIf(receiver) => format!(
                "{} CCSDS frames, {} RS bytes corrected (12 kHz IF)",
                receiver.frame_count(),
                receiver.rs_corrected_bytes()
            ),
            Self::Bytes(session) => format!(
                "{} packet rejects, {} non-SSDV frames",
                session.crc_errors(),
                session.ignored_frames()
            ),
        }
    }
}

fn main() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    write_json(
        &mut writer,
        &json!({
            "type": "hello",
            "protocol": PROTOCOL_VERSION,
            "name": "ssdv",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    )?;

    let mut output_dir = default_output_dir();
    let mut input = "iq".to_string();
    let mut crc_inits = vec![DEFAULT_CRC_INIT];
    let mut frame_header_len = 5usize;
    let mut pipeline = Pipeline::new(
        &input,
        output_dir.clone(),
        crc_inits.clone(),
        frame_header_len,
    )?;
    let mut configured = false;
    let mut iq_warned = false;
    let mut stream_rate_ok = true;

    loop {
        let message = match read_message(&mut reader) {
            Ok(message) => message,
            Err(_) => break,
        };

        match message {
            WireMessage::Json(value) => match value.get("type").and_then(Value::as_str) {
                Some("configure") => {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(dir) = params.get("output_dir").and_then(Value::as_str) {
                        output_dir = if dir.is_empty() {
                            None
                        } else {
                            Some(PathBuf::from(dir))
                        };
                    }
                    if let Some(mode) = params.get("input").and_then(Value::as_str) {
                        input = mode.to_string();
                    }
                    if let Some(values) = params.get("crc_inits").and_then(Value::as_array) {
                        let values: Vec<String> = values
                            .iter()
                            .map(|value| match value {
                                Value::String(text) => text.clone(),
                                other => other.to_string(),
                            })
                            .collect();
                        crc_inits = parse_crc_inits(&values);
                    }
                    if let Some(len) = params.get("frame_header_len").and_then(Value::as_u64) {
                        frame_header_len = len as usize;
                    }
                    pipeline = Pipeline::new(
                        &input,
                        output_dir.clone(),
                        crc_inits.clone(),
                        frame_header_len,
                    )?;
                    if let Some(ms) = params.get("preview_interval_ms").and_then(Value::as_u64)
                        && let Pipeline::Bytes(session) = &mut pipeline
                    {
                        session.set_preview_interval(Duration::from_millis(ms.max(50)));
                    }
                    configured = true;
                    stream_rate_ok = true;
                    iq_warned = false;
                    write_json(&mut writer, &json!({ "type": "ready" }))?;
                    let crc_text: Vec<String> = crc_inits
                        .iter()
                        .map(|init| format!("0x{init:08X}"))
                        .collect();
                    send_status(
                        &mut writer,
                        StatusLevel::Info,
                        &format!(
                            "SSDV CRC init [{}], frame header {} bytes",
                            crc_text.join(", "),
                            frame_header_len
                        ),
                    )?;
                }
                Some("stream") => {
                    if let Some(rate) = value.get("sample_rate_hz").and_then(Value::as_f64) {
                        stream_rate_ok = (rate - 48_000.0).abs() <= 1.0;
                        if !stream_rate_ok && !iq_warned {
                            iq_warned = true;
                            send_status(
                                &mut writer,
                                StatusLevel::Warning,
                                &format!(
                                    "SSDV expects 48 kHz; got {rate:.0} Hz. \
                                     Use the BPSK demod, BPSK IF for mono 12 kHz \
                                     audio, and a 48 kHz channel rate."
                                ),
                            )?;
                        }
                    }
                    if let Some(kind) = value.get("kind").and_then(Value::as_str)
                        && !iq_warned
                    {
                        let mismatch = (input == "iq" && kind != "iq")
                            || ((input == "audio-if" || input == "audio") && kind != "audio");
                        if mismatch {
                            iq_warned = true;
                            send_status(
                                &mut writer,
                                StatusLevel::Warning,
                                &format!("SSDV sidecar expects {input}, received {kind}"),
                            )?;
                        }
                    }
                }
                Some("shutdown") => {
                    for event in pipeline.flush() {
                        send_event(&mut writer, event)?;
                    }
                    break;
                }
                _ => {}
            },
            WireMessage::Iq { samples, .. } => {
                if !configured || !stream_rate_ok {
                    continue;
                }
                for event in pipeline.process_iq(&samples)? {
                    send_event(&mut writer, event)?;
                }
            }
            WireMessage::Bytes { bytes, .. } => {
                if !configured {
                    continue;
                }
                match pipeline.process_bytes(&bytes) {
                    Ok(events) => {
                        for event in events {
                            send_event(&mut writer, event)?;
                        }
                    }
                    Err(error) => send_error(&mut writer, &format!("SSDV input: {error}"))?,
                }
            }
            WireMessage::Audio { samples, .. } => {
                if !configured || !stream_rate_ok {
                    continue;
                }
                for event in pipeline.process_audio(&samples)? {
                    send_event(&mut writer, event)?;
                }
            }
            WireMessage::SoftBits { .. } => {}
        }
    }

    let summary = pipeline.summary();
    if !summary.is_empty() {
        eprintln!("[ssdv] {summary}");
    }
    Ok(())
}

fn default_output_dir() -> Option<PathBuf> {
    let dir = std::env::current_dir().ok()?.join("captures");
    Some(dir)
}

fn send_event<W: Write>(writer: &mut W, event: SessionEvent) -> Result<()> {
    match event {
        SessionEvent::Status { level, message } => send_status(writer, level, &message),
        SessionEvent::Progress {
            title,
            received,
            total_hint,
            missing,
            fraction,
            ..
        } => {
            let missing = if missing > 0 {
                format!(", {missing} missing")
            } else {
                String::new()
            };
            let title = if total_hint == 0 {
                format!("{title} · {received} packet(s), waiting for more")
            } else {
                format!("{title} · {received}/{total_hint} estimated packets{missing}")
            };
            let update = DisplayUpdate::Progress { title, fraction };
            write_json(
                writer,
                &serde_json::to_value(PluginEvent::Display { update })?,
            )
        }
        SessionEvent::Image(image) => {
            let location = image
                .png_path
                .as_ref()
                .or(image.jpeg_path.as_ref())
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "not saved".to_string());
            let state = if image.complete && image.missing_packets == 0 {
                "complete"
            } else if image.complete {
                "partial"
            } else {
                "preview"
            };
            send_status(
                writer,
                StatusLevel::Info,
                &format!(
                    "SSDV ID {} {}x{} q{}: {} packets, {} missing, {state} -> {location}",
                    image.image_id,
                    image.width,
                    image.height,
                    image.quality,
                    image.received_packets,
                    image.missing_packets
                ),
            )?;
            let suffix = if image.complete && image.missing_packets == 0 {
                " (complete)".to_string()
            } else if image.complete {
                format!(" (partial, {} missing)", image.missing_packets)
            } else {
                " (preview)".to_string()
            };
            let update = DisplayUpdate::Image {
                title: format!(
                    "SSDV ID {} {}x{}{}",
                    image.image_id, image.width, image.height, suffix
                ),
                format: ImageFormat::Png,
                width: image.width.into(),
                height: image.height.into(),
                bytes: image.png,
            };
            write_json(
                writer,
                &serde_json::to_value(PluginEvent::Display { update })?,
            )
        }
    }
}

fn send_status<W: Write>(writer: &mut W, level: StatusLevel, message: &str) -> Result<()> {
    let event = PluginEvent::Status {
        level,
        message: message.to_string(),
    };
    write_json(writer, &serde_json::to_value(event)?)
}

fn send_error<W: Write>(writer: &mut W, message: &str) -> Result<()> {
    let event = PluginEvent::Error {
        message: message.to_string(),
        fatal: false,
    };
    write_json(writer, &serde_json::to_value(event)?)
}
