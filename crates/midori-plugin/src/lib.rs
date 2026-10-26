//! Plugin contracts for Midori decoder sidecars.
//!
//! Decoders run out of process so they can be written in any language, carry
//! their own license, and crash without taking the engine down. This crate
//! defines the language-neutral manifest and event types plus the host trait.
//! The wire format and supervisor implementation will live alongside it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use midori_core::{Frame, FrameMeta, StreamKind};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

pub mod sidecar;
pub mod wire;

pub use sidecar::SidecarPlugin;

/// What a plugin wants to receive from the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputSpec {
    Iq {
        sample_rate_hz: f64,
        /// Optional request for a specific centre frequency; the engine may
        /// tune a VFO to satisfy it.
        center_freq_hz: Option<f64>,
    },
    Audio {
        sample_rate_hz: f64,
        channels: u32,
    },
    SoftBits {
        symbol_rate_hz: f64,
    },
    Bytes {
        #[serde(default)]
        protocol: Option<String>,
    },
}

/// What a plugin can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Packets,
    Display,
    Audio,
    Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    Bool,
    Int,
    Float,
    String,
    Enum,
}

/// Parameter schema used by the UI to generate controls automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    pub kind: ParamKind,
    #[serde(default)]
    pub default: serde_json::Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub description: String,
}

/// A decoder/consumer plugin description.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub protocol: u32,
    /// Command used to spawn the sidecar.
    pub command: Vec<String>,
    pub input: InputSpec,
    #[serde(default)]
    pub outputs: Vec<OutputKind>,
    #[serde(default)]
    pub params: Vec<ParamSpec>,
}

impl Manifest {
    pub fn from_json(json: &str) -> Result<Self> {
        let manifest: Self = serde_json::from_str(json).context("parsing plugin manifest")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_json(&text)
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("plugin manifest has an empty name");
        }
        if self.command.is_empty() {
            bail!("plugin '{}' has no command", self.name);
        }
        if self.protocol != PROTOCOL_VERSION {
            bail!(
                "plugin '{}' speaks protocol {}, engine expects {}",
                self.name,
                self.protocol,
                PROTOCOL_VERSION
            );
        }
        match &self.input {
            InputSpec::Iq { sample_rate_hz, .. } if *sample_rate_hz <= 0.0 => {
                bail!("plugin '{}' has an invalid IQ sample rate", self.name)
            }
            InputSpec::Audio {
                sample_rate_hz,
                channels,
            } if *sample_rate_hz <= 0.0 || *channels == 0 => {
                bail!("plugin '{}' has an invalid audio input spec", self.name)
            }
            InputSpec::SoftBits { symbol_rate_hz } if *symbol_rate_hz <= 0.0 => {
                bail!("plugin '{}' has an invalid symbol rate", self.name)
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Png,
    Jpeg,
    Rgba8,
}

/// Generic UI payload. Plugins never link egui; the host renders these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayUpdate {
    Text {
        title: String,
        lines: Vec<String>,
    },
    Image {
        title: String,
        format: ImageFormat,
        /// Image width in pixels. Zero for formats that carry their own
        /// dimensions (PNG/JPEG).
        #[serde(default)]
        width: u32,
        /// Image height in pixels.
        #[serde(default)]
        height: u32,
        bytes: Vec<u8>,
    },
    Table {
        title: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Progress {
        title: String,
        fraction: f32,
    },
}

/// Events a sidecar can send back to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginEvent {
    Packet { meta: FrameMeta, data: Vec<u8> },
    Display { update: DisplayUpdate },
    Status { level: StatusLevel, message: String },
    Error { message: String, fatal: bool },
}

/// Host-side handle for one running plugin.
///
/// The concrete sidecar supervisor will implement this. Keeping it as a trait
/// lets tests use in-process mocks and lets the engine host several plugin
/// implementations (sidecar now, WASM later if it ever makes sense).
pub trait PluginHost: Send {
    fn manifest(&self) -> &Manifest;

    fn start(&mut self) -> Result<()>;

    fn configure(&mut self, params: &serde_json::Value) -> Result<()>;

    /// The single stream kind this plugin accepts. The engine uses it to fan
    /// out only matching frames instead of flooding a decoder with IQ or audio
    /// it does not understand.
    fn input_kind(&self) -> StreamKind;

    /// Send one typed frame. The host is responsible for serialising it
    /// according to the plugin's input specification and applying a drop
    /// policy if the sidecar cannot keep up.
    fn send_frame(&mut self, frame: Frame) -> Result<()>;

    /// Non-blocking poll for events produced by the plugin.
    fn try_recv(&mut self) -> Result<Option<PluginEvent>>;

    /// Stop the plugin and return any events it emitted while shutting down.
    ///
    /// Plugins may flush buffered results (final decoded text, partial images,
    /// ...) after the shutdown request, so callers must forward the returned
    /// events through the same path as `try_recv` events.
    fn stop(&mut self) -> Result<Vec<PluginEvent>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
    {
      "name": "example",
      "version": "0.1.0",
      "protocol": 1,
      "command": ["example-plugin"],
      "input": { "type": "audio", "sample_rate_hz": 48000, "channels": 1 },
      "outputs": ["packets", "display"],
      "params": [
        { "name": "mode", "kind": "enum", "values": ["a", "b"], "default": "a" }
      ]
    }
    "#;

    #[test]
    fn parses_manifest() {
        let manifest = Manifest::from_json(MANIFEST).unwrap();
        assert_eq!(manifest.name, "example");
        assert!(matches!(
            manifest.input,
            InputSpec::Audio {
                sample_rate_hz: 48_000.0,
                channels: 1
            }
        ));
        assert_eq!(manifest.params.len(), 1);
    }

    #[test]
    fn rejects_wrong_protocol() {
        let bad = MANIFEST.replace("\"protocol\": 1", "\"protocol\": 2");
        assert!(Manifest::from_json(&bad).is_err());
    }

    #[test]
    fn display_event_roundtrip() {
        let event = PluginEvent::Display {
            update: DisplayUpdate::Text {
                title: "CW".to_string(),
                lines: vec!["CQ CQ".to_string()],
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: PluginEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, PluginEvent::Display { .. }));
    }

    #[test]
    fn image_event_carries_dimensions_and_defaults_them() {
        let event = PluginEvent::Display {
            update: DisplayUpdate::Image {
                title: "SSTV".to_string(),
                format: ImageFormat::Png,
                width: 640,
                height: 496,
                bytes: vec![1, 2, 3],
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        let decoded: PluginEvent = serde_json::from_value(value).unwrap();
        match decoded {
            PluginEvent::Display {
                update:
                    DisplayUpdate::Image {
                        width,
                        height,
                        bytes,
                        ..
                    },
            } => {
                assert_eq!((width, height), (640, 496));
                assert_eq!(bytes, vec![1, 2, 3]);
            }
            other => panic!("expected image, got {other:?}"),
        }

        // Events from older plugins without dimensions still deserialize.
        let legacy = serde_json::json!({
            "type": "display",
            "update": {
                "kind": "image",
                "title": "old",
                "format": "png",
                "bytes": [1, 2]
            }
        });
        let decoded: PluginEvent = serde_json::from_value(legacy).unwrap();
        assert!(matches!(
            decoded,
            PluginEvent::Display {
                update: DisplayUpdate::Image {
                    width: 0,
                    height: 0,
                    ..
                }
            }
        ));
    }
}
