//! SigMF recording as a session sink.
//!
//! The sink writes raw IQ frames. Control is split into a cheap cloneable
//! handle so the worker can start/stop recording while the session owns the
//! sink itself.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use midori_core::{Frame, Sink, StreamData, StreamSpec};
use midori_sigmf::SigMfWriter;

struct ActiveRecording {
    path: PathBuf,
    sample_rate_hz: f64,
    writer: SigMfWriter,
}

#[derive(Default)]
struct RecorderState {
    active: Option<ActiveRecording>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RecordingInfo {
    pub path: PathBuf,
    pub samples: u64,
    pub seconds: f64,
}

pub struct RecorderSink {
    state: Arc<Mutex<RecorderState>>,
}

#[derive(Clone)]
pub struct RecorderHandle {
    state: Arc<Mutex<RecorderState>>,
}

impl RecorderSink {
    pub fn new() -> (Self, RecorderHandle) {
        let state = Arc::new(Mutex::new(RecorderState::default()));
        (
            Self {
                state: Arc::clone(&state),
            },
            RecorderHandle { state },
        )
    }
}

impl Sink for RecorderSink {
    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        let StreamData::Iq(samples) = &frame.data else {
            return Ok(());
        };

        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("recorder lock poisoned: {error}"))?;

        let mut write_error = None;
        if let Some(active) = state.active.as_mut()
            && let Err(error) = active.writer.write_block(samples)
        {
            write_error = Some(error.to_string());
        }
        if let Some(error) = write_error {
            state.error = Some(format!("IQ write failed: {error}"));
            state.active = None;
        }
        Ok(())
    }
}

impl Drop for RecorderSink {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock()
            && let Some(active) = state.active.take()
        {
            let _ = active.writer.finish();
        }
    }
}

impl RecorderHandle {
    pub fn start(&self, path: PathBuf, spec: StreamSpec) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("recorder lock poisoned: {error}"))?;
        if state.active.is_some() {
            bail!("recording already in progress");
        }
        let writer = SigMfWriter::create(
            &path,
            spec.sample_rate_hz,
            spec.center_freq_hz,
            spec.full_scale,
            "Midori GUI capture",
        )?;
        state.active = Some(ActiveRecording {
            path,
            sample_rate_hz: spec.sample_rate_hz,
            writer,
        });
        state.error = None;
        Ok(())
    }

    pub fn stop(&self) -> Result<Option<RecordingInfo>> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| anyhow!("recorder lock poisoned: {error}"))?;
        let Some(active) = state.active.take() else {
            return Ok(None);
        };
        let samples = active.writer.samples_written();
        let seconds = samples as f64 / active.sample_rate_hz;
        active.writer.finish()?;
        Ok(Some(RecordingInfo {
            path: active.path,
            samples,
            seconds,
        }))
    }

    pub fn is_recording(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.active.is_some())
            .unwrap_or(false)
    }

    pub fn take_error(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| state.error.take())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use midori_core::{BlockFlags, FrameMeta, Sample, SampleFormat, StreamData};
    use midori_sigmf::SigMfReader;

    use super::*;

    fn temp_base(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("midori-recorder-{name}-{nanos}"))
    }

    #[test]
    fn recorder_sink_writes_iq_frames() {
        let base = temp_base("iq");
        let spec = StreamSpec {
            sample_rate_hz: 2_400_000.0,
            center_freq_hz: 91_600_000.0,
            bandwidth_hz: 2_000_000.0,
            format: SampleFormat::Ci16Le,
            full_scale: 2048.0,
        };
        let (mut sink, handle) = RecorderSink::new();
        handle.start(base.clone(), spec).unwrap();
        assert!(handle.is_recording());

        let samples: Arc<[Sample]> =
            Arc::from(vec![Sample::new(0.25, -0.25); 256].into_boxed_slice());
        let meta = FrameMeta {
            seq: 0,
            start_sample: 0,
            timestamp_ns: None,
            sample_rate_hz: spec.sample_rate_hz,
            center_freq_hz: spec.center_freq_hz,
            bandwidth_hz: spec.bandwidth_hz,
            vfo_id: None,
            flags: BlockFlags::default(),
        };
        sink.on_frame(Frame::new(meta, StreamData::Iq(samples)))
            .unwrap();
        let info = handle.stop().unwrap().expect("recording info");
        assert_eq!(info.samples, 256);
        assert!(!handle.is_recording());

        let mut reader = SigMfReader::open(&base).unwrap();
        let samples = reader.read_block(512).unwrap();
        assert_eq!(samples.len(), 256);

        let _ = std::fs::remove_file(format!("{}.sigmf-data", base.display()));
        let _ = std::fs::remove_file(format!("{}.sigmf-meta", base.display()));
    }
}
