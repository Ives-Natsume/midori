//! Demodulator construction for digital VFO channels.
//!
//! All per-mode knowledge about which DSP block to build lives here. The VFO
//! channel only knows how to feed samples to a [`Demod`] and frame its output.

use anyhow::{Result, bail};
use midori_core::{DemodMode, Sample, StreamKind, StreamSpec, VfoConfig};
use midori_dsp::{
    AmDemod, AmDemodConfig, BpskDemod, BpskDemodConfig, CwDemod, CwDemodConfig, FmDemod,
    FmDemodConfig,
};

pub(crate) enum Demod {
    Am(AmDemod),
    Fm(FmDemod),
    Cw(CwDemod),
    Bpsk(BpskDemod),
    /// 12 kHz real-IF pass-through. No mixing or filtering: the source file is
    /// already the audio-IF stream expected by the 9k6 decoder.
    IfAudio,
}

impl Demod {
    /// Build the demodulator described by `config` for a source with `spec`.
    /// `offset_hz` is the absolute VFO offset including Doppler correction.
    pub(crate) fn build(config: &VfoConfig, spec: &StreamSpec, offset_hz: f64) -> Result<Self> {
        let demod = match config.demod {
            DemodMode::Fm | DemodMode::Sstv => Self::Fm(FmDemod::new(FmDemodConfig {
                input_rate_hz: spec.sample_rate_hz,
                channel_rate_hz: config.channel_rate_hz,
                output_rate_hz: config.audio_rate_hz,
                offset_hz,
                channel_bandwidth_hz: config.bandwidth_hz,
                deemphasis_us: config.deemphasis_us,
                audio_bandwidth_hz: config.audio_bandwidth_hz,
            })),
            DemodMode::Am => Self::Am(AmDemod::new(AmDemodConfig {
                input_rate_hz: spec.sample_rate_hz,
                output_rate_hz: config.audio_rate_hz,
                offset_hz,
                bandwidth_hz: config.bandwidth_hz,
            })),
            DemodMode::Cw => Self::Cw(CwDemod::new(CwDemodConfig {
                input_rate_hz: spec.sample_rate_hz,
                channel_rate_hz: config.channel_rate_hz,
                output_rate_hz: config.audio_rate_hz,
                offset_hz,
                bandwidth_hz: config.bandwidth_hz,
                bfo_hz: config.bfo_hz,
                audio_bandwidth_hz: config.audio_bandwidth_hz,
            })),
            DemodMode::Bpsk => Self::Bpsk(BpskDemod::new(BpskDemodConfig {
                input_rate_hz: spec.sample_rate_hz,
                channel_rate_hz: config.channel_rate_hz,
                output_rate_hz: config.audio_rate_hz,
                offset_hz,
                channel_bandwidth_hz: config.bandwidth_hz,
            })),
            DemodMode::BpskIf => Self::IfAudio,
            DemodMode::Usb | DemodMode::Lsb => bail!(
                "demod mode {} ({}) is not implemented yet",
                config.demod.label(),
                config.demod.config_key()
            ),
        };
        Ok(demod)
    }

    pub(crate) fn output_kind(&self) -> StreamKind {
        match self {
            Self::Bpsk(_) => StreamKind::Iq,
            Self::Am(_) | Self::Fm(_) | Self::Cw(_) | Self::IfAudio => StreamKind::Audio,
        }
    }

    pub(crate) fn is_if_audio(&self) -> bool {
        matches!(self, Self::IfAudio)
    }

    pub(crate) fn set_offset(&mut self, offset_hz: f64) {
        match self {
            Self::Am(demod) => demod.set_offset(offset_hz),
            Self::Fm(demod) => demod.set_offset(offset_hz),
            Self::Cw(demod) => demod.set_offset(offset_hz),
            Self::Bpsk(demod) => demod.set_offset(offset_hz),
            Self::IfAudio => {}
        }
    }

    pub(crate) fn process_audio(&mut self, input: &[Sample], out: &mut Vec<f32>) {
        match self {
            Self::Am(demod) => demod.process(input, out),
            Self::Fm(demod) => demod.process(input, out),
            Self::Cw(demod) => demod.process(input, out),
            Self::Bpsk(_) | Self::IfAudio => unreachable!("not an audio demodulator"),
        }
    }

    pub(crate) fn process_iq(&mut self, input: &[Sample], out: &mut Vec<Sample>) {
        match self {
            Self::Bpsk(demod) => demod.process(input, out),
            Self::Am(_) | Self::Fm(_) | Self::Cw(_) | Self::IfAudio => {
                unreachable!("not an IQ demodulator")
            }
        }
    }

    pub(crate) fn iq_output_rate_hz(&self) -> Option<f64> {
        match self {
            Self::Bpsk(demod) => Some(demod.output_rate_hz()),
            Self::Am(_) | Self::Fm(_) | Self::Cw(_) | Self::IfAudio => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use midori_core::{DemodMode, SampleFormat, StreamSpec, VfoConfig};

    use super::*;

    fn spec() -> StreamSpec {
        StreamSpec {
            sample_rate_hz: 2_400_000.0,
            center_freq_hz: 145_800_000.0,
            bandwidth_hz: 2_000_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        }
    }

    fn config(mode: DemodMode) -> VfoConfig {
        VfoConfig::new(1, 0.0, 15_000.0, mode)
    }

    #[test]
    fn builds_every_implemented_mode() {
        for mode in DemodMode::implemented() {
            let mut config = config(mode);
            config.channel_rate_hz = 48_000.0;
            let demod = Demod::build(&config, &spec(), 0.0)
                .unwrap_or_else(|error| panic!("{mode:?} failed to build: {error}"));
            assert_eq!(demod.output_kind(), mode.output_kind());
        }
    }

    #[test]
    fn reserved_modes_return_a_clear_error() {
        for mode in [DemodMode::Usb, DemodMode::Lsb] {
            let error = match Demod::build(&config(mode), &spec(), 0.0) {
                Ok(_) => panic!("{mode:?} unexpectedly built"),
                Err(error) => error,
            };
            let message = error.to_string();
            assert!(message.contains(mode.label()), "{message}");
            assert!(message.contains("not implemented"), "{message}");
        }
    }
}
