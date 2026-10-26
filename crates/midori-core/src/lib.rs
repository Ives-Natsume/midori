//! Core types and contracts shared by every Midori component.
//!
//! The core deliberately knows nothing about a particular SDR, UI toolkit, or
//! decoder. Hardware backends produce [`IqBlock`]s, channels turn them into
//! typed [`Frame`]s, and sinks (waterfall, audio, recorder, decoder sidecars)
//! consume the output.
//!
//! Units are always SI: hertz, seconds, dBFS, UTC nanoseconds. Keep it that way.

use std::sync::Arc;

use num_complex::Complex32;
use serde::{Deserialize, Serialize};

/// The sample type used inside the engine.
///
/// Sources normalise their native format to `cf32` at the boundary. Keeping a
/// single internal format avoids format branches all over the DSP code.
pub type Sample = Complex32;

/// On-disk / on-wire sample formats a source or recorder may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    /// Interleaved signed 16-bit little-endian I/Q.
    Ci16Le,
    /// Interleaved 32-bit float little-endian I/Q.
    Cf32Le,
}

/// Everything a downstream stage needs to interpret a block.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamSpec {
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    /// Bandwidth of the signal represented by this stream. For IQ this is the
    /// source RF bandwidth; for audio it is the audio bandwidth.
    pub bandwidth_hz: f64,
    pub format: SampleFormat,
    /// Integer value corresponding to a full-scale sample in the source's
    /// native format. Pluto's 12-bit ADC, stored in 16-bit words, uses 2048.
    /// `cf32` sources use 1.0.
    pub full_scale: f64,
}

/// Non-fatal conditions attached to a block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockFlags {
    /// The source reported a buffer overrun before this block.
    pub overflow: bool,
    /// Samples are missing or discontinuous relative to the previous block.
    pub discontinuity: bool,
}

/// A contiguous block of complex baseband samples.
///
/// `samples` is shared so one source block can feed several independent sinks
/// (waterfall, recorder, several decoder channels) without copying.
#[derive(Debug, Clone)]
pub struct IqBlock {
    pub samples: Arc<[Sample]>,
    pub spec: StreamSpec,
    /// Index of the first sample since the start of the stream.
    pub start_sample: u64,
    /// Wall-clock timestamp of the first sample, when the source can provide
    /// one. Pluto does not have a per-sample clock, so this is captured when
    /// the first buffer arrives.
    pub timestamp_ns: Option<u64>,
    pub flags: BlockFlags,
}

