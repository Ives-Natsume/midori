//! Out-of-process plugin supervisor with handshake and I/O timeouts.
//!
//! The sidecar handshake is deliberately small:
//!
//! 1. plugin → host: `{"type":"hello","protocol":1,"name":"...","version":"..."}`
//! 2. host → plugin: `{"type":"configure","params":{...}}`
//! 3. plugin → host: `{"type":"ready"}`
//! 4. host → plugin: `{"type":"stream", ...}` before the first data frame
//! 5. host → plugin: audio / soft-bit / byte frames
//! 6. plugin → host: JSON `PluginEvent`s
//! 7. host → plugin: `{"type":"shutdown"}`
//!
//! Both directions run on dedicated threads with bounded queues. A plugin that
//! stops reading, stops responding, or dies cannot block the SDR worker; the
//! host times out the handshake and drops queued frames if the plugin is slow.

use std::io::{BufReader, BufWriter};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError, bounded};
use midori_core::{Frame, FrameMeta, StreamData};
use serde_json::{Value, json};

use crate::wire::{
    WireMessage, read_message, write_audio, write_bytes, write_json, write_soft_bits,
};
use crate::{Manifest, PROTOCOL_VERSION, PluginEvent, PluginHost};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
const OUTBOUND_QUEUE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamKey {
    kind: String,
    sample_rate_bits: u64,
    center_freq_bits: u64,
    vfo_id: Option<u32>,
}

enum Outbound {
    Json(Value),
    Audio {
        seq: u64,
        timestamp_ns: f64,
        samples: Arc<[f32]>,
    },
    SoftBits {
        seq: u64,
        timestamp_ns: f64,
        bits: Arc<[u8]>,
    },
    Bytes {
        seq: u64,
        timestamp_ns: f64,
        bytes: Arc<[u8]>,
    },
}

pub struct SidecarPlugin {
    manifest: Manifest,
    child: Option<Child>,
    outbound: Option<Sender<Outbound>>,
    inbound: Option<Receiver<WireMessage>>,
    writer_thread: Option<JoinHandle<()>>,
    reader_thread: Option<JoinHandle<()>>,
    stream: Option<StreamKey>,
    configured: bool,
    closed_reported: bool,
}

impl SidecarPlugin {
    pub fn new(manifest: Manifest) -> Result<Self> {
        manifest.validate()?;
        Ok(Self {
            manifest,
            child: None,
            outbound: None,
            inbound: None,
            writer_thread: None,
            reader_thread: None,
            stream: None,
            configured: false,
            closed_reported: false,
        })
    }

    fn spawn_writer(stdin: ChildStdin) -> (Sender<Outbound>, JoinHandle<()>) {
        let (tx, rx) = bounded::<Outbound>(OUTBOUND_QUEUE);
        let handle = thread::Builder::new()
            .name("plugin-writer".to_string())
            .spawn(move || {
                let mut writer = BufWriter::new(stdin);
                while let Ok(message) = rx.recv() {
                    let result = match message {
                        Outbound::Json(value) => write_json(&mut writer, &value),
                        Outbound::Audio {
                            seq,
                            timestamp_ns,
                            samples,
                        } => write_audio(&mut writer, seq, timestamp_ns, &samples),
                        Outbound::SoftBits {
                            seq,
                            timestamp_ns,
                            bits,
                        } => write_soft_bits(&mut writer, seq, timestamp_ns, &bits),
                        Outbound::Bytes {
                            seq,
                            timestamp_ns,
                            bytes,
                        } => write_bytes(&mut writer, seq, timestamp_ns, &bytes),
                    };
                    if result.is_err() {
                        break;
                    }
                }
            })
            .expect("spawning plugin writer");
        (tx, handle)
    }

