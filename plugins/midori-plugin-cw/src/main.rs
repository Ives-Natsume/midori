use std::io::{BufReader, BufWriter, Write};

use anyhow::Result;
use midori_plugin::wire::{WireMessage, read_message, write_json};
use midori_plugin::{DisplayUpdate, PROTOCOL_VERSION, PluginEvent, StatusLevel};
use midori_plugin_cw::{MorseDecoder, MorseEvent};
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
            "name": "cw",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    )?;

    let mut sample_rate = 48_000.0;
    let mut tone_hz = 700.0;
    let mut decoder = MorseDecoder::new(sample_rate, tone_hz);
    let mut line = String::new();
    let mut configured = false;
    let mut samples_since_status = 0u64;

    loop {
        let message = match read_message(&mut reader) {
            Ok(message) => message,
            Err(_) => break,
        };

        match message {
            WireMessage::Json(value) => match value.get("type").and_then(Value::as_str) {
                Some("configure") => {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(value) = params.get("tone_hz").and_then(Value::as_f64) {
                        tone_hz = value;
                    }
                    decoder = MorseDecoder::new(sample_rate, tone_hz);
                    configured = true;
                    write_json(&mut writer, &json!({ "type": "ready" }))?;
                }
                Some("stream") => {
                    if let Some(value) = value.get("sample_rate_hz").and_then(Value::as_f64)
                        && (value - sample_rate).abs() > 1.0
                    {
                        sample_rate = value;
                        decoder = MorseDecoder::new(sample_rate, tone_hz);
                    }
                }
                Some("shutdown") => break,
                _ => {}
            },
            WireMessage::Audio { samples, .. } => {
                if !configured {
                    continue;
                }
                samples_since_status += samples.len() as u64;
                send_events(&mut writer, &mut line, decoder.process(&samples))?;
                if samples_since_status >= sample_rate as u64 {
                    samples_since_status = 0;
                    send_status(&mut writer, decoder.wpm())?;
                }
            }
            WireMessage::SoftBits { .. } | WireMessage::Bytes { .. } => {}
        }
    }

    send_events(&mut writer, &mut line, decoder.flush())?;

    Ok(())
}

fn send_events<W: Write>(writer: &mut W, line: &mut String, events: Vec<MorseEvent>) -> Result<()> {
    for event in events {
        match event {
            MorseEvent::Char(character) => {
                line.push(character);
                trim_line(line);
                send_text(writer, line)?;
            }
            MorseEvent::Word => {
                line.push(' ');
                trim_line(line);
                send_text(writer, line)?;
            }
        }
    }
    Ok(())
}

fn trim_line(line: &mut String) {
    const MAX: usize = 200;
    if line.len() > MAX {
        let cut = line.len() - MAX;
        line.drain(..cut);
    }
}

fn send_text<W: Write>(writer: &mut W, line: &str) -> Result<()> {
    let event = PluginEvent::Display {
        update: DisplayUpdate::Text {
            title: "CW".to_string(),
            lines: vec![line.to_string()],
        },
    };
    write_json(writer, &serde_json::to_value(event)?)
}

fn send_status<W: Write>(writer: &mut W, wpm: f32) -> Result<()> {
    let event = PluginEvent::Status {
        level: StatusLevel::Info,
        message: format!("{wpm:.0} WPM"),
    };
    write_json(writer, &serde_json::to_value(event)?)
}
