//! Midori command-line tools.
//!
//! This is the first consumer of the core. It records IQ to SigMF, inspects
//! recordings, prints averaged spectra, and demodulates AM channels to WAV.
//! The GUI and decoder sidecars will use the same crates.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use midori_core::{GainMode, IqSource, SourceConfig};
use midori_dsp::{
    AmDemod, AmDemodConfig, SpectrumAnalyzer, fftshift, frequency_axis, median, top_peaks,
};
use midori_sigmf::{SigMfReader, SigMfWriter};
use midori_source_libiio::LibiioSource;

const MB: f64 = 1024.0 * 1024.0;

struct Args {
    positional: Vec<String>,
    flags: HashMap<String, String>,
}

impl Args {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut positional = Vec::new();
        let mut flags = HashMap::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if let Some(rest) = arg.strip_prefix("--") {
                if let Some((key, value)) = rest.split_once('=') {
                    flags.insert(key.to_string(), value.to_string());
                } else if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                    flags.insert(rest.to_string(), args[i + 1].clone());
                    i += 1;
                } else {
                    flags.insert(rest.to_string(), "true".to_string());
                }
            } else {
                positional.push(arg.clone());
            }
            i += 1;
        }
        Self { positional, flags }
    }

    fn command(&self) -> Option<&str> {
        self.positional.first().map(String::as_str)
    }

    fn str(&self, key: &str, default: &str) -> String {
        self.flags
            .get(key)
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    fn optional_str(&self, key: &str) -> Option<String> {
        self.flags.get(key).cloned()
    }

    fn f64(&self, key: &str, default: f64) -> Result<f64> {
        match self.flags.get(key) {
            Some(value) => value
                .parse::<f64>()
                .with_context(|| format!("--{key} expects a number, got '{value}'")),
            None => Ok(default),
        }
    }

    fn usize(&self, key: &str, default: usize) -> Result<usize> {
        match self.flags.get(key) {
            Some(value) => value
                .parse::<usize>()
                .with_context(|| format!("--{key} expects an integer, got '{value}'")),
            None => Ok(default),
        }
    }

    fn path(&self, key: &str, default: Option<&Path>) -> Result<PathBuf> {
        if let Some(value) = self.flags.get(key) {
            return Ok(PathBuf::from(value));
        }
        if let Some(value) = self.positional.get(1) {
            return Ok(PathBuf::from(value));
        }
        default
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("missing input path (or --{key})"))
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command() {
        Some("record") => cmd_record(&args),
        Some("info") => cmd_info(&args),
        Some("spectrum") => cmd_spectrum(&args),
        Some("am") => cmd_am(&args),
        Some("help") | Some("--help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => {
            print_help();
            bail!("unknown command '{other}'");
        }
    }
}

fn print_help() {
    println!(
        r#"midori - SDR capture and analysis tools

USAGE:
  midori record   [options]       Record IQ from a Pluto to SigMF
  midori info     <capture>       Show SigMF metadata
  midori spectrum <capture>       Print the averaged spectrum and top peaks
  midori am       <capture>       Demodulate an AM channel to a WAV file

RECORD OPTIONS:
  --freq <MHz>       Centre frequency (default 120.0)
  --rate <MSPS>      Sample rate (default 2.084)
  --bw <MHz>         RF bandwidth (default = sample rate)
  --gain <dB>        Manual RX gain (default 40)
  --port <name>      RF port (default A_BALANCED, i.e. RX1A)
  --seconds <s>      Capture duration (default 20)
  --buffer <n>       Samples per libiio buffer (default 32768)
  --out <path>       Output base path, without extension (required)
  --uri <uri>        libiio URI (default usb:)
  --desc <text>      SigMF description

SPECTRUM OPTIONS:
  --fft <n>          FFT size (default 8192)
  --top <n>          Number of peaks to print (default 15)
  --min-dist <n>     Minimum bin distance between peaks (default 5)
  --out <csv>        Also write freq_hz,power_db as CSV

AM OPTIONS:
  --freq <MHz>       Absolute channel frequency (default: capture centre)
  --bw <Hz>          Channel bandwidth (default 6000)
  --rate <Hz>        Audio sample rate (default 48000)
  --out <wav>        Output WAV path (default: <capture>.wav)

EXAMPLES:
  midori record --freq 120.0 --gain 40 --seconds 30 --out captures/airband_120m
  midori spectrum captures/airband_120m --top 20
  midori am captures/airband_120m --freq 120.0 --out captures/airband_120m.wav
"#
    );
}

