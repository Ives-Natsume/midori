//! Offline CW decoder for I/Q WAV recordings.
//!
//! ```bash
//! midori-cw-decode data/BOTAN_TLM_CW_437393500Hz.wav --offset -13500
//! midori-cw-decode recording.wav --sidecar
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use midori_core::{BlockFlags, Frame, FrameMeta, StreamData};
use midori_plugin::{
    DisplayUpdate, InputSpec, Manifest, OutputKind, PROTOCOL_VERSION, PluginEvent, PluginHost,
    SidecarPlugin,
};
use midori_plugin_cw::offline::{DEFAULT_BFO_HZ, DEFAULT_OUTPUT_RATE_HZ, OfflineConfig};
use midori_plugin_cw::{MorseDecoder, MorseEvent, offline};

struct Args {
    input: PathBuf,
    offset_hz: Option<f64>,
    scan_range_hz: f64,
    search_hz: f64,
    bfo_hz: f64,
    output_rate_hz: f64,
    wav_out: Option<PathBuf>,
    sidecar: bool,
    plugin: Option<PathBuf>,
    track: bool,
    offset_search: bool,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let config = OfflineConfig {
        offset_hz: args.offset_hz,
        scan_range_hz: args.scan_range_hz,
        search_hz: args.search_hz,
        bfo_hz: args.bfo_hz,
        output_rate_hz: args.output_rate_hz,
        track: args.track,
    };

    let (result, direct_text, direct_wpm) = decode_best(&args.input, &config, args.offset_search)?;
    if args.offset_hz.is_none() && args.offset_search {
        println!("selected carrier: {:.1} Hz", result.offset_hz);
    }
    println!(
        "input: {} | {:.3} MSPS | {} ch | {} bits | {:.2} s",
        args.input.display(),
        result.spec.sample_rate_hz / 1e6,
        result.spec.channels,
        result.spec.bits_per_sample,
        result.spec.frames as f64 / result.spec.sample_rate_hz
    );
    println!(
        "carrier: {:.1} Hz from centre | search +/-{:.0} Hz | working {:.0} Hz",
        result.offset_hz, args.search_hz, result.working_rate_hz
    );
    if args.track {
        println!(
            "track: {:.1} .. {:.1} Hz | signal starts at {:.3} s",
            result.track_min_hz, result.track_max_hz, result.signal_start_s
        );
    }
    println!(
        "audio: {:.0} Hz | {:.2} s | {} samples",
        result.audio_rate_hz,
        result.audio.len() as f64 / result.audio_rate_hz,
        result.audio.len()
    );

    if let Some(path) = &args.wav_out {
        write_wav(path, &result.audio, result.audio_rate_hz)?;
        println!("wrote {}", path.display());
    }

    if args.sidecar {
        let plugin_path = args.plugin.clone().unwrap_or_else(default_plugin_path);
        let text = decode_with_sidecar(
            &plugin_path,
            &result.audio,
            result.audio_rate_hz,
            args.bfo_hz,
        )?;
        println!("\nsidecar text: {text:?}");
    } else {
        println!("\ntext: {direct_text:?}");
        println!("wpm:  {direct_wpm:.1}");
    }

    Ok(())
}

fn decode_best(
    input: &Path,
    config: &OfflineConfig,
    offset_search: bool,
) -> Result<(offline::DemodResult, String, f32)> {
    let first = offline::demodulate_wav(input, config)?;
    let (mut best_text, mut best_wpm) =
        decode_direct(&first.audio, first.audio_rate_hz, config.bfo_hz);
    let mut best = (score_text(&best_text), first);
    if !offset_search || config.offset_hz.is_some() {
        return Ok((best.1, best_text, best_wpm));
    }

    let auto_offset = best.1.offset_hz;
    for delta in [-40.0, -30.0, -20.0, -10.0, 10.0, 20.0, 30.0, 40.0] {
        let mut candidate_config = config.clone();
        candidate_config.offset_hz = Some(auto_offset + delta);
        let result = match offline::demodulate_wav(input, &candidate_config) {
            Ok(result) => result,
            Err(_) => continue,
        };
        let (text, wpm) = decode_direct(&result.audio, result.audio_rate_hz, config.bfo_hz);
        let score = score_text(&text);
        if score > best.0 {
            best = (score, result);
            best_text = text;
            best_wpm = wpm;
        }
    }
    Ok((best.1, best_text, best_wpm))
}

fn score_text(text: &str) -> f64 {
    let mut score = 0.0;
    for word in text.split_whitespace() {
        let length = word.chars().count() as f64;
        score += length * length;
    }
    score + 0.5 * text.chars().filter(|c| c.is_ascii_alphanumeric()).count() as f64
}

fn decode_direct(audio: &[f32], sample_rate_hz: f64, tone_hz: f64) -> (String, f32) {
    let mut decoder = MorseDecoder::new(sample_rate_hz, tone_hz);
    let mut text = String::new();
    for block in audio.chunks(512) {
        for event in decoder.process(block) {
            match event {
                MorseEvent::Char(character) => text.push(character),
                MorseEvent::Word => text.push(' '),
            }
        }
    }
    for event in decoder.flush() {
        match event {
            MorseEvent::Char(character) => text.push(character),
            MorseEvent::Word => text.push(' '),
        }
    }
    (text, decoder.wpm())
}