impl IqBlock {
    pub fn new(samples: Arc<[Sample]>, spec: StreamSpec, start_sample: u64) -> Self {
        Self {
            samples,
            spec,
            start_sample,
            timestamp_ns: None,
            flags: BlockFlags::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Metadata for this block as a generic frame.
    pub fn meta(&self, seq: u64) -> FrameMeta {
        FrameMeta {
            seq,
            start_sample: self.start_sample,
            timestamp_ns: self.timestamp_ns,
            sample_rate_hz: self.spec.sample_rate_hz,
            center_freq_hz: self.spec.center_freq_hz,
            nominal_freq_hz: self.spec.center_freq_hz,
            bandwidth_hz: self.spec.bandwidth_hz,
            vfo_id: None,
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
            flags: self.flags,
        }
    }

    /// Convert into a generic IQ frame, typically for a wideband sink such as
    /// a recorder or an IQ decoder plugin.
    pub fn into_frame(self, seq: u64) -> Frame {
        let meta = self.meta(seq);
        Frame {
            meta,
            data: StreamData::Iq(self.samples),
        }
    }
}

/// Metadata carried by every frame between the engine and a sink/plugin.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameMeta {
    /// Monotonic sequence number within a stream.
    pub seq: u64,
    /// Index of the first sample since the start of the stream.
    pub start_sample: u64,
    /// UTC timestamp of the first sample, when known.
    pub timestamp_ns: Option<u64>,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub bandwidth_hz: f64,
    /// Nominal (un-Doppler-shifted) frequency the user/preset asked for.
    #[serde(default)]
    pub nominal_freq_hz: f64,
    /// Which digital VFO produced this frame, if any.
    pub vfo_id: Option<VfoId>,
    /// Current Doppler correction added to the VFO offset.
    #[serde(default)]
    pub doppler_hz: f64,
    /// Current Doppler rate, Hz/s.
    #[serde(default)]
    pub doppler_rate_hz_s: f64,
    pub flags: BlockFlags,
}

/// The payload types that can cross a stream boundary.
#[derive(Debug, Clone)]
pub enum StreamData {
    /// Complex baseband IQ.
    Iq(Arc<[Sample]>),
    /// Mono real audio, normalised to roughly [-1, 1].
    Audio(Arc<[f32]>),
    /// Soft bits / LLRs, one byte per symbol unless documented otherwise.
    SoftBits(Arc<[u8]>),
    /// Framed or raw bytes, e.g. AX.25 frames or SSDV packets.
    Bytes(Arc<[u8]>),
}

impl StreamData {
    pub fn kind(&self) -> StreamKind {
        match self {
            Self::Iq(_) => StreamKind::Iq,
            Self::Audio(_) => StreamKind::Audio,
            Self::SoftBits(_) => StreamKind::SoftBits,
            Self::Bytes(_) => StreamKind::Bytes,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Iq(samples) => samples.len(),
            Self::Audio(samples) => samples.len(),
            Self::SoftBits(bits) => bits.len(),
            Self::Bytes(bytes) => bytes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Iq,
    Audio,
    SoftBits,
    Bytes,
}

/// A typed frame plus its timing/frequency metadata.
#[derive(Debug, Clone)]
pub struct Frame {
    pub meta: FrameMeta,
    pub data: StreamData,
}

impl Frame {
    pub fn new(meta: FrameMeta, data: StreamData) -> Self {
        Self { meta, data }
    }
}

/// Digital VFO / channel definition. The hardware LO stays fixed; this offset
/// selects a channel inside the captured band.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfoConfig {
    pub id: VfoId,
    /// Offset from the source centre frequency.
    pub offset_hz: f64,
    /// Channel bandwidth.
    pub bandwidth_hz: f64,
    pub demod: DemodMode,
    pub audio_rate_hz: f64,
    /// Intermediate complex sample rate after channel selection.
    pub channel_rate_hz: f64,
    /// Audio bandwidth passed by the demodulator.
    pub audio_bandwidth_hz: f64,
    /// FM de-emphasis in microseconds; 0 disables it.
    pub deemphasis_us: f64,
    /// CW beat-frequency oscillator, Hz. The CW demod places the carrier at
    /// this audio tone.
    pub bfo_hz: f64,
}

pub type VfoId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemodMode {
    Am,
    Fm,
    Usb,
    Lsb,
    Cw,
    /// FM demodulation with settings tuned for SSTV (narrow audio passband,
    /// no de-emphasis). The actual image decoding happens in a sidecar.
    Sstv,
    /// Complex data channel for linear BPSK downlinks such as ASRTU-1 SSDV.
    /// The channel is mixed, filtered, and resampled to a fixed IQ rate; symbol
    /// timing, FEC, and framing are handled by the SSDV sidecar.
    Bpsk,
    /// Real 12 kHz intermediate-frequency audio, passed through at the file's
    /// native rate. Useful for receivers that expose mono 48 kHz IF instead of
    /// zero-IF I/Q.
    BpskIf,
}

impl DemodMode {
    /// Every demod mode, ordered for UI presentation. Unimplemented modes are
    /// included here so config keys and serde round-trips stay stable; call
    /// [`Self::implemented`] for the selectable subset.
    pub const ALL: &'static [DemodMode] = &[
        Self::Fm,
        Self::Am,
        Self::Usb,
        Self::Lsb,
        Self::Cw,
        Self::Sstv,
        Self::Bpsk,
        Self::BpskIf,
    ];