fn cmd_record(args: &Args) -> Result<()> {
    let out = args
        .optional_str("out")
        .ok_or_else(|| anyhow::anyhow!("record requires --out <base-path>"))?;
    let freq_mhz = args.f64("freq", 120.0)?;
    let rate_msps = args.f64("rate", 2.084)?;
    let bw_mhz = args.f64("bw", rate_msps)?;
    let gain_db = args.f64("gain", 40.0)?;
    let seconds = args.f64("seconds", 20.0)?;
    let buffer_samples = args.usize("buffer", 32_768)?;
    let uri = args.optional_str("uri");
    let port = args.str("port", "A_BALANCED");
    let desc = args.str("desc", "Midori airband test capture");

    if seconds <= 0.0 {
        bail!("--seconds must be positive");
    }

    let mut source = LibiioSource::open(uri.as_deref())?;
    let caps = source.caps();
    println!("source: {}", caps.name);
    println!(
        "range: {:.3}-{:.3} MHz, gain {:.0}..{:.0} dB",
        caps.min_freq_hz / 1e6,
        caps.max_freq_hz / 1e6,
        caps.gain_range_db.0,
        caps.gain_range_db.1
    );

    let config = SourceConfig {
        center_freq_hz: freq_mhz * 1e6,
        sample_rate_hz: rate_msps * 1e6,
        bandwidth_hz: bw_mhz * 1e6,
        gain_db,
        gain_mode: GainMode::Manual,
        rf_port: Some(port),
        buffer_samples,
    };

    let spec = source.configure(config)?;
    println!(
        "tuned: {:.6} MHz, {:.6} MSPS, port {}",
        spec.center_freq_hz / 1e6,
        spec.sample_rate_hz / 1e6,
        args.str("port", "A_BALANCED")
    );

    source.start()?;

    let out_path = normalize_capture_base(Path::new(&out));
    let mut writer = SigMfWriter::create(
        &out_path,
        spec.sample_rate_hz,
        spec.center_freq_hz,
        spec.full_scale,
        &desc,
    )?;

    println!(
        "recording {seconds:.1} s to {}.sigmf-data ...",
        out_path.display()
    );
    let started = Instant::now();
    let mut last_report = 0.0f64;
    let mut blocks = 0u64;

    while started.elapsed().as_secs_f64() < seconds {
        let block = source.read_block()?;
        writer.write_block(&block.samples)?;
        blocks += 1;

        let elapsed = started.elapsed().as_secs_f64();
        if elapsed - last_report >= 1.0 {
            last_report = elapsed;
            let samples = writer.samples_written();
            let mib = samples as f64 * 4.0 / MB;
            print!("\r  {elapsed:5.1} s  {samples:>10} samples  {mib:8.1} MiB  {blocks} blocks");
            std::io::stdout().flush()?;
        }
    }
    println!();

    let samples = writer.samples_written();
    let seconds_recorded = samples as f64 / spec.sample_rate_hz;
    writer.finish()?;
    source.stop()?;

    println!(
        "done: {:.2} s, {} samples ({:.1} MiB)",
        seconds_recorded,
        samples,
        samples as f64 * 4.0 / MB
    );
    println!("  {}.sigmf-data", out_path.display());
    println!("  {}.sigmf-meta", out_path.display());
    Ok(())
}

fn normalize_capture_base(path: &Path) -> PathBuf {
    if path.extension().and_then(|ext| ext.to_str()) == Some("sigmf") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    }
}

fn cmd_info(args: &Args) -> Result<()> {
    let path = args.path("file", None)?;
    let reader = SigMfReader::open(&path)?;
    let info = reader.info();
    println!("capture:  {}", info.base.display());
    println!("datatype: {}", info.datatype);
    println!("rate:     {:.6} MSPS", info.sample_rate_hz / 1e6);
    println!("centre:   {:.6} MHz", info.center_freq_hz / 1e6);
    println!("full scale: {:.1}", info.full_scale);
    match info.sample_count {
        Some(n) => {
            println!("samples:  {n}");
            if let Some(duration) = info.duration_seconds() {
                println!("duration: {duration:.2} s");
            }
        }
        None => println!("samples:  unknown"),
    }
    Ok(())
}

