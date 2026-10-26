//! SSDV sidecar: 9k6 BPSK IQ or raw packets in, PNG image events out.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use midori_plugin_sdk::{
    DisplayUpdate, Emitter, FrameInfo, ImageFormat, Plugin, PluginEvent, Sample, StatusLevel,
    StreamInfo, StreamKind, default_output_dir, run, stream_kind_name,
};
use midori_plugin_ssdv::{
    Bpsk9600AudioReceiver, Bpsk9600Receiver, DEFAULT_CRC_INIT, SessionEvent, SsdvSession,
    parse_crc_inits,
};
use serde_json::Value;

fn main() -> Result<()> {
    run(SsdvPlugin::new()?)
}

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

    fn process_iq(&mut self, samples: &[Sample]) -> Result<Vec<SessionEvent>> {
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
            Self::Bytes(session) => session.flush(),
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

struct SsdvPlugin {
    output_dir: Option<PathBuf>,
    input: String,
    crc_inits: Vec<u32>,
    frame_header_len: usize,
    pipeline: Pipeline,
    iq_warned: bool,
    stream_rate_ok: bool,
}

impl SsdvPlugin {
    fn new() -> Result<Self> {
        let output_dir = default_output_dir();
        let input = "iq".to_string();
        let crc_inits = vec![DEFAULT_CRC_INIT];
        let frame_header_len = 5usize;
        let pipeline = Pipeline::new(
            &input,
            output_dir.clone(),
            crc_inits.clone(),
            frame_header_len,
        )?;
        Ok(Self {
            output_dir,
            input,
            crc_inits,
            frame_header_len,
            pipeline,
            iq_warned: false,
            stream_rate_ok: true,
        })
    }
}

impl Plugin for SsdvPlugin {
    fn name(&self) -> &str {
        "ssdv"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn configure(&mut self, params: &Value) -> Result<()> {
        if let Some(dir) = params.get("output_dir").and_then(Value::as_str) {
            self.output_dir = if dir.is_empty() {
                None
            } else {
                Some(PathBuf::from(dir))
            };
        }
        if let Some(mode) = params.get("input").and_then(Value::as_str) {
            self.input = mode.to_string();
        }
        if let Some(values) = params.get("crc_inits").and_then(Value::as_array) {
            let values: Vec<String> = values
                .iter()
                .map(|value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect();
            self.crc_inits = parse_crc_inits(&values);
        }
        if let Some(len) = params.get("frame_header_len").and_then(Value::as_u64) {
            self.frame_header_len = len as usize;
        }
        self.pipeline = Pipeline::new(
            &self.input,
            self.output_dir.clone(),
            self.crc_inits.clone(),
            self.frame_header_len,
        )?;
        if let Some(ms) = params.get("preview_interval_ms").and_then(Value::as_u64)
            && let Pipeline::Bytes(session) = &mut self.pipeline
        {
            session.set_preview_interval(Duration::from_millis(ms.max(50)));
        }
        self.stream_rate_ok = true;
        self.iq_warned = false;
        Ok(())
    }

    fn on_ready(&mut self, out: &mut Emitter<'_>) -> Result<()> {
        let crc_text: Vec<String> = self
            .crc_inits
            .iter()
            .map(|init| format!("0x{init:08X}"))
            .collect();
        out.status(
            StatusLevel::Info,
            format!(
                "SSDV CRC init [{}], frame header {} bytes",
                crc_text.join(", "),
                self.frame_header_len
            ),
        )
    }

    fn on_stream(&mut self, info: &StreamInfo, out: &mut Emitter<'_>) -> Result<()> {
        let rate = info.sample_rate_hz;
        self.stream_rate_ok = (rate - 48_000.0).abs() <= 1.0;
        if !self.stream_rate_ok && !self.iq_warned {
            self.iq_warned = true;
            out.status(
                StatusLevel::Warning,
                format!(
                    "SSDV expects 48 kHz; got {rate:.0} Hz. \
                     Use the BPSK demod, BPSK IF for mono 12 kHz \
                     audio, and a 48 kHz channel rate."
                ),
            )?;
        }
        if !self.iq_warned {
            let expected = match self.input.as_str() {
                "iq" => Some(StreamKind::Iq),
                "audio-if" | "audio" => Some(StreamKind::Audio),
                _ => None,
            };
            if let Some(expected) = expected
                && info.kind != expected
            {
                self.iq_warned = true;
                out.status(
                    StatusLevel::Warning,
                    format!(
                        "SSDV sidecar expects {}, received {}",
                        self.input,
                        stream_kind_name(info.kind)
                    ),
                )?;
            }
        }
        Ok(())
    }

    fn on_iq(
        &mut self,
        _meta: &FrameInfo,
        samples: &[Sample],
        out: &mut Emitter<'_>,
    ) -> Result<()> {
        if !self.stream_rate_ok {
            return Ok(());
        }
        for event in self.pipeline.process_iq(samples)? {
            send_event(event, out)?;
        }
        Ok(())
    }

    fn on_audio(
        &mut self,
        _meta: &FrameInfo,
        samples: &[f32],
        out: &mut Emitter<'_>,
    ) -> Result<()> {
        if !self.stream_rate_ok {
            return Ok(());
        }
        for event in self.pipeline.process_audio(samples)? {
            send_event(event, out)?;
        }
        Ok(())
    }

    fn on_bytes(&mut self, _meta: &FrameInfo, bytes: &[u8], out: &mut Emitter<'_>) -> Result<()> {
        match self.pipeline.process_bytes(bytes) {
            Ok(events) => {
                for event in events {
                    send_event(event, out)?;
                }
            }
            Err(error) => out.error(format!("SSDV input: {error}"), false)?,
        }
        Ok(())
    }

    fn on_shutdown(&mut self, out: &mut Emitter<'_>) -> Result<()> {
        for event in self.pipeline.flush() {
            send_event(event, out)?;
        }
        let summary = self.pipeline.summary();
        if !summary.is_empty() {
            eprintln!("[ssdv] {summary}");
        }
        Ok(())
    }
}

fn send_event(event: SessionEvent, out: &mut Emitter<'_>) -> Result<()> {
    match event {
        SessionEvent::Status { level, message } => out.status(level, message),
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
            out.event(PluginEvent::Display { update })
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
            out.status(
                StatusLevel::Info,
                format!(
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
            out.image(
                format!(
                    "SSDV ID {} {}x{}{}",
                    image.image_id, image.width, image.height, suffix
                ),
                ImageFormat::Png,
                image.width.into(),
                image.height.into(),
                image.png,
            )
        }
    }
}
