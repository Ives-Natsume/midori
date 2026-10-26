//! Plugin contracts for Midori decoder sidecars.
//!
//! Decoders run out of process so they can be written in any language, carry
//! their own license, and crash without taking the engine down. This crate
//! defines the language-neutral manifest and event types, the binary wire
//! framing, and the sidecar supervisor.
//!
//! Protocol version 2 carries timestamps as exact `u64` UTC nanoseconds (with
//! [`wire::NO_TIMESTAMP`] for "unknown") and rate-limits `stream` messages
//! that only update frequency metadata. See [`wire`] and [`sidecar`] for the
//! framing and handshake details.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use midori_core::{Frame, FrameMeta, StreamKind};
use serde::{Deserialize, Serialize};
pub use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 2;

pub mod sidecar;
pub mod wire;

pub use sidecar::SidecarPlugin;

/// Stream metadata carried by the host's `stream` control message.
///
/// The JSON field names are part of the protocol; they are intentionally
/// identical to the previous ad-hoc `json!` objects built by the host.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub kind: StreamKind,
    pub sample_rate_hz: f64,
    /// Audio channel count. The current engine always sends mono.
    pub channels: u32,
    pub center_freq_hz: f64,
    /// Nominal (un-Doppler-shifted) frequency the user/preset asked for.
    pub nominal_freq_hz: f64,
    pub bandwidth_hz: f64,
    /// Doppler correction currently applied to the VFO.
    pub doppler_hz: f64,
    /// Doppler rate, Hz/s.
    pub doppler_rate_hz_s: f64,
    pub vfo_id: Option<u32>,
}

/// Human-readable name for a stream kind, matching the `stream` JSON.
pub fn stream_kind_name(kind: StreamKind) -> &'static str {
    match kind {
        StreamKind::Iq => "iq",
        StreamKind::Audio => "audio",
        StreamKind::SoftBits => "soft_bits",
        StreamKind::Bytes => "bytes",
    }
}

impl StreamInfo {
    /// Build the stream metadata for a frame. `channels` is always 1 because
    /// sidecar audio and IQ streams are mono.
    pub fn from_meta(kind: StreamKind, meta: &FrameMeta) -> Self {
        Self {
            kind,
            sample_rate_hz: meta.sample_rate_hz,
            channels: 1,
            center_freq_hz: meta.center_freq_hz,
            nominal_freq_hz: meta.nominal_freq_hz,
            bandwidth_hz: meta.bandwidth_hz,
            doppler_hz: meta.doppler_hz,
            doppler_rate_hz_s: meta.doppler_rate_hz_s,
            vfo_id: meta.vfo_id,
        }
    }
}

/// JSON control messages exchanged with a sidecar (protocol version 2).
///
/// This is the typed counterpart of the ad-hoc `json!` objects the host used
/// to build and inspect. Plugins written in Rust can use it through
/// `midori-plugin-sdk`; other languages see the same JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Hello {
        /// Missing fields default to zero/empty so a malformed handshake can
        /// still be reported as a protocol mismatch rather than a JSON error.
        #[serde(default)]
        protocol: u32,
        #[serde(default)]
        name: String,
        #[serde(default)]
        version: String,
    },
    Configure {
        params: Value,
    },
    Ready,
    Error {
        message: String,
    },
    Stream(StreamInfo),
    Shutdown,
}

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
    /// An ordered list of strings, used for parameters that take a set of
    /// values (for example SSDV CRC initial values).
    StringList,
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
    /// Demodulator modes this plugin's input makes sense for, using the
    /// snake_case names of the engine's `DemodMode` (for example `"cw"`,
    /// `"sstv"`, `"bpsk_if"`). The host keeps the field as strings so a
    /// manifest never has to be rebuilt when the engine gains a mode.
    ///
    /// An empty (or missing) list means the plugin accepts any mode.
    #[serde(default)]
    pub demod_modes: Vec<String>,
}

