//! SSTV sidecar: audio in, PNG image events out.

use std::path::PathBuf;

use anyhow::Result;
use midori_plugin_sdk::{
    DisplayUpdate, Emitter, FrameInfo, ImageFormat, Plugin, PluginEvent, StatusLevel, StreamInfo,
    default_output_dir, run,
};
use midori_plugin_sstv::{SessionEvent, SstvSession};
use serde_json::Value;

fn main() -> Result<()> {
    run(SstvPlugin::new()?)
}

struct SstvPlugin {
    sample_rate: u32,
    output_dir: Option<PathBuf>,
    session: SstvSession,
    /// Deferred configure error, reported after `ready` (the SDK does not give
    /// `configure` an event emitter).
    pending_error: Option<String>,
}

impl SstvPlugin {
    fn new() -> Result<Self> {
        let sample_rate = 48_000u32;
        let output_dir = default_output_dir();
        let mut session = SstvSession::new(sample_rate)?;
        session.set_output_dir(output_dir.clone());
        Ok(Self {
            sample_rate,
            output_dir,
            session,
            pending_error: None,
        })
    }

    fn send_event(&mut self, event: SessionEvent, out: &mut Emitter<'_>) -> Result<()> {
        match event {
            SessionEvent::VisDetected {
                mode_name,
                header_shift_hz,
                ..
            } => out.status(
                StatusLevel::Info,
                format!("VIS {mode_name} (header {header_shift_hz:+.0} Hz)"),
            ),
            SessionEvent::UnknownVis {
                code,
                header_shift_hz,
            } => out.status(
                StatusLevel::Warning,
                format!("unknown VIS {code} (header {header_shift_hz:+.0} Hz)"),
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
                out.event(PluginEvent::Display { update })
            }
            SessionEvent::Image(image) => {
                let location = image
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "not saved".to_string());
                out.status(
                    StatusLevel::Info,
                    format!(
                        "{} {}x{} -> {location}",
                        image.mode_name, image.width, image.height
                    ),
                )?;
                out.image(
                    format!("SSTV {}", image.mode_name),
                    ImageFormat::Png,
                    image.width,
                    image.height,
                    image.png,
                )
            }
        }
    }
}

impl Plugin for SstvPlugin {
    fn name(&self) -> &str {
        "sstv"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn configure(&mut self, params: &Value) -> Result<()> {
        if let Some(value) = params.get("output_dir").and_then(Value::as_str) {
            self.output_dir = if value.is_empty() {
                None
            } else {
                Some(PathBuf::from(value))
            };
        }
        if let Some(value) = params.get("sample_rate_hz").and_then(Value::as_u64) {
            self.sample_rate = value as u32;
            if let Err(error) = self.session.set_sample_rate(self.sample_rate) {
                // The host sees a non-fatal error, but configuration still
                // succeeds at the previous rate.
                self.pending_error = Some(format!("sample rate: {error}"));
            }
        }
        self.session.set_output_dir(self.output_dir.clone());
        Ok(())
    }

    fn on_ready(&mut self, out: &mut Emitter<'_>) -> Result<()> {
        if let Some(message) = self.pending_error.take() {
            out.error(message, false)?;
        }
        Ok(())
    }

    fn on_stream(&mut self, info: &StreamInfo, out: &mut Emitter<'_>) -> Result<()> {
        let rate = info.sample_rate_hz;
        if rate > 0.0 && (rate.round() as u32) != self.sample_rate {
            self.sample_rate = rate.round() as u32;
            if let Err(error) = self.session.set_sample_rate(self.sample_rate) {
                out.error(format!("sample rate change: {error}"), false)?;
            } else {
                out.status(
                    StatusLevel::Info,
                    format!("audio rate {} Hz", self.sample_rate),
                )?;
            }
        }
        Ok(())
    }

    fn on_audio(
        &mut self,
        _meta: &FrameInfo,
        samples: &[f32],
        out: &mut Emitter<'_>,
    ) -> Result<()> {
        for event in self.session.process(samples) {
            self.send_event(event, out)?;
        }
        Ok(())
    }
}
