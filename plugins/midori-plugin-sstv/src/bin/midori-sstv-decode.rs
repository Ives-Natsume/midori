//! Offline SSTV decoder: WAV in, PNG out.
//!
//! This is the no-hardware test path. It uses the same [`SstvSession`] as the
//! sidecar, so a recording can be checked without a Pluto or a transmitter.
//!
//! ```bash
//! cargo run --release -p midori-plugin-sstv --bin midori-sstv-decode -- \
//!   recording.wav --out-dir captures
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use hound::{SampleFormat, WavReader};
use midori_plugin_sstv::{SessionEvent, SstvSession};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: midori-sstv-decode <input.wav> [--out-dir DIR]"))?;
    let mut out_dir = PathBuf::from("captures");
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--out-dir" => {
                out_dir = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--out-dir needs a value"))?,
                );
            }
            other => bail!("unknown argument '{other}'"),
        }
    }

    let mut reader = WavReader::open(&input).with_context(|| format!("opening {input}"))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        bail!("WAV has no channels or no sample rate");
    }

    let mono = match spec.sample_format {
        SampleFormat::Float => {
            let samples: Vec<f32> = reader
                .samples::<f32>()
                .collect::<Result<Vec<_>, _>>()
                .context("reading float WAV")?;
            mix_to_mono(&samples, spec.channels as usize)
        }
        SampleFormat::Int => {
            let scale = (1u64 << (spec.bits_per_sample.saturating_sub(1)).min(30)) as f32;
            let samples: Vec<f32> = reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / scale))
                .collect::<Result<Vec<_>, _>>()
                .context("reading integer WAV")?;
            mix_to_mono(&samples, spec.channels as usize)
        }
    };

    println!(
        "input: {input} | {} Hz | {} channel(s) | {:.1} s",
        spec.sample_rate,
        spec.channels,
        mono.len() as f64 / spec.sample_rate as f64
    );

    let mut session = SstvSession::new(spec.sample_rate)?;
    session.set_output_dir(Some(out_dir.clone()));
    let mut images = 0usize;
    let mut last_progress = None;

    for block in mono.chunks(4_096) {
        for event in session.process(block) {
            match event {
                SessionEvent::VisDetected {
                    mode_name,
                    header_shift_hz,
                    ..
                } => println!("VIS {mode_name} (header {header_shift_hz:+.0} Hz)"),
                SessionEvent::UnknownVis {
                    code,
                    header_shift_hz,
                } => eprintln!("unknown VIS {code} (header {header_shift_hz:+.0} Hz)"),
                SessionEvent::Progress {
                    mode_name,
                    line,
                    total_lines,
                    ..
                } => {
                    if last_progress != Some(line / 16) {
                        last_progress = Some(line / 16);
                        println!("{mode_name}: {line}/{total_lines}");
                    }
                }
                SessionEvent::Image(image) => {
                    images += 1;
                    match &image.path {
                        Some(path) => println!(
                            "image: {} {}x{} -> {}",
                            image.mode_name,
                            image.width,
                            image.height,
                            path.display()
                        ),
                        None => println!(
                            "image: {} {}x{} (not saved)",
                            image.mode_name, image.width, image.height
                        ),
                    }
                }
            }
        }
    }

    if images == 0 {
        bail!("no SSTV image found; check the mode, audio level, and tuning");
    }
    Ok(())
}

fn mix_to_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}
