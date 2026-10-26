//! Digital VFO channel: channel selection, demodulation, and audio framing.

use std::sync::Arc;

use anyhow::Result;
use midori_core::{
    Channel, DemodMode, Frame, FrameMeta, IqBlock, Sample, StreamData, StreamKind, StreamSpec,
    VfoConfig,
};
use midori_dsp::{AudioAgc, CarrierTracker};

use crate::demod::Demod;

/// One digital channel inside a session.
pub struct VfoChannel {
    config: VfoConfig,
    demod: Option<Demod>,
    agc: AudioAgc,
    seq: u64,
    audio_start_sample: u64,
    audio_scratch: Vec<f32>,
    iq_scratch: Vec<Sample>,
    /// Current Doppler shift added to the configured VFO offset.
    doppler_hz: f64,
    doppler_rate_hz_s: f64,
    /// Last configured stream spec, used to rebuild the channel in place.
    spec: Option<StreamSpec>,
    /// Real-time CW carrier tracker (AFC) and whether the user enabled it.
    afc_enabled: bool,
    carrier: Option<CarrierTracker>,
    last_afc_snr_db: Option<f32>,
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
            iq_scratch: Vec::new(),
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
            spec: None,
            afc_enabled: false,
            carrier: None,
            last_afc_snr_db: None,
        }
    }

    /// Absolute VFO offset currently in use, including Doppler correction.
    pub fn current_offset_hz(&self) -> f64 {
        self.config.offset_hz + self.doppler_hz
    }

    /// SNR of the last AFC carrier estimate, when the tracker is active.
    pub fn afc_snr_db(&self) -> Option<f32> {
        self.last_afc_snr_db
    }

    fn rebuild_carrier(&mut self) {
        self.carrier = None;
        self.last_afc_snr_db = None;
        if !self.afc_enabled || self.config.demod != DemodMode::Cw {
            return;
        }
        let Some(spec) = self.spec else {
            return;
        };
        self.carrier = Some(CarrierTracker::new(
            spec.sample_rate_hz,
            self.current_offset_hz(),
            12_000.0,
            3_000.0,
        ));
    }

    fn track_carrier(&mut self, iq: &IqBlock) {
        let Some(carrier) = self.carrier.as_mut() else {
            return;
        };
        let Some(estimate) = carrier.process(&iq.samples) else {
            return;
        };
        self.last_afc_snr_db = Some(estimate.snr_db);
        let center_hz = carrier.center_hz();
        let search_hz = carrier.search_hz();

        let absolute = center_hz + estimate.residual_hz;
        let max_offset = (iq.spec.sample_rate_hz / 2.0 - self.config.bandwidth_hz / 2.0).max(0.0);
        let new_offset = (absolute - self.doppler_hz).clamp(-max_offset, max_offset);
        if (new_offset - self.config.offset_hz).abs() > 0.25 {
            self.config.offset_hz = new_offset;
            self.retune_demod();
        }

        // Keep the wide acquisition window roughly centred on the carrier as
        // it drifts, without resetting it on every small correction.
        if estimate.residual_hz.abs() > 0.75 * search_hz {
            let current = self.current_offset_hz();
            if let Some(carrier) = self.carrier.as_mut() {
                carrier.set_center(current);
            }
        }
    }

    fn retune_demod(&mut self) {
        let absolute = self.current_offset_hz();
        if let Some(demod) = &mut self.demod {
            demod.set_offset(absolute);
        }
    }
}

impl Channel for VfoChannel {
    fn config(&self) -> &VfoConfig {
        &self.config
    }

    fn configure(&mut self, spec: &StreamSpec) -> Result<()> {
        let offset_hz = self.current_offset_hz();
        let demod = Demod::build(&self.config, spec, offset_hz)?;

        self.demod = Some(demod);
        self.spec = Some(*spec);
        self.agc.reset();
        self.seq = 0;
        self.audio_start_sample = 0;
        self.rebuild_carrier();
        Ok(())
    }

    fn set_doppler(&mut self, offset_hz: f64, rate_hz_s: f64) {
        self.doppler_hz = offset_hz;
        self.doppler_rate_hz_s = rate_hz_s;
        self.retune_demod();
        // A large correction (for example enabling SGP4 after tuning) should
        // move the acquisition window with the receiver.
        let current = self.current_offset_hz();
        if let Some(carrier) = &mut self.carrier
            && (carrier.center_hz() - current).abs() > carrier.search_hz() * 0.5
        {
            carrier.set_center(current);
        }
    }

    fn set_offset(&mut self, offset_hz: f64) {
        self.config.offset_hz = offset_hz;
        let current = self.current_offset_hz();
        if let Some(carrier) = &mut self.carrier {
            carrier.set_center(current);
        }
        self.retune_demod();
    }

    fn set_afc(&mut self, enabled: bool) {
        if self.afc_enabled != enabled {
            self.afc_enabled = enabled;
            self.rebuild_carrier();
        }
    }

