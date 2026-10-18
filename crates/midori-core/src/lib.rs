//! Core types shared by every Midori component.
//!
//! The core deliberately knows nothing about a particular SDR, UI toolkit, or
//! decoder. Hardware backends produce [`IqBlock`]s, DSP stages consume them,
//! and sinks (waterfall, audio, recorder, decoder sidecars) consume the output.

use std::sync::Arc;

use num_complex::Complex32;

/// The sample type used inside the engine.
///
/// Sources normalise their native format to `cf32` at the boundary. Keeping a
/// single internal format avoids format branches all over the DSP code.
pub type Sample = Complex32;

/// On-disk / on-wire sample formats a source or recorder may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Interleaved signed 16-bit little-endian I/Q.
    Ci16Le,
    /// Interleaved 32-bit float little-endian I/Q.
    Cf32Le,
}

/// Everything a downstream stage needs to interpret a block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamSpec {
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub format: SampleFormat,
    /// Integer value corresponding to a full-scale sample in the source's
    /// native format. Pluto's 12-bit ADC, stored in 16-bit words, uses 2048.
    /// `cf32` sources use 1.0.
    pub full_scale: f64,
}

/// Non-fatal conditions attached to a block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
}

/// Gain control modes exposed by AD9361-style front ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}
