//! Small, dependency-light DSP building blocks.
//!
//! v1 intentionally ships a handful of useful stages rather than a generic
//! flowgraph: a Welch spectrum analyser and an AM channel demodulator. More
//! stages (SSB/CW/FM, decimators, resamplers) can be added without changing
//! the core stream types.

use std::f32::consts::PI;
use std::sync::Arc;

use midori_core::{Sample, StreamSpec};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

// ---------------------------------------------------------------------------
// Spectrum
// ---------------------------------------------------------------------------

/// Welch-style averaged power spectrum.
pub struct SpectrumAnalyzer {
    fft: Arc<dyn Fft<f32>>,
    fft_size: usize,
    window: Vec<f32>,
    window_sum: f64,
    carry: Vec<Sample>,
    fft_buffer: Vec<Complex32>,
    power_sum: Vec<f64>,
    frames: u64,
}

impl SpectrumAnalyzer {
    pub fn new(fft_size: usize) -> Self {
        assert!(fft_size >= 2, "FFT size must be at least 2");
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let window: Vec<f32> = (0..fft_size)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (fft_size - 1) as f32).cos())
            .collect();
        let window_sum = window.iter().map(|&w| w as f64).sum();

        Self {
            fft,
            fft_size,
            window,
            window_sum,
            carry: Vec::with_capacity(fft_size * 2),
            fft_buffer: vec![Complex32::new(0.0, 0.0); fft_size],
            power_sum: vec![0.0; fft_size],
            frames: 0,
        }
    }

    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Feed arbitrary-length blocks. Complete FFT frames are consumed; the
    /// remainder is kept for the next call.
    pub fn process(&mut self, input: &[Sample]) {
        self.carry.extend_from_slice(input);

        let mut pos = 0;
        while self.carry.len() - pos >= self.fft_size {
            for (dst, (sample, window)) in self.fft_buffer.iter_mut().zip(
                self.carry[pos..pos + self.fft_size]
                    .iter()
                    .zip(self.window.iter()),
            ) {
                *dst = Complex32::new(sample.re * window, sample.im * window);
            }
            self.fft.process(&mut self.fft_buffer);

            let norm = self.window_sum * self.window_sum;
            for (sum, value) in self.power_sum.iter_mut().zip(self.fft_buffer.iter()) {
                let power =
                    (value.re as f64 * value.re as f64 + value.im as f64 * value.im as f64) / norm;
                *sum += power;
            }
            self.frames += 1;
            pos += self.fft_size;
        }

        if pos > 0 {
            self.carry.drain(..pos);
        }
    }

    pub fn reset(&mut self) {
        self.carry.clear();
        self.power_sum.fill(0.0);
        self.frames = 0;
    }

    /// Averaged spectrum in dBFS. A full-scale complex tone at an FFT bin is
    /// approximately 0 dB.
    pub fn spectrum_db(&self) -> Vec<f32> {
        if self.frames == 0 {
            return vec![f32::NEG_INFINITY; self.fft_size];
        }
        let frames = self.frames as f64;
        self.power_sum
            .iter()
            .map(|&p| (10.0 * (p / frames + 1e-30).log10()) as f32)
            .collect()
    }

    pub fn spectrum_db_with_spec(&self, spec: &StreamSpec) -> (Vec<f32>, Vec<f64>) {
        let db = self.spectrum_db();
        let freqs = frequency_axis(spec.center_freq_hz, spec.sample_rate_hz, db.len());
        (db, freqs)
    }
}

/// Move DC to the centre of the spectrum.
pub fn fftshift(spectrum: &[f32]) -> Vec<f32> {
    let half = spectrum.len() / 2;
    let mut shifted = Vec::with_capacity(spectrum.len());
    shifted.extend_from_slice(&spectrum[half..]);
    shifted.extend_from_slice(&spectrum[..half]);
    shifted
}

