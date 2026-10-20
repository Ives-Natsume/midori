//! Offline SSDV decoder.
//!
//! Two input paths:
//!
//! * RIFF/RF64 WAV with interleaved I/Q: channelize at `--offset`, run the
//!   built-in 9k6 BPSK/CCSDS demodulator, then collect SSDV images.
//! * Raw dumps: 223-byte CCSDS frames, 218-byte DSLWP packets, 256-byte
//!   classic SSDV packets, or a base64 file containing any of those.
//!
//! This is deliberately a separate binary from the sidecar so recordings can
//! be replayed without a radio, exactly like `midori-sstv-decode`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use base64::Engine;
use midori_dsp::{BpskDemod, BpskDemodConfig};
use midori_plugin::StatusLevel;
use midori_plugin_ssdv::{
    Bpsk9600AudioReceiver, Bpsk9600Receiver, DEFAULT_CRC_INIT, SessionEvent, SsdvSession,
    parse_crc_inits,
};
use midori_source_file::WavIqReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Auto,
    Iq,
    Frames,
    Packets,
}

struct Args {
    input: PathBuf,
    out_dir: PathBuf,
    offset_hz: f64,
    bandwidth_hz: f64,
    mode: InputMode,
    audio_if: bool,
    crc_inits: Vec<String>,
    frame_header_len: usize,
}

fn main() -> ExitCode {
    match run() {
        Ok(images) if images > 0 => ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!("no SSDV images decoded");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<usize> {
    let args = parse_args()?;
    let crc_inits = parse_crc_inits(&args.crc_inits);
    let crc_text: Vec<String> = crc_inits
        .iter()
        .map(|init| format!("0x{init:08X}"))
        .collect();
    eprintln!(
        "SSDV CRC init [{}], frame header {} bytes",
        crc_text.join(", "),
        args.frame_header_len
    );
    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("creating {}", args.out_dir.display()))?;

    let is_wav = args
        .input
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
        || looks_like_wav(&args.input)?;

    match args.mode {
        InputMode::Iq if args.audio_if => decode_wav_audio_if(&args),
        InputMode::Iq => decode_wav(&args),
        InputMode::Frames | InputMode::Packets => decode_raw(&args, args.mode),
        InputMode::Auto if is_wav && args.audio_if => decode_wav_audio_if(&args),
        InputMode::Auto if is_wav => {
            let reader = WavIqReader::open(&args.input)?;
            if reader.spec().channels == 1 {
                eprintln!(
                    "mono WAV detected; treating it as 48 kHz real 12 kHz-IF audio \
                     (use --iq to force complex input)"
                );
                decode_wav_audio_if(&args)
            } else {
                decode_wav(&args)
            }
        }
        InputMode::Auto => decode_raw(&args, InputMode::Auto),
    }
}

fn parse_args() -> Result<Args> {
    let mut input = None;
    let mut out_dir = PathBuf::from("captures");
    let mut offset_hz = 0.0;
    let mut bandwidth_hz = 30_000.0;
    let mut mode = InputMode::Auto;
    let mut audio_if = false;
    let mut crc_inits = vec![format!("0x{DEFAULT_CRC_INIT:08X}")];
    let mut frame_header_len = 5usize;
    let mut crc_inits_set = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--out-dir" => {
                out_dir = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--out-dir needs a path"))?,
                );
            }
            "--offset" => {
                offset_hz = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--offset needs Hz"))?
                    .parse()
                    .context("parsing --offset")?;
            }
            "--offset-khz" => {
                let khz: f64 = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--offset-khz needs kHz"))?
                    .parse()
                    .context("parsing --offset-khz")?;
                offset_hz = khz * 1_000.0;
            }
            "--bandwidth" | "--bw" => {
                bandwidth_hz = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--bandwidth needs Hz"))?
                    .parse()
                    .context("parsing --bandwidth")?;
            }
            "--iq" => mode = InputMode::Iq,
            "--audio-if" => {
                mode = InputMode::Iq;
                audio_if = true;
            }
            "--crc-init" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--crc-init needs a value"))?;
                if !crc_inits_set {
                    crc_inits.clear();
                    crc_inits_set = true;
                }
                crc_inits.push(value);
            }
            "--frame-header-len" => {
                frame_header_len = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--frame-header-len needs a value"))?
                    .parse()
                    .context("parsing --frame-header-len")?;
            }
            "--frames" => mode = InputMode::Frames,
            "--packets" => mode = InputMode::Packets,
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other => {
                if input.replace(PathBuf::from(other)).is_some() {
                    bail!("more than one input file given");
                }
            }
        }
    }

    Ok(Args {
        input: input.ok_or_else(|| anyhow::anyhow!("usage: midori-ssdv-decode <file>"))?,
        out_dir,
        offset_hz,
        bandwidth_hz,
        mode,
        audio_if,
        crc_inits,
        frame_header_len,
    })
}

