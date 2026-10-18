//! Digital VFO channel: channel selection, demodulation, and audio framing.

use std::sync::Arc;

use anyhow::{Result, bail};
use midori_core::{
    Channel, DemodMode, Frame, FrameMeta, IqBlock, StreamData, StreamSpec, VfoConfig,
};
use midori_dsp::{
    AmDemod, AmDemodConfig, AudioAgc, CwDemod, CwDemodConfig, FmDemod, FmDemodConfig,
};

enum Demod {
    Am(AmDemod),
    Fm(FmDemod),
    Cw(CwDemod),
}

/// One digital channel inside a session.
pub struct VfoChannel {
    config: VfoConfig,
    demod: Option<Demod>,
    agc: AudioAgc,
    seq: u64,
    audio_start_sample: u64,
    audio_scratch: Vec<f32>,
    /// Current Doppler shift added to the configured VFO offset.
    doppler_hz: f64,
    doppler_rate_hz_s: f64,
}

impl VfoChannel {
    pub fn new(config: VfoConfig) -> Self {
        Self {
            config,
            demod: None,
            agc: AudioAgc::new(),
            seq: 0,
            audio_start_sample: 0,
            audio_scratch: Vec::new(),
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
        }
    }

    /// Absolute VFO offset currently in use, including Doppler correction.
    pub fn current_offset_hz(&self) -> f64 {
        self.config.offset_hz + self.doppler_hz
    }
}

impl Channel for VfoChannel {
    fn config(&self) -> &VfoConfig {
        &self.config
    }

    fn configure(&mut self, spec: &StreamSpec) -> Result<()> {
        let offset_hz = self.current_offset_hz();
        let demod = match self.config.demod {
            DemodMode::Fm => Demod::Fm(FmDemod::new(FmDemodConfig {
                input_rate_hz: spec.sample_rate_hz,
                channel_rate_hz: self.config.channel_rate_hz,
                output_rate_hz: self.config.audio_rate_hz,
                offset_hz,
                channel_bandwidth_hz: self.config.bandwidth_hz,
                deemphasis_us: self.config.deemphasis_us,
                audio_bandwidth_hz: self.config.audio_bandwidth_hz,
            })),
            DemodMode::Am => Demod::Am(AmDemod::new(AmDemodConfig {
                input_rate_hz: spec.sample_rate_hz,
                output_rate_hz: self.config.audio_rate_hz,
                offset_hz,
                bandwidth_hz: self.config.bandwidth_hz,
            })),
            DemodMode::Cw => Demod::Cw(CwDemod::new(CwDemodConfig {
                input_rate_hz: spec.sample_rate_hz,
                channel_rate_hz: self.config.channel_rate_hz,
                output_rate_hz: self.config.audio_rate_hz,
                offset_hz,
                bandwidth_hz: self.config.bandwidth_hz,
                bfo_hz: self.config.bfo_hz,
                audio_bandwidth_hz: self.config.audio_bandwidth_hz,
            })),
            other => bail!("demod mode {other:?} is not implemented yet"),
        };

        self.demod = Some(demod);
        self.agc.reset();
        self.seq = 0;
        self.audio_start_sample = 0;
        Ok(())
    }

    fn set_doppler(&mut self, offset_hz: f64, rate_hz_s: f64) {
        self.doppler_hz = offset_hz;
        self.doppler_rate_hz_s = rate_hz_s;
        let absolute = self.current_offset_hz();
        match &mut self.demod {
            Some(Demod::Am(demod)) => demod.set_offset(absolute),
            Some(Demod::Fm(demod)) => demod.set_offset(absolute),
            Some(Demod::Cw(demod)) => demod.set_offset(absolute),
            None => {}
        }
    }