/// Frequency of each FFT bin after `fftshift`.
pub fn frequency_axis(center_freq_hz: f64, sample_rate_hz: f64, bins: usize) -> Vec<f64> {
    let half = bins as f64 / 2.0;
    (0..bins)
        .map(|i| center_freq_hz + (i as f64 - half) * sample_rate_hz / bins as f64)
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct Peak {
    pub index: usize,
    pub frequency_hz: f64,
    pub power_db: f32,
}

/// Greedily pick the strongest bins while keeping `min_distance` bins apart.
pub fn top_peaks(
    spectrum_db: &[f32],
    frequencies_hz: &[f64],
    count: usize,
    min_distance: usize,
) -> Vec<Peak> {
    let len = spectrum_db.len().min(frequencies_hz.len());
    if len == 0 || count == 0 {
        return Vec::new();
    }
    let mut indices: Vec<usize> = (0..len).collect();
    indices.sort_by(|&a, &b| {
        spectrum_db[b]
            .partial_cmp(&spectrum_db[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut picked: Vec<Peak> = Vec::with_capacity(count);
    for index in indices {
        if picked
            .iter()
            .any(|p| p.index.abs_diff(index) < min_distance)
        {
            continue;
        }
        picked.push(Peak {
            index,
            frequency_hz: frequencies_hz[index],
            power_db: spectrum_db[index],
        });
        if picked.len() == count {
            break;
        }
    }
    picked
}

/// Median of a spectrum, useful as a quick noise-floor estimate.
pub fn median(spectrum_db: &[f32]) -> f32 {
    let mut values: Vec<f32> = spectrum_db
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    if values.is_empty() {
        return f32::NEG_INFINITY;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

// ---------------------------------------------------------------------------
// Biquad
// ---------------------------------------------------------------------------

/// Second-order IIR section (transposed direct form II), f64 state.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// RBJ low-pass biquad.
    pub fn lowpass(sample_rate_hz: f64, cutoff_hz: f64, q: f64) -> Self {
        let cutoff_hz = cutoff_hz.clamp(1.0, sample_rate_hz * 0.45);
        let w0 = 2.0 * std::f64::consts::PI * cutoff_hz / sample_rate_hz;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * q);
        let a0 = 1.0 + alpha;

        Self {
            b0: ((1.0 - cos_w0) / 2.0) / a0,
            b1: (1.0 - cos_w0) / a0,
            b2: ((1.0 - cos_w0) / 2.0) / a0,
            a1: (-2.0 * cos_w0) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Fourth-order Butterworth low-pass built from two biquads.
#[derive(Debug, Clone, Copy)]
pub struct Lowpass4 {
    sections: [Biquad; 2],
}

impl Lowpass4 {
    pub fn new(sample_rate_hz: f64, cutoff_hz: f64) -> Self {
        // Q values for a 4th-order Butterworth response.
        Self {
            sections: [
                Biquad::lowpass(sample_rate_hz, cutoff_hz, 0.541_196_1),
                Biquad::lowpass(sample_rate_hz, cutoff_hz, 1.306_563),
            ],
        }
    }

    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let y = self.sections[0].process(x);
        self.sections[1].process(y)
    }

    pub fn reset(&mut self) {
        for section in &mut self.sections {
            section.reset();
        }
    }
}

// ---------------------------------------------------------------------------
// AM demodulator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct AmDemodConfig {
    pub input_rate_hz: f64,
    pub output_rate_hz: f64,
    /// Channel offset relative to the capture centre.
    pub offset_hz: f64,
    /// RF bandwidth of the AM channel (filter cutoff is half of this).
    pub bandwidth_hz: f64,
}

impl Default for AmDemodConfig {
    fn default() -> Self {
        Self {
            input_rate_hz: 2_084_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 0.0,
            bandwidth_hz: 6_000.0,
        }
    }
}

/// AM channel demodulator: NCO → low-pass → envelope → DC block → resample.
pub struct AmDemod {
    config: AmDemodConfig,
    phase: f64,
    phase_inc: f64,
    lp_i: Lowpass4,
    lp_q: Lowpass4,
    dc_alpha: f64,
    dc_prev_in: f64,
    dc_prev_out: f64,
    resample_next: f64,
    resample_step: f64,
    input_index: f64,
    previous: f64,
}

impl AmDemod {
    pub fn new(config: AmDemodConfig) -> Self {
        let cutoff = (config.bandwidth_hz / 2.0).clamp(100.0, config.input_rate_hz * 0.45);
        // High-pass corner around 100 Hz removes the carrier/DC term.
        let dc_hz = 100.0f64;
        let dc_alpha =
            (1.0 - 2.0 * std::f64::consts::PI * dc_hz / config.input_rate_hz).clamp(0.0, 0.99999);

        Self {
            config,
            phase: 0.0,
            phase_inc: 2.0 * std::f64::consts::PI * config.offset_hz / config.input_rate_hz,
            lp_i: Lowpass4::new(config.input_rate_hz, cutoff),
            lp_q: Lowpass4::new(config.input_rate_hz, cutoff),
            dc_alpha,
            dc_prev_in: 0.0,
            dc_prev_out: 0.0,
            resample_next: 1.0,
            resample_step: config.input_rate_hz / config.output_rate_hz,
            input_index: 0.0,
            previous: 0.0,
        }
    }

    pub fn output_rate_hz(&self) -> f64 {
        self.config.output_rate_hz
    }

    /// Append demodulated audio to `output`.
    pub fn process(&mut self, input: &[Sample], output: &mut Vec<f32>) {
        for sample in input {
            let (sin, cos) = self.phase.sin_cos();
            let i = sample.re as f64 * cos + sample.im as f64 * sin;
            let q = sample.im as f64 * cos - sample.re as f64 * sin;

            self.phase += self.phase_inc;
            if self.phase > std::f64::consts::TAU {
                self.phase -= std::f64::consts::TAU;
            }

            let i = self.lp_i.process(i);
            let q = self.lp_q.process(q);
            let envelope = (i * i + q * q).sqrt();

            // One-pole DC blocker.
            let audio = envelope - self.dc_prev_in + self.dc_alpha * self.dc_prev_out;
            self.dc_prev_in = envelope;
            self.dc_prev_out = audio;

            // Linear resampling to the output rate. The signal is already
            // band-limited by the low-pass, so this is adequate for voice.
            self.input_index += 1.0;
            while self.resample_next <= self.input_index {
                let frac = (self.resample_next - (self.input_index - 1.0)).clamp(0.0, 1.0);
                let value = self.previous + (audio - self.previous) * frac;
                output.push(value as f32);
                self.resample_next += self.resample_step;
            }
            self.previous = audio;
        }
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.lp_i.reset();
        self.lp_q.reset();
        self.dc_prev_in = 0.0;
        self.dc_prev_out = 0.0;
        self.resample_next = 1.0;
        self.input_index = 0.0;
        self.previous = 0.0;
    }
}

// ---------------------------------------------------------------------------
// FM demodulator
// ---------------------------------------------------------------------------

/// Numerically controlled oscillator that mixes a channel to baseband.
#[derive(Debug, Clone)]
pub struct Nco {
    phase: f64,
    phase_inc: f64,
}

impl Nco {
    pub fn new(sample_rate_hz: f64, offset_hz: f64) -> Self {
        Self {
            phase: 0.0,
            phase_inc: std::f64::consts::TAU * offset_hz / sample_rate_hz,
        }
    }

    pub fn set_offset(&mut self, sample_rate_hz: f64, offset_hz: f64) {
        self.phase_inc = std::f64::consts::TAU * offset_hz / sample_rate_hz;
    }

    /// Mix one sample by `exp(-j * phase)`.
    #[inline]
    pub fn mix(&mut self, sample: Complex32) -> Complex32 {
        let (sin, cos) = self.phase.sin_cos();
        let i = sample.re as f64 * cos + sample.im as f64 * sin;
        let q = sample.im as f64 * cos - sample.re as f64 * sin;

        self.phase += self.phase_inc;
        if self.phase > std::f64::consts::TAU {
            self.phase -= std::f64::consts::TAU;
        }
        Complex32::new(i as f32, q as f32)
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// Mix a channel to baseband, low-pass it, and decimate it.
///
/// This is the digital VFO primitive used by AM/FM/SSB/CW and by decoder
/// channels. It deliberately does not know about demodulation.
pub struct ComplexChannelizer {
    nco: Nco,
    lp_i: Lowpass4,
    lp_q: Lowpass4,
    decimation: usize,
    count: usize,
    output_rate_hz: f64,
}

impl ComplexChannelizer {
    pub fn new(input_rate_hz: f64, output_rate_hz: f64, offset_hz: f64, bandwidth_hz: f64) -> Self {
        let ratio = (input_rate_hz / output_rate_hz).max(1.0);
        let decimation = ratio.round().max(1.0) as usize;
        let actual_output_rate = input_rate_hz / decimation as f64;
        let cutoff = (bandwidth_hz / 2.0).clamp(100.0, input_rate_hz * 0.45);

        Self {
            nco: Nco::new(input_rate_hz, offset_hz),
            lp_i: Lowpass4::new(input_rate_hz, cutoff),
            lp_q: Lowpass4::new(input_rate_hz, cutoff),
            decimation,
            count: 0,
            output_rate_hz: actual_output_rate,
        }
    }

    pub fn output_rate_hz(&self) -> f64 {
        self.output_rate_hz
    }

    pub fn set_offset(&mut self, input_rate_hz: f64, offset_hz: f64) {
        self.nco.set_offset(input_rate_hz, offset_hz);
    }

    /// Append decimated complex baseband samples to `output`.
    pub fn process(&mut self, input: &[Sample], output: &mut Vec<Complex32>) {
        for &sample in input {
            let mixed = self.nco.mix(sample);
            let i = self.lp_i.process(mixed.re as f64);
            let q = self.lp_q.process(mixed.im as f64);

            self.count += 1;
            if self.count >= self.decimation {
                self.count = 0;
                output.push(Complex32::new(i as f32, q as f32));
            }
        }
    }

    pub fn reset(&mut self) {
        self.nco.reset();
        self.lp_i.reset();
        self.lp_q.reset();
        self.count = 0;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FmDemodConfig {
    pub input_rate_hz: f64,
    /// Intermediate rate after channel selection. 240 kHz is a good default
    /// for broadcast FM; 48 kHz is enough for narrowband voice.
    pub channel_rate_hz: f64,
    pub output_rate_hz: f64,
    /// Channel offset relative to the capture centre.
    pub offset_hz: f64,
    /// RF bandwidth of the FM channel.
    pub channel_bandwidth_hz: f64,
    /// De-emphasis time constant: 50 us (Europe/China), 75 us (Americas), or 0.
    pub deemphasis_us: f64,
    pub audio_bandwidth_hz: f64,
}

impl Default for FmDemodConfig {
    fn default() -> Self {
        Self {
            input_rate_hz: 2_400_000.0,
            channel_rate_hz: 240_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 0.0,
            channel_bandwidth_hz: 200_000.0,
            deemphasis_us: 50.0,
            audio_bandwidth_hz: 15_000.0,
        }
    }
}

/// One-pole low-pass used for de-emphasis.
#[derive(Debug, Clone, Copy)]
struct OnePoleLowpass {
    alpha: f64,
    state: f64,
}

impl OnePoleLowpass {
    fn from_time_constant(sample_rate_hz: f64, tau_s: f64) -> Self {
        let dt = 1.0 / sample_rate_hz;
        Self {
            alpha: (dt / (tau_s + dt)).clamp(0.0, 1.0),
            state: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        self.state += self.alpha * (x - self.state);
        self.state
    }

    fn reset(&mut self) {
        self.state = 0.0;
    }
}

/// FM channel demodulator: channelize -> discriminator -> de-emphasis ->
/// audio low-pass -> resample.
pub struct FmDemod {
    channelizer: ComplexChannelizer,
    channel_rate_hz: f64,
    previous: Option<Complex32>,
    deemph: Option<OnePoleLowpass>,
    audio_lp: Lowpass4,
    baseband: Vec<Complex32>,
    resample_next: f64,
    resample_step: f64,
    input_index: f64,
    previous_audio: f64,
}

impl FmDemod {
    pub fn new(config: FmDemodConfig) -> Self {
        let channelizer = ComplexChannelizer::new(
            config.input_rate_hz,
            config.channel_rate_hz,
            config.offset_hz,
            config.channel_bandwidth_hz,
        );
        let channel_rate_hz = channelizer.output_rate_hz();
        let audio_bandwidth = config
            .audio_bandwidth_hz
            .clamp(100.0, channel_rate_hz * 0.45);
        let deemph = if config.deemphasis_us > 0.0 {
            Some(OnePoleLowpass::from_time_constant(
                channel_rate_hz,
                config.deemphasis_us * 1e-6,
            ))
        } else {
            None
        };

        Self {
            channelizer,
            channel_rate_hz,
            previous: None,
            deemph,
            audio_lp: Lowpass4::new(channel_rate_hz, audio_bandwidth),
            baseband: Vec::new(),
            resample_next: 1.0,
            resample_step: channel_rate_hz / config.output_rate_hz,
            input_index: 0.0,
            previous_audio: 0.0,
        }
    }

    pub fn output_rate_hz(&self) -> f64 {
        self.channel_rate_hz / self.resample_step
    }

    /// Append demodulated audio to `output`.
    pub fn process(&mut self, input: &[Sample], output: &mut Vec<f32>) {
        self.baseband.clear();
        self.channelizer.process(input, &mut self.baseband);

        for index in 0..self.baseband.len() {
            let sample = self.baseband[index];
            let Some(previous) = self.previous else {
                self.previous = Some(sample);
                continue;
            };

            let product = sample * previous.conj();
            let dphi = product.im.atan2(product.re) as f64;
            let mut audio = dphi * self.channel_rate_hz / std::f64::consts::TAU;

            if let Some(deemphasis) = &mut self.deemph {
                audio = deemphasis.process(audio);
            }
            audio = self.audio_lp.process(audio);

            self.input_index += 1.0;
            while self.resample_next <= self.input_index {
                let frac = (self.resample_next - (self.input_index - 1.0)).clamp(0.0, 1.0);
                let value = self.previous_audio + (audio - self.previous_audio) * frac;
                output.push(value as f32);
                self.resample_next += self.resample_step;
            }
            self.previous_audio = audio;
            self.previous = Some(sample);
        }
    }

    pub fn reset(&mut self) {
        self.channelizer.reset();
        self.previous = None;
        if let Some(deemphasis) = &mut self.deemph {
            deemphasis.reset();
        }
        self.audio_lp.reset();
        self.baseband.clear();
        self.resample_next = 1.0;
        self.input_index = 0.0;
        self.previous_audio = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spectrum_peak_is_at_tone_bin() {
        let n = 8192;
        let bin = 100;
        let input: Vec<Sample> = (0..n)
            .map(|i| {
                let phase = 2.0 * PI * bin as f32 * i as f32 / n as f32;
                Complex32::new(phase.cos(), phase.sin())
            })
            .collect();
        let mut analyzer = SpectrumAnalyzer::new(n);
        analyzer.process(&input);
        let db = analyzer.spectrum_db();
        let (peak_index, peak) = db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(peak_index, bin);
        assert!((*peak - 0.0).abs() < 1.0, "peak was {peak} dB");
    }

    #[test]
    fn fm_demod_recovers_audio_tone() {
        let fs = 240_000.0;
        let offset = 20_000.0;
        let deviation = 3_000.0;
        let tone = 1_000.0;
        let n = 96_000;
        let mut phase = 0.0f64;
        let input: Vec<Sample> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let instantaneous =
                    offset + deviation * (2.0 * std::f64::consts::PI * tone * t).sin();
                phase += 2.0 * std::f64::consts::PI * instantaneous / fs;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();

        let mut demod = FmDemod::new(FmDemodConfig {
            input_rate_hz: fs,
            channel_rate_hz: 48_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: offset,
            channel_bandwidth_hz: 16_000.0,
            deemphasis_us: 0.0,
            audio_bandwidth_hz: 4_000.0,
        });
        let mut audio = Vec::new();
        demod.process(&input, &mut audio);

        assert!(audio.len() > 15_000, "got {} samples", audio.len());
        let steady = &audio[2_000..];
        let rms = (steady.iter().map(|&x| x * x).sum::<f32>() / steady.len() as f32).sqrt();
        assert!(rms > 100.0, "discriminator RMS too low: {rms}");

        let crossings = steady
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let measured = crossings as f64 / 2.0 / (steady.len() as f64 / 48_000.0);
        assert!(
            (measured - tone).abs() < 100.0,
            "measured {measured} Hz, expected {tone} Hz"
        );
    }

    #[test]
    fn am_demod_recovers_audio_tone() {
        let fs = 240_000.0;
        let offset = 20_000.0;
        let tone = 1_000.0;
        let n = 96_000;
        let input: Vec<Sample> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let envelope = 1.0 + 0.5 * (2.0 * std::f64::consts::PI * tone * t).cos();
                let phase = 2.0 * std::f64::consts::PI * offset * t;
                Complex32::new(
                    (envelope * phase.cos()) as f32,
                    (envelope * phase.sin()) as f32,
                )
            })
            .collect();

        let mut demod = AmDemod::new(AmDemodConfig {
            input_rate_hz: fs,
            output_rate_hz: 48_000.0,
            offset_hz: offset,
            bandwidth_hz: 8_000.0,
        });
        let mut audio = Vec::new();
        demod.process(&input, &mut audio);

        assert!(audio.len() > 15_000, "got {} samples", audio.len());
        let steady = &audio[2_000..];
        let rms = (steady.iter().map(|&x| x * x).sum::<f32>() / steady.len() as f32).sqrt();
        assert!(rms > 0.02, "audio RMS too low: {rms}");

        let crossings = steady
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let measured = crossings as f64 / 2.0 / (steady.len() as f64 / 48_000.0);
        assert!(
            (measured - tone).abs() < 100.0,
            "measured {measured} Hz, expected {tone} Hz"
        );
    }
}
