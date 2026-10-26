//! Length-prefixed binary framing for plugin sidecars.
//!
//! Header (protocol version 2):
//!
//! ```text
//! u32 payload_len (LE)
//! u8  kind
//! u64 seq (LE)
//! u64 timestamp_ns (LE, UTC nanoseconds or [`NO_TIMESTAMP`])
//! payload
//! ```
//!
//! The timestamp is an exact `u64` so it can carry `midori_core`'s UTC
//! nanosecond clock without the precision loss of a `f64`. An unknown timestamp
//! is encoded as [`NO_TIMESTAMP`] (`u64::MAX`); the Rust API uses
//! `Option<u64>` and only applies the sentinel on the wire.
//!
//! Control and event messages are JSON payloads (`kind = 1`). Audio is
//! interleaved f32 LE samples (`kind = 2`); soft bits and bytes use kinds 3
//! and 4. Keeping control JSON separate from the data plane means plugins can
//! be written in any language.

use std::io::{Read, Write};

use anyhow::{Result, bail};
use midori_core::Sample;
use num_complex::Complex32;
use serde_json::Value;

pub const HEADER_LEN: usize = 21;
/// Wire sentinel for "timestamp unknown". Never a valid UTC nanosecond value.
pub const NO_TIMESTAMP: u64 = u64::MAX;
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

pub const KIND_JSON: u8 = 1;
pub const KIND_AUDIO: u8 = 2;
pub const KIND_SOFTBITS: u8 = 3;
pub const KIND_BYTES: u8 = 4;
pub const KIND_IQ: u8 = 5;

#[derive(Debug, Clone)]
pub enum WireMessage {
    Json(Value),
    Audio {
        seq: u64,
        timestamp_ns: Option<u64>,
        samples: Vec<f32>,
    },
    SoftBits {
        seq: u64,
        timestamp_ns: Option<u64>,
        bits: Vec<u8>,
    },
    Bytes {
        seq: u64,
        timestamp_ns: Option<u64>,
        bytes: Vec<u8>,
    },
    Iq {
        seq: u64,
        timestamp_ns: Option<u64>,
        samples: Vec<Sample>,
    },
}

pub fn write_json<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    write_frame(writer, KIND_JSON, 0, None, &payload)
}

pub fn write_audio<W: Write>(
    writer: &mut W,
    seq: u64,
    timestamp_ns: Option<u64>,
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
    timestamp_ns: Option<u64>,
    bits: &[u8],
) -> Result<()> {
    write_frame(writer, KIND_SOFTBITS, seq, timestamp_ns, bits)
}

pub fn write_bytes<W: Write>(
    writer: &mut W,
    seq: u64,
    timestamp_ns: Option<u64>,
    bytes: &[u8],
) -> Result<()> {
    write_frame(writer, KIND_BYTES, seq, timestamp_ns, bytes)
}

/// Write interleaved complex samples as f32 LE pairs. This is the sidecar IQ
/// path: decoders receive baseband IQ at a rate they request in their
/// manifest, so they do not need to know about the SDR's sample rate.
pub fn write_iq<W: Write>(
    writer: &mut W,
    seq: u64,
    timestamp_ns: Option<u64>,
    samples: &[Sample],
) -> Result<()> {
    let mut payload = Vec::with_capacity(samples.len() * 8);
    for sample in samples {
        payload.extend_from_slice(&sample.re.to_le_bytes());
        payload.extend_from_slice(&sample.im.to_le_bytes());
    }
    write_frame(writer, KIND_IQ, seq, timestamp_ns, &payload)
}