/// Whether `mode` is listed in a manifest's `demod_modes`.
///
/// An empty list means "any mode", so plugins written before mode routing
/// existed keep receiving frames.
pub fn demod_modes_match(modes: &[String], mode: &str) -> bool {
    modes.is_empty() || modes.iter().any(|candidate| candidate == mode)
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
        for mode in &self.demod_modes {
            if mode.trim().is_empty() {
                bail!("plugin '{}' has an empty demod mode", self.name);
            }
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

    /// Whether this plugin should receive frames while the VFO is in `mode`.
    pub fn accepts_demod(&self, mode: &str) -> bool {
        demod_modes_match(&self.demod_modes, mode)
    }

    /// Resolve the plugin's full parameter object from the declared defaults
    /// plus host overrides.
    ///
    /// Every parameter appears in the result (defaults are included). Overrides
    /// must refer to a declared parameter and match its type, range, and enum
    /// values. Unknown keys are rejected so a typo cannot silently leave a
    /// decoder on its default.
    pub fn resolve_params(&self, overrides: &Value) -> Result<Value> {
        let Some(overrides) = overrides.as_object() else {
            bail!(
                "plugin '{}' parameter overrides must be a JSON object",
                self.name
            );
        };

        let mut resolved = serde_json::Map::new();
        for spec in &self.params {
            if spec.name.trim().is_empty() {
                bail!("plugin '{}' has a parameter with an empty name", self.name);
            }
            if resolved.contains_key(&spec.name) {
                bail!(
                    "plugin '{}' declares parameter '{}' more than once",
                    self.name,
                    spec.name
                );
            }
            if !spec.default.is_null() {
                validate_param_value(spec, &spec.default).with_context(|| {
                    format!(
                        "plugin '{}' default for parameter '{}'",
                        self.name, spec.name
                    )
                })?;
            }
            resolved.insert(spec.name.clone(), spec.default.clone());
        }

        for (name, value) in overrides {
            let Some(spec) = self.params.iter().find(|spec| spec.name == *name) else {
                bail!("plugin '{}' has no parameter '{}'", self.name, name);
            };
            validate_param_value(spec, value)
                .with_context(|| format!("plugin '{}' parameter '{}'", self.name, name))?;
            resolved.insert(name.clone(), value.clone());
        }

        Ok(Value::Object(resolved))
    }
}

fn validate_param_value(spec: &ParamSpec, value: &Value) -> Result<()> {
    match spec.kind {
        ParamKind::Bool => {
            if !value.is_boolean() {
                bail!("expected a boolean");
            }
        }
        ParamKind::Int => {
            let Some(integer) = value.as_i64() else {
                bail!("expected an integer");
            };
            check_range(spec, integer as f64)?;
        }
        ParamKind::Float => {
            let Some(number) = value.as_f64() else {
                bail!("expected a number");
            };
            // JSON has no NaN/Infinity literals, but serde_json values can be
            // produced by callers in other ways in the future; never let a
            // non-finite value reach a DSP decoder.
            if !number.is_finite() {
                bail!("expected a finite number");
            }
            check_range(spec, number)?;
        }
        ParamKind::String => {
            if !value.is_string() {
                bail!("expected a string");
            }
        }
        ParamKind::Enum => {
            let Some(text) = value.as_str() else {
                bail!("expected a string");
            };
            if !spec.values.iter().any(|allowed| allowed == text) {
                bail!("value '{text}' is not one of [{}]", spec.values.join(", "));
            }
        }
        ParamKind::StringList => {
            let Some(items) = value.as_array() else {
                bail!("expected an array of strings");
            };
            if let Some(item) = items.iter().find(|item| !item.is_string()) {
                bail!("expected an array of strings, found {item}");
            }
        }
    }
    Ok(())
}

fn check_range(spec: &ParamSpec, value: f64) -> Result<()> {
    if let Some(min) = spec.min
        && value < min
    {
        bail!("value {value} is below the minimum {min}");
    }
    if let Some(max) = spec.max
        && value > max
    {
        bail!("value {value} is above the maximum {max}");
    }
    Ok(())
}

/// One manifest loaded from disk by [`discover`].
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub manifest: Manifest,
    pub manifest_path: PathBuf,
    /// Directory containing the manifest. Relative commands resolve here first.
    pub directory: PathBuf,
    /// `manifest.command` with `command[0]` resolved for this installation.
    ///
    /// See [`discover`] for the lookup order.
    pub command: Vec<String>,
}

impl DiscoveredPlugin {
    /// Return a copy of the manifest whose command starts with `program`.
    ///
    /// The remaining arguments are preserved. The app uses this to honour its
    /// per-plugin environment overrides without losing the rest of the command
    /// line from the manifest.
    pub fn sidecar_manifest(&self, program: impl Into<String>) -> Manifest {
        let mut manifest = self.manifest.clone();
        manifest.command = std::iter::once(program.into())
            .chain(self.command.iter().skip(1).cloned())
            .collect();
        manifest
    }

