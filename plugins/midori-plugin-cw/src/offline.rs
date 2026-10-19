//! Offline I/Q → CW audio pipeline with signal-derived carrier tracking.
//!
//! This is the no-hardware test path. It deliberately does not use SGP4: the
//! CW carrier is tracked directly from the recording, so a re-entered
//! satellite or a missing TLE does not matter.

use std::path::Path;

use anyhow::{Result, bail};
use num_complex::Complex32;
use rustfft::FftPlanner;

use midori_core::Sample;
use midori_dsp::{ComplexChannelizer, CwDemod, CwDemodConfig};

use crate::iq_wav::{WavIqReader, WavIqSpec};

pub const WORKING_RATE_HZ: f64 = 60_000.0;
pub const DEFAULT_SEARCH_HZ: f64 = 3_000.0;
pub const DEFAULT_BFO_HZ: f64 = 700.0;
pub const DEFAULT_OUTPUT_RATE_HZ: f64 = 48_000.0;

#[derive(Debug, Clone)]
pub struct OfflineConfig {
    /// Coarse carrier offset from the recording centre. `None` means scan.
    pub offset_hz: Option<f64>,
    /// Half-width of the automatic coarse scan.
    pub scan_range_hz: f64,
    /// Half-width of the carrier search around the coarse offset.
    pub search_hz: f64,
    pub bfo_hz: f64,
    pub output_rate_hz: f64,
    pub track: bool,
}