    /// Human-readable name for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Am => "AM",
            Self::Fm => "FM",
            Self::Usb => "USB",
            Self::Lsb => "LSB",
            Self::Cw => "CW",
            Self::Sstv => "SSTV",
            Self::Bpsk => "BPSK",
            Self::BpskIf => "BPSK IF",
        }
    }

    /// Whether the engine can build a demodulator for this mode.
    pub fn is_implemented(self) -> bool {
        match self {
            Self::Am | Self::Fm | Self::Cw | Self::Sstv | Self::Bpsk | Self::BpskIf => true,
            Self::Usb | Self::Lsb => false,
        }
    }

    /// Kind of frame the channel produces. Unimplemented modes report the kind
    /// they are expected to produce once implemented.
    pub fn output_kind(self) -> StreamKind {
        match self {
            Self::Am | Self::Fm | Self::Cw | Self::Sstv | Self::BpskIf => StreamKind::Audio,
            Self::Bpsk => StreamKind::Iq,
            Self::Usb | Self::Lsb => StreamKind::Audio,
        }
    }

    /// Stable snake_case identifier used in config files and wire messages.
    /// This is the same string serde produces with `rename_all = "snake_case"`.
    pub fn config_key(self) -> &'static str {
        match self {
            Self::Am => "am",
            Self::Fm => "fm",
            Self::Usb => "usb",
            Self::Lsb => "lsb",
            Self::Cw => "cw",
            Self::Sstv => "sstv",
            Self::Bpsk => "bpsk",
            Self::BpskIf => "bpsk_if",
        }
    }

    /// Modes the engine and UI can actually use.
    pub fn implemented() -> impl Iterator<Item = DemodMode> {
        Self::ALL
            .iter()
            .copied()
            .filter(|mode| mode.is_implemented())
    }
}

impl VfoConfig {
    pub fn new(id: VfoId, offset_hz: f64, bandwidth_hz: f64, demod: DemodMode) -> Self {
        Self {
            id,
            offset_hz,
            bandwidth_hz,
            demod,
            audio_rate_hz: 48_000.0,
            channel_rate_hz: 240_000.0,
            audio_bandwidth_hz: 15_000.0,
            deemphasis_us: 50.0,
            bfo_hz: 700.0,
        }
    }
}

/// Orbital tracking parameters. The SGP4 propagation and pass prediction live
/// in `midori-orbit`; this type is the core-side contract used by presets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackingConfig {
    pub norad_id: u32,
    pub downlink_hz: f64,
    pub uplink_hz: Option<f64>,
    /// TLE lines, if already fetched. A service can refresh them later.
    pub tle: Option<String>,
}

/// Supplies a Doppler correction for a given UTC time.
pub trait DopplerSource: Send {
    /// Correction to add to the VFO offset. Positive means the observed
    /// signal is higher than the nominal frequency.
    fn offset_hz(&self, utc_ns: u64) -> f64;

    fn rate_hz_s(&self, _utc_ns: u64) -> f64 {
        0.0
    }

    /// Update the nominal signal frequency the correction is computed for.
    /// Called when the hardware LO changes.
    fn set_nominal_hz(&mut self, _hz: f64) {}
}

/// A fixed Doppler offset, useful for tests and manual operation.
#[derive(Debug, Clone, Copy)]
pub struct FixedDoppler {
    pub offset_hz: f64,
}

impl DopplerSource for FixedDoppler {
    fn offset_hz(&self, _utc_ns: u64) -> f64 {
        self.offset_hz
    }
}

/// A channel turns wideband IQ into one or more typed frames (audio, soft
/// bits, bytes, or a narrower IQ stream).
pub trait Channel: Send {
    fn config(&self) -> &VfoConfig;

    /// Called once the source sample rate/centre frequency are known.
    fn configure(&mut self, _spec: &StreamSpec) -> anyhow::Result<()> {
        Ok(())
    }

