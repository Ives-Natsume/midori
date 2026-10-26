//! Bridge between the session sink graph and a decoder sidecar.
//!
//! The sink and the worker share one host behind a mutex: the sink forwards
//! audio frames, the worker polls events for the UI.

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use midori_core::{Frame, Sink, StreamKind};
use midori_plugin::{PluginEvent, PluginHost};

pub struct PluginSink {
    host: Arc<Mutex<Box<dyn PluginHost>>>,
    input_kind: StreamKind,
    dropped: u64,
}

#[derive(Clone)]
pub struct PluginHandle {
    host: Arc<Mutex<Box<dyn PluginHost>>>,
}

pub fn plugin_sink(host: Box<dyn PluginHost>) -> (PluginSink, PluginHandle) {
    let input_kind = host.input_kind();
    let host = Arc::new(Mutex::new(host));
    (
        PluginSink {
            host: Arc::clone(&host),
            input_kind,
            dropped: 0,
        },
        PluginHandle { host },
    )
}

impl Sink for PluginSink {
    fn on_frame(&mut self, frame: Frame) -> Result<()> {
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
}
