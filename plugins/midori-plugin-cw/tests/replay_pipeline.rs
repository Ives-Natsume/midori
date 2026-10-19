use std::f64::consts::TAU;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use midori_core::{DemodMode, Session, SourceConfig, VfoConfig};
use midori_engine::{LiveSession, VfoChannel, plugin_sink};
use midori_plugin::{
    DisplayUpdate, InputSpec, Manifest, OutputKind, PluginEvent, PluginHost, SidecarPlugin,
};
use midori_source_file::WavIqSource;

const SAMPLE_RATE: f64 = 240_000.0;
const CARRIER_OFFSET_HZ: f64 = -13_500.0;
const TONE_HZ: f64 = 700.0;

fn cw_marks(character: char) -> &'static str {
    match character {
        'C' => "-.-.",
        'Q' => "--.-",
        _ => panic!("no pattern"),
    }
}

fn cw_iq() -> Vec<(f32, f32)> {
    let dot_s = 0.060;
    let mut keyed = vec![false; (0.5 * SAMPLE_RATE) as usize];
    for (word_index, word) in ["CQ"].iter().enumerate() {
        if word_index > 0 {
            keyed.extend(std::iter::repeat_n(
                false,
                (7.0 * dot_s * SAMPLE_RATE) as usize,
            ));
        }
        for (char_index, character) in word.chars().enumerate() {
            if char_index > 0 {
                keyed.extend(std::iter::repeat_n(
                    false,
                    (3.0 * dot_s * SAMPLE_RATE) as usize,
                ));
            }
            let marks = cw_marks(character);
            for (mark_index, mark) in marks.chars().enumerate() {
                if mark_index > 0 {
                    keyed.extend(std::iter::repeat_n(false, (dot_s * SAMPLE_RATE) as usize));
                }
                let units = if mark == '.' { 1.0 } else { 3.0 };
                keyed.extend(std::iter::repeat_n(
                    true,
                    (units * dot_s * SAMPLE_RATE) as usize,
                ));
            }
        }
    }
    keyed.extend(std::iter::repeat_n(false, (0.5 * SAMPLE_RATE) as usize));

    keyed
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            if !key {
                return (0.0, 0.0);
            }
            let phase = TAU * CARRIER_OFFSET_HZ * index as f64 / SAMPLE_RATE;
            ((phase.cos() * 0.5) as f32, (phase.sin() * 0.5) as f32)
        })
        .collect()
}

fn write_iq_wav() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("midori-cw-replay-{nanos}.wav"));
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).unwrap();
    for (i, q) in cw_iq() {
        writer.write_sample((i * 32767.0) as i16).unwrap();
        writer.write_sample((q * 32767.0) as i16).unwrap();
    }
    writer.finalize().unwrap();
    path
}

#[test]
fn file_source_vfo_offset_feeds_the_cw_sidecar() {
    let path = write_iq_wav();

    let mut source = WavIqSource::open(&path, false).unwrap();
    source.set_realtime(false);
    let source_config = SourceConfig {
        center_freq_hz: 437_393_500.0,
        buffer_samples: 4_096,
        ..SourceConfig::default()
    };

    let vfo = VfoConfig {
        id: 0,
        offset_hz: 0.0,
        bandwidth_hz: 500.0,
        demod: DemodMode::Cw,
        audio_rate_hz: 48_000.0,
        channel_rate_hz: 48_000.0,
        audio_bandwidth_hz: 2_000.0,
        deemphasis_us: 0.0,
        bfo_hz: TONE_HZ,
    };

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
    let mut plugin = SidecarPlugin::new(manifest).unwrap();
    plugin.start().unwrap();
    plugin
        .configure(&serde_json::json!({"tone_hz": TONE_HZ}))
        .unwrap();
    let (sink, handle) = plugin_sink(Box::new(plugin));

    let mut session = LiveSession::new(
        source_config,
        Box::new(source),
        vec![Box::new(VfoChannel::new(vfo))],
        vec![Box::new(sink)],
    );
    session.start().unwrap();
    // This is exactly what clicking the CW carrier in the GUI does.
    session.set_vfo_offset(CARRIER_OFFSET_HZ);
    while !session.source_finished() {
        session.step().unwrap();
    }
    session.stop().unwrap();

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
        if text.contains("CQ") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = handle.stop();
    let _ = std::fs::remove_file(path);
    assert!(text.contains("CQ"), "decoded text was {text:?}");
}
