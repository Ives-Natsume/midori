//! Bridge between the session sink graph and a decoder sidecar.
//!
//! The sink and the worker share one host behind a mutex: the sink forwards
//! audio frames, the worker polls events for the UI.

use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use midori_core::{Frame, Sink, StreamData};
use midori_plugin::{PluginEvent, PluginHost};

pub struct PluginSink {
    host: Arc<Mutex<Box<dyn PluginHost>>>,
    dropped: u64,
}

#[derive(Clone)]
pub struct PluginHandle {
    host: Arc<Mutex<Box<dyn PluginHost>>>,
}

pub fn plugin_sink(host: Box<dyn PluginHost>) -> (PluginSink, PluginHandle) {
    let host = Arc::new(Mutex::new(host));
    (
        PluginSink {
            host: Arc::clone(&host),
            dropped: 0,
        },
        PluginHandle { host },
    )
}

impl Sink for PluginSink {
    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        if !matches!(&frame.data, StreamData::Audio(_)) {
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

    pub fn stop(&self) -> Result<()> {
        let mut host = self
            .host
            .lock()
            .map_err(|error| anyhow!("plugin lock poisoned: {error}"))?;
        host.stop()
    }
}