fn cmd_spectrum(args: &Args) -> Result<()> {
    let path = args.path("file", None)?;
    let fft_size = args.usize("fft", 8192)?;
    let top = args.usize("top", 15)?;
    let min_distance = args.usize("min-dist", 5)?;
    if fft_size < 16 {
        bail!("--fft must be at least 16");
    }

    let mut reader = SigMfReader::open(&path)?;
    let spec = reader.spec();
    let mut analyzer = SpectrumAnalyzer::new(fft_size);
    let mut blocks = 0u64;
    loop {
        let block = reader.read_block(65_536)?;
        if block.is_empty() {
            break;
        }
        analyzer.process(&block);
        blocks += 1;
    }
    if analyzer.frames() == 0 {
        bail!("capture is shorter than one FFT frame");
    }

    let db = analyzer.spectrum_db();
    let shifted = fftshift(&db);
    let frequencies = frequency_axis(spec.center_freq_hz, spec.sample_rate_hz, shifted.len());
    let noise = median(&shifted);
    let peaks = top_peaks(&shifted, &frequencies, top, min_distance);

    println!(
        "capture: {:.6} MHz @ {:.6} MSPS | FFT {} | {} frames | {} blocks",
        spec.center_freq_hz / 1e6,
        spec.sample_rate_hz / 1e6,
        fft_size,
        analyzer.frames(),
        blocks
    );
    println!("noise floor (median): {noise:.1} dBFS");
    println!("top peaks:");
    for peak in peaks {
        println!(
            "  {:>12.6} MHz  {:>8.1} dBFS  (+{:>5.1} dB over median)",
            peak.frequency_hz / 1e6,
            peak.power_db,
            peak.power_db - noise
        );
    }

    if let Some(csv) = args.optional_str("out") {
        let file = File::create(&csv).with_context(|| format!("creating {csv}"))?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "freq_hz,power_db")?;
        for (freq, power) in frequencies.iter().zip(shifted.iter()) {
            writeln!(writer, "{freq:.3},{power:.3}")?;
        }
        writer.flush()?;
        println!("spectrum CSV: {csv}");
    }

    Ok(())
}

fn cmd_am(args: &Args) -> Result<()> {
    let path = args.path("file", None)?;
    let mut reader = SigMfReader::open(&path)?;
    let spec = reader.spec();
    let freq_mhz = args.f64("freq", spec.center_freq_hz / 1e6)?;
    let bandwidth_hz = args.f64("bw", 6_000.0)?;
    let audio_rate = args.f64("rate", 48_000.0)?;
    let out = args
        .optional_str("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{}.wav", path.display())));

    let offset_hz = freq_mhz * 1e6 - spec.center_freq_hz;
    if offset_hz.abs() > spec.sample_rate_hz * 0.45 {
        bail!(
            "channel {:.6} MHz is outside the capture ({:.6} MHz ± {:.3} MHz)",
            freq_mhz,
            spec.center_freq_hz / 1e6,
            spec.sample_rate_hz / 2e6
        );
    }

    println!(
        "AM demod: {:.6} MHz (offset {:+.1} kHz), BW {:.0} Hz, audio {:.0} Hz",
        freq_mhz,
        offset_hz / 1e3,
        bandwidth_hz,
        audio_rate
    );

    let mut demod = AmDemod::new(AmDemodConfig {
        input_rate_hz: spec.sample_rate_hz,
        output_rate_hz: audio_rate,
        offset_hz,
        bandwidth_hz,
    });
    let mut audio = Vec::new();
    loop {
        let block = reader.read_block(65_536)?;
        if block.is_empty() {
            break;
        }
        demod.process(&block, &mut audio);
    }
    if audio.is_empty() {
        bail!("no audio produced (empty capture?)");
    }

    normalize(&mut audio);
    let peak = audio.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();

    write_wav(&out, &audio, audio_rate.round() as u32)?;
    println!(
        "wrote {} ({:.2} s, {} samples, peak {:.2}, rms {:.3})",
        out.display(),
        audio.len() as f64 / audio_rate,
        audio.len(),
        peak,
        rms
    );
    Ok(())
}

fn normalize(audio: &mut [f32]) {
    if audio.is_empty() {
        return;
    }
    let mean = audio.iter().sum::<f32>() / audio.len() as f32;
    for sample in audio.iter_mut() {
        *sample -= mean;
    }
    let peak = audio.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    if peak > 1e-6 {
        let gain = 0.8 / peak;
        for sample in audio.iter_mut() {
            *sample *= gain;
        }
    }
}

fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = BufWriter::new(File::create(path)?);
    let data_len = (samples.len() * 2) as u32;
    let riff_len = 36 + data_len;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_len.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?; // PCM
    file.write_all(&1u16.to_le_bytes())?; // mono
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&(sample_rate * 2).to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;

    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        file.write_all(&value.to_le_bytes())?;
    }
    file.flush()?;
    Ok(())
}
