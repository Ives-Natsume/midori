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

    /// Retune the NCO without resetting the filter state. Used for Doppler
    /// correction, so phase stays continuous across frequency updates.
    pub fn set_offset(&mut self, offset_hz: f64) {
        self.config.offset_hz = offset_hz;
        self.phase_inc = 2.0 * std::f64::consts::PI * offset_hz / self.config.input_rate_hz;
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
// Audio AGC
// ---------------------------------------------------------------------------

/// Simple per-sample AGC for demodulated audio.
///
/// It is intentionally gentle and cheap; it is not a broadcast-grade
/// compressor. The goal is to keep FM discriminator output (which is in Hz)
/// and AM envelope output in a usable range for the sound card.
#[derive(Debug, Clone)]
pub struct AudioAgc {
    gain: f32,
    target: f32,
    attack: f32,
    release: f32,
    min_gain: f32,
    max_gain: f32,
}

impl Default for AudioAgc {
    fn default() -> Self {
        Self {
            gain: 1.0,
            target: 0.25,
            attack: 0.02,
            release: 0.0005,
            min_gain: 1e-5,
            max_gain: 1_000.0,
        }
    }
}

impl AudioAgc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn process(&mut self, samples: &mut [f32]) {
        for sample in samples {
            let magnitude = sample.abs();
            let desired = if magnitude > 1e-6 {
                self.target / magnitude
            } else {
                self.gain
            };
            let coefficient = if desired < self.gain {
                self.attack
            } else {
                self.release
            };
            self.gain = (self.gain + coefficient * (desired - self.gain))
                .clamp(self.min_gain, self.max_gain);
            *sample = (*sample * self.gain).clamp(-1.0, 1.0);
        }
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
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
    input_rate_hz: f64,
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
            input_rate_hz,
            output_rate_hz: actual_output_rate,
        }
    }

    pub fn output_rate_hz(&self) -> f64 {
        self.output_rate_hz
    }

    pub fn set_offset(&mut self, offset_hz: f64) {
        self.nco.set_offset(self.input_rate_hz, offset_hz);
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

    /// Retune the channel NCO while keeping filter/discriminator state.
    pub fn set_offset(&mut self, offset_hz: f64) {
        self.channelizer.set_offset(offset_hz);
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

// ---------------------------------------------------------------------------
// BPSK / data channel
// ---------------------------------------------------------------------------

/// Configuration for a complex data channel.
///
/// This is the receive side of a linear BPSK downlink: the channel is mixed to
/// baseband, low-pass filtered, decimated, and resampled to a fixed complex
/// rate. Carrier recovery, timing recovery, FEC, and framing belong to the
/// decoder plugin, not to this stage.
#[derive(Debug, Clone, Copy)]
pub struct BpskDemodConfig {
    pub input_rate_hz: f64,
    /// Intermediate rate after channel selection. The channelizer rounds the
    /// decimation to an integer.
    pub channel_rate_hz: f64,
    /// Complex output rate expected by the decoder.
    pub output_rate_hz: f64,
    /// Channel offset relative to the capture centre.
    pub offset_hz: f64,
    /// RF bandwidth of the data channel.
    pub channel_bandwidth_hz: f64,
}

impl Default for BpskDemodConfig {
    fn default() -> Self {
        Self {
            input_rate_hz: 2_400_000.0,
            channel_rate_hz: 48_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 0.0,
            channel_bandwidth_hz: 30_000.0,
        }
    }
}

/// Channelize and resample IQ for an external data decoder.
///
/// The output is complex baseband at [`BpskDemodConfig::output_rate_hz`]. A
/// small linear resampler bridges the integer decimation of
/// [`ComplexChannelizer`] to the decoder's nominal rate. At 9k6 the signal is
/// already band-limited well below the output rate, so this is transparent in
/// practice while keeping the stage allocation-free after start-up.
pub struct BpskDemod {
    channelizer: ComplexChannelizer,
    channel_rate_hz: f64,
    output_rate_hz: f64,
    baseband: Vec<Complex32>,
    resample_step: f64,
    input_index: f64,
    resample_next: f64,
    previous: Option<Complex32>,
}

impl BpskDemod {
    pub fn new(config: BpskDemodConfig) -> Self {
        let channelizer = ComplexChannelizer::new(
            config.input_rate_hz,
            config.channel_rate_hz,
            config.offset_hz,
            config.channel_bandwidth_hz,
        );
        let channel_rate_hz = channelizer.output_rate_hz();
        let output_rate_hz = config.output_rate_hz.max(1.0);

        Self {
            channelizer,
            channel_rate_hz,
            output_rate_hz,
            baseband: Vec::new(),
            resample_step: channel_rate_hz / output_rate_hz,
            input_index: 0.0,
            resample_next: 1.0,
            previous: None,
        }
    }

    pub fn output_rate_hz(&self) -> f64 {
        self.output_rate_hz
    }

    pub fn channel_rate_hz(&self) -> f64 {
        self.channel_rate_hz
    }

    /// Retune the channel NCO while keeping filter and resampler state.
    pub fn set_offset(&mut self, offset_hz: f64) {
        self.channelizer.set_offset(offset_hz);
    }

    /// Append complex baseband samples to `output` at the configured rate.
    pub fn process(&mut self, input: &[Sample], output: &mut Vec<Complex32>) {
        self.baseband.clear();
        self.channelizer.process(input, &mut self.baseband);

        for index in 0..self.baseband.len() {
            let sample = self.baseband[index];
            let Some(previous) = self.previous else {
                self.previous = Some(sample);
                continue;
            };

            self.input_index += 1.0;
            while self.resample_next <= self.input_index {
                let frac = (self.resample_next - (self.input_index - 1.0)).clamp(0.0, 1.0);
                let re = previous.re as f64 + (sample.re as f64 - previous.re as f64) * frac;
                let im = previous.im as f64 + (sample.im as f64 - previous.im as f64) * frac;
                output.push(Complex32::new(re as f32, im as f32));
                self.resample_next += self.resample_step;
            }
            self.previous = Some(sample);
        }
    }

    pub fn reset(&mut self) {
        self.channelizer.reset();
        self.baseband.clear();
        self.input_index = 0.0;
        self.resample_next = 1.0;
        self.previous = None;
    }
}

// ---------------------------------------------------------------------------
// CW demodulator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct CwDemodConfig {
    pub input_rate_hz: f64,
    pub channel_rate_hz: f64,
    pub output_rate_hz: f64,
    pub offset_hz: f64,
    /// Complex channel bandwidth before the BFO.
    pub bandwidth_hz: f64,
    /// Beat-frequency oscillator, e.g. 700 Hz.
    pub bfo_hz: f64,
    pub audio_bandwidth_hz: f64,
}

impl Default for CwDemodConfig {
    fn default() -> Self {
        Self {
            input_rate_hz: 2_400_000.0,
            channel_rate_hz: 48_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 0.0,
            bandwidth_hz: 500.0,
            bfo_hz: 700.0,
            audio_bandwidth_hz: 2_000.0,
        }
    }
}

/// CW product demodulator: channelize, mix with a BFO, take the real part.
pub struct CwDemod {
    channelizer: ComplexChannelizer,
    channel_rate_hz: f64,
    bfo_phase: f64,
    bfo_phase_inc: f64,
    audio_lp: Lowpass4,
    baseband: Vec<Complex32>,
    resample_next: f64,
    resample_step: f64,
    input_index: f64,
    previous_audio: f64,
}

impl CwDemod {
    pub fn new(config: CwDemodConfig) -> Self {
        let channelizer = ComplexChannelizer::new(
            config.input_rate_hz,
            config.channel_rate_hz,
            config.offset_hz,
            config.bandwidth_hz,
        );
        let channel_rate_hz = channelizer.output_rate_hz();
        let audio_bandwidth = config
            .audio_bandwidth_hz
            .clamp(100.0, channel_rate_hz * 0.45);

        Self {
            channelizer,
            channel_rate_hz,
            bfo_phase: 0.0,
            bfo_phase_inc: std::f64::consts::TAU * config.bfo_hz / channel_rate_hz,
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

    /// Retune the channel NCO while keeping filter and BFO state.
    pub fn set_offset(&mut self, offset_hz: f64) {
        self.channelizer.set_offset(offset_hz);
    }

    pub fn process(&mut self, input: &[Sample], output: &mut Vec<f32>) {
        self.baseband.clear();
        self.channelizer.process(input, &mut self.baseband);

        for index in 0..self.baseband.len() {
            let sample = self.baseband[index];
            let (sin, cos) = self.bfo_phase.sin_cos();
            let audio = sample.re as f64 * cos - sample.im as f64 * sin;
            self.bfo_phase += self.bfo_phase_inc;
            if self.bfo_phase > std::f64::consts::TAU {
                self.bfo_phase -= std::f64::consts::TAU;
            }

            let audio = self.audio_lp.process(audio);
            self.input_index += 1.0;
            while self.resample_next <= self.input_index {
                let frac = (self.resample_next - (self.input_index - 1.0)).clamp(0.0, 1.0);
                let value = self.previous_audio + (audio - self.previous_audio) * frac;
                output.push(value as f32);
                self.resample_next += self.resample_step;
            }
            self.previous_audio = audio;
        }
    }

    pub fn reset(&mut self) {
        self.channelizer.reset();
        self.bfo_phase = 0.0;
        self.audio_lp.reset();
        self.baseband.clear();
        self.resample_next = 1.0;
        self.input_index = 0.0;
        self.previous_audio = 0.0;
    }
}

// ---------------------------------------------------------------------------
// Carrier tracking / automatic tuning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct CarrierEstimate {
    /// Carrier frequency relative to the tracker centre, in Hz.
    pub residual_hz: f64,
    /// Peak power above the local noise floor, in dB.
    pub snr_db: f32,
}

/// Causal narrowband carrier tracker used for CW AFC.
///
/// The tracker keeps a wide complex channel centred on `center_hz`, decimates
/// it to a low working rate, and measures the strongest peak in a short FFT
/// window. The wide window means the tracker can keep a drifting carrier in
/// view even when the narrow demodulator filter would have lost it. The
/// caller decides how to translate the residual into a VFO/demod offset.
pub struct CarrierTracker {
    channelizer: ComplexChannelizer,
    fft: Arc<dyn Fft<f32>>,
    fft_size: usize,
    window: Vec<f32>,
    buffer: Vec<Complex32>,
    work: Vec<Complex32>,
    levels: Vec<f32>,
    center_hz: f64,
    search_hz: f64,
    working_rate_hz: f64,
    last_residual_hz: Option<f64>,
    rejected: u8,
}

impl CarrierTracker {
    pub fn new(input_rate_hz: f64, center_hz: f64, working_rate_hz: f64, search_hz: f64) -> Self {
        let working_rate_hz = working_rate_hz.clamp(4_000.0, input_rate_hz.max(4_000.0));
        let ratio = (input_rate_hz / working_rate_hz).max(1.0);
        let decimation = ratio.round().max(1.0) as usize;
        let actual_rate = input_rate_hz / decimation as f64;
        let search_hz = search_hz.clamp(200.0, actual_rate * 0.45);
        let channelizer =
            ComplexChannelizer::new(input_rate_hz, actual_rate, center_hz, search_hz * 2.0);

        // 2048 symbols at 12 kHz is ~170 ms and gives ~6 Hz bins. Parabolic
        // interpolation gets the estimate close enough for a 700 Hz BFO and
        // the decoder's 50 Hz envelope filter.
        let fft_size = 2_048;
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let window: Vec<f32> = (0..fft_size)
            .map(|index| {
                let x = index as f32 / (fft_size - 1) as f32;
                0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
            })
            .collect();

        Self {
            channelizer,
            fft,
            fft_size,
            window,
            buffer: Vec::with_capacity(fft_size * 2),
            work: vec![Complex32::new(0.0, 0.0); fft_size],
            levels: vec![0.0; fft_size],
            center_hz,
            search_hz,
            working_rate_hz: actual_rate,
            last_residual_hz: None,
            rejected: 0,
        }
    }

    pub fn center_hz(&self) -> f64 {
        self.center_hz
    }

    pub fn search_hz(&self) -> f64 {
        self.search_hz
    }

    pub fn working_rate_hz(&self) -> f64 {
        self.working_rate_hz
    }

    /// Move the wide acquisition window and discard the partial FFT buffer.
    pub fn set_center(&mut self, center_hz: f64) {
        self.center_hz = center_hz;
        self.channelizer.set_offset(center_hz);
        self.buffer.clear();
        self.last_residual_hz = None;
        self.rejected = 0;
    }

    /// Feed IQ samples. Returns `Some` when a fresh estimate is available.
    pub fn process(&mut self, input: &[Sample]) -> Option<CarrierEstimate> {
        self.channelizer.process(input, &mut self.buffer);
        let mut last = None;
        // A source block can hold more than one FFT frame at low sample
        // rates. Drain every complete frame so the buffer cannot grow without
        // bound.
        while self.buffer.len() >= self.fft_size {
            if let Some(estimate) = self.process_frame() {
                last = Some(estimate);
            }
        }
        last
    }

    fn process_frame(&mut self) -> Option<CarrierEstimate> {
        // Subtract the complex mean to remove LO leakage/DC without losing
        // keyed tones (a continuous carrier at exactly DC is not interesting:
        // it is already on the VFO).
        let mean = self.buffer[..self.fft_size]
            .iter()
            .fold(Complex32::new(0.0, 0.0), |acc, sample| acc + *sample)
            / self.fft_size as f32;
        for (dst, (sample, window)) in self
            .work
            .iter_mut()
            .zip(self.buffer[..self.fft_size].iter().zip(self.window.iter()))
        {
            *dst = (*sample - mean) * *window;
        }
        self.fft.process(&mut self.work);

        let bin_hz = self.working_rate_hz / self.fft_size as f64;
        let half = self.fft_size / 2;
        for (index, value) in self.work.iter().enumerate() {
            self.levels[index] = (10.0 * (value.norm_sqr() as f64 + 1e-20).log10()) as f32;
        }
        let noise = median(&self.levels);

        let mut best_index = 0usize;
        let mut best_power = f32::NEG_INFINITY;
        for index in 0..self.fft_size {
            let signed = if index <= half {
                index as i64
            } else {
                index as i64 - self.fft_size as i64
            };
            let frequency = signed as f64 * bin_hz;
            // Ignore the exact DC bin and the extreme edges of the channel.
            if frequency.abs() <= bin_hz * 1.5 || frequency.abs() > self.search_hz {
                continue;
            }
            if self.levels[index] > best_power {
                best_power = self.levels[index];
                best_index = index;
            }
        }

        // A CW carrier is a strong narrow peak. Requiring 18 dB over the local
        // median rejects key-up noise peaks that would otherwise throw the VFO
        // onto an unrelated signal.
        let threshold = 18.0;
        let mut result = None;
        if best_power.is_finite() && best_power - noise >= threshold {
            let mut delta = 0.0f64;
            if best_index > 0 && best_index + 1 < self.fft_size {
                let y0 = self.levels[best_index - 1] as f64;
                let y1 = self.levels[best_index] as f64;
                let y2 = self.levels[best_index + 1] as f64;
                let denominator = y0 - 2.0 * y1 + y2;
                if denominator.abs() > 1e-9 {
                    delta = (0.5 * (y0 - y2) / denominator).clamp(-0.5, 0.5);
                }
            }
            let signed = if best_index <= half {
                best_index as f64
            } else {
                best_index as f64 - self.fft_size as f64
            };
            let residual_hz = (signed + delta) * bin_hz;

            // A CW carrier drifts slowly. Reject a peak that is not close to
            // the previous accepted track, but allow reacquisition after a
            // short run of rejects (for example after a retune).
            let continuous = self
                .last_residual_hz
                .is_none_or(|last| (residual_hz - last).abs() <= 120.0);
            if continuous {
                self.last_residual_hz = Some(residual_hz);
                self.rejected = 0;
                result = Some(CarrierEstimate {
                    residual_hz,
                    snr_db: best_power - noise,
                });
            } else {
                self.rejected += 1;
                if self.rejected >= 8 {
                    self.last_residual_hz = Some(residual_hz);
                    self.rejected = 0;
                    result = Some(CarrierEstimate {
                        residual_hz,
                        snr_db: best_power - noise,
                    });
                }
            }
        }

        self.buffer.drain(..self.fft_size);
        result
    }
}

/// Coarse whole-band narrowband peak scanner used by GUI auto-tune.
///
/// It mirrors the offline CW offset estimator: collect high-resolution FFT
/// frames, then prefer the first frame with a strong peak over the single
/// strongest frame. That starting point is much better for a drifting carrier
/// than the peak of a max-hold spectrum.
#[derive(Debug, Clone, Copy)]
struct OffsetPeak {
    frequency_hz: f64,
    power: f64,
    snr_db: f32,
}

pub struct OffsetScan {
    sample_rate_hz: f64,
    fft_size: usize,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    carry: Vec<Sample>,
    work: Vec<Complex32>,
    bins: Vec<usize>,
    levels: Vec<f32>,
    frames: Vec<OffsetPeak>,
    max_frames: usize,
}

impl OffsetScan {
    pub fn new(sample_rate_hz: f64, fft_size: usize, scan_range_hz: f64) -> Self {
        let fft_size = fft_size.max(256).next_power_of_two();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(fft_size);
        let window: Vec<f32> = (0..fft_size)
            .map(|index| {
                let x = index as f32 / (fft_size - 1) as f32;
                0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
            })
            .collect();
        let scan_range_hz = scan_range_hz.clamp(1_000.0, sample_rate_hz * 0.45);
        let bin_hz = sample_rate_hz / fft_size as f64;
        let half = fft_size / 2;
        let bins: Vec<usize> = (0..fft_size)
            .filter(|&index| {
                let frequency = if index <= half {
                    index as f64 * bin_hz
                } else {
                    (index as f64 - fft_size as f64) * bin_hz
                };
                frequency.abs() >= 500.0
                    && frequency.abs() <= sample_rate_hz / 2.0 - 1_000.0
                    && frequency.abs() <= scan_range_hz
            })
            .collect();

        Self {
            sample_rate_hz,
            fft_size,
            fft,
            window,
            carry: Vec::with_capacity(fft_size * 2),
            work: vec![Complex32::new(0.0, 0.0); fft_size],
            levels: vec![0.0; bins.len()],
            bins,
            frames: Vec::new(),
            max_frames: 1_024,
        }
    }

    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    pub fn process(&mut self, input: &[Sample]) {
        self.carry.extend_from_slice(input);
        while self.carry.len() >= self.fft_size && self.frames.len() < self.max_frames {
            for (dst, (sample, window)) in self
                .work
                .iter_mut()
                .zip(self.carry[..self.fft_size].iter().zip(self.window.iter()))
            {
                *dst = *sample * *window;
            }
            self.fft.process(&mut self.work);

            let bin_hz = self.sample_rate_hz / self.fft_size as f64;
            let half = self.fft_size / 2;
            let mut best_index = 0usize;
            let mut best_power = f64::NEG_INFINITY;
            for (slot, &index) in self.bins.iter().enumerate() {
                let power = self.work[index].norm_sqr() as f64;
                self.levels[slot] = (10.0 * (power + 1e-30).log10()) as f32;
                if power > best_power {
                    best_power = power;
                    best_index = index;
                }
            }
            if self.bins.is_empty() || !best_power.is_finite() || best_power <= 0.0 {
                self.carry.drain(..self.fft_size);
                continue;
            }

            let noise = median(&self.levels);
            let peak_db = (10.0 * (best_power + 1e-30).log10()) as f32;
            let snr_db = peak_db - noise;

            let mut refined = if best_index <= half {
                best_index as f64 * bin_hz
            } else {
                (best_index as f64 - self.fft_size as f64) * bin_hz
            };
            if best_index > 0 && best_index + 1 < self.fft_size {
                let y0 = (self.work[best_index - 1].norm_sqr() as f64 + 1e-12).log10();
                let y1 = (best_power + 1e-12).log10();
                let y2 = (self.work[best_index + 1].norm_sqr() as f64 + 1e-12).log10();
                let denominator = y0 - 2.0 * y1 + y2;
                if denominator.abs() > 1e-9 {
                    refined += (0.5 * (y0 - y2) / denominator).clamp(-0.5, 0.5) * bin_hz;
                }
            }

            self.frames.push(OffsetPeak {
                frequency_hz: refined,
                power: best_power,
                snr_db,
            });
            self.carry.drain(..self.fft_size);
        }

        if self.frames.len() >= self.max_frames {
            self.carry.clear();
        }
    }

    /// First strong peak, falling back to the strongest qualifying frame.
    ///
    /// A noise-only scan produces peaks about 10 dB above the median; requiring
    /// 15 dB and either two frames or one 20 dB peak prevents auto-tune from
    /// inventing a frequency when no CW signal is present.
    pub fn result(&self) -> Option<f64> {
        const MIN_SNR_DB: f32 = 15.0;
        let strong: Vec<&OffsetPeak> = self
            .frames
            .iter()
            .filter(|peak| peak.snr_db >= MIN_SNR_DB)
            .collect();
        if strong.len() < 2 && !strong.iter().any(|peak| peak.snr_db >= 20.0) {
            return None;
        }
        let max_power = strong.iter().map(|peak| peak.power).fold(0.0f64, f64::max);
        if max_power <= 0.0 {
            return None;
        }
        for peak in strong {
            if peak.power > 0.55 * max_power {
                return Some(peak.frequency_hz);
            }
        }
        self.frames
            .iter()
            .max_by(|a, b| a.power.total_cmp(&b.power))
            .map(|peak| peak.frequency_hz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complex_tone(sample_rate: f64, frequency: f64, count: usize, amplitude: f32) -> Vec<Sample> {
        let mut phase = 0.0f64;
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            samples.push(Sample::new(
                (phase.cos() as f32) * amplitude,
                (phase.sin() as f32) * amplitude,
            ));
            phase += std::f64::consts::TAU * frequency / sample_rate;
            if phase > std::f64::consts::TAU {
                phase -= std::f64::consts::TAU;
            }
        }
        samples
    }

    #[test]
    fn carrier_tracker_measures_a_tone_offset() {
        let sample_rate = 240_000.0;
        let tone = 1_234.0;
        let samples = complex_tone(sample_rate, tone, 240_000, 0.2);
        let mut tracker = CarrierTracker::new(sample_rate, 0.0, 12_000.0, 3_000.0);
        let mut estimate = None;
        for block in samples.chunks(8_192) {
            if let Some(value) = tracker.process(block) {
                estimate = Some(value);
            }
        }
        let estimate = estimate.expect("estimate");
        assert!(
            (estimate.residual_hz - tone).abs() < 20.0,
            "tracked {} Hz, expected {tone} Hz",
            estimate.residual_hz
        );
    }

    #[test]
    fn offset_scan_finds_a_narrowband_tone() {
        let sample_rate = 240_000.0;
        let tone = -12_345.0;
        let samples = complex_tone(sample_rate, tone, 240_000 * 2, 0.2);
        let mut scan = OffsetScan::new(sample_rate, 32_768, 100_000.0);
        for block in samples.chunks(32_768) {
            scan.process(block);
        }
        let estimate = scan.result().expect("peak");
        assert!(
            (estimate - tone).abs() < 100.0,
            "scanned {estimate} Hz, expected {tone} Hz"
        );
    }

    #[test]
    fn offset_scan_rejects_noise_only() {
        let sample_rate = 240_000.0;
        let mut state = 0x1234_5678u32;
        let mut samples = Vec::with_capacity(480_000);
        for _ in 0..480_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let i = ((state >> 8) as f32 / 16_777_216.0 - 0.5) * 0.1;
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let q = ((state >> 8) as f32 / 16_777_216.0 - 0.5) * 0.1;
            samples.push(Sample::new(i, q));
        }
        let mut scan = OffsetScan::new(sample_rate, 32_768, 100_000.0);
        for block in samples.chunks(32_768) {
            scan.process(block);
        }
        assert!(
            scan.result().is_none(),
            "noise-only scan invented {:?}",
            scan.result()
        );
    }

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
    fn cw_demod_produces_bfo_tone() {
        let fs = 240_000.0;
        let n = 48_000;
        let input: Vec<Sample> = (0..n).map(|_| Sample::new(0.5, 0.0)).collect();
        let mut demod = CwDemod::new(CwDemodConfig {
            input_rate_hz: fs,
            channel_rate_hz: 48_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 0.0,
            bandwidth_hz: 500.0,
            bfo_hz: 700.0,
            audio_bandwidth_hz: 2_000.0,
        });
        let mut audio = Vec::new();
        demod.process(&input, &mut audio);

        assert!(audio.len() > 8_000, "got {} samples", audio.len());
        let steady = &audio[2_000..];
        let crossings = steady
            .windows(2)
            .filter(|window| (window[0] < 0.0) != (window[1] < 0.0))
            .count();
        let measured = crossings as f64 / 2.0 / (steady.len() as f64 / 48_000.0);
        assert!(
            (measured - 700.0).abs() < 30.0,
            "measured {measured} Hz, expected 700 Hz"
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
