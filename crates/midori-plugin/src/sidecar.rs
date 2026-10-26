//! Out-of-process plugin supervisor with handshake and I/O timeouts.
//!
//! The sidecar handshake is deliberately small (protocol version 2):
//!
//! 1. plugin → host: `{"type":"hello","protocol":2,"name":"...","version":"..."}`
//! 2. host → plugin: `{"type":"configure","params":{...}}`
//! 3. plugin → host: `{"type":"ready"}`
//! 4. host → plugin: `{"type":"stream", ...}` before the first data frame
//! 5. host → plugin: audio / soft-bit / byte frames
//! 6. plugin → host: JSON `PluginEvent`s
//! 7. host → plugin: `{"type":"shutdown"}`
//!
//! A `stream` message is sent immediately when the stream kind, sample rate,
//! or VFO id changes. Frequency metadata (centre/nominal frequency, bandwidth,
//! Doppler) can move every block, so updates that only change those fields are
//! rate-limited to one message per second. Plugins must therefore tolerate
//! repeated `stream` messages and must not reset decoder state unless the kind
//! or sample rate actually changed.
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
use midori_core::{Frame, FrameMeta, StreamData, StreamKind};
use serde_json::Value;

use crate::wire::{
    WireMessage, read_message, write_audio, write_bytes, write_iq, write_json, write_soft_bits,
};
use crate::{
    ControlMessage, Manifest, PROTOCOL_VERSION, PluginEvent, PluginHost, StreamInfo,
    stream_kind_name,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
const OUTBOUND_QUEUE: usize = 32;
/// Minimum spacing between `stream` messages that only update frequency
/// metadata. Kind, sample-rate and VFO changes bypass this limit.
const STREAM_METADATA_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamKey {
    kind: StreamKind,
    sample_rate_bits: u64,
    center_freq_bits: u64,
    nominal_freq_bits: u64,
    bandwidth_bits: u64,
    doppler_bits: u64,
    doppler_rate_bits: u64,
    vfo_id: Option<u32>,
}

impl StreamKey {
    fn new(kind: StreamKind, meta: &FrameMeta) -> Self {
        Self {
            kind,
            sample_rate_bits: meta.sample_rate_hz.to_bits(),
            center_freq_bits: meta.center_freq_hz.to_bits(),
            nominal_freq_bits: meta.nominal_freq_hz.to_bits(),
            bandwidth_bits: meta.bandwidth_hz.to_bits(),
            doppler_bits: meta.doppler_hz.to_bits(),
            doppler_rate_bits: meta.doppler_rate_hz_s.to_bits(),
            vfo_id: meta.vfo_id,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StreamState {
    key: StreamKey,
    sent_at: Instant,
}

/// Decide whether a `stream` message is due.
///
/// Kind, sample rate and VFO changes are structural and always resend
/// immediately. Frequency-only metadata changes are coalesced so live
/// Doppler/AFC updates cannot fill the outbound queue.
fn stream_update_due(prev: Option<&StreamState>, next: &StreamKey, now: Instant) -> bool {
    let Some(prev) = prev else {
        return true;
    };
    if prev.key.kind != next.kind
        || prev.key.sample_rate_bits != next.sample_rate_bits
        || prev.key.vfo_id != next.vfo_id
    {
        return true;
    }
    if prev.key == *next {
        return false;
    }
    now.saturating_duration_since(prev.sent_at) >= STREAM_METADATA_INTERVAL
}

enum Outbound {
    Control(ControlMessage),
    Audio {
        seq: u64,
        timestamp_ns: Option<u64>,
        samples: Arc<[f32]>,
    },
    SoftBits {
        seq: u64,
        timestamp_ns: Option<u64>,
        bits: Arc<[u8]>,
    },
    Bytes {
        seq: u64,
        timestamp_ns: Option<u64>,
        bytes: Arc<[u8]>,
    },
    Iq {
        seq: u64,
        timestamp_ns: Option<u64>,
        samples: Arc<[midori_core::Sample]>,
    },
}

pub struct SidecarPlugin {
    manifest: Manifest,
    child: Option<Child>,
    outbound: Option<Sender<Outbound>>,
    inbound: Option<Receiver<WireMessage>>,
    writer_thread: Option<JoinHandle<()>>,
    reader_thread: Option<JoinHandle<()>>,
    stream: Option<StreamState>,
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
                        Outbound::Control(control) => serde_json::to_value(&control)
                            .map_err(anyhow::Error::from)
                            .and_then(|value| write_json(&mut writer, &value)),
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
                        Outbound::Iq {
                            seq,
                            timestamp_ns,
                            samples,
                        } => write_iq(&mut writer, seq, timestamp_ns, &samples),
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
        let (tx, rx) = bounded::<WireMessage>(512);
        let handle = thread::Builder::new()
            .name("plugin-reader".to_string())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                while let Ok(message) = read_message(&mut reader) {
                    // Block when the host is behind instead of dropping events.
                    // The worker drains this queue continuously, and stop()
                    // drains while joining the reader, so the send cannot
                    // deadlock against shutdown.
                    if tx.send(message).is_err() {
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

    fn ensure_stream(&mut self, kind: StreamKind, meta: &FrameMeta) -> Result<()> {
        let now = Instant::now();
        let key = StreamKey::new(kind, meta);
        if !stream_update_due(self.stream.as_ref(), &key, now) {
            return Ok(());
        }

        self.enqueue(Outbound::Control(ControlMessage::Stream(
            StreamInfo::from_meta(kind, meta),
        )))?;
        self.stream = Some(StreamState { key, sent_at: now });
        Ok(())
    }

    fn enqueue(&self, message: Outbound) -> Result<()> {
        let outbound = self
            .outbound
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plugin is not started"))?;
        match outbound.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => bail!("plugin outbound queue is full"),
            Err(TrySendError::Disconnected(_)) => bail!("plugin writer has stopped"),
        }
    }
}

impl PluginHost for SidecarPlugin {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn input_kind(&self) -> StreamKind {
        match &self.manifest.input {
            crate::InputSpec::Iq { .. } => StreamKind::Iq,
            crate::InputSpec::Audio { .. } => StreamKind::Audio,
            crate::InputSpec::SoftBits { .. } => StreamKind::SoftBits,
            crate::InputSpec::Bytes { .. } => StreamKind::Bytes,
        }
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

        let (protocol, name) = match serde_json::from_value::<ControlMessage>(hello) {
            Ok(ControlMessage::Hello { protocol, name, .. }) => (protocol, name),
            Ok(_) => {
                let _ = self.stop();
                bail!("plugin handshake did not start with hello");
            }
            Err(error) => {
                let _ = self.stop();
                return Err(error).context("parsing plugin hello");
            }
        };
        if protocol != PROTOCOL_VERSION {
            let _ = self.stop();
            bail!(
                "plugin '{}' speaks protocol {protocol}, engine expects {PROTOCOL_VERSION}",
                self.manifest.name
            );
        }
        let name = if name.is_empty() {
            self.manifest.name.as_str()
        } else {
            name.as_str()
        };
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
        let message = ControlMessage::Configure {
            params: params.clone(),
        };
        match outbound.send_timeout(Outbound::Control(message), WRITE_TIMEOUT) {
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
                Ok(WireMessage::Json(value)) => {
                    match serde_json::from_value::<ControlMessage>(value) {
                        Ok(ControlMessage::Ready) => break,
                        Ok(ControlMessage::Error { message }) => {
                            let _ = self.stop();
                            bail!(
                                "plugin '{}' rejected configuration: {message}",
                                self.manifest.name
                            );
                        }
                        _ => continue,
                    }
                }
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
        let kind = frame.data.kind();
        let expected = self.input_kind();
        if kind != expected {
            bail!(
                "plugin '{}' expects {} input, received {}",
                self.manifest.name,
                stream_kind_name(expected),
                stream_kind_name(kind)
            );
        }

        self.ensure_stream(kind, &meta)?;
        let message = match frame.data {
            StreamData::Audio(samples) => Outbound::Audio {
                seq: meta.seq,
                timestamp_ns: meta.timestamp_ns,
                samples,
            },
            StreamData::SoftBits(bits) => Outbound::SoftBits {
                seq: meta.seq,
                timestamp_ns: meta.timestamp_ns,
                bits,
            },
            StreamData::Bytes(bytes) => Outbound::Bytes {
                seq: meta.seq,
                timestamp_ns: meta.timestamp_ns,
                bytes,
            },
            StreamData::Iq(samples) => Outbound::Iq {
                seq: meta.seq,
                timestamp_ns: meta.timestamp_ns,
                samples,
            },
        };
        self.enqueue(message)
    }

    fn try_recv(&mut self) -> Result<Option<PluginEvent>> {
        let Some(inbound) = &self.inbound else {
            return Ok(None);
        };
        loop {
            match inbound.try_recv() {
                Ok(message) => {
                    if let Some(event) = decode_event(message) {
                        return Ok(Some(event));
                    }
                }
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

    fn stop(&mut self) -> Result<Vec<PluginEvent>> {
        // Queue the shutdown request while the writer is still alive, then
        // drop the sender. The writer drains anything already queued, flushes
        // its buffer, and closes the child's stdin so it sees EOF and exits.
        if let Some(outbound) = self.outbound.as_ref() {
            let _ = outbound.send_timeout(
                Outbound::Control(ControlMessage::Shutdown),
                Duration::from_millis(200),
            );
        }
        self.outbound = None;

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

        // Keep draining while the reader is alive: it uses a blocking send,
        // so joining it first would deadlock if the plugin flushed more than
        // the queue depth. Events emitted after the shutdown request (final CW
        // text, SSDV partial images, ...) are returned to the caller.
        let mut events = Vec::new();
        let mut reader = self.reader_thread.take();
        let drain_deadline = Instant::now() + STOP_TIMEOUT;
        let mut drain_timed_out = false;
        if let Some(inbound) = self.inbound.take() {
            loop {
                if reader.as_ref().is_none_or(JoinHandle::is_finished) {
                    while let Ok(message) = inbound.try_recv() {
                        if let Some(event) = decode_event(message) {
                            events.push(event);
                        }
                    }
                    break;
                }
                if Instant::now() >= drain_deadline {
                    drain_timed_out = true;
                    break;
                }
                match inbound.recv_timeout(Duration::from_millis(20)) {
                    Ok(message) => {
                        if let Some(event) = decode_event(message) {
                            events.push(event);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        // A reader wedged on a stdout handle inherited by a grandchild is
        // detached: joining it would hang shutdown forever.
        if !drain_timed_out && let Some(handle) = reader.take() {
            let _ = handle.join();
        }

        self.stream = None;
        self.configured = false;
        self.closed_reported = false;
        Ok(events)
    }
}

fn decode_event(message: WireMessage) -> Option<PluginEvent> {
    match message {
        WireMessage::Json(value) => Some(match serde_json::from_value::<PluginEvent>(value) {
            Ok(event) => event,
            Err(error) => PluginEvent::Error {
                message: format!("invalid plugin event: {error}"),
                fatal: false,
            },
        }),
        _ => None,
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
    use midori_core::BlockFlags;

    fn audio_manifest() -> Manifest {
        Manifest {
            name: "test".to_string(),
            version: "0.1.0".to_string(),
            protocol: PROTOCOL_VERSION,
            command: vec!["true".to_string()],
            input: InputSpec::Audio {
                sample_rate_hz: 48_000.0,
                channels: 1,
            },
            outputs: vec![OutputKind::Display],
            params: vec![],
            demod_modes: Vec::new(),
        }
    }

    fn frame_meta(center_freq_hz: f64, sample_rate_hz: f64) -> FrameMeta {
        FrameMeta {
            seq: 0,
            start_sample: 0,
            timestamp_ns: Some(1),
            sample_rate_hz,
            center_freq_hz,
            nominal_freq_hz: center_freq_hz,
            bandwidth_hz: 3_000.0,
            vfo_id: Some(0),
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
            flags: BlockFlags::default(),
        }
    }

    fn recv_stream(rx: &Receiver<Outbound>) -> Value {
        match rx.try_recv() {
            Ok(Outbound::Control(control)) => serde_json::to_value(control).unwrap(),
            Ok(_) => panic!("expected a stream JSON message"),
            Err(error) => panic!("missing stream message: {error}"),
        }
    }

    #[test]
    fn frequency_only_updates_are_rate_limited() {
        let start = Instant::now();
        let mut meta = frame_meta(144_000_000.0, 48_000.0);
        let key = StreamKey::new(StreamKind::Audio, &meta);
        assert!(stream_update_due(None, &key, start));
        let state = StreamState {
            key,
            sent_at: start,
        };

        // A VFO whose Doppler/AFC moves every block must not resend more than
        // once per second.
        for step in 1..=9 {
            meta.center_freq_hz += 10.0;
            meta.doppler_hz += 1.0;
            let next = StreamKey::new(StreamKind::Audio, &meta);
            let now = start + Duration::from_millis(step * 100);
            assert!(!stream_update_due(Some(&state), &next, now));
        }

        meta.center_freq_hz += 10.0;
        let next = StreamKey::new(StreamKind::Audio, &meta);
        assert!(stream_update_due(
            Some(&state),
            &next,
            start + STREAM_METADATA_INTERVAL
        ));
    }

    #[test]
    fn structural_changes_resend_immediately() {
        let start = Instant::now();
        let meta = frame_meta(144_000_000.0, 48_000.0);
        let key = StreamKey::new(StreamKind::Audio, &meta);
        let state = StreamState {
            key,
            sent_at: start,
        };
        let now = start + Duration::from_millis(1);

        let mut rate_changed = meta;
        rate_changed.sample_rate_hz = 96_000.0;
        assert!(stream_update_due(
            Some(&state),
            &StreamKey::new(StreamKind::Audio, &rate_changed),
            now
        ));

        assert!(stream_update_due(
            Some(&state),
            &StreamKey::new(StreamKind::Iq, &meta),
            now
        ));

        let mut vfo_changed = meta;
        vfo_changed.vfo_id = Some(3);
        assert!(stream_update_due(
            Some(&state),
            &StreamKey::new(StreamKind::Audio, &vfo_changed),
            now
        ));
    }

    #[test]
    fn ensure_stream_coalesces_frequency_updates() {
        let mut plugin = SidecarPlugin::new(audio_manifest()).unwrap();
        let (tx, rx) = bounded(OUTBOUND_QUEUE);
        plugin.outbound = Some(tx);

        let mut meta = frame_meta(144_000_000.0, 48_000.0);
        plugin.ensure_stream(StreamKind::Audio, &meta).unwrap();
        assert_eq!(recv_stream(&rx)["kind"], "audio");

        for _ in 0..32 {
            meta.center_freq_hz += 10.0;
            meta.doppler_hz += 1.0;
            plugin.ensure_stream(StreamKind::Audio, &meta).unwrap();
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        // A sample-rate change bypasses the metadata rate limit.
        meta.sample_rate_hz = 96_000.0;
        plugin.ensure_stream(StreamKind::Audio, &meta).unwrap();
        assert_eq!(recv_stream(&rx)["sample_rate_hz"], 96_000.0);
    }

    #[test]
    fn send_frame_rejects_mismatched_input_kind() {
        let mut plugin = SidecarPlugin::new(audio_manifest()).unwrap();
        let bytes: Arc<[u8]> = Arc::from(vec![1u8, 2, 3].into_boxed_slice());
        let frame = Frame::new(
            frame_meta(144_000_000.0, 48_000.0),
            StreamData::Bytes(bytes),
        );

        let error = plugin.send_frame(frame).unwrap_err();
        assert!(
            format!("{error:#}").contains("expects audio input, received bytes"),
            "unexpected error: {error:#}"
        );
    }

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
            demod_modes: Vec::new(),
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
