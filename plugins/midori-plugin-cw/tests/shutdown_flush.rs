//! Events flushed in response to shutdown must be returned by `stop()`.

use std::f64::consts::TAU;
use std::sync::Arc;
use std::time::Duration;

use midori_core::{BlockFlags, Frame, FrameMeta, StreamData};
use midori_plugin::{
    DisplayUpdate, InputSpec, Manifest, OutputKind, PluginEvent, PluginHost, SidecarPlugin,
};

fn tone(sample_rate: f64, tone_hz: f64, ms: f64, amplitude: f32) -> Vec<f32> {
    let count = (sample_rate * ms / 1000.0) as usize;
    (0..count)
        .map(|i| {
            let phase = TAU * tone_hz * i as f64 / sample_rate;
            amplitude * phase.sin() as f32
        })
        .collect()
}

fn silence(sample_rate: f64, ms: f64) -> Vec<f32> {
    vec![0.0; (sample_rate * ms / 1000.0) as usize]
}

fn push_marks(audio: &mut Vec<f32>, sample_rate: f64, tone_hz: f64, marks: &str) {
    let dot_ms = 60.0;
    let mut first = true;
    for symbol in marks.chars() {
        if !first {
            audio.extend(silence(sample_rate, dot_ms));
        }
        first = false;
        match symbol {
            '.' => audio.extend(tone(sample_rate, tone_hz, dot_ms, 0.5)),
            '-' => audio.extend(tone(sample_rate, tone_hz, dot_ms * 3.0, 0.5)),
            _ => {}
        }
    }
}

#[test]
fn shutdown_flush_delivers_final_text() {
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
    // `start_at_signal` skips the pre-signal gate. The audio ends on a mark
    // with no inter-character gap, so the character can only be emitted by the
    // decoder's shutdown flush.
    plugin
        .configure(&serde_json::json!({
            "tone_hz": 700.0,
            "start_at_signal": true,
        }))
        .expect("configure");

    let sample_rate = 48_000.0;
    let mut audio = Vec::new();
    push_marks(&mut audio, sample_rate, 700.0, "-.-.");

    for (seq, block) in audio.chunks(1024).enumerate() {
        let meta = FrameMeta {
            seq: seq as u64,
            start_sample: (seq * 1024) as u64,
            timestamp_ns: Some(seq as u64 * 1_000_000_000),
            sample_rate_hz: sample_rate,
            center_freq_hz: 144_000_000.0,
            nominal_freq_hz: 144_000_000.0,
            bandwidth_hz: 3_000.0,
            vfo_id: Some(0),
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
            flags: BlockFlags::default(),
        };
        let samples: Arc<[f32]> = Arc::from(block.to_vec().into_boxed_slice());
        plugin
            .send_frame(Frame::new(meta, StreamData::Audio(samples)))
            .expect("send audio");
        std::thread::sleep(Duration::from_millis(2));
    }

    // Nothing should have been decoded yet: there is no character-length gap.
    let mut text = String::new();
    while let Ok(Some(event)) = plugin.try_recv() {
        if let PluginEvent::Display {
            update: DisplayUpdate::Text { lines, .. },
        } = event
        {
            text.push_str(&lines.join("\n"));
        }
    }
    assert!(
        !text.contains('C'),
        "character decoded before shutdown: {text:?}"
    );

    let final_events = plugin.stop().expect("stop");
    for event in final_events {
        if let PluginEvent::Display {
            update: DisplayUpdate::Text { lines, .. },
        } = event
        {
            text.push_str(&lines.join("\n"));
        }
    }

    assert!(
        text.contains('C'),
        "final text was not flushed by stop(): {text:?}"
    );
}