    /// Apply the current Doppler correction. `offset_hz` is an absolute shift
    /// added to the channel offset; `rate_hz_s` is informational and may be
    /// exposed in frame metadata.
    fn set_doppler(&mut self, _offset_hz: f64, _rate_hz_s: f64) {}

    /// Retune the digital VFO inside the captured band. Used by file playback
    /// and, eventually, click-to-tune without moving the hardware LO.
    fn set_offset(&mut self, _offset_hz: f64) {}

    /// Enable or disable an automatic carrier tracker for channels that support
    /// one. The default implementation ignores the request.
    fn set_afc(&mut self, _enabled: bool) {}

    /// Rebuild the channel in place with a new VFO/demod configuration.
    ///
    /// Channels that do not support live reconfiguration keep the default
    /// implementation, which returns an error rather than silently ignoring
    /// the request: callers must not be told a change took effect when it did
    /// not.
    fn reconfigure(&mut self, _config: VfoConfig, _spec: &StreamSpec) -> anyhow::Result<()> {
        anyhow::bail!("channel does not support reconfiguration")
    }

    /// Process one source block. Implementations append zero or more frames.
    fn process(&mut self, iq: &IqBlock, out: &mut Vec<Frame>) -> anyhow::Result<()>;
}

/// A consumer of frames: recorder, audio device, waterfall, plugin, ...
pub trait Sink: Send {
    fn on_frame(&mut self, frame: Frame) -> anyhow::Result<()>;

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStats {
    pub blocks: u64,
    pub samples: u64,
    pub frames: u64,
    /// Frames produced by the session that were never handed to a sink because
    /// that sink had been disabled after an earlier `on_frame` error. The frame
    /// on which a sink failed is not counted; only the frames skipped
    /// afterwards. One frame skipped for two disabled sinks counts twice.
    pub dropped: u64,
}

/// The runtime that owns a source, its channels, and its sinks.
///
/// `midori-engine::LiveSession` is the concrete implementation used by the
/// application. The application talks to a session rather than to libiio
/// directly.
pub trait Session: Send {
    fn spec(&self) -> Option<StreamSpec>;

    fn start(&mut self) -> anyhow::Result<()>;

    /// Read and process one source block.
    fn step(&mut self) -> anyhow::Result<SessionStats>;

    /// Flush sinks and stop the source. Implementations should always attempt
    /// to stop the source even when a sink flush fails, and return the error
    /// that occurred.
    fn stop(&mut self) -> anyhow::Result<()>;
}

/// Gain control modes exposed by AD9361-style front ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GainMode {
    #[default]
    Manual,
    SlowAttack,
    FastAttack,
    Hybrid,
}

impl GainMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::SlowAttack => "slow_attack",
            Self::FastAttack => "fast_attack",
            Self::Hybrid => "hybrid",
        }
    }
}

/// Requested source configuration.
#[derive(Debug, Clone)]
pub struct SourceConfig {
    /// Hardware local-oscillator frequency.
    pub center_freq_hz: f64,
    pub sample_rate_hz: f64,
    pub bandwidth_hz: f64,
    pub gain_db: f64,
    pub gain_mode: GainMode,
    /// Backend-specific port name, e.g. `A_BALANCED` for Pluto RX1A.
    pub rf_port: Option<String>,
    /// Samples per channel requested from the backend buffer.
    pub buffer_samples: usize,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            center_freq_hz: 120_000_000.0,
            sample_rate_hz: 2_084_000.0,
            bandwidth_hz: 2_000_000.0,
            gain_db: 40.0,
            gain_mode: GainMode::Manual,
            rf_port: Some("A_BALANCED".to_string()),
            buffer_samples: 32768,
        }
    }
}