impl Default for OfflineConfig {
    fn default() -> Self {
        Self {
            offset_hz: None,
            scan_range_hz: 100_000.0,
            search_hz: DEFAULT_SEARCH_HZ,
            bfo_hz: DEFAULT_BFO_HZ,
            output_rate_hz: DEFAULT_OUTPUT_RATE_HZ,
            track: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DemodResult {
    pub spec: WavIqSpec,
    pub offset_hz: f64,
    pub working_rate_hz: f64,
    pub audio_rate_hz: f64,
    pub audio: Vec<f32>,
    pub signal_start_s: f64,
    pub track_min_hz: f64,
    pub track_max_hz: f64,
}

pub fn demodulate_wav(path: &Path, config: &OfflineConfig) -> Result<DemodResult> {
    let mut reader = WavIqReader::open(path)?;
    let spec = reader.spec();
    if spec.channels < 2 {
        bail!(
            "expected interleaved I/Q (2 channels), got {}",
            spec.channels
        );
    }

    let offset_hz = match config.offset_hz {
        Some(offset) => offset,
        None => estimate_offset(path, &spec, config.scan_range_hz)?,
    };

    let bandwidth = (config.search_hz * 2.0).clamp(5_000.0, 200_000.0);
    let mut channelizer =
        ComplexChannelizer::new(spec.sample_rate_hz, WORKING_RATE_HZ, offset_hz, bandwidth);
    let working_rate_hz = channelizer.output_rate_hz();

    let mut baseband = Vec::new();
    let mut block = Vec::new();
    loop {
        let samples = reader.read_iq(65_536)?;
        if samples.is_empty() {
            break;
        }
        block.clear();
        channelizer.process(&samples, &mut block);
        baseband.extend_from_slice(&block);
    }
    if baseband.is_empty() {
        bail!("no IQ samples read");
    }

    let (track_centers, track) = if config.track {
        let (centers, freqs) = track_carrier(&baseband, working_rate_hz, config.search_hz)?;
        (centers, freqs)
    } else {
        (Vec::new(), Vec::new())
    };
    let track_min_hz = track.iter().copied().fold(f64::INFINITY, f64::min);
    let track_max_hz = track.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    let corrected = if config.track {
        correct_baseband(&baseband, working_rate_hz, &track_centers, &track)
    } else {
        baseband
    };

    let signal_start = find_signal_start(&corrected, working_rate_hz);
    let signal_start_s = signal_start as f64 / working_rate_hz;
    let corrected = &corrected[signal_start.min(corrected.len())..];

    let mut audio = baseband_to_audio(
        corrected,
        working_rate_hz,
        config.output_rate_hz,
        config.bfo_hz,
    );
    normalize_audio(&mut audio);

    Ok(DemodResult {
        spec,
        offset_hz,
        working_rate_hz,
        audio_rate_hz: config.output_rate_hz,
        audio,
        signal_start_s,
        track_min_hz,
        track_max_hz,
    })
}

/// Scan a short prefix and return the strongest narrowband peak, ignoring DC.
pub fn estimate_offset(path: &Path, spec: &WavIqSpec, scan_range_hz: f64) -> Result<f64> {
    const FFT_SIZE: usize = 65_536;
    let mut reader = WavIqReader::open(path)?;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|index| {
            let x = index as f32 / (FFT_SIZE - 1) as f32;
            0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
        })
        .collect();

    let bin_hz = spec.sample_rate_hz / FFT_SIZE as f64;
    let mut buffer = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    let mut chunks: Vec<(f64, f64)> = Vec::new();
    let mut frames = 0usize;
    // Look at up to ~8 seconds. The strongest narrowband burst is a better
    // coarse estimate than an average over a drifting carrier.
    while frames < 256 {
        let samples = reader.read_iq(FFT_SIZE)?;
        if samples.len() < FFT_SIZE {
            break;
        }
        for index in 0..FFT_SIZE {
            buffer[index] = samples[index] * window[index];
        }
        fft.process(&mut buffer);
        let mut best = (0.0f64, f64::NEG_INFINITY);
        for index in 0..FFT_SIZE {
            let frequency = if index <= FFT_SIZE / 2 {
                index as f64 * bin_hz
            } else {
                (index as f64 - FFT_SIZE as f64) * bin_hz
            };
            if frequency.abs() < 500.0
                || frequency.abs() > spec.sample_rate_hz / 2.0 - 1_000.0
                || frequency.abs() > scan_range_hz
            {
                continue;
            }
            let value = buffer[index].norm_sqr() as f64;
            if value > best.1 {
                let mut refined = frequency;
                if index > 0 && index + 1 < FFT_SIZE {
                    let y0 = (buffer[index - 1].norm_sqr() as f64 + 1e-12).log10();
                    let y1 = (value + 1e-12).log10();
                    let y2 = (buffer[index + 1].norm_sqr() as f64 + 1e-12).log10();
                    let denominator = y0 - 2.0 * y1 + y2;
                    if denominator.abs() > 1e-9 {
                        refined += (0.5 * (y0 - y2) / denominator).clamp(-0.5, 0.5) * bin_hz;
                    }
                }
                best = (refined, value);
            }
        }
        chunks.push(best);
        frames += 1;
    }
    if frames == 0 {
        bail!("recording is shorter than one FFT frame");
    }
    let max_power = chunks.iter().map(|(_, power)| *power).fold(0.0, f64::max);
    if max_power <= 0.0 {
        return Ok(0.0);
    }
    // Use the first strong burst rather than the strongest one: the carrier
    // drifts during a pass, so the earliest burst is the best coarse centre
    // for the tracker that follows.
    for (frequency, power) in &chunks {
        if *power > 0.55 * max_power {
            return Ok(*frequency);
        }
    }
    chunks
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(frequency, _)| *frequency)
        .ok_or_else(|| anyhow::anyhow!("no carrier found"))
}

/// Track a narrowband CW carrier through the recording.
///
/// The tracker uses a short-time spectrum and a Viterbi pass with a
/// smoothness penalty, then removes low-confidence frames and smooths the
/// result. It is offline and non-causal, which is fine for a recording.
pub fn track_carrier(
    baseband: &[Sample],
    sample_rate_hz: f64,
    search_hz: f64,
) -> Result<(Vec<f64>, Vec<f64>)> {
    const FFT_SIZE: usize = 2_048;
    const HOP: usize = 128;
    const TRANSITION: i32 = 3;
    const LAMBDA: f32 = 0.8;

    if baseband.len() < FFT_SIZE {
        bail!("baseband is too short to track");
    }

    let bin_hz = sample_rate_hz / FFT_SIZE as f64;
    let mut bins = Vec::new();
    for index in 0..FFT_SIZE {
        let frequency = if index <= FFT_SIZE / 2 {
            index as f64 * bin_hz
        } else {
            (index as f64 - FFT_SIZE as f64) * bin_hz
        };
        if frequency.abs() <= search_hz {
            bins.push((frequency, index));
        }
    }
    if bins.is_empty() {
        bail!("carrier search range is empty");
    }
    bins.sort_by(|a, b| a.0.total_cmp(&b.0));
    let nb = bins.len();
    let starts: Vec<usize> = (0..baseband.len().saturating_sub(FFT_SIZE))
        .step_by(HOP)
        .collect();
    let nt = starts.len();
    if nt == 0 {
        bail!("baseband is too short to track");
    }

    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|index| {
            let x = index as f32 / (FFT_SIZE - 1) as f32;
            0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
        })
        .collect();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut buffer = vec![Complex32::new(0.0, 0.0); FFT_SIZE];
    let mut logp = vec![0.0f32; nt * nb];
    let mut scratch = vec![0.0f32; nb];