    fn spawn_reader(stdout: ChildStdout) -> (Receiver<WireMessage>, JoinHandle<()>) {
        let (tx, rx) = bounded::<WireMessage>(64);
        let handle = thread::Builder::new()
            .name("plugin-reader".to_string())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(message) = read_message(&mut reader) {
                    if let Err(TrySendError::Disconnected(_)) = tx.try_send(message) {
                        break;
                    }
                }
            })
            .expect("spawning plugin reader");
        (rx, handle)
    }

    fn recv_message(&self, timeout: Duration) -> Result<WireMessage> {
        let Some(inbound) = &self.inbound else {
            bail!("plugin is not started");
        };
        match inbound.recv_timeout(timeout) {
            Ok(message) => Ok(message),
            Err(RecvTimeoutError::Timeout) => bail!("plugin handshake timed out"),
            Err(RecvTimeoutError::Disconnected) => bail!("plugin closed its output"),
        }
    }

    fn ensure_stream(&mut self, kind: &str, meta: &FrameMeta) -> Result<()> {
        let key = StreamKey {
            kind: kind.to_string(),
            sample_rate_bits: meta.sample_rate_hz.to_bits(),
            center_freq_bits: meta.center_freq_hz.to_bits(),
            vfo_id: meta.vfo_id,
        };
        if self.stream.as_ref() == Some(&key) {
            return Ok(());
        }

        let outbound = self
            .outbound
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
        let message = json!({
            "type": "stream",
            "kind": kind,
            "sample_rate_hz": meta.sample_rate_hz,
            "channels": 1,
            "center_freq_hz": meta.center_freq_hz,
            "nominal_freq_hz": meta.nominal_freq_hz,
            "bandwidth_hz": meta.bandwidth_hz,
            "doppler_hz": meta.doppler_hz,
            "doppler_rate_hz_s": meta.doppler_rate_hz_s,
            "vfo_id": meta.vfo_id,
        });
        match outbound.try_send(Outbound::Json(message)) {
            Ok(()) => {
                self.stream = Some(key);
                Ok(())
            }
            Err(TrySendError::Full(_)) => bail!("plugin outbound queue is full"),
            Err(TrySendError::Disconnected(_)) => bail!("plugin writer has stopped"),
        }
    }

    fn timestamp(meta: &FrameMeta) -> f64 {
        meta.timestamp_ns.unwrap_or(0) as f64
    }
}

impl PluginHost for SidecarPlugin {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn start(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }

