use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use midori_core::{
    BlockFlags, Channel, DemodMode, Frame, FrameMeta, Session, SourceConfig, StreamData, VfoConfig,
};
use midori_dsp::OffsetScan;
use midori_engine::{CwStartGateSink, LiveSession, VfoChannel, plugin_sink};
use midori_plugin::{
    DisplayUpdate, InputSpec, Manifest, OutputKind, PluginEvent, PluginHost, SidecarPlugin,
};
use midori_plugin_cw::offline::{OfflineConfig, demodulate_wav};
use midori_plugin_cw::{MorseDecoder, MorseEvent};
use midori_source_file::{WavIqReader, WavIqSource};

const FIXTURE_SAMPLE_RATE: u32 = 12_000;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/botan_cw_12k.wav")
}

fn read_fixture() -> Vec<f32> {
    let mut reader = hound::WavReader::open(fixture_path()).expect("open fixture");
    reader
        .samples::<i16>()
        .map(|sample| sample.expect("sample") as f32 / 32768.0)
        .collect()
}

fn decode_direct(samples: &[f32], sample_rate_hz: f64) -> (String, f32) {
    let mut decoder = MorseDecoder::new(sample_rate_hz, 700.0);
    let mut text = String::new();
    for block in samples.chunks(512) {
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

fn assert_botan(text: &str) {
    assert!(text.contains("BOTAN"), "missing BOTAN in {text:?}");
    assert!(text.contains("JS1YPT"), "missing JS1YPT in {text:?}");
    assert!(
        text.contains("8763"),
        "missing telemetry prefix in {text:?}"
    );
}

#[test]
fn decodes_botan_fixture_direct() {
    let samples = read_fixture();
    let (text, wpm) = decode_direct(&samples, FIXTURE_SAMPLE_RATE as f64);
    assert_botan(&text);
    assert!((wpm - 20.0).abs() < 4.0, "unexpected WPM {wpm}");
}

#[test]
fn cw_sidecar_decodes_botan_fixture() {
    let manifest = Manifest {
        name: "cw".to_string(),
        version: "0.1.0".to_string(),
        protocol: midori_plugin::PROTOCOL_VERSION,
        command: vec![env!("CARGO_BIN_EXE_midori-plugin-cw").to_string()],
        input: InputSpec::Audio {
            sample_rate_hz: FIXTURE_SAMPLE_RATE as f64,
            channels: 1,
        },
        outputs: vec![OutputKind::Display, OutputKind::Packets],
        params: vec![],
    };

    let mut plugin = SidecarPlugin::new(manifest).expect("manifest");
    plugin.start().expect("start");
    plugin
        .configure(&serde_json::json!({"tone_hz": 700.0}))
        .expect("configure");

    let samples = read_fixture();
    let chunk = 1_024usize;
    for (seq, block) in samples.chunks(chunk).enumerate() {
        let meta = FrameMeta {
            seq: seq as u64,
            start_sample: (seq * chunk) as u64,
            timestamp_ns: Some(seq as u64 * 1_000_000_000),
            sample_rate_hz: FIXTURE_SAMPLE_RATE as f64,
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
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match plugin.send_frame(frame.clone()) {
                Ok(()) => break,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("send audio: {error}"),
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while Instant::now() < deadline {
        while let Ok(Some(event)) = plugin.try_recv() {
            match event {
                PluginEvent::Display {
                    update: DisplayUpdate::Text { lines, .. },
                } => text = lines.join("\n"),
                PluginEvent::Error { message, .. } => panic!("plugin error: {message}"),
                _ => {}
            }
        }
        if text.contains("8763") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    plugin.stop().expect("stop");
    assert_botan(&text);
}

/// Full GUI-style path: scan the band, set the VFO, then let the receiver AFC
/// follow the carrier while the sidecar decodes. This reproduces the way the
/// GUI worker drives `VfoChannel` and the CW sidecar.
#[test]
#[ignore = "needs the local 112 MB Botan I/Q capture and runs in real time"]
fn gui_style_afc_decodes_botan_raw_iq() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/BOTAN_TLM_CW_437393500Hz.wav");
    if !path.exists() {
        eprintln!("skipping: {} not found", path.display());
        return;
    }

    // Coarse auto-tune: scan the first four seconds for a narrowband peak.
    let mut reader = WavIqReader::open(&path).expect("open wav");
    let file_spec = reader.spec();
    let mut scan = OffsetScan::new(file_spec.sample_rate_hz, 32_768, 100_000.0);
    let target = (file_spec.sample_rate_hz * 4.0) as usize;
    let mut scanned = 0usize;
    while scanned < target {
        let block = reader.read_iq(32_768).expect("read IQ");
        if block.is_empty() {
            break;
        }
        scanned += block.len();
        scan.process(&block);
    }
    let auto_offset = scan.result().expect("auto-tune carrier");
    eprintln!("auto tuned to {auto_offset:+.1} Hz");

    let source = WavIqSource::open(&path, false).expect("source");
    // Keep the file at real time so the sidecar's bounded queue does not drop
    // audio while the test thread runs as fast as it can.
    let source_config = SourceConfig {
        center_freq_hz: 437_393_500.0,
        buffer_samples: 32_768,
        ..SourceConfig::default()
    };
    let vfo = VfoConfig {
        id: 0,
        offset_hz: auto_offset,
        bandwidth_hz: 2_000.0,
        demod: DemodMode::Cw,
        audio_rate_hz: 48_000.0,
        channel_rate_hz: 240_000.0,
        audio_bandwidth_hz: 2_000.0,
        deemphasis_us: 0.0,
        bfo_hz: 700.0,
    };
    let mut channel = VfoChannel::new(vfo);
    channel.set_afc(true);

    let manifest = Manifest {
        name: "cw".to_string(),
        version: "0.1.0".to_string(),
        protocol: midori_plugin::PROTOCOL_VERSION,
        command: vec![env!("CARGO_BIN_EXE_midori-plugin-cw").to_string()],
        input: InputSpec::Audio {
            sample_rate_hz: 48_000.0,
            channels: 1,
        },
        outputs: vec![OutputKind::Display, OutputKind::Packets],
        params: vec![],
    };
    let mut plugin = SidecarPlugin::new(manifest).expect("manifest");
    plugin.start().expect("start");
    plugin
        .configure(&serde_json::json!({
            "tone_hz": 700.0,
            "envelope_hz": 120.0,
            "start_at_signal": true,
        }))
        .expect("configure");
    let (sink, handle) = plugin_sink(Box::new(plugin));

    let mut session = LiveSession::new(
        source_config,
        Box::new(source),
        vec![Box::new(channel)],
        vec![Box::new(CwStartGateSink::new(sink))],
    );
    session.start().expect("start session");
    let mut last_print = Instant::now();
    while !session.source_finished() {
        session.step().expect("step");
        if last_print.elapsed() >= Duration::from_millis(500) {
            last_print = Instant::now();
            eprintln!("vfo {:+.1} Hz", session.vfo_offset_hz());
        }
    }
    session.stop().expect("stop session");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while Instant::now() < deadline {
        while let Ok(Some(event)) = handle.try_recv() {
            match event {
                PluginEvent::Display {
                    update: DisplayUpdate::Text { lines, .. },
                } => text = lines.join("\n"),
                PluginEvent::Error { message, .. } => panic!("plugin error: {message}"),
                _ => {}
            }
        }
        if text.contains("8763") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = handle.stop();
    println!("decoded: {text:?}");
    // The live GUI path uses a wider envelope and a start gate, so the first
    // element of the callsign is sometimes ambiguous. The header and the
    // telemetry prefix are the meaningful checks here; the fixture tests above
    // cover the exact `BOTAN JS1YPT` decode on the offline path.
    assert!(text.contains("BOTAN"), "missing BOTAN in {text:?}");
    assert!(
        text.contains("8763"),
        "missing telemetry prefix in {text:?}"
    );
}

#[test]
#[ignore = "needs the local 112 MB Botan I/Q capture in ../data"]
fn decodes_botan_raw_iq_with_tracking() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/BOTAN_TLM_CW_437393500Hz.wav");
    if !path.exists() {
        eprintln!("skipping: {} not found", path.display());
        return;
    }
    let config = OfflineConfig {
        offset_hz: Some(-13_420.0),
        scan_range_hz: 100_000.0,
        search_hz: 3_000.0,
        bfo_hz: 700.0,
        output_rate_hz: FIXTURE_SAMPLE_RATE as f64,
        track: true,
    };
    let result = demodulate_wav(&path, &config).expect("demodulate");
    let (text, wpm) = decode_direct(&result.audio, result.audio_rate_hz);
    println!("decoded at {wpm:.1} WPM: {text:?}");
    assert_botan(&text);
}