    for (frame, start) in starts.iter().copied().enumerate() {
        for index in 0..FFT_SIZE {
            buffer[index] = baseband[start + index] * window[index];
        }
        fft.process(&mut buffer);
        for (bin, (_, index)) in bins.iter().enumerate() {
            logp[frame * nb + bin] = 10.0 * (buffer[*index].norm_sqr() + 1e-12).log10();
        }
        scratch.copy_from_slice(&logp[frame * nb..(frame + 1) * nb]);
        scratch.sort_by(|a, b| a.total_cmp(b));
        let median = scratch[nb / 2];
        for bin in 0..nb {
            logp[frame * nb + bin] = (logp[frame * nb + bin] - median).clamp(-20.0, 80.0);
        }
    }

    let mut dp_prev = logp[0..nb].to_vec();
    let mut dp_cur = vec![0.0f32; nb];
    let mut back = vec![0i8; nt * nb];
    for frame in 1..nt {
        for bin in 0..nb {
            let mut best = f32::NEG_INFINITY;
            let mut best_delta = 0i8;
            for delta in -TRANSITION..=TRANSITION {
                let previous = bin as i32 + delta;
                if previous < 0 || previous >= nb as i32 {
                    continue;
                }
                let value = dp_prev[previous as usize] - LAMBDA * (delta * delta) as f32;
                if value > best {
                    best = value;
                    best_delta = delta as i8;
                }
            }
            back[frame * nb + bin] = best_delta;
            dp_cur[bin] = best + logp[frame * nb + bin];
        }
        std::mem::swap(&mut dp_prev, &mut dp_cur);
    }

    let mut path = vec![0usize; nt];
    path[nt - 1] = dp_prev
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(index, _)| index)
        .unwrap_or(0);
    for frame in (1..nt).rev() {
        let delta = back[frame * nb + path[frame]] as i32;
        path[frame - 1] = (path[frame] as i32 + delta).clamp(0, nb as i32 - 1) as usize;
    }

    let mut frequencies = vec![0.0f64; nt];
    let mut valid = vec![false; nt];
    for frame in 0..nt {
        let bin = path[frame];
        let value = logp[frame * nb + bin];
        let mut delta = 0.0f64;
        if bin > 0 && bin + 1 < nb {
            let y0 = logp[frame * nb + bin - 1] as f64;
            let y1 = value as f64;
            let y2 = logp[frame * nb + bin + 1] as f64;
            let denominator = y0 - 2.0 * y1 + y2;
            if denominator.abs() > 1e-6 {
                delta = (0.5 * (y0 - y2) / denominator).clamp(-0.5, 0.5);
            }
        }
        frequencies[frame] = bins[bin].0 + delta * bin_hz;
        valid[frame] = value > 2.0;
    }

    if !valid.iter().any(|value| *value) {
        bail!("no carrier found in the search range");
    }
    interpolate_invalid(&mut frequencies, &valid);
    let frequencies = smooth_track(&frequencies, sample_rate_hz, HOP);

    let centers: Vec<f64> = starts
        .iter()
        .map(|start| (*start as f64 + FFT_SIZE as f64 / 2.0) / sample_rate_hz)
        .collect();
    Ok((centers, frequencies))
}