        let program = self
            .manifest
            .command
            .first()
            .ok_or_else(|| anyhow::anyhow!("plugin manifest has no command"))?;
        let args = &self.manifest.command[1..];

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning plugin '{}'", self.manifest.name))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("plugin stdin is not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("plugin stdout is not piped"))?;

        let (outbound, writer_thread) = Self::spawn_writer(stdin);
        let (inbound, reader_thread) = Self::spawn_reader(stdout);

        self.child = Some(child);
        self.outbound = Some(outbound);
        self.inbound = Some(inbound);
        self.writer_thread = Some(writer_thread);
        self.reader_thread = Some(reader_thread);
        self.stream = None;
        self.configured = false;
        self.closed_reported = false;

        let hello = match self.recv_message(HANDSHAKE_TIMEOUT) {
            Ok(WireMessage::Json(value)) => value,
            Ok(_) => {
                let _ = self.stop();
                bail!("plugin sent a non-JSON handshake");
            }
            Err(error) => {
                let _ = self.stop();
                return Err(error.context("reading plugin hello"));
            }
        };

        if hello.get("type").and_then(Value::as_str) != Some("hello") {
            let _ = self.stop();
            bail!("plugin handshake did not start with hello");
        }
        let protocol = hello.get("protocol").and_then(Value::as_u64).unwrap_or(0) as u32;
        if protocol != PROTOCOL_VERSION {
            let _ = self.stop();
            bail!(
                "plugin '{}' speaks protocol {protocol}, engine expects {PROTOCOL_VERSION}",
                self.manifest.name
            );
        }
        let name = hello
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&self.manifest.name);
        if name != self.manifest.name {
            eprintln!(
                "[plugin] warning: manifest name '{}' differs from hello name '{name}'",
                self.manifest.name
            );
        }

        eprintln!("[plugin] started '{}'", self.manifest.name);
        Ok(())
    }

    fn configure(&mut self, params: &Value) -> Result<()> {
        if self.configured {
            bail!("plugin '{}' is already configured", self.manifest.name);
        }
        let outbound = self
            .outbound
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
        let message = json!({"type": "configure", "params": params});
        match outbound.send_timeout(Outbound::Json(message), WRITE_TIMEOUT) {
            Ok(()) => {}
            Err(_) => {
                let _ = self.stop();
                bail!("plugin configure write timed out");
            }
        }

        let deadline = Instant::now() + CONFIGURE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.stop();
                bail!("plugin did not acknowledge configuration");
            }
            match self.recv_message(remaining) {
                Ok(WireMessage::Json(value)) => match value.get("type").and_then(Value::as_str) {
                    Some("ready") => break,
                    Some("error") => {
                        let message = value
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string();
                        let _ = self.stop();
                        bail!(
                            "plugin '{}' rejected configuration: {message}",
                            self.manifest.name
                        );
                    }
                    _ => continue,
                },
                Ok(_) => continue,
                Err(error) => {
                    let _ = self.stop();
                    return Err(error);
                }
            }
        }

        self.configured = true;
        self.stream = None;
        eprintln!("[plugin] configured '{}'", self.manifest.name);
        Ok(())
    }

    fn send_frame(&mut self, frame: Frame) -> Result<()> {
        let meta = frame.meta;
        let outbound = self
            .outbound
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;

        match frame.data {
            StreamData::Audio(samples) => {
                self.ensure_stream("audio", &meta)?;
                let message = Outbound::Audio {
                    seq: meta.seq,
                    timestamp_ns: Self::timestamp(&meta),
                    samples,
                };
                match outbound.try_send(message) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(_)) => bail!("plugin outbound queue is full"),
                    Err(TrySendError::Disconnected(_)) => bail!("plugin writer has stopped"),
                }
            }
            StreamData::SoftBits(bits) => {
                self.ensure_stream("soft_bits", &meta)?;
                let message = Outbound::SoftBits {
                    seq: meta.seq,
                    timestamp_ns: Self::timestamp(&meta),
                    bits,
                };
                match outbound.try_send(message) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(_)) => bail!("plugin outbound queue is full"),
                    Err(TrySendError::Disconnected(_)) => bail!("plugin writer has stopped"),
                }
            }
            StreamData::Bytes(bytes) => {
                self.ensure_stream("bytes", &meta)?;
                let message = Outbound::Bytes {
                    seq: meta.seq,
                    timestamp_ns: Self::timestamp(&meta),
                    bytes,
                };
                match outbound.try_send(message) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(_)) => bail!("plugin outbound queue is full"),
                    Err(TrySendError::Disconnected(_)) => bail!("plugin writer has stopped"),
                }
            }
            StreamData::Iq(_) => bail!("IQ sidecar streams are not supported yet"),
        }
    }

    fn try_recv(&mut self) -> Result<Option<PluginEvent>> {
        let Some(inbound) = &self.inbound else {
            return Ok(None);
        };
        loop {
            match inbound.try_recv() {
                Ok(WireMessage::Json(value)) => {
                    return match serde_json::from_value::<PluginEvent>(value) {
                        Ok(event) => Ok(Some(event)),
                        Err(error) => Ok(Some(PluginEvent::Error {
                            message: format!("invalid plugin event: {error}"),
                            fatal: false,
                        })),
                    };
                }
                Ok(_) => continue,
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    if self.closed_reported {
                        return Ok(None);
                    }
                    self.closed_reported = true;
                    return Ok(Some(PluginEvent::Error {
                        message: "plugin closed its output".to_string(),
                        fatal: true,
                    }));
                }
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(outbound) = self.outbound.as_ref() {
            let _ = outbound.send_timeout(
                Outbound::Json(json!({"type": "shutdown"})),
                Duration::from_millis(200),
            );
        }
        self.outbound = None;
        self.inbound = None;

        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + STOP_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        }

        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }

        self.stream = None;
        self.configured = false;
        self.closed_reported = false;
        Ok(())
    }
}

impl Drop for SidecarPlugin {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InputSpec, OutputKind};

    #[test]
    #[cfg(unix)]
    fn handshake_timeout_kills_plugin() {
        let manifest = Manifest {
            name: "hang".to_string(),
            version: "0.1.0".to_string(),
            protocol: PROTOCOL_VERSION,
            command: vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            input: InputSpec::Audio {
                sample_rate_hz: 48_000.0,
                channels: 1,
            },
            outputs: vec![OutputKind::Display],
            params: vec![],
        };

        let mut plugin = SidecarPlugin::new(manifest).unwrap();
        let started = Instant::now();
        let error = plugin.start().unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(
            format!("{error:#}").contains("timed out"),
            "unexpected error: {error:#}"
        );
    }
}
