//! Out-of-process plugin supervisor.
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

use std::io::{BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, TryRecvError, bounded};
use midori_core::{Frame, FrameMeta, StreamData};
use serde_json::{Value, json};

use crate::wire::{
    WireMessage, read_message, write_audio, write_bytes, write_json, write_soft_bits,
};
use crate::{Manifest, PROTOCOL_VERSION, PluginEvent, PluginHost};

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamKey {
    kind: String,
    sample_rate_bits: u64,
    center_freq_bits: u64,
    vfo_id: Option<u32>,
}

pub struct SidecarPlugin {
    manifest: Manifest,
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    reader: Option<BufReader<ChildStdout>>,
    events: Option<Receiver<PluginEvent>>,
    reader_thread: Option<JoinHandle<()>>,
    stream: Option<StreamKey>,
}

impl SidecarPlugin {
    pub fn new(manifest: Manifest) -> Result<Self> {
        manifest.validate()?;
        Ok(Self {
            manifest,
            child: None,
            stdin: None,
            reader: None,
            events: None,
            reader_thread: None,
            stream: None,
        })
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

        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
        write_json(
            stdin,
            &json!({
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
            }),
        )?;
        self.stream = Some(key);
        Ok(())
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
        let mut reader = BufReader::new(stdout);

        let hello = match read_message(&mut reader) {
            Ok(WireMessage::Json(value)) => value,
            Ok(_) => {
                let _ = child.kill();
                bail!("plugin sent a non-JSON handshake");
            }
            Err(error) => {
                let _ = child.kill();
                return Err(error.context("reading plugin hello"));
            }
        };

        if hello.get("type").and_then(Value::as_str) != Some("hello") {
            let _ = child.kill();
            bail!("plugin handshake did not start with hello");
        }
        let protocol = hello.get("protocol").and_then(Value::as_u64).unwrap_or(0) as u32;
        if protocol != PROTOCOL_VERSION {
            let _ = child.kill();
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

        self.child = Some(child);
        self.stdin = Some(BufWriter::new(stdin));
        self.reader = Some(reader);
        eprintln!("[plugin] started '{}'", self.manifest.name);
        Ok(())
    }

    fn configure(&mut self, params: &Value) -> Result<()> {
        if self.events.is_some() {
            bail!("plugin '{}' is already configured", self.manifest.name);
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
        write_json(stdin, &json!({"type": "configure", "params": params}))?;

        let mut reader = self
            .reader
            .take()
            .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
        let ready = match read_message(&mut reader) {
            Ok(WireMessage::Json(value)) => value,
            Ok(_) => bail!("plugin sent a non-JSON configure response"),
            Err(error) => return Err(error.context("reading plugin ready")),
        };
        match ready.get("type").and_then(Value::as_str) {
            Some("ready") => {}
            Some("error") => bail!(
                "plugin '{}' rejected configuration: {}",
                self.manifest.name,
                ready
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ),
            _ => bail!("plugin '{}' did not send ready", self.manifest.name),
        }

        let (tx, rx) = bounded(64);
        let plugin_name = self.manifest.name.clone();
        let handle = thread::Builder::new()
            .name(format!("plugin-{plugin_name}"))
            .spawn(move || {
                loop {
                    match read_message(&mut reader) {
                        Ok(WireMessage::Json(value)) => {
                            match serde_json::from_value::<PluginEvent>(value) {
                                Ok(event) => {
                                    let _ = tx.try_send(event);
                                }
                                Err(error) => {
                                    let _ = tx.try_send(PluginEvent::Error {
                                        message: format!("invalid plugin event: {error}"),
                                        fatal: false,
                                    });
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = tx.try_send(PluginEvent::Error {
                                message: format!("plugin stream closed: {error}"),
                                fatal: true,
                            });
                            break;
                        }
                    }
                }
            })
            .context("spawning plugin reader thread")?;

        self.events = Some(rx);
        self.reader_thread = Some(handle);
        self.stream = None;
        eprintln!("[plugin] configured '{}'", self.manifest.name);
        Ok(())
    }

    fn send_frame(&mut self, frame: Frame) -> Result<()> {
        let meta = frame.meta;
        match frame.data {
            StreamData::Audio(samples) => {
                self.ensure_stream("audio", &meta)?;
                let stdin = self
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
                write_audio(stdin, meta.seq, Self::timestamp(&meta), &samples)?;
            }
            StreamData::SoftBits(bits) => {
                self.ensure_stream("soft_bits", &meta)?;
                let stdin = self
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
                write_soft_bits(stdin, meta.seq, Self::timestamp(&meta), &bits)?;
            }
            StreamData::Bytes(bytes) => {
                self.ensure_stream("bytes", &meta)?;
                let stdin = self
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
                write_bytes(stdin, meta.seq, Self::timestamp(&meta), &bytes)?;
            }
            StreamData::Iq(_) => {
                bail!("IQ sidecar streams are not supported yet")
            }
        }
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<PluginEvent>> {
        let Some(rx) = &self.events else {
            return Ok(None);
        };
        match rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(None),
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = write_json(stdin, &json!({"type": "shutdown"}));
            let _ = stdin.flush();
        }
        self.stdin = None;
        self.reader = None;
        self.events = None;

        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(1);
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

        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        self.stream = None;
        Ok(())
    }
}

impl Drop for SidecarPlugin {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