    fn process(&mut self, iq: &IqBlock, out: &mut Vec<Frame>) -> Result<()> {
        let Some(demod) = &mut self.demod else {
            return Ok(());
        };

        self.audio_scratch.clear();
        match demod {
            Demod::Am(demod) => demod.process(&iq.samples, &mut self.audio_scratch),
            Demod::Fm(demod) => demod.process(&iq.samples, &mut self.audio_scratch),
            Demod::Cw(demod) => demod.process(&iq.samples, &mut self.audio_scratch),
        }
        if self.audio_scratch.is_empty() {
            return Ok(());
        }

        self.agc.process(&mut self.audio_scratch);

        let tuned_freq_hz = iq.spec.center_freq_hz + self.current_offset_hz();
        let meta = FrameMeta {
            seq: self.seq,
            start_sample: self.audio_start_sample,
            timestamp_ns: iq.timestamp_ns,
            sample_rate_hz: self.config.audio_rate_hz,
            center_freq_hz: tuned_freq_hz,
            nominal_freq_hz: iq.spec.center_freq_hz + self.config.offset_hz,
            bandwidth_hz: self.config.audio_bandwidth_hz,
            vfo_id: Some(self.config.id),
            doppler_hz: self.doppler_hz,
            doppler_rate_hz_s: self.doppler_rate_hz_s,
            flags: iq.flags,
        };
        self.seq += 1;
        self.audio_start_sample += self.audio_scratch.len() as u64;

        let audio: Arc<[f32]> = Arc::from(self.audio_scratch.as_slice());
        out.push(Frame::new(meta, StreamData::Audio(audio)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use midori_core::{BlockFlags, IqBlock, Sample, SampleFormat, StreamSpec};

    use super::*;

    #[test]
    fn vfo_channel_produces_audio_from_fm() {
        let fs = 2_400_000.0;
        let n = 240_000;
        let deviation = 25_000.0;
        let tone = 1_000.0;
        let mut phase = 0.0f64;
        let samples: Vec<Sample> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let instantaneous = deviation * (2.0 * std::f64::consts::PI * tone * t).sin();
                phase += 2.0 * std::f64::consts::PI * instantaneous / fs;
                Sample::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();

        let spec = StreamSpec {
            sample_rate_hz: fs,
            center_freq_hz: 91_600_000.0,
            bandwidth_hz: 2_000_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        let iq = IqBlock {
            samples: Arc::from(samples.into_boxed_slice()),
            spec,
            start_sample: 0,
            timestamp_ns: None,
            flags: BlockFlags::default(),
        };

        let mut channel = VfoChannel::new(VfoConfig {
            id: 7,
            offset_hz: 0.0,
            bandwidth_hz: 200_000.0,
            demod: DemodMode::Fm,
            audio_rate_hz: 48_000.0,
            channel_rate_hz: 240_000.0,
            audio_bandwidth_hz: 15_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        });
        channel.configure(&spec).unwrap();

        let mut frames = Vec::new();
        channel.process(&iq, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].meta.vfo_id, Some(7));
        assert_eq!(frames[0].meta.sample_rate_hz, 48_000.0);

        let StreamData::Audio(audio) = &frames[0].data else {
            panic!("expected audio frame");
        };
        assert!(audio.len() > 4_000, "got {} samples", audio.len());

        let steady = &audio[500..];
        let crossings = steady
            .windows(2)
            .filter(|window| (window[0] < 0.0) != (window[1] < 0.0))
            .count();
        let measured = crossings as f64 / 2.0 / (steady.len() as f64 / 48_000.0);
        assert!(
            (measured - tone).abs() < 100.0,
            "measured {measured} Hz, expected {tone} Hz"
        );
    }

    #[test]
    fn doppler_retunes_the_channel_nco_without_resetting_state() {
        let fs = 2_400_000.0;
        let tone_offset = 2_000.0;
        let n = 240_000;
        let samples: Vec<Sample> = (0..n)
            .map(|i| {
                let phase = std::f64::consts::TAU * tone_offset * i as f64 / fs;
                Sample::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();

        let spec = StreamSpec {
            sample_rate_hz: fs,
            center_freq_hz: 145_800_000.0,
            bandwidth_hz: 2_000_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        let iq = IqBlock {
            samples: Arc::from(samples.into_boxed_slice()),
            spec,
            start_sample: 0,
            timestamp_ns: None,
            flags: BlockFlags::default(),
        };

        let mut channel = VfoChannel::new(VfoConfig {
            id: 1,
            offset_hz: 0.0,
            bandwidth_hz: 5_000.0,
            demod: DemodMode::Cw,
            audio_rate_hz: 48_000.0,
            channel_rate_hz: 48_000.0,
            audio_bandwidth_hz: 2_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        });
        channel.configure(&spec).unwrap();

        let mut frames = Vec::new();
        channel.process(&iq, &mut frames).unwrap();
        let StreamData::Audio(before) = &frames[0].data else {
            panic!("expected audio frame");
        };
        let measured_before = zero_crossing_hz(&before[1_000..]);
        assert!(
            (measured_before - (tone_offset + 700.0)).abs() < 150.0,
            "before Doppler: measured {measured_before} Hz"
        );

        channel.set_doppler(tone_offset, 0.0);
        frames.clear();
        channel.process(&iq, &mut frames).unwrap();
        let StreamData::Audio(after) = &frames[0].data else {
            panic!("expected audio frame");
        };
        let measured_after = zero_crossing_hz(&after[1_000..]);
        assert!(
            (measured_after - 700.0).abs() < 150.0,
            "after Doppler: measured {measured_after} Hz"
        );
        assert_eq!(frames[0].meta.doppler_hz, tone_offset);
    }

    fn zero_crossing_hz(samples: &[f32]) -> f64 {
        let crossings = samples
            .windows(2)
            .filter(|window| (window[0] < 0.0) != (window[1] < 0.0))
            .count();
        crossings as f64 / 2.0 / (samples.len() as f64 / 48_000.0)
    }
}