    /// Consume this entry, returning a manifest with the resolved command.
    pub fn into_manifest(mut self) -> Manifest {
        self.manifest.command = self.command;
        self.manifest
    }
}

/// A manifest that could not be loaded.
#[derive(Debug, Clone)]
pub struct DiscoveryError {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for DiscoveryError {}

/// Discover plugin manifests in `dirs`, in search-path order.
///
/// Each directory is scanned for `*.json` manifest files and for
/// `*/manifest.json` bundle layouts. Entries are de-duplicated by canonical
/// path, and the first definition of a name wins because callers search the
/// returned list in order.
///
/// For every manifest, a relative `command[0]` is resolved in this order:
///
/// 1. relative to the directory containing the manifest;
/// 2. relative to the directory containing the current executable;
/// 3. left unchanged, so the operating system performs a `PATH` lookup.
///
/// Invalid manifests are returned in the second half of the tuple instead of
/// aborting the scan, so one broken third-party plugin cannot hide the others.
pub fn discover(dirs: &[PathBuf]) -> (Vec<DiscoveredPlugin>, Vec<DiscoveryError>) {
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                errors.push(DiscoveryError {
                    path: dir.clone(),
                    message: format!("reading plugin directory: {error}"),
                });
                continue;
            }
        };

        let mut candidates = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    errors.push(DiscoveryError {
                        path: dir.clone(),
                        message: format!("reading plugin directory entry: {error}"),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    errors.push(DiscoveryError {
                        path: path.clone(),
                        message: format!("reading entry type: {error}"),
                    });
                    continue;
                }
            };
            if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            {
                candidates.push(path);
            } else if file_type.is_dir() {
                let manifest = path.join("manifest.json");
                if manifest.is_file() {
                    candidates.push(manifest);
                }
            }
        }
        candidates.sort();

        for manifest_path in candidates {
            let dedupe_key = manifest_path
                .canonicalize()
                .unwrap_or_else(|_| manifest_path.clone());
            if !seen.insert(dedupe_key) {
                continue;
            }
            match load_discovered(&manifest_path) {
                Ok(plugin) => plugins.push(plugin),
                Err(error) => errors.push(DiscoveryError {
                    path: manifest_path,
                    message: format!("{error:#}"),
                }),
            }
        }
    }

    (plugins, errors)
}

fn load_discovered(manifest_path: &Path) -> Result<DiscoveredPlugin> {
    let manifest = Manifest::from_path(manifest_path)?;
    let directory = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let command = resolve_command(&directory, &manifest.command);
    Ok(DiscoveredPlugin {
        manifest,
        manifest_path: manifest_path.to_path_buf(),
        directory,
        command,
    })
}

fn resolve_command(manifest_dir: &Path, command: &[String]) -> Vec<String> {
    let mut resolved = command.to_vec();
    let Some(program) = resolved.first_mut() else {
        return resolved;
    };

    let program_path = Path::new(program);
    if program_path.is_absolute() {
        return resolved;
    }

    if let Some(found) = find_executable(&manifest_dir.join(program_path)) {
        *program = found.display().to_string();
        return resolved;
    }

    if let Ok(executable) = std::env::current_exe()
        && let Some(executable_dir) = executable.parent()
        && let Some(found) = find_executable(&executable_dir.join(program_path))
    {
        *program = found.display().to_string();
    }

    // Not found next to the manifest or the host: leave the bare name so the
    // OS can perform a PATH lookup when the sidecar is spawned.
    resolved
}

fn find_executable(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        let with_extension = path.with_extension("exe");
        if with_extension.is_file() {
            return Some(with_extension);
        }
    }
    None
}

/// Default plugin search path, in order:
///
/// 1. entries from `MIDORI_PLUGIN_PATH` (OS path-list separated);
/// 2. `<directory of the current executable>/plugins`;
/// 3. a development-tree `<workspace>/plugins`, discovered relative to the
///    executable (`target/debug/midori-gui` → `../../plugins`, and one level
///    deeper for test binaries in `target/debug/deps`).
pub fn default_plugin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(paths) = std::env::var_os("MIDORI_PLUGIN_PATH") {
        for path in std::env::split_paths(&paths) {
            if !path.as_os_str().is_empty() {
                push_unique(&mut dirs, path);
            }
        }
    }

    if let Ok(executable) = std::env::current_exe()
        && let Some(executable_dir) = executable.parent()
    {
        push_unique(&mut dirs, executable_dir.join("plugins"));
        for relative in ["../../plugins", "../../../plugins"] {
            if let Ok(path) = executable_dir.join(relative).canonicalize()
                && path.is_dir()
            {
                push_unique(&mut dirs, path);
            }
        }
    }

    dirs
}

