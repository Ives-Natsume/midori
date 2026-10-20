//! Real-capture regressions. These are `#[ignore]` because the recordings are
//! large and intentionally not committed.
//!
//! ```text
//! cargo test -p midori-plugin-ssdv --release --test asrtu_live -- --ignored
//! ```
//!
//! Required local files:
//! - `data/ASRTU-1_TLM&SSDV_BPSK_435460378Hz.wav` (zero-IF I/Q)
//! - `data/BY-04_SSDV_CZ-6C.wav` (mono 48 kHz, 12 kHz real IF)

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use midori_dsp::{BpskDemod, BpskDemodConfig};
use midori_plugin_ssdv::{Bpsk9600AudioReceiver, Bpsk9600Receiver, SessionEvent};
use midori_source_file::WavIqReader;

fn data_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("data")
        .join(name)
}

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("midori-ssdv-{name}-{nanos}"))
}

#[test]
#[ignore = "needs the local ASRTU-1 I/Q capture"]
fn decodes_asrtu1_iq_capture() {
    let path = data_file("ASRTU-1_TLM&SSDV_BPSK_435460378Hz.wav");
    assert!(path.exists(), "missing {}", path.display());
    let out_dir = temp_dir("asrtu1");
    let mut reader = WavIqReader::open(&path).unwrap();
    let spec = reader.spec();
    let mut demod = BpskDemod::new(BpskDemodConfig {
        input_rate_hz: spec.sample_rate_hz,
        channel_rate_hz: 48_000.0,
        output_rate_hz: 48_000.0,
        // 436.210 MHz image downlink, 435.460378 MHz recording centre.
        offset_hz: 750_000.0,
        channel_bandwidth_hz: 30_000.0,
    });
    let mut receiver =
        Bpsk9600Receiver::with_crc_inits(Some(out_dir.clone()), vec![0x4EE4_FDE1]).unwrap();
    let mut iq = Vec::new();
    let mut images = Vec::new();
    loop {
        let block = reader.read_iq(32_768).unwrap();
        if block.is_empty() {
            break;
        }
        iq.clear();
        demod.process(&block, &mut iq);
        collect(receiver.process_iq(&iq), &mut images);
    }
    collect(receiver.flush(), &mut images);

    let image = images
        .iter()
        .find(|image| image.complete && image.image_id == 43)
        .expect("ASRTU-1 image 43");
    assert_eq!((image.width, image.height), (320, 240));
    assert_eq!(image.received_packets, 25);
    assert_eq!(receiver.frame_count(), 26);
    let _ = std::fs::remove_dir_all(out_dir);
}

#[test]
#[ignore = "needs the local BY-04 12 kHz-IF capture"]
fn decodes_by04_if_audio_capture() {
    let path = data_file("BY-04_SSDV_CZ-6C.wav");
    assert!(path.exists(), "missing {}", path.display());
    let out_dir = temp_dir("by04");
    let mut reader = WavIqReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 1);
    let mut receiver = Bpsk9600AudioReceiver::with_crc_inits(
        Some(out_dir.clone()),
        vec![0x4EE4_FDE1, 0x6AAA_C1C5],
    )
    .unwrap();
    let mut images = Vec::new();
    loop {
        let block = reader.read_iq(32_768).unwrap();
        if block.is_empty() {
            break;
        }
        collect(receiver.process_audio(&block), &mut images);
    }
    collect(receiver.flush(), &mut images);

    let complete = images
        .iter()
        .filter(|image| image.complete && (image.width, image.height) == (640, 480))
        .count();
    assert!(
        complete >= 3,
        "expected several complete BY-04 images, got {complete}"
    );
    let _ = std::fs::remove_dir_all(out_dir);
}

fn collect(events: Vec<SessionEvent>, images: &mut Vec<midori_plugin_ssdv::SsdvImage>) {
    for event in events {
        if let SessionEvent::Image(image) = event {
            images.push(image);
        }
    }
}
