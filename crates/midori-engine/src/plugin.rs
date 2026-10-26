//! Bridge between the session sink graph and a decoder sidecar.
//!
//! The sink and the worker share one host behind a mutex: the sink forwards
//! audio frames, the worker polls events for the UI.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use midori_core::{Frame, Sink, StreamKind};
use midori_plugin::{PluginEvent, PluginHost, demod_modes_match};

pub struct PluginSink {
    host: Arc<Mutex<Box<dyn PluginHost>>>,
    input_kind: StreamKind,
    /// Set by the worker when the current demod mode is not in the manifest's
    /// `demod_modes`. Inactive sinks discard frames without touching the host
    /// and without counting them as drops.
    active: Arc<AtomicBool>,
    dropped: u64,
}

#[derive(Clone)]
pub struct PluginHandle {
    host: Arc<Mutex<Box<dyn PluginHost>>>,
    active: Arc<AtomicBool>,
    demod_modes: Arc<Vec<String>>,
}

pub fn plugin_sink(host: Box<dyn PluginHost>) -> (PluginSink, PluginHandle) {
    let input_kind = host.input_kind();
    let demod_modes = Arc::new(host.manifest().demod_modes.clone());
    let active = Arc::new(AtomicBool::new(true));
    let host = Arc::new(Mutex::new(host));
    (
        PluginSink {
            host: Arc::clone(&host),
            input_kind,
            active: Arc::clone(&active),
            dropped: 0,
        },
        PluginHandle {
            host,
            active,
            demod_modes,
        },
    )
}

impl Sink for PluginSink {
    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        if !self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        if frame.data.kind() != self.input_kind {
            return Ok(());
        }
        // The session fans raw source IQ to every sink as well as channel
        // frames. An IQ plugin wants the selected VFO's complex baseband, not
        // the multi-megasample wideband block, so ignore frames without a VFO.
        if self.input_kind == StreamKind::Iq && frame.meta.vfo_id.is_none() {
            return Ok(());
        }
        let mut host = self
            .host
            .lock()
            .map_err(|error| anyhow!("plugin lock poisoned: {error}"))?;
        if let Err(error) = host.send_frame(frame) {
            if self.dropped == 0 || self.dropped.is_multiple_of(100) {
                eprintln!(
                    "[plugin] frame dropped ({error}); total {}",
                    self.dropped + 1
                );
            }
            self.dropped += 1;
        }
        Ok(())
    }
}

impl PluginHandle {
    pub fn try_recv(&self) -> Result<Option<PluginEvent>> {
        let mut host = self
            .host
            .lock()
            .map_err(|error| anyhow!("plugin lock poisoned: {error}"))?;
        host.try_recv()
    }

    /// Stop the plugin, returning events it flushed while shutting down.
    pub fn stop(&self) -> Result<Vec<PluginEvent>> {
        let mut host = self
            .host
            .lock()
            .map_err(|error| anyhow!("plugin lock poisoned: {error}"))?;
        host.stop()
    }

    /// Whether the manifest says this plugin should see `mode`.
    ///
    /// An empty `demod_modes` list accepts every mode.
    pub fn accepts_demod(&self, mode: &str) -> bool {
        demod_modes_match(&self.demod_modes, mode)
    }

    /// Enable or disable frame delivery, returning the previous state.
    pub fn set_active(&self, active: bool) -> bool {
        self.active.swap(active, Ordering::SeqCst)
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use midori_core::{BlockFlags, FrameMeta, StreamData};
    use midori_plugin::{InputSpec, Manifest, OutputKind, PROTOCOL_VERSION, Value};

    struct MockHost {
        manifest: Manifest,
        sent: Arc<AtomicUsize>,
        fail: bool,
    }

    impl PluginHost for MockHost {
        fn manifest(&self) -> &Manifest {
            &self.manifest
        }

        fn start(&mut self) -> Result<()> {
            Ok(())
        }

        fn configure(&mut self, _params: &Value) -> Result<()> {
            Ok(())
        }

        fn input_kind(&self) -> StreamKind {
            StreamKind::Audio
        }

        fn send_frame(&mut self, _frame: Frame) -> Result<()> {
            if self.fail {
                return Err(anyhow!("mock host is full"));
            }
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn try_recv(&mut self) -> Result<Option<PluginEvent>> {
            Ok(None)
        }

        fn stop(&mut self) -> Result<Vec<PluginEvent>> {
            Ok(Vec::new())
        }
    }

    fn manifest(demod_modes: &[&str]) -> Manifest {
        Manifest {
            name: "mock".to_string(),
            version: "0.1.0".to_string(),
            protocol: PROTOCOL_VERSION,
            command: vec!["mock".to_string()],
            input: InputSpec::Audio {
                sample_rate_hz: 48_000.0,
                channels: 1,
            },
            outputs: vec![OutputKind::Display],
            params: vec![],
            demod_modes: demod_modes.iter().map(|mode| mode.to_string()).collect(),
        }
    }

    fn audio_frame() -> Frame {
        Frame::new(
            FrameMeta {
                seq: 0,
                start_sample: 0,
                timestamp_ns: None,
                sample_rate_hz: 48_000.0,
                center_freq_hz: 0.0,
                nominal_freq_hz: 0.0,
                bandwidth_hz: 3_000.0,
                vfo_id: Some(0),
                doppler_hz: 0.0,
                doppler_rate_hz_s: 0.0,
                flags: BlockFlags::default(),
            },
            StreamData::Audio(Arc::from(vec![0.0f32; 8])),
        )
    }

    fn mock_sink(
        demod_modes: &[&str],
        sent: &Arc<AtomicUsize>,
        fail: bool,
    ) -> (PluginSink, PluginHandle) {
        plugin_sink(Box::new(MockHost {
            manifest: manifest(demod_modes),
            sent: Arc::clone(sent),
            fail,
        }))
    }

    #[test]
    fn inactive_plugin_receives_no_frames_and_counts_no_drops() {
        let sent = Arc::new(AtomicUsize::new(0));
        let (mut sink, handle) = mock_sink(&["cw"], &sent, false);

        assert!(handle.is_active());
        sink.on_frame(audio_frame()).unwrap();
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        assert_eq!(sink.dropped, 0);

        assert!(handle.set_active(false));
        assert!(!handle.is_active());
        sink.on_frame(audio_frame()).unwrap();
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        assert_eq!(sink.dropped, 0, "paused frames must not count as drops");

        assert!(!handle.set_active(true));
        sink.on_frame(audio_frame()).unwrap();
        assert_eq!(sent.load(Ordering::SeqCst), 2);
        assert_eq!(sink.dropped, 0);
    }

    #[test]
    fn host_errors_are_still_counted_as_drops_when_active() {
        let sent = Arc::new(AtomicUsize::new(0));
        let (mut sink, handle) = mock_sink(&[], &sent, true);
        assert!(handle.is_active());
        sink.on_frame(audio_frame()).unwrap();
        assert_eq!(sink.dropped, 1);
        assert_eq!(sent.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn handle_routes_by_manifest_demod_modes() {
        let sent = Arc::new(AtomicUsize::new(0));
        let (_sink, handle) = mock_sink(&["bpsk", "bpsk_if"], &sent, false);
        assert!(handle.accepts_demod("bpsk"));
        assert!(handle.accepts_demod("bpsk_if"));
        assert!(!handle.accepts_demod("fm"));

        let (_sink, any_mode) = mock_sink(&[], &sent, false);
        assert!(any_mode.accepts_demod("fm"));
    }
}
