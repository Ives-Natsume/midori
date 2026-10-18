//! Length-prefixed binary framing for plugin sidecars.
//!
//! Header:
//!
//! ```text
//! u32 payload_len (LE)
//! u8  kind
//! u64 seq (LE)
//! f64 timestamp_ns (LE)
//! payload
//! ```
//!
//! Control and event messages are JSON payloads (`kind = 1`). Audio is
//! interleaved f32 LE samples (`kind = 2`); soft bits and bytes use kinds 3
//! and 4. Keeping control JSON separate from the data plane means plugins can
//! be written in any language.

use std::io::{Read, Write};

use anyhow::{Result, bail};
use serde_json::Value;

pub const HEADER_LEN: usize = 21;
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

pub const KIND_JSON: u8 = 1;
pub const KIND_AUDIO: u8 = 2;
pub const KIND_SOFTBITS: u8 = 3;
pub const KIND_BYTES: u8 = 4;

#[derive(Debug, Clone)]
pub enum WireMessage {
    Json(Value),
    Audio {
        seq: u64,
        timestamp_ns: f64,
        samples: Vec<f32>,
    },
    SoftBits {
        seq: u64,
        timestamp_ns: f64,
        bits: Vec<u8>,
    },
    Bytes {
        seq: u64,
        timestamp_ns: f64,
        bytes: Vec<u8>,
    },
}

pub fn write_json<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    write_frame(writer, KIND_JSON, 0, 0.0, &payload)
}

pub fn write_audio<W: Write>(
    writer: &mut W,
    seq: u64,
    timestamp_ns: f64,
    samples: &[f32],
) -> Result<()> {
    let mut payload = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        payload.extend_from_slice(&sample.to_le_bytes());
    }
    write_frame(writer, KIND_AUDIO, seq, timestamp_ns, &payload)
}

pub fn write_soft_bits<W: Write>(
    writer: &mut W,
    seq: u64,
    timestamp_ns: f64,
    bits: &[u8],
) -> Result<()> {
    write_frame(writer, KIND_SOFTBITS, seq, timestamp_ns, bits)
}

pub fn write_bytes<W: Write>(
    writer: &mut W,
    seq: u64,
    timestamp_ns: f64,
    bytes: &[u8],
) -> Result<()> {
    write_frame(writer, KIND_BYTES, seq, timestamp_ns, bytes)
}

fn write_frame<W: Write>(
    writer: &mut W,
    kind: u8,
    seq: u64,
    timestamp_ns: f64,
    payload: &[u8],
) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| anyhow::anyhow!("frame too large"))?;
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&len.to_le_bytes());
    header[4] = kind;
    header[5..13].copy_from_slice(&seq.to_le_bytes());
    header[13..21].copy_from_slice(&timestamp_ns.to_le_bytes());

    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_message<R: Read>(reader: &mut R) -> Result<WireMessage> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;

    let len = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        bail!("plugin frame payload is too large: {len}");
    }
    let kind = header[4];
    let seq = u64::from_le_bytes(header[5..13].try_into().unwrap());
    let timestamp_ns = f64::from_le_bytes(header[13..21].try_into().unwrap());

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;

    match kind {
        KIND_JSON => Ok(WireMessage::Json(serde_json::from_slice(&payload)?)),
        KIND_AUDIO => {
            if !payload.len().is_multiple_of(4) {
                bail!("audio frame length is not a multiple of 4");
            }
            let samples = payload
                .chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect();
            Ok(WireMessage::Audio {
                seq,
                timestamp_ns,
                samples,
            })
        }
        KIND_SOFTBITS => Ok(WireMessage::SoftBits {
            seq,
            timestamp_ns,
            bits: payload,
        }),
        KIND_BYTES => Ok(WireMessage::Bytes {
            seq,
            timestamp_ns,
            bytes: payload,
        }),
        other => bail!("unknown plugin frame kind {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn json_roundtrip() {
        let mut buffer = Vec::new();
        write_json(&mut buffer, &serde_json::json!({"type":"hello"})).unwrap();
        let message = read_message(&mut Cursor::new(buffer)).unwrap();
        match message {
            WireMessage::Json(value) => assert_eq!(value["type"], "hello"),
            _ => panic!("expected JSON"),
        }
    }

    #[test]
    fn audio_roundtrip() {
        let mut buffer = Vec::new();
        write_audio(&mut buffer, 7, 1234.5, &[0.25, -0.5, 0.0]).unwrap();
        let message = read_message(&mut Cursor::new(buffer)).unwrap();
        match message {
            WireMessage::Audio {
                seq,
                timestamp_ns,
                samples,
            } => {
                assert_eq!(seq, 7);
                assert_eq!(timestamp_ns, 1234.5);
                assert_eq!(samples, vec![0.25, -0.5, 0.0]);
            }
            _ => panic!("expected audio"),
        }
    }
}