fn push_unique(dirs: &mut Vec<PathBuf>, path: PathBuf) {
    if !dirs.iter().any(|existing| existing == &path) {
        dirs.push(path);
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
    use std::sync::atomic::{AtomicU64, Ordering};

    const MANIFEST: &str = r#"
    {
      "name": "example",
      "version": "0.1.0",
      "protocol": 2,
      "command": ["example-plugin"],
      "input": { "type": "audio", "sample_rate_hz": 48000, "channels": 1 },
      "outputs": ["packets", "display"],
      "params": [
        { "name": "mode", "kind": "enum", "values": ["a", "b"], "default": "a" }
      ],
      "demod_modes": ["cw", "fm"]
    }
    "#;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "midori-plugin-test-{}-{}-{label}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("creating temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("creating parent dir");
        }
        std::fs::write(path, text).expect("writing test file");
    }

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
        assert_eq!(manifest.demod_modes, vec!["cw", "fm"]);
    }

    #[test]
    fn rejects_wrong_protocol() {
        let bad = MANIFEST.replace("\"protocol\": 2", "\"protocol\": 1");
        assert!(Manifest::from_json(&bad).is_err());
    }

    #[test]
    fn missing_demod_modes_means_any_mode() {
        let json = MANIFEST.replace(",\n      \"demod_modes\": [\"cw\", \"fm\"]", "");
        let manifest = Manifest::from_json(&json).unwrap();
        assert!(manifest.demod_modes.is_empty());
        assert!(manifest.accepts_demod("anything"));
    }

    #[test]
    fn rejects_empty_demod_mode() {
        let bad = MANIFEST.replace("[\"cw\", \"fm\"]", "[\"cw\", \"  \"]");
        assert!(Manifest::from_json(&bad).is_err());
    }

    #[test]
    fn demod_mode_matching() {
        assert!(demod_modes_match(&[], "fm"));
        let modes = vec!["cw".to_string(), "sstv".to_string()];
        assert!(demod_modes_match(&modes, "cw"));
        assert!(demod_modes_match(&modes, "sstv"));
        assert!(!demod_modes_match(&modes, "fm"));
        assert!(!demod_modes_match(&modes, "CW"));
    }

    #[test]
    fn discovery_finds_files_and_bundles() {
        let root = TempDir::new("discovery-valid");
        write(
            &root.path().join("one.json"),
            &MANIFEST.replace("example", "one"),
        );
        write(
            &root.path().join("two/manifest.json"),
            &MANIFEST.replace("example", "two"),
        );

        let (plugins, errors) = discover(&[root.path().to_path_buf()]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        let names: Vec<&str> = plugins
            .iter()
            .map(|plugin| plugin.manifest.name.as_str())
            .collect();
        assert_eq!(names, ["one", "two"]);
    }

    #[test]
    fn discovery_collects_invalid_manifests() {
        let root = TempDir::new("discovery-invalid");
        write(&root.path().join("good.json"), MANIFEST);
        write(&root.path().join("bad.json"), "{ this is not json");

        let (plugins, errors) = discover(&[root.path().to_path_buf()]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].path.ends_with("bad.json"),
            "unexpected error path: {}",
            errors[0].path.display()
        );
        assert!(!errors[0].message.is_empty());
    }

    #[test]
    fn discovery_resolves_command_next_to_manifest() {
        let root = TempDir::new("discovery-relative");
        write(
            &root.path().join("bundle/manifest.json"),
            r#"{
              "name": "relative",
              "version": "0.1.0",
              "protocol": 2,
              "command": ["fake-sidecar", "--flag"],
              "input": { "type": "audio", "sample_rate_hz": 48000, "channels": 1 }
            }"#,
        );
        write(&root.path().join("bundle/fake-sidecar"), "");

        let (plugins, errors) = discover(&[root.path().to_path_buf()]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(plugins.len(), 1);
        let plugin = &plugins[0];
        assert_eq!(
            PathBuf::from(&plugin.command[0]),
            root.path().join("bundle/fake-sidecar")
        );
        assert_eq!(plugin.command[1], "--flag");
    }

    #[test]
    fn discovery_resolves_command_next_to_executable() {
        let executable = std::env::current_exe().expect("current exe");
        let file_name = executable
            .file_name()
            .expect("exe file name")
            .to_string_lossy()
            .into_owned();
        let root = TempDir::new("discovery-exe-dir");
        write(
            &root.path().join("manifest.json"),
            &format!(
                r#"{{
                  "name": "exe-relative",
                  "version": "0.1.0",
                  "protocol": 2,
                  "command": ["{file_name}"],
                  "input": {{ "type": "audio", "sample_rate_hz": 48000, "channels": 1 }}
                }}"#
            ),
        );

        let (plugins, errors) = discover(&[root.path().to_path_buf()]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(
            PathBuf::from(&plugins[0].command[0]),
            executable.parent().unwrap().join(file_name)
        );
    }

    #[test]
    fn discovery_leaves_unresolved_command_for_path_lookup() {
        let root = TempDir::new("discovery-path");
        write(
            &root.path().join("manifest.json"),
            r#"{
              "name": "path-lookup",
              "version": "0.1.0",
              "protocol": 2,
              "command": ["midori-plugin-does-not-exist-anywhere"],
              "input": { "type": "audio", "sample_rate_hz": 48000, "channels": 1 }
            }"#,
        );

        let (plugins, errors) = discover(&[root.path().to_path_buf()]);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(
            plugins[0].command[0],
            "midori-plugin-does-not-exist-anywhere"
        );
    }

    #[test]
    fn discovery_deduplicates_directories() {
        let root = TempDir::new("discovery-dedupe");
        write(&root.path().join("manifest.json"), MANIFEST);
        let (plugins, errors) = discover(&[root.path().to_path_buf(), root.path().to_path_buf()]);
        assert!(errors.is_empty());
        assert_eq!(plugins.len(), 1);
    }

    #[test]
    fn default_plugin_dirs_start_with_the_environment() {
        let root = TempDir::new("default-dirs");
        // Environment variables are process-wide; serialise this test against
        // the other tests in this module that do not touch them.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());

        let previous = std::env::var_os("MIDORI_PLUGIN_PATH");
        // SAFETY: the lock above serialises access within this test binary.
        unsafe { std::env::set_var("MIDORI_PLUGIN_PATH", root.path()) };
        let dirs = default_plugin_dirs();
        match previous {
            Some(value) => unsafe { std::env::set_var("MIDORI_PLUGIN_PATH", value) },
            None => unsafe { std::env::remove_var("MIDORI_PLUGIN_PATH") },
        };

        assert_eq!(dirs.first().map(PathBuf::as_path), Some(root.path()));
    }

    const PARAM_MANIFEST: &str = r#"
    {
      "name": "params",
      "version": "0.1.0",
      "protocol": 2,
      "command": ["params-plugin"],
      "input": { "type": "audio", "sample_rate_hz": 48000, "channels": 1 },
      "params": [
        { "name": "gain", "kind": "float", "default": 1.0, "min": 0.0, "max": 10.0 },
        { "name": "count", "kind": "int", "default": 2, "min": 1, "max": 5 },
        { "name": "enabled", "kind": "bool", "default": true },
        { "name": "label", "kind": "string", "default": "hi" },
        { "name": "mode", "kind": "enum", "values": ["a", "b"], "default": "a" },
        { "name": "list", "kind": "string_list", "default": ["x"] }
      ]
    }
    "#;

    fn param_manifest() -> Manifest {
        Manifest::from_json(PARAM_MANIFEST).unwrap()
    }

    #[test]
    fn resolve_params_returns_defaults() {
        let manifest = param_manifest();
        assert_eq!(
            manifest.resolve_params(&serde_json::json!({})).unwrap(),
            serde_json::json!({
                "gain": 1.0,
                "count": 2,
                "enabled": true,
                "label": "hi",
                "mode": "a",
                "list": ["x"]
            })
        );
    }

    #[test]
    fn resolve_params_applies_overrides() {
        let manifest = param_manifest();
        let resolved = manifest
            .resolve_params(&serde_json::json!({
                "gain": 2.5,
                "count": 3,
                "enabled": false,
                "label": "bye",
                "mode": "b",
                "list": ["y", "z"]
            }))
            .unwrap();
        assert_eq!(
            resolved,
            serde_json::json!({
                "gain": 2.5,
                "count": 3,
                "enabled": false,
                "label": "bye",
                "mode": "b",
                "list": ["y", "z"]
            })
        );
    }

    #[test]
    fn resolve_params_rejects_unknown_keys() {
        let error = param_manifest()
            .resolve_params(&serde_json::json!({"missing": 1}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("has no parameter 'missing'"));
    }

    #[test]
    fn resolve_params_rejects_non_object_overrides() {
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!([1, 2]))
                .is_err()
        );
    }

    #[test]
    fn resolve_params_rejects_bool_mismatch() {
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"enabled": 1}))
                .is_err()
        );
    }

    #[test]
    fn resolve_params_rejects_int_mismatch() {
        for value in [serde_json::json!(1.5), serde_json::json!("3")] {
            assert!(
                param_manifest()
                    .resolve_params(&serde_json::json!({"count": value}))
                    .is_err()
            );
        }
    }

    #[test]
    fn resolve_params_rejects_float_mismatch() {
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"gain": "fast"}))
                .is_err()
        );
        // serde_json has no NaN literal; a non-finite value becomes JSON null,
        // which must be rejected rather than reaching the decoder.
        assert!(serde_json::to_value(f64::NAN).unwrap().is_null());
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"gain": f64::NAN}))
                .is_err()
        );
    }

    #[test]
    fn resolve_params_rejects_string_mismatch() {
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"label": 1}))
                .is_err()
        );
    }

    #[test]
    fn resolve_params_rejects_enum_values_not_listed() {
        let error = param_manifest()
            .resolve_params(&serde_json::json!({"mode": "c"}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("is not one of [a, b]"));
    }

    #[test]
    fn resolve_params_rejects_out_of_range_values() {
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"gain": -1.0}))
                .is_err()
        );
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"gain": 11.0}))
                .is_err()
        );
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"count": 0}))
                .is_err()
        );
        assert!(
            param_manifest()
                .resolve_params(&serde_json::json!({"count": 6}))
                .is_err()
        );
    }

    #[test]
    fn resolve_params_rejects_invalid_string_lists() {
        for value in [serde_json::json!("x"), serde_json::json!(["x", 1])] {
            assert!(
                param_manifest()
                    .resolve_params(&serde_json::json!({"list": value}))
                    .is_err()
            );
        }
    }

    #[test]
    fn resolve_params_rejects_invalid_defaults() {
        let bad = PARAM_MANIFEST.replace("\"default\": 1.0", "\"default\": 99.0");
        let manifest = Manifest::from_json(&bad).unwrap();
        assert!(manifest.resolve_params(&serde_json::json!({})).is_err());
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
    fn stream_control_json_matches_host_output() {
        let info = StreamInfo {
            kind: StreamKind::Audio,
            sample_rate_hz: 48_000.0,
            channels: 1,
            center_freq_hz: 145_960_000.0,
            nominal_freq_hz: 145_955_000.0,
            bandwidth_hz: 15_000.0,
            doppler_hz: 12.5,
            doppler_rate_hz_s: -0.25,
            vfo_id: Some(0),
        };
        let value = serde_json::to_value(ControlMessage::Stream(info)).unwrap();
        // Keys and values must stay byte-for-byte compatible with the JSON the
        // host built before the typed messages existed.
        assert_eq!(
            value,
            serde_json::json!({
                "type": "stream",
                "kind": "audio",
                "sample_rate_hz": 48_000.0,
                "channels": 1,
                "center_freq_hz": 145_960_000.0,
                "nominal_freq_hz": 145_955_000.0,
                "bandwidth_hz": 15_000.0,
                "doppler_hz": 12.5,
                "doppler_rate_hz_s": -0.25,
                "vfo_id": 0
            })
        );
    }

    #[test]
    fn control_messages_roundtrip() {
        let messages = vec![
            ControlMessage::Hello {
                protocol: PROTOCOL_VERSION,
                name: "cw".to_string(),
                version: "0.1.0".to_string(),
            },
            ControlMessage::Configure {
                params: serde_json::json!({"tone_hz": 700.0}),
            },
            ControlMessage::Ready,
            ControlMessage::Error {
                message: "bad params".to_string(),
            },
            ControlMessage::Shutdown,
        ];
        for message in messages {
            let json = serde_json::to_string(&message).unwrap();
            let decoded: ControlMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn error_control_omits_fatal_flag() {
        let value = serde_json::to_value(ControlMessage::Error {
            message: "boom".to_string(),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type": "error", "message": "boom"})
        );
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