/// Static capabilities of a source, used by the UI and by preset validation.
#[derive(Debug, Clone)]
pub struct SourceCaps {
    pub name: String,
    pub min_freq_hz: f64,
    pub max_freq_hz: f64,
    pub min_sample_rate_hz: f64,
    pub max_sample_rate_hz: f64,
    pub gain_range_db: (f64, f64),
    pub rf_ports: Vec<String>,
}

/// A source of IQ samples.
///
/// This is intentionally a blocking, pull-based interface for v1. The engine
/// wraps it in a thread and a bounded channel when it needs streaming; keeping
/// the hardware interface simple makes it easy to write file/network sources,
/// tests, and CLI tools.
pub trait IqSource: Send {
    fn caps(&self) -> SourceCaps;

    /// Apply a configuration. Returns the actual stream parameters, which may
    /// differ slightly from the request (PLL rounding, gain steps, ...).
    fn configure(&mut self, config: SourceConfig) -> anyhow::Result<StreamSpec>;

    fn start(&mut self) -> anyhow::Result<()>;

    /// Block until the next buffer is available.
    fn read_block(&mut self) -> anyhow::Result<IqBlock>;

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn set_frequency(&mut self, hz: f64) -> anyhow::Result<()>;

    fn set_gain(&mut self, db: f64) -> anyhow::Result<()>;

    /// True when a finite source (file playback) has delivered its last block.
    /// Live sources keep returning false.
    fn is_finished(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use serde::Serialize;
    use serde::ser::Impossible;

    use super::*;

    /// Number of `DemodMode` variants. Update together with `variant_metadata`;
    /// the exhaustive match there makes a forgotten update a compile error.
    const VARIANT_COUNT: usize = 8;

    /// Exhaustive per-variant metadata used to prove `ALL` and the accessors
    /// stay in sync. Adding a variant fails to compile until this match and
    /// `VARIANT_COUNT` are updated.
    fn variant_metadata(mode: DemodMode) -> (usize, &'static str, &'static str, bool, StreamKind) {
        match mode {
            DemodMode::Am => (0, "AM", "am", true, StreamKind::Audio),
            DemodMode::Fm => (1, "FM", "fm", true, StreamKind::Audio),
            DemodMode::Usb => (2, "USB", "usb", false, StreamKind::Audio),
            DemodMode::Lsb => (3, "LSB", "lsb", false, StreamKind::Audio),
            DemodMode::Cw => (4, "CW", "cw", true, StreamKind::Audio),
            DemodMode::Sstv => (5, "SSTV", "sstv", true, StreamKind::Audio),
            DemodMode::Bpsk => (6, "BPSK", "bpsk", true, StreamKind::Iq),
            DemodMode::BpskIf => (7, "BPSK IF", "bpsk_if", true, StreamKind::Audio),
        }
    }

    #[test]
    fn all_contains_every_variant_and_metadata_matches() {
        let mut seen: [Option<DemodMode>; VARIANT_COUNT] = [None; VARIANT_COUNT];

        for &mode in DemodMode::ALL {
            let (index, label, key, implemented, kind) = variant_metadata(mode);
            assert!(index < VARIANT_COUNT);
            assert!(
                seen[index].is_none(),
                "duplicate {mode:?} in DemodMode::ALL"
            );
            seen[index] = Some(mode);

            assert_eq!(mode.label(), label);
            assert_eq!(mode.config_key(), key);
            assert_eq!(mode.is_implemented(), implemented);
            assert_eq!(mode.output_kind(), kind);
            assert_eq!(serialized_variant(mode), key);
        }

        assert!(
            seen.iter().all(Option::is_some),
            "DemodMode::ALL is missing a variant"
        );
    }

    #[test]
    fn implemented_filters_reserved_modes() {
        let modes: Vec<_> = DemodMode::implemented().collect();
        assert_eq!(
            modes,
            [
                DemodMode::Fm,
                DemodMode::Am,
                DemodMode::Cw,
                DemodMode::Sstv,
                DemodMode::Bpsk,
                DemodMode::BpskIf,
            ]
        );
        assert!(modes.iter().all(|mode| mode.is_implemented()));
        assert!(!modes.contains(&DemodMode::Usb));
        assert!(!modes.contains(&DemodMode::Lsb));
    }

    /// Capture the variant name serde writes for a unit enum.
    fn serialized_variant(mode: DemodMode) -> &'static str {
        mode.serialize(VariantSerializer).expect("enum serialises")
    }