fn print_help() {
    eprintln!(
        "midori-ssdv-decode <input> [options]\n\
         \n\
         Inputs: RIFF/RF64 I/Q WAV, 223-byte CCSDS frames, 218-byte DSLWP\n\
         packets, 256-byte classic SSDV packets, or a base64 dump of any.\n\
         \n\
         Options:\n\
           --out-dir DIR       output directory (default: captures)\n\
           --offset HZ         channel offset inside an I/Q recording\n\
           --offset-khz KHZ    same, in kHz\n\
           --bandwidth HZ      channel filter bandwidth (default: 30000)\n\
           --iq                force WAV/IQ input\n\
           --audio-if          force 48 kHz real 12 kHz-IF audio input\n\
           --frames            force 223-byte CCSDS frames\n\
           --packets           force raw SSDV packets (218 or 256 bytes)\n\
           --crc-init VALUE    accepted CRC32 initial value, repeatable or\n\
                               comma-separated (hex or decimal); default 0xFFFFFFFF\n\
           --frame-header-len N  bytes before the 218-byte packet in a 223-byte\n\
                               frame (default: 5)\n"
    );
}

fn decode_wav(args: &Args) -> Result<usize> {
    let mut reader = WavIqReader::open(&args.input)?;
    let spec = reader.spec();
    eprintln!(
        "input: {} | {} ch | {:.3} MSPS | {:.1} s | offset {:+.0} Hz",
        args.input.display(),
        spec.channels,
        spec.sample_rate_hz / 1e6,
        spec.frames as f64 / spec.sample_rate_hz,
        args.offset_hz
    );

    let mut demod = BpskDemod::new(BpskDemodConfig {
        input_rate_hz: spec.sample_rate_hz,
        channel_rate_hz: 48_000.0,
        output_rate_hz: 48_000.0,
        offset_hz: args.offset_hz,
        channel_bandwidth_hz: args.bandwidth_hz,
    });
    let mut receiver = Bpsk9600Receiver::with_config(
        Some(args.out_dir.clone()),
        parse_crc_inits(&args.crc_inits),
        args.frame_header_len,
    )?;
    let mut images = 0usize;
    let mut iq = Vec::new();
    let mut samples_read = 0u64;

    loop {
        let block = reader.read_iq(32_768)?;
        if block.is_empty() {
            break;
        }
        samples_read += block.len() as u64;
        iq.clear();
        demod.process(&block, &mut iq);
        images += report(receiver.process_iq(&iq))?;
    }
    images += report(receiver.flush())?;

    eprintln!(
        "processed {:.1} s of IQ ({} samples), {} CCSDS frames, {} RS bytes corrected",
        samples_read as f64 / spec.sample_rate_hz,
        samples_read,
        receiver.frame_count(),
        receiver.rs_corrected_bytes()
    );
    Ok(images)
}

fn decode_wav_audio_if(args: &Args) -> Result<usize> {
    let mut reader = WavIqReader::open(&args.input)?;
    let spec = reader.spec();
    if spec.channels != 1 {
        bail!(
            "--audio-if needs a mono WAV, got {} channels",
            spec.channels
        );
    }
    if (spec.sample_rate_hz - 48_000.0).abs() > 1.0 {
        bail!("--audio-if needs 48 kHz, got {:.0} Hz", spec.sample_rate_hz);
    }
    eprintln!(
        "input: {} | mono 48 kHz real IF | {:.1} s",
        args.input.display(),
        spec.frames as f64 / spec.sample_rate_hz
    );

    let mut receiver = Bpsk9600AudioReceiver::with_config(
        Some(args.out_dir.clone()),
        parse_crc_inits(&args.crc_inits),
        args.frame_header_len,
    )?;
    let mut images = 0usize;
    loop {
        let block = reader.read_iq(32_768)?;
        if block.is_empty() {
            break;
        }
        images += report(receiver.process_audio(&block))?;
    }
    images += report(receiver.flush())?;
    eprintln!(
        "{} CCSDS frames, {} RS bytes corrected",
        receiver.frame_count(),
        receiver.rs_corrected_bytes()
    );
    Ok(images)
}