fn decode_with_sidecar(
    plugin_path: &Path,
    audio: &[f32],
    sample_rate_hz: f64,
    tone_hz: f64,
) -> Result<String> {
    if !plugin_path.exists() {
        bail!(
            "CW sidecar not found at {}; build the workspace or set MIDORI_CW_PLUGIN",
            plugin_path.display()
        );
    }

    let manifest = Manifest {
        name: "cw".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol: PROTOCOL_VERSION,
        command: vec![plugin_path.display().to_string()],
        input: InputSpec::Audio {
            sample_rate_hz,
            channels: 1,
        },
        outputs: vec![OutputKind::Display, OutputKind::Packets],
        params: vec![],
    };
    let mut plugin = SidecarPlugin::new(manifest).context("creating CW sidecar")?;
    plugin.start().context("starting CW sidecar")?;
    plugin
        .configure(&serde_json::json!({ "tone_hz": tone_hz }))
        .context("configuring CW sidecar")?;

    let chunk = 4_096usize;
    for (seq, block) in audio.chunks(chunk).enumerate() {
        let meta = FrameMeta {
            seq: seq as u64,
            start_sample: (seq * chunk) as u64,
            timestamp_ns: Some((seq * chunk) as u64 * 1_000_000_000 / sample_rate_hz as u64),
            sample_rate_hz,
            center_freq_hz: 0.0,
            nominal_freq_hz: 0.0,
            bandwidth_hz: 3_000.0,
            vfo_id: Some(0),
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
            flags: BlockFlags::default(),
        };
        let samples: Arc<[f32]> = Arc::from(block.to_vec().into_boxed_slice());
        let frame = Frame::new(meta, StreamData::Audio(samples));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match plugin.send_frame(frame.clone()) {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => bail!("sending audio to CW sidecar: {error}"),
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut text = String::new();
    while Instant::now() < deadline {
        match plugin.try_recv() {
            Ok(Some(PluginEvent::Display {
                update: DisplayUpdate::Text { lines, .. },
            })) => {
                text = lines.join("\n");
            }
            Ok(Some(PluginEvent::Status { message, .. })) => {
                eprintln!("[sidecar] {message}");
            }
            Ok(Some(PluginEvent::Error { message, .. })) => {
                bail!("CW sidecar error: {message}");
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => bail!("reading CW sidecar event: {error}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    plugin.stop().ok();
    Ok(text)
}

fn write_wav(path: &Path, audio: &[f32], sample_rate_hz: f64) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: sample_rate_hz.round() as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    for sample in audio {
        writer.write_sample((sample.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

fn default_plugin_path() -> PathBuf {
    if let Some(path) = std::env::var_os("MIDORI_CW_PLUGIN") {
        return PathBuf::from(path);
    }
    let name = if cfg!(windows) {
        "midori-plugin-cw.exe"
    } else {
        "midori-plugin-cw"
    };
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        print_help();
        bail!("missing input WAV");
    };
    if input == "--help" || input == "-h" {
        print_help();
        std::process::exit(0);
    }

    let mut parsed = Args {
        input: PathBuf::from(input),
        offset_hz: None,
        scan_range_hz: 100_000.0,
        search_hz: 3_000.0,
        bfo_hz: DEFAULT_BFO_HZ,
        output_rate_hz: DEFAULT_OUTPUT_RATE_HZ,
        wav_out: None,
        sidecar: false,
        plugin: None,
        track: true,
        offset_search: true,
    };

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--offset" => parsed.offset_hz = Some(next_f64(&mut args, "--offset")?),
            "--scan-range" => {
                parsed.scan_range_hz = next_f64(&mut args, "--scan-range")?.abs().max(1_000.0);
            }
            "--search" => parsed.search_hz = next_f64(&mut args, "--search")?.max(100.0),
            "--bfo" => parsed.bfo_hz = next_f64(&mut args, "--bfo")?,
            "--rate" => parsed.output_rate_hz = next_f64(&mut args, "--rate")?,
            "--wav-out" => {
                parsed.wav_out = Some(PathBuf::from(next_string(&mut args, "--wav-out")?));
            }
            "--sidecar" => parsed.sidecar = true,
            "--plugin" => parsed.plugin = Some(PathBuf::from(next_string(&mut args, "--plugin")?)),
            "--no-track" => parsed.track = false,
            "--no-offset-search" => parsed.offset_search = false,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unknown argument '{other}'"),
        }
    }

    Ok(parsed)
}

fn next_string(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
}

fn next_f64(args: &mut impl Iterator<Item = String>, name: &str) -> Result<f64> {
    let value = next_string(args, name)?;
    value
        .parse::<f64>()
        .with_context(|| format!("invalid value '{value}' for {name}"))
}

fn print_help() {
    println!(
        "midori-cw-decode - offline CW decoder for I/Q WAV recordings\n\
\n\
Usage:\n\
  midori-cw-decode <input.wav> [options]\n\
\n\
Options:\n\
  --offset <Hz>      coarse carrier offset from the recording centre (default: scan)\n\
  --scan-range <Hz>  automatic scan half-width (default: 100000)\n\
  --search <Hz>      carrier search half-width (default: 3000)\n\
  --bfo <Hz>         audio beat tone (default: 700)\n\
  --rate <Hz>        output audio rate (default: 48000)\n\
  --wav-out <path>   write the demodulated audio as mono 16-bit WAV\n\
  --sidecar          send audio through the real midori-plugin-cw sidecar\n\
  --plugin <path>    sidecar binary (default: next to this executable)\n\
  --no-track         disable signal-derived carrier tracking\n\
  --no-offset-search use only the first coarse offset estimate\n\
  -h, --help         show this help"
    );
}