fn interpolate_invalid(values: &mut [f64], valid: &[bool]) {
    let valid_indices: Vec<usize> = valid
        .iter()
        .enumerate()
        .filter_map(|(index, is_valid)| is_valid.then_some(index))
        .collect();
    if valid_indices.is_empty() {
        return;
    }
    if valid_indices.len() == 1 {
        let value = values[valid_indices[0]];
        values.fill(value);
        return;
    }
    let original = values.to_vec();
    for index in 0..values.len() {
        if valid[index] {
            continue;
        }
        match valid_indices.binary_search(&index) {
            Ok(_) => {}
            Err(position) => {
                if position == 0 {
                    values[index] = original[valid_indices[0]];
                } else if position >= valid_indices.len() {
                    values[index] = original[*valid_indices.last().unwrap()];
                } else {
                    let before = valid_indices[position - 1];
                    let after = valid_indices[position];
                    let weight = (index - before) as f64 / (after - before) as f64;
                    values[index] =
                        original[before] + (original[after] - original[before]) * weight;
                }
            }
        }
    }
}

fn smooth_track(values: &[f64], sample_rate_hz: f64, hop: usize) -> Vec<f64> {
    if values.len() < 3 {
        return values.to_vec();
    }
    let mut median = values.to_vec();
    let radius = 2;
    let mut scratch = Vec::with_capacity(radius * 2 + 1);
    for (index, value) in median.iter_mut().enumerate() {
        scratch.clear();
        let start = index.saturating_sub(radius);
        let end = (index + radius + 1).min(values.len());
        scratch.extend_from_slice(&values[start..end]);
        scratch.sort_by(|a, b| a.total_cmp(b));
        *value = scratch[scratch.len() / 2];
    }

    let window = ((0.10 * sample_rate_hz / hop as f64).round() as usize).max(1);
    let mut smoothed = vec![0.0f64; values.len()];
    for (index, value) in smoothed.iter_mut().enumerate() {
        let start = index.saturating_sub(window / 2);
        let end = (index + window / 2 + 1).min(values.len());
        let sum: f64 = median[start..end].iter().sum();
        *value = sum / (end - start) as f64;
    }
    smoothed
}

fn correct_baseband(
    baseband: &[Sample],
    sample_rate_hz: f64,
    centers: &[f64],
    frequencies: &[f64],
) -> Vec<Sample> {
    if centers.is_empty() || frequencies.is_empty() {
        return baseband.to_vec();
    }
    let mut output = Vec::with_capacity(baseband.len());
    let mut phase = 0.0f64;
    let mut frame = 0usize;
    for (index, sample) in baseband.iter().copied().enumerate() {
        let time = index as f64 / sample_rate_hz;
        while frame + 1 < centers.len() && centers[frame + 1] < time {
            frame += 1;
        }
        let frequency = if time <= centers[0] {
            frequencies[0]
        } else if frame + 1 >= centers.len() {
            frequencies[frequencies.len() - 1]
        } else {
            let before = centers[frame];
            let after = centers[frame + 1];
            let weight = ((time - before) / (after - before)).clamp(0.0, 1.0);
            frequencies[frame] + (frequencies[frame + 1] - frequencies[frame]) * weight
        };
        phase += std::f64::consts::TAU * frequency / sample_rate_hz;
        if phase > std::f64::consts::TAU {
            phase -= std::f64::consts::TAU;
        }
        let rotation = Complex32::from_polar(1.0, -phase as f32);
        output.push(sample * rotation);
    }
    output
}

fn find_signal_start(baseband: &[Sample], sample_rate_hz: f64) -> usize {
    if baseband.is_empty() {
        return 0;
    }
    let alpha = 1.0 - (-std::f64::consts::TAU * 25.0 / sample_rate_hz).exp();
    let mut i = 0.0f64;
    let mut q = 0.0f64;
    let mut envelope = Vec::with_capacity(baseband.len());
    for sample in baseband {
        i += alpha * (sample.re as f64 - i);
        q += alpha * (sample.im as f64 - q);
        envelope.push((i * i + q * q).sqrt());
    }
    let smooth = (0.010 * sample_rate_hz).round().max(1.0) as usize;
    let mut smoothed = vec![0.0f64; envelope.len()];
    let mut sum = 0.0;
    for index in 0..envelope.len() {
        sum += envelope[index];
        if index >= smooth {
            sum -= envelope[index - smooth];
        }
        smoothed[index] = sum / (index + 1).min(smooth) as f64;
    }
    let mut sorted = smoothed.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let peak = sorted[((sorted.len() - 1) as f64 * 0.995).round() as usize];
    if peak <= 0.0 {
        return 0;
    }
    let threshold = 0.30 * peak;
    for (index, value) in smoothed.iter().enumerate() {
        if *value > threshold {
            return index.saturating_sub((0.020 * sample_rate_hz) as usize);
        }
    }
    0
}