fn decode_raw(args: &Args, mode: InputMode) -> Result<usize> {
    let mut bytes =
        std::fs::read(&args.input).with_context(|| format!("reading {}", args.input.display()))?;
    if args
        .input
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("b64"))
        || looks_like_base64(&bytes)
    {
        let text: String = bytes
            .iter()
            .filter(|byte| !byte.is_ascii_whitespace())
            .map(|byte| *byte as char)
            .collect();
        bytes = base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .context("decoding base64 input")?;
    }
    eprintln!("input: {} | {} bytes", args.input.display(), bytes.len());

    let mut session = SsdvSession::with_config(
        Some(args.out_dir.clone()),
        parse_crc_inits(&args.crc_inits),
        args.frame_header_len,
    );
    let mode = match mode {
        InputMode::Auto => {
            if bytes.len().is_multiple_of(223) {
                InputMode::Frames
            } else if bytes.len().is_multiple_of(218) || bytes.len().is_multiple_of(256) {
                InputMode::Packets
            } else {
                bail!(
                    "raw input is {} bytes; not a multiple of 223, 218, or 256",
                    bytes.len()
                );
            }
        }
        mode => mode,
    };

    let events = match mode {
        InputMode::Frames => {
            let mut events = Vec::new();
            for frame in bytes.chunks_exact(223) {
                events.extend(session.process_frame(frame));
            }
            events
        }
        InputMode::Packets => {
            let mut events = Vec::new();
            if bytes.len().is_multiple_of(218) {
                for packet in bytes.chunks_exact(218) {
                    events.extend(session.process_dslwp_packet(packet));
                }
            } else if bytes.len().is_multiple_of(256) {
                for packet in bytes.chunks_exact(256) {
                    events.extend(session.process_standard_packet(packet));
                }
            } else {
                bail!(
                    "packet input is {} bytes; not a multiple of 218 or 256",
                    bytes.len()
                );
            }
            events
        }
        InputMode::Auto => session.process_bytes(&bytes)?,
        InputMode::Iq => unreachable!(),
    };
    let images = report(events)?;
    eprintln!(
        "{} packet rejects, {} non-SSDV frames",
        session.crc_errors(),
        session.ignored_frames()
    );
    Ok(images)
}

fn report(events: Vec<SessionEvent>) -> Result<usize> {
    let mut images = 0usize;
    for event in events {
        match event {
            SessionEvent::Status { level, message } => {
                let prefix = match level {
                    StatusLevel::Info => "info",
                    StatusLevel::Warning => "warn",
                    StatusLevel::Error => "error",
                };
                eprintln!("[{prefix}] {message}");
            }
            SessionEvent::Progress {
                received,
                total_hint,
                missing,
                fraction,
                ..
            } => {
                if total_hint == 0 {
                    eprintln!(
                        "  {received} packet(s), {missing} missing, waiting for more ({:.0}%)",
                        fraction * 100.0
                    );
                } else {
                    eprintln!(
                        "  packets {received}/{total_hint} estimated, {missing} missing ({:.0}%)",
                        fraction * 100.0
                    );
                }
            }
            SessionEvent::Image(image) => {
                images += 1;
                let path = image
                    .png_path
                    .as_ref()
                    .or(image.jpeg_path.as_ref())
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "not saved".to_string());
                eprintln!(
                    "image ID {} {}x{} q{}: {} packets, {} missing, {} -> {}",
                    image.image_id,
                    image.width,
                    image.height,
                    image.quality,
                    image.received_packets,
                    image.missing_packets,
                    if image.complete {
                        "complete"
                    } else {
                        "preview"
                    },
                    path
                );
            }
        }
    }
    Ok(images)
}

fn looks_like_wav(path: &Path) -> Result<bool> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    use std::io::Read;
    let mut header = [0u8; 4];
    if file.read(&mut header)? != 4 {
        return Ok(false);
    }
    Ok(&header == b"RIFF" || &header == b"RF64")
}

fn looks_like_base64(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > 64 * 1024 * 1024 {
        return false;
    }
    bytes.iter().all(|byte| {
        byte.is_ascii_whitespace()
            || byte.is_ascii_alphanumeric()
            || matches!(byte, b'+' | b'/' | b'=')
    })
}
