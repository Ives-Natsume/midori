use std::sync::Arc;
use std::time::{Duration, Instant};

use midori_core::{BlockFlags, Frame, FrameMeta, StreamData};
use midori_plugin::{
    DisplayUpdate, ImageFormat, InputSpec, Manifest, OutputKind, PluginEvent, PluginHost,
    SidecarPlugin,
};
use rasterwave::{EncodeOptions, Rgb, RgbImage, SstvEncoder, SstvMode};

const SAMPLE_RATE: u32 = 44_100;

type Decoded = (ImageFormat, u32, u32, Vec<u8>);

fn test_image(width: u32, height: u32) -> RgbImage {
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.push(Rgb::new(
                (x * 255 / width.max(1)) as u8,
                (y * 255 / height.max(1)) as u8,
                ((x + y) * 255 / (width + height).max(1)) as u8,
            ));
        }
    }
    RgbImage::new(width, height, pixels).unwrap()
}

fn encode_pd120() -> Vec<f32> {
    let mode = SstvMode::Pd120;
    let spec = mode.spec();
    let image = test_image(spec.width, spec.height);
    let mut encoder = SstvEncoder::new(image, mode, SAMPLE_RATE, EncodeOptions::default()).unwrap();
    let mut audio = Vec::new();
    let mut buffer = vec![0.0f32; 4096];
    while !encoder.is_finished() {
        let written = encoder.read_samples(&mut buffer);
        audio.extend_from_slice(&buffer[..written]);
    }
    audio
}

fn temp_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("midori-sstv-test-{nanos}"))
}

/// Drain queued events. Returns true once an image arrived.
fn drain(
    plugin: &mut SidecarPlugin,
    image: &mut Option<Decoded>,
    saved_path: &mut Option<String>,
) -> bool {
    loop {
        let event = match plugin.try_recv() {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(error) => panic!("plugin event: {error}"),
        };
        match event {
            PluginEvent::Display {
                update:
                    DisplayUpdate::Image {
                        format,
                        width,
                        height,
                        bytes,
                        ..
                    },
            } => *image = Some((format, width, height, bytes)),
            PluginEvent::Status { message, .. } => {
                if let Some(path) = message.rsplit("->").next()
                    && message.contains("->")
                {
                    *saved_path = Some(path.trim().to_string());
                }
            }
            PluginEvent::Error { message, .. } => panic!("plugin error: {message}"),
            _ => {}
        }
    }
    image.is_some()
}

#[test]
#[ignore = "full SSTV decode is slow in debug; run with --release -- --ignored"]
fn sstv_sidecar_decodes_pd120() {
    let output_dir = temp_dir();
    let manifest = Manifest {
        name: "sstv".to_string(),
        version: "0.1.0".to_string(),
        protocol: midori_plugin::PROTOCOL_VERSION,
        command: vec![env!("CARGO_BIN_EXE_midori-plugin-sstv").to_string()],
        input: InputSpec::Audio {
            sample_rate_hz: SAMPLE_RATE as f64,
            channels: 1,
        },
        outputs: vec![OutputKind::Display],
        params: vec![],
        demod_modes: Vec::new(),
    };

    let mut plugin = SidecarPlugin::new(manifest).expect("manifest");
    plugin.start().expect("start");
    plugin
        .configure(&serde_json::json!({
            "output_dir": output_dir.display().to_string(),
        }))
        .expect("configure");

    let audio = encode_pd120();
    let chunk = 4_410usize;
    let mut image = None;
    let mut saved_path = None;

    for (seq, block) in audio.chunks(chunk).enumerate() {
        let meta = FrameMeta {
            seq: seq as u64,
            start_sample: (seq * chunk) as u64,
            timestamp_ns: Some(seq as u64 * 1_000_000_000),
            sample_rate_hz: SAMPLE_RATE as f64,
            center_freq_hz: 145_800_000.0,
            nominal_freq_hz: 145_800_000.0,
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
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("send audio: {error}"),
            }
        }
        // Drain while streaming so progress events cannot fill the host queue
        // and cause the image event to be dropped.
        drain(&mut plugin, &mut image, &mut saved_path);
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    while image.is_none() && Instant::now() < deadline {
        drain(&mut plugin, &mut image, &mut saved_path);
        std::thread::sleep(Duration::from_millis(20));
    }

    plugin.stop().expect("stop");

    let (format, width, height, bytes) = image.expect("decoded image");
    assert_eq!(format, ImageFormat::Png);
    assert_eq!((width, height), (640, 496));
    assert!(bytes.starts_with(b"\x89PNG"));

    let saved = saved_path.expect("saved path status");
    assert!(
        std::path::Path::new(&saved).exists(),
        "expected saved PNG at {saved}"
    );
    let _ = std::fs::remove_dir_all(output_dir);
}