fn write_frame<W: Write>(
    writer: &mut W,
    kind: u8,
    seq: u64,
    timestamp_ns: Option<u64>,
    payload: &[u8],
) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| anyhow::anyhow!("frame too large"))?;
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&len.to_le_bytes());
    header[4] = kind;
    header[5..13].copy_from_slice(&seq.to_le_bytes());
    header[13..21].copy_from_slice(&timestamp_ns.unwrap_or(NO_TIMESTAMP).to_le_bytes());

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
    let timestamp_ns = match u64::from_le_bytes(header[13..21].try_into().unwrap()) {
        NO_TIMESTAMP => None,
        timestamp_ns => Some(timestamp_ns),
    };

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
        KIND_IQ => {
            if !payload.len().is_multiple_of(8) {
                bail!("IQ frame length is not a multiple of 8");
            }
            let samples = payload
                .chunks_exact(8)
                .map(|chunk| {
                    Complex32::new(
                        f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                        f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
                    )
                })
                .collect();
            Ok(WireMessage::Iq {
                seq,
                timestamp_ns,
                samples,
            })
        }
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

    const TIMESTAMP_NS: u64 = 1_792_466_014_123_456_789;

    #[test]
    fn header_length_is_stable() {
        assert_eq!(HEADER_LEN, 21);
        let mut buffer = Vec::new();
        write_audio(&mut buffer, 1, Some(TIMESTAMP_NS), &[0.0, 1.0]).unwrap();
        assert_eq!(buffer.len(), HEADER_LEN + 8);
    }

    #[test]
    fn audio_roundtrip() {
        let mut buffer = Vec::new();
        write_audio(&mut buffer, 7, Some(1234), &[0.25, -0.5, 0.0]).unwrap();
        let message = read_message(&mut Cursor::new(buffer)).unwrap();
        match message {
            WireMessage::Audio {
                seq,
                timestamp_ns,
                samples,
            } => {
                assert_eq!(seq, 7);
                assert_eq!(timestamp_ns, Some(1234));
                assert_eq!(samples, vec![0.25, -0.5, 0.0]);
            }
            _ => panic!("expected audio"),
        }
    }

    #[test]
    fn timestamp_roundtrip_is_exact() {
        let mut buffer = Vec::new();
        write_audio(&mut buffer, 1, Some(TIMESTAMP_NS), &[0.0]).unwrap();
        match read_message(&mut Cursor::new(buffer)).unwrap() {
            WireMessage::Audio { timestamp_ns, .. } => {
                assert_eq!(timestamp_ns, Some(TIMESTAMP_NS));
            }
            _ => panic!("expected audio"),
        }
    }

    #[test]
    fn missing_timestamp_roundtrips_as_none() {
        let mut buffer = Vec::new();
        write_audio(&mut buffer, 1, None, &[0.0]).unwrap();
        match read_message(&mut Cursor::new(buffer)).unwrap() {
            WireMessage::Audio { timestamp_ns, .. } => assert_eq!(timestamp_ns, None),
            _ => panic!("expected audio"),
        }
    }

    #[test]
    fn zero_timestamp_is_not_the_sentinel() {
        let mut buffer = Vec::new();
        write_audio(&mut buffer, 1, Some(0), &[0.0]).unwrap();
        match read_message(&mut Cursor::new(buffer)).unwrap() {
            WireMessage::Audio { timestamp_ns, .. } => assert_eq!(timestamp_ns, Some(0)),
            _ => panic!("expected audio"),
        }
    }

    #[test]
    fn iq_roundtrip() {
        let samples = [Complex32::new(0.5, -0.25), Complex32::new(-1.0, 0.0)];
        let mut buffer = Vec::new();
        write_iq(&mut buffer, 9, Some(TIMESTAMP_NS), &samples).unwrap();
        let message = read_message(&mut Cursor::new(buffer)).unwrap();
        match message {
            WireMessage::Iq {
                seq,
                timestamp_ns,
                samples: decoded,
            } => {
                assert_eq!(seq, 9);
                assert_eq!(timestamp_ns, Some(TIMESTAMP_NS));
                assert_eq!(decoded, samples.to_vec());
            }
            _ => panic!("expected IQ"),
        }
    }
}