fn baseband_to_audio(
    baseband: &[Sample],
    input_rate_hz: f64,
    output_rate_hz: f64,
    bfo_hz: f64,
) -> Vec<f32> {
    let mut demod = CwDemod::new(CwDemodConfig {
        input_rate_hz,
        channel_rate_hz: output_rate_hz,
        output_rate_hz,
        offset_hz: 0.0,
        bandwidth_hz: 3_000.0,
        bfo_hz,
        audio_bandwidth_hz: 2_000.0,
    });
    let mut audio = Vec::new();
    for block in baseband.chunks(4_096) {
        demod.process(block, &mut audio);
    }
    audio
}

fn normalize_audio(audio: &mut [f32]) {
    let peak = audio.iter().copied().map(f32::abs).fold(0.0f32, f32::max);
    if peak > 1e-6 {
        let scale = 0.8 / peak;
        for sample in audio {
            *sample = (*sample * scale).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_peak_of_a_constant_tone() {
        let sample_rate = 60_000.0;
        let n = 4096;
        let mut buffer: Vec<Complex32> = (0..n)
            .map(|index| {
                let phase = std::f64::consts::TAU * 200.0 * index as f64 / sample_rate;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        fft.process(&mut buffer);
        let bin_hz = sample_rate / n as f64;
        let peak = (0..n)
            .max_by(|a, b| buffer[*a].norm_sqr().total_cmp(&buffer[*b].norm_sqr()))
            .unwrap();
        let frequency = if peak <= n / 2 {
            peak as f64 * bin_hz
        } else {
            (peak as f64 - n as f64) * bin_hz
        };
        println!("fft peak bin {peak} freq {frequency}");
        assert!((frequency - 200.0).abs() < 20.0);
    }

    #[test]
    fn tracks_a_drifting_tone() {
        let sample_rate = 60_000.0;
        let count = 60_000 * 3;
        let mut phase = 0.0f64;
        let mut samples = Vec::with_capacity(count);
        for index in 0..count {
            let time = index as f64 / sample_rate;
            let frequency = 200.0 - 80.0 * time;
            phase += std::f64::consts::TAU * frequency / sample_rate;
            samples.push(Sample::new(
                (phase.cos() as f32) * 0.2,
                (phase.sin() as f32) * 0.2,
            ));
        }
        let (centers, track) = track_carrier(&samples, sample_rate, 1_000.0).unwrap();
        for index in (0..track.len()).step_by(track.len() / 6) {
            println!(
                "t={:.3} track={:.1} expected={:.1}",
                centers[index],
                track[index],
                200.0 - 80.0 * centers[index]
            );
        }
        let middle = track[track.len() / 2];
        let expected = 200.0 - 80.0 * centers[centers.len() / 2];
        assert!(
            (middle - expected).abs() < 30.0,
            "tracked {middle} Hz, expected {expected} Hz"
        );
    }

    #[test]
    fn trims_leading_noise() {
        let sample_rate = 60_000.0;
        let mut samples = vec![Sample::new(0.005, 0.0); sample_rate as usize];
        for index in 0..sample_rate as usize {
            let phase = std::f64::consts::TAU * 200.0 * index as f64 / sample_rate;
            samples.push(Sample::new(
                0.2 * phase.cos() as f32,
                0.2 * phase.sin() as f32,
            ));
        }
        let start = find_signal_start(&samples, sample_rate);
        assert!(
            start > 50_000 && start < 61_000,
            "unexpected signal start sample {start}"
        );
    }
}