    fn reconfigure(&mut self, config: VfoConfig, spec: &StreamSpec) -> Result<()> {
        self.config = config;
        self.configure(spec)
    }

    fn process(&mut self, iq: &IqBlock, out: &mut Vec<Frame>) -> Result<()> {
        self.track_carrier(iq);

        let tuned_freq_hz = iq.spec.center_freq_hz + self.current_offset_hz();

        let Some(demod) = &mut self.demod else {
            return Ok(());
        };

        // A real-IF source is already the mono audio stream the 9k6 decoder
        // expects. Pass the real part through and skip audio AGC.
        if demod.is_if_audio() {
            self.audio_scratch.clear();
            self.audio_scratch
                .extend(iq.samples.iter().map(|sample| sample.re));
            if self.audio_scratch.is_empty() {
                return Ok(());
            }
            let meta = FrameMeta {
                seq: self.seq,
                start_sample: self.audio_start_sample,
                timestamp_ns: iq.timestamp_ns,
                sample_rate_hz: iq.spec.sample_rate_hz,
                center_freq_hz: iq.spec.center_freq_hz,
                nominal_freq_hz: iq.spec.center_freq_hz,
                bandwidth_hz: iq.spec.sample_rate_hz,
                vfo_id: Some(self.config.id),
                doppler_hz: self.doppler_hz,
                doppler_rate_hz_s: self.doppler_rate_hz_s,
                flags: iq.flags,
            };
            self.seq += 1;
            self.audio_start_sample += self.audio_scratch.len() as u64;
            let audio: Arc<[f32]> = Arc::from(self.audio_scratch.as_slice());
            out.push(Frame::new(meta, StreamData::Audio(audio)));
            return Ok(());
        }

        // A BPSK channel is not demodulated here: the decoder wants complex
        // baseband so it can run its own carrier/timing loops and FEC. Keep
        // the same VFO/Doppler metadata as the audio paths.
        if demod.output_kind() == StreamKind::Iq {
            self.iq_scratch.clear();
            demod.process_iq(&iq.samples, &mut self.iq_scratch);
            if self.iq_scratch.is_empty() {
                return Ok(());
            }
            let output_rate_hz = demod
                .iq_output_rate_hz()
                .expect("IQ demodulator reports an output rate");
            let meta = FrameMeta {
                seq: self.seq,
                start_sample: self.audio_start_sample,
                timestamp_ns: iq.timestamp_ns,
                sample_rate_hz: output_rate_hz,
                center_freq_hz: tuned_freq_hz,
                nominal_freq_hz: iq.spec.center_freq_hz + self.config.offset_hz,
                bandwidth_hz: self.config.bandwidth_hz,
                vfo_id: Some(self.config.id),
                doppler_hz: self.doppler_hz,
                doppler_rate_hz_s: self.doppler_rate_hz_s,
                flags: iq.flags,
            };
            self.seq += 1;
            self.audio_start_sample += self.iq_scratch.len() as u64;
            let samples: Arc<[Sample]> = Arc::from(self.iq_scratch.as_slice());
            out.push(Frame::new(meta, StreamData::Iq(samples)));
            return Ok(());
        }

        self.audio_scratch.clear();
        demod.process_audio(&iq.samples, &mut self.audio_scratch);
        if self.audio_scratch.is_empty() {
            return Ok(());
        }

        // CW keying is the signal. The per-sample AGC fills key-up gaps with
        // amplified noise and compresses the mark/gap contrast, so leave the
        // product-detector audio untouched for the CW decoder.
        if self.config.demod != DemodMode::Cw {
            self.agc.process(&mut self.audio_scratch);
        }

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

    #[test]
    fn cw_afc_follows_a_drifting_carrier() {
        let fs = 240_000.0;
        let start_hz = 4_000.0;
        let drift_hz_s = 100.0;
        let seconds = 4.0;
        let count = (fs * seconds) as usize;
        let mut phase = 0.0f64;
        let samples: Vec<Sample> = (0..count)
            .map(|index| {
                let time = index as f64 / fs;
                let frequency = start_hz + drift_hz_s * time;
                phase += std::f64::consts::TAU * frequency / fs;
                Sample::new(phase.cos() as f32 * 0.3, phase.sin() as f32 * 0.3)
            })
            .collect();

        let spec = StreamSpec {
            sample_rate_hz: fs,
            center_freq_hz: 437_393_500.0,
            bandwidth_hz: 240_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        let mut channel = VfoChannel::new(VfoConfig {
            id: 3,
            offset_hz: start_hz,
            bandwidth_hz: 500.0,
            demod: DemodMode::Cw,
            audio_rate_hz: 48_000.0,
            channel_rate_hz: 48_000.0,
            audio_bandwidth_hz: 2_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        });
        channel.configure(&spec).unwrap();
        channel.set_afc(true);
        assert!(channel.afc_snr_db().is_none());

        let mut frames = Vec::new();
        for block in samples.chunks(32_768) {
            let iq = IqBlock {
                samples: Arc::from(block.to_vec().into_boxed_slice()),
                spec,
                start_sample: 0,
                timestamp_ns: None,
                flags: BlockFlags::default(),
            };
            frames.clear();
            channel.process(&iq, &mut frames).unwrap();
        }

        let expected = start_hz + drift_hz_s * seconds;
        let tracked = channel.current_offset_hz();
        assert!(
            (tracked - expected).abs() < 120.0,
            "AFC ended at {tracked:.1} Hz, expected about {expected:.1} Hz"
        );
        assert!(channel.afc_snr_db().is_some());
    }

    #[test]
    fn bpsk_mode_outputs_complex_baseband_at_48k() {
        let fs = 2_400_000.0;
        let offset = 3_000.0;
        let n = 240_000;
        let samples: Vec<Sample> = (0..n)
            .map(|index| {
                let phase = std::f64::consts::TAU * offset * index as f64 / fs;
                Sample::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();

        let spec = StreamSpec {
            sample_rate_hz: fs,
            center_freq_hz: 436_210_000.0,
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
            id: 4,
            offset_hz: offset,
            bandwidth_hz: 30_000.0,
            demod: DemodMode::Bpsk,
            audio_rate_hz: 48_000.0,
            channel_rate_hz: 48_000.0,
            audio_bandwidth_hz: 30_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        });
        channel.configure(&spec).unwrap();
        let mut frames = Vec::new();
        channel.process(&iq, &mut frames).unwrap();

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].meta.vfo_id, Some(4));
        assert_eq!(frames[0].meta.sample_rate_hz, 48_000.0);
        let StreamData::Iq(samples) = &frames[0].data else {
            panic!("expected IQ frame");
        };
        assert!(samples.len() > 4_000, "got {} samples", samples.len());

        // The channel NCO removes the 3 kHz offset, so the residual phase
        // step should be tiny once the filter has settled.
        let steady = &samples[200..];
        let mut sum = 0.0f64;
        for pair in steady.windows(2) {
            let product = pair[1] * pair[0].conj();
            sum += product.im.atan2(product.re) as f64;
        }
        let mean_step = sum / (steady.len() - 1) as f64;
        assert!(
            mean_step.abs() < 0.02,
            "residual phase step {mean_step:.4} rad/sample"
        );
        let power: f32 =
            steady.iter().map(|sample| sample.norm_sqr()).sum::<f32>() / steady.len() as f32;
        assert!(power > 0.05, "channel output power {power}");
    }

    #[test]
    fn bpsk_if_mode_passes_real_if_audio_through() {
        let fs = 48_000.0;
        let tone = 12_000.0;
        let n = 48_000;
        let samples: Vec<Sample> = (0..n)
            .map(|index| {
                let phase = std::f64::consts::TAU * tone * index as f64 / fs;
                Sample::new(phase.cos() as f32, 0.0)
            })
            .collect();
        let spec = StreamSpec {
            sample_rate_hz: fs,
            center_freq_hz: 0.0,
            bandwidth_hz: fs,
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
            id: 5,
            offset_hz: 0.0,
            bandwidth_hz: 30_000.0,
            demod: DemodMode::BpskIf,
            audio_rate_hz: 48_000.0,
            channel_rate_hz: 48_000.0,
            audio_bandwidth_hz: 30_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        });
        channel.configure(&spec).unwrap();
        let mut frames = Vec::new();
        channel.process(&iq, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].meta.sample_rate_hz, 48_000.0);
        let StreamData::Audio(audio) = &frames[0].data else {
            panic!("expected real-IF audio");
        };
        assert_eq!(audio.len(), n);
        let crossings = audio
            .windows(2)
            .filter(|window| (window[0] < 0.0) != (window[1] < 0.0))
            .count();
        let measured = crossings as f64 / 2.0 / (audio.len() as f64 / fs);
        assert!(
            (measured - tone).abs() < 50.0,
            "measured {measured} Hz, expected {tone} Hz"
        );
    }

    #[test]
    fn sstv_mode_uses_the_fm_demodulator() {
        let spec = StreamSpec {
            sample_rate_hz: 2_400_000.0,
            center_freq_hz: 145_800_000.0,
            bandwidth_hz: 2_000_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        let samples: Vec<Sample> = vec![Sample::new(0.5, 0.0); 24_000];
        let iq = IqBlock {
            samples: Arc::from(samples.into_boxed_slice()),
            spec,
            start_sample: 0,
            timestamp_ns: None,
            flags: BlockFlags::default(),
        };
        let mut channel = VfoChannel::new(VfoConfig {
            id: 2,
            offset_hz: 0.0,
            bandwidth_hz: 15_000.0,
            demod: DemodMode::Sstv,
            audio_rate_hz: 48_000.0,
            channel_rate_hz: 48_000.0,
            audio_bandwidth_hz: 3_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        });
        channel.configure(&spec).unwrap();
        let mut frames = Vec::new();
        channel.process(&iq, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].meta.vfo_id, Some(2));
    }
}
