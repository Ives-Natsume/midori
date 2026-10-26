//! CW sidecar: audio in, decoded text events out.

use anyhow::Result;
use midori_plugin_cw::{MorseDecoder, MorseEvent};
use midori_plugin_sdk::{Emitter, FrameInfo, Plugin, StatusLevel, StreamInfo, run};
use serde_json::Value;

fn main() -> Result<()> {
    run(CwPlugin::new())
}

struct CwPlugin {
    sample_rate: f64,
    tone_hz: f64,
    envelope_hz: f64,
    start_at_signal: bool,
    decoder: MorseDecoder,
    line: String,
    samples_since_status: u64,
}

impl CwPlugin {
    fn new() -> Self {
        let sample_rate = 48_000.0;
        let tone_hz = 700.0;
        let envelope_hz = 50.0;
        Self {
            sample_rate,
            tone_hz,
            envelope_hz,
            start_at_signal: false,
            decoder: MorseDecoder::new_with_envelope(sample_rate, tone_hz, envelope_hz),
            line: String::new(),
            samples_since_status: 0,
        }
    }

    fn reset_decoder(&mut self) {
        self.decoder =
            MorseDecoder::new_with_envelope(self.sample_rate, self.tone_hz, self.envelope_hz);
        if self.start_at_signal {
            self.decoder.assume_signal();
        }
    }

    fn send_events(&mut self, events: Vec<MorseEvent>, out: &mut Emitter<'_>) -> Result<()> {
        for event in events {
            match event {
                MorseEvent::Char(character) => {
                    self.line.push(character);
                    trim_line(&mut self.line);
                    out.text("CW", vec![self.line.clone()])?;
                }
                MorseEvent::Word => {
                    self.line.push(' ');
                    trim_line(&mut self.line);
                    out.text("CW", vec![self.line.clone()])?;
                }
            }
        }
        Ok(())
    }
}

impl Plugin for CwPlugin {
    fn name(&self) -> &str {
        "cw"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn configure(&mut self, params: &Value) -> Result<()> {
        if let Some(value) = params.get("tone_hz").and_then(Value::as_f64) {
            self.tone_hz = value;
        }
        if let Some(value) = params.get("envelope_hz").and_then(Value::as_f64) {
            self.envelope_hz = value;
        }
        self.start_at_signal = params
            .get("start_at_signal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.reset_decoder();
        Ok(())
    }

    fn on_stream(&mut self, info: &StreamInfo, _out: &mut Emitter<'_>) -> Result<()> {
        // Repeated stream messages are usual; only a rate change rebuilds the
        // decoder.
        if (info.sample_rate_hz - self.sample_rate).abs() > 1.0 {
            self.sample_rate = info.sample_rate_hz;
            self.reset_decoder();
        }
        Ok(())
    }

    fn on_audio(
        &mut self,
        _meta: &FrameInfo,
        samples: &[f32],
        out: &mut Emitter<'_>,
    ) -> Result<()> {
        self.samples_since_status += samples.len() as u64;
        let events = self.decoder.process(samples);
        self.send_events(events, out)?;
        if self.samples_since_status >= self.sample_rate as u64 {
            self.samples_since_status = 0;
            out.status(
                StatusLevel::Info,
                format!(
                    "{:.0} WPM · {:.0} Hz",
                    self.decoder.wpm(),
                    self.decoder.tone_hz()
                ),
            )?;
        }
        Ok(())
    }

    fn on_shutdown(&mut self, out: &mut Emitter<'_>) -> Result<()> {
        let events = self.decoder.flush();
        self.send_events(events, out)
    }
}

fn trim_line(line: &mut String) {
    const MAX: usize = 200;
    if line.len() > MAX {
        let cut = line.len() - MAX;
        line.drain(..cut);
    }
}
