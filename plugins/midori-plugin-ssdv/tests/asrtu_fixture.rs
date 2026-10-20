//! ASRTU-1 SSDV regression fixture.
//!
//! The fixture is a base64 dump of 223-byte CCSDS frames captured from an
//! ASRTU-1 (AO-123) pass and published in BG7ZDQ/ASRTU_Series_Receiver
//! (`tests/fixtures/asrtu1_ssdv_capture.b64`, MIT). It begins at SSDV packet
//! 34, ends at packet 59 with the end-of-image flag, and reconstructs a
//! 320x240 image with image ID 43.

use base64::Engine;
use midori_plugin_ssdv::{CCSDS_FRAME_LEN, SessionEvent, SsdvSession};

const FIXTURE: &str = include_str!("fixtures/asrtu1_ssdv_frames.b64");

#[test]
fn decodes_asrtu1_ssdv_fixture() {
    let frames = base64::engine::general_purpose::STANDARD
        .decode(FIXTURE.trim().as_bytes())
        .expect("fixture base64");
    assert_eq!(frames.len() % CCSDS_FRAME_LEN, 0);

    let mut session = SsdvSession::with_crc_inits(None, vec![0x4EE4_FDE1]);
    let mut last_image = None;
    let mut previews = 0;
    for frame in frames.chunks_exact(CCSDS_FRAME_LEN) {
        for event in session.process_frame(frame) {
            if let SessionEvent::Image(image) = event {
                if !image.complete {
                    previews += 1;
                }
                last_image = Some(image);
            }
        }
    }

    let image = last_image.expect("fixture should produce an image");
    assert!(image.complete, "EOI packet 59 should complete the image");
    assert_eq!(image.image_id, 43);
    assert_eq!((image.width, image.height), (320, 240));
    assert_eq!(image.quality, 3);
    assert_eq!(image.received_packets, 25);
    assert_eq!(image.missing_packets, 35);
    assert!(previews >= 1);
    assert!(!image.jpeg.is_empty());
    assert!(
        image.jpeg.len() < 10_000,
        "unexpected JPEG padding: {} bytes",
        image.jpeg.len()
    );
    assert!(!image.png.is_empty());

    let png = png::Decoder::new(std::io::Cursor::new(&image.png))
        .read_info()
        .expect("valid PNG");
    assert_eq!(png.info().width, 320);
    assert_eq!(png.info().height, 240);
    // The fixture ends with a non-SSDV frame; the CRC check rejects it.
    assert_eq!(session.crc_errors(), 1);
    assert_eq!(session.ignored_frames(), 0);
}

#[test]
fn duplicate_frames_are_ignored() {
    let frames = base64::engine::general_purpose::STANDARD
        .decode(FIXTURE.trim().as_bytes())
        .expect("fixture base64");
    let mut session = SsdvSession::with_crc_inits(None, vec![0x4EE4_FDE1]);
    for frame in frames.chunks_exact(CCSDS_FRAME_LEN) {
        session.process_frame(frame);
    }
    let mut images = 0;
    for frame in frames.chunks_exact(CCSDS_FRAME_LEN) {
        for event in session.process_frame(frame) {
            if matches!(event, SessionEvent::Image(_)) {
                images += 1;
            }
        }
    }
    assert_eq!(images, 0, "an identical replay must not rebuild the image");
}

#[test]
fn progress_starts_low_and_is_monotonic() {
    let frames = base64::engine::general_purpose::STANDARD
        .decode(FIXTURE.trim().as_bytes())
        .expect("fixture base64");
    let mut session = SsdvSession::with_crc_inits(None, vec![0x4EE4_FDE1]);
    session.set_preview_interval(std::time::Duration::ZERO);

    let mut fractions = Vec::new();
    for frame in frames.chunks_exact(CCSDS_FRAME_LEN) {
        for event in session.process_frame(frame) {
            if let SessionEvent::Progress { fraction, .. } = event {
                fractions.push(fraction);
            }
        }
    }

    assert!(fractions.len() >= 2, "expected several progress updates");
    assert!(
        fractions[0] < 0.2,
        "first progress update jumped to {:.0}%",
        fractions[0] * 100.0
    );
    for pair in fractions.windows(2) {
        assert!(
            pair[1] + f32::EPSILON >= pair[0],
            "progress went backwards: {:.1}% -> {:.1}%",
            pair[0] * 100.0,
            pair[1] * 100.0
        );
    }
    assert!(
        fractions.last().copied().unwrap_or(0.0) < 0.6,
        "partial image should not report near-complete"
    );
}
