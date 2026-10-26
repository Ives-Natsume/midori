//! End-to-end sidecar test: 223-byte CCSDS frames in, PNG image event out.

use std::io::{BufReader, BufWriter};
use std::process::{Command, Stdio};

use base64::Engine;
use midori_plugin::wire::{WireMessage, read_message, write_bytes, write_json};
use midori_plugin::{DisplayUpdate, PluginEvent};
use serde_json::json;

const FIXTURE: &str = include_str!("fixtures/asrtu1_ssdv_frames.b64");

#[test]
fn sidecar_decodes_raw_ccsds_frames() {
    let frames = base64::engine::general_purpose::STANDARD
        .decode(FIXTURE.trim().as_bytes())
        .expect("fixture base64");

    let mut child = Command::new(env!("CARGO_BIN_EXE_midori-plugin-ssdv"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sidecar");
    let mut writer = BufWriter::new(child.stdin.take().expect("stdin"));
    let mut reader = BufReader::new(child.stdout.take().expect("stdout"));

    eprintln!("test: waiting for hello");
    match read_message(&mut reader).expect("hello") {
        WireMessage::Json(value) => assert_eq!(value["type"], "hello"),
        other => panic!("expected hello, got {other:?}"),
    }

    eprintln!("test: sending configure");
    write_json(
        &mut writer,
        &json!({
            "type": "configure",
            "params": {
                "input": "bytes",
                "output_dir": "",
                "preview_interval_ms": 50,
                "crc_inits": ["0x4EE4FDE1"]
            }
        }),
    )
    .unwrap();
    eprintln!("test: waiting for ready");
    match read_message(&mut reader).expect("ready") {
        WireMessage::Json(value) => assert_eq!(value["type"], "ready"),
        other => panic!("expected ready, got {other:?}"),
    }

    eprintln!("test: sending {} frames", frames.len() / 223);
    for (seq, frame) in frames.chunks_exact(223).enumerate() {
        write_bytes(&mut writer, seq as u64, None, frame).unwrap();
    }

    eprintln!("test: reading events");
    let mut image = None;
    for _ in 0..128 {
        match read_message(&mut reader).expect("event") {
            WireMessage::Json(value) => {
                let event: PluginEvent = serde_json::from_value(value).expect("plugin event");
                if let PluginEvent::Display {
                    update:
                        DisplayUpdate::Image {
                            title,
                            width,
                            height,
                            bytes,
                            ..
                        },
                } = event
                    && !title.contains("preview")
                {
                    image = Some((width, height, bytes));
                    break;
                }
            }
            other => panic!("unexpected data frame {other:?}"),
        }
    }

    eprintln!("test: got image");
    let (width, height, png) = image.expect("complete image event");
    assert_eq!((width, height), (320, 240));
    let info = png::Decoder::new(std::io::Cursor::new(&png))
        .read_info()
        .expect("PNG");
    assert_eq!((info.info().width, info.info().height), (320, 240));

    write_json(&mut writer, &json!({"type": "shutdown"})).unwrap();
    drop(writer);
    drop(reader);
    let status = child.wait().expect("wait");
    assert!(status.success());
}