    #[derive(Debug)]
    struct Unsupported;

    impl fmt::Display for Unsupported {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("only unit variants are supported")
        }
    }

    impl std::error::Error for Unsupported {}

    impl serde::ser::Error for Unsupported {
        fn custom<T: fmt::Display>(_message: T) -> Self {
            Self
        }
    }

    struct VariantSerializer;

    macro_rules! unsupported_scalar {
        ($($method:ident($ty:ty)),* $(,)?) => {
            $(
                fn $method(self, _value: $ty) -> Result<Self::Ok, Self::Error> {
                    Err(Unsupported)
                }
            )*
        };
    }

    impl serde::Serializer for VariantSerializer {
        type Ok = &'static str;
        type Error = Unsupported;
        type SerializeSeq = Impossible<Self::Ok, Self::Error>;
        type SerializeTuple = Impossible<Self::Ok, Self::Error>;
        type SerializeTupleStruct = Impossible<Self::Ok, Self::Error>;
        type SerializeTupleVariant = Impossible<Self::Ok, Self::Error>;
        type SerializeMap = Impossible<Self::Ok, Self::Error>;
        type SerializeStruct = Impossible<Self::Ok, Self::Error>;
        type SerializeStructVariant = Impossible<Self::Ok, Self::Error>;

        unsupported_scalar!(
            serialize_bool(bool),
            serialize_i8(i8),
            serialize_i16(i16),
            serialize_i32(i32),
            serialize_i64(i64),
            serialize_i128(i128),
            serialize_u8(u8),
            serialize_u16(u16),
            serialize_u32(u32),
            serialize_u64(u64),
            serialize_u128(u128),
            serialize_f32(f32),
            serialize_f64(f64),
            serialize_char(char),
            serialize_str(&str),
            serialize_bytes(&[u8]),
        );

        fn serialize_unit_variant(
            self,
            _name: &'static str,
            _index: u32,
            variant: &'static str,
        ) -> Result<Self::Ok, Self::Error> {
            Ok(variant)
        }

        fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_some<T>(self, _value: &T) -> Result<Self::Ok, Self::Error>
        where
            T: ?Sized + serde::Serialize,
        {
            Err(Unsupported)
        }

        fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_newtype_struct<T>(
            self,
            _name: &'static str,
            _value: &T,
        ) -> Result<Self::Ok, Self::Error>
        where
            T: ?Sized + serde::Serialize,
        {
            Err(Unsupported)
        }

        fn serialize_newtype_variant<T>(
            self,
            _name: &'static str,
            _index: u32,
            _variant: &'static str,
            _value: &T,
        ) -> Result<Self::Ok, Self::Error>
        where
            T: ?Sized + serde::Serialize,
        {
            Err(Unsupported)
        }

        fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_tuple_struct(
            self,
            _name: &'static str,
            _len: usize,
        ) -> Result<Self::SerializeTupleStruct, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_tuple_variant(
            self,
            _name: &'static str,
            _index: u32,
            _variant: &'static str,
            _len: usize,
        ) -> Result<Self::SerializeTupleVariant, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_struct(
            self,
            _name: &'static str,
            _len: usize,
        ) -> Result<Self::SerializeStruct, Self::Error> {
            Err(Unsupported)
        }

        fn serialize_struct_variant(
            self,
            _name: &'static str,
            _index: u32,
            _variant: &'static str,
            _len: usize,
        ) -> Result<Self::SerializeStructVariant, Self::Error> {
            Err(Unsupported)
        }
    }
}
