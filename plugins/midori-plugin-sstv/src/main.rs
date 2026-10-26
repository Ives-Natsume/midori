//! SSTV sidecar: audio in, PNG image events out.

use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;

use anyhow::Result;
use midori_plugin::wire::{WireMessage, read_message, write_json};
use midori_plugin::{DisplayUpdate, ImageFormat, PROTOCOL_VERSION, PluginEvent, StatusLevel};
use midori_plugin_sstv::{SessionEvent, SstvSession};
use serde_json::{Value, json};

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
            "name": "sstv",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    )?;

    let mut sample_rate = 48_000u32;
    let mut output_dir = default_output_dir();
    let mut session = SstvSession::new(sample_rate)?;
    session.set_output_dir(output_dir.clone());
    let mut configured = false;

    loop {
        let message = match read_message(&mut reader) {
            Ok(message) => message,
            // stdin EOF means the host closed the pipe: exit as cleanly as an
            // explicit shutdown. SstvSession has no partial-image flush hook,
            // so an incomplete image is discarded rather than emitted.
            Err(_) => break,
        };

        match message {
            WireMessage::Json(value) => match value.get("type").and_then(Value::as_str) {
                Some("configure") => {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(value) = params.get("output_dir").and_then(Value::as_str) {
                        output_dir = if value.is_empty() {
                            None
                        } else {
                            Some(PathBuf::from(value))
                        };
                    }
                    if let Some(value) = params.get("sample_rate_hz").and_then(Value::as_u64) {
                        sample_rate = value as u32;
                        if let Err(error) = session.set_sample_rate(sample_rate) {
                            send_error(&mut writer, &format!("sample rate: {error}"))?;
                        }
                    }
                    session.set_output_dir(output_dir.clone());
                    configured = true;
                    write_json(&mut writer, &json!({ "type": "ready" }))?;
                }
                Some("stream") => {
                    if let Some(value) = value.get("sample_rate_hz").and_then(Value::as_f64)
                        && value > 0.0
                        && (value.round() as u32) != sample_rate
                    {
                        sample_rate = value.round() as u32;
                        if let Err(error) = session.set_sample_rate(sample_rate) {
                            send_error(&mut writer, &format!("sample rate change: {error}"))?;
                        } else {
                            send_status(
                                &mut writer,
                                StatusLevel::Info,
                                &format!("audio rate {sample_rate} Hz"),
                            )?;
                        }
                    }
                }
                Some("shutdown") => break,
                _ => {}
            },
            WireMessage::Audio { samples, .. } => {
                if !configured {
                    continue;
                }
                for event in session.process(&samples) {
                    send_event(&mut writer, event)?;
                }
            }
            WireMessage::SoftBits { .. } | WireMessage::Bytes { .. } | WireMessage::Iq { .. } => {}
        }
    }

    Ok(())
}

fn default_output_dir() -> Option<PathBuf> {
    let dir = std::env::current_dir().ok()?.join("captures");
    Some(dir)
}

fn send_event<W: Write>(writer: &mut W, event: SessionEvent) -> Result<()> {
    match event {
        SessionEvent::VisDetected {
            mode_name,
            header_shift_hz,
            ..
        } => send_status(
            writer,
            StatusLevel::Info,
            &format!("VIS {mode_name} (header {:+.0} Hz)", header_shift_hz),
        ),
        SessionEvent::UnknownVis {
            code,
            header_shift_hz,
        } => send_status(
            writer,
            StatusLevel::Warning,
            &format!("unknown VIS {code} (header {:+.0} Hz)", header_shift_hz),
        ),
        SessionEvent::Progress {
            mode_name,
            line,
            total_lines,
            ..
        } => {
            let update = DisplayUpdate::Progress {
                title: format!("{mode_name} {line}/{total_lines}"),
                fraction: (line as f32 / total_lines.max(1) as f32).clamp(0.0, 1.0),
            };
            write_json(
                writer,
                &serde_json::to_value(PluginEvent::Display { update })?,
            )
        }
        SessionEvent::Image(image) => {
            let location = image
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "not saved".to_string());
            send_status(
                writer,
                StatusLevel::Info,
                &format!(
                    "{} {}x{} -> {location}",
                    image.mode_name, image.width, image.height
                ),
            )?;
            let update = DisplayUpdate::Image {
                title: format!("SSTV {}", image.mode_name),
                format: ImageFormat::Png,
                width: image.width,
                height: image.height,
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
