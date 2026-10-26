//! Plugin discovery, manifests, and sidecar start/configure helpers.

use std::path::{Path, PathBuf};

use midori_engine::{PluginHandle, PluginSink, plugin_sink};
use midori_plugin::{
    DiscoveredPlugin, InputSpec, Manifest, PluginHost, SidecarPlugin, default_plugin_dirs, discover,
};

use crate::worker::events::{EventSender, WorkerEvent};

/// Bundled copies of the shipped manifests. They are used when discovery finds
/// no manifest for a plugin (for example when running a standalone binary from
/// a build directory without the `plugins/` tree).
const CW_MANIFEST_JSON: &str = include_str!("../../../../plugins/midori-plugin-cw/manifest.json");
const SSTV_MANIFEST_JSON: &str =
    include_str!("../../../../plugins/midori-plugin-sstv/manifest.json");
const SSDV_MANIFEST_JSON: &str =
    include_str!("../../../../plugins/midori-plugin-ssdv/manifest.json");

/// SSDV preview interval used by the reference plugin.
const SSDV_PREVIEW_INTERVAL_MS: u64 = 650;

/// All manifests discovered on the default search path.
///
/// Discovery runs once per worker so one broken third-party manifest cannot
/// spam a message every time a decoder is enabled. The first plugin with a
/// given name wins, matching search-path order.
pub(super) struct PluginCatalog {
    plugins: Vec<DiscoveredPlugin>,
}

impl PluginCatalog {
    pub(super) fn load(event_tx: &EventSender) -> Self {
        let (plugins, errors) = discover(&default_plugin_dirs());
        for error in errors {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "plugin manifest {}: {}",
                error.path.display(),
                error.message
            )));
        }
        Self { plugins }
    }

    fn find(&self, name: &str) -> Option<&DiscoveredPlugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest.name == name)
    }
}

pub(super) fn start_cw_plugin(
    catalog: &PluginCatalog,
    event_tx: &EventSender,
    tone_hz: f64,
) -> Option<(PluginSink, PluginHandle)> {
    let discovered = catalog.find("cw");
    let program = plugin_program(
        event_tx,
        "CW",
        "MIDORI_CW_PLUGIN",
        "midori-plugin-cw",
        discovered,
    )?;
    let manifest = match discovered {
        Some(plugin) => plugin.sidecar_manifest(program.display().to_string()),
        None => {
            event_tx.best_effort(WorkerEvent::Info(
                "CW plugin manifest not found; using the built-in manifest".to_string(),
            ));
            builtin_manifest(event_tx, "CW", CW_MANIFEST_JSON, &program)?
        }
    };
    start_sidecar(event_tx, "CW", manifest, cw_overrides(tone_hz))
}

pub(super) fn start_sstv_plugin(
    catalog: &PluginCatalog,
    event_tx: &EventSender,
    output_dir: &Path,
) -> Option<(PluginSink, PluginHandle)> {
    let discovered = catalog.find("sstv");
    let program = plugin_program(
        event_tx,
        "SSTV",
        "MIDORI_SSTV_PLUGIN",
        "midori-plugin-sstv",
        discovered,
    )?;
    let manifest = match discovered {
        Some(plugin) => plugin.sidecar_manifest(program.display().to_string()),
        None => {
            event_tx.best_effort(WorkerEvent::Info(
                "SSTV plugin manifest not found; using the built-in manifest".to_string(),
            ));
            builtin_manifest(event_tx, "SSTV", SSTV_MANIFEST_JSON, &program)?
        }
    };
    start_sidecar(event_tx, "SSTV", manifest, sstv_overrides(output_dir))
}

pub(super) fn start_ssdv_plugin(
    catalog: &PluginCatalog,
    event_tx: &EventSender,
    output_dir: &Path,
    crc_inits: &[String],
    frame_header_len: usize,
    audio_if: bool,
) -> Option<(PluginSink, PluginHandle)> {
    let discovered = catalog.find("ssdv");
    let program = plugin_program(
        event_tx,
        "SSDV",
        "MIDORI_SSDV_PLUGIN",
        "midori-plugin-ssdv",
        discovered,
    )?;
    let mut manifest = match discovered {
        Some(plugin) => plugin.sidecar_manifest(program.display().to_string()),
        None => {
            event_tx.best_effort(WorkerEvent::Info(
                "SSDV plugin manifest not found; using the built-in manifest".to_string(),
            ));
            builtin_manifest(event_tx, "SSDV", SSDV_MANIFEST_JSON, &program)?
        }
    };
    // The SSDV sidecar exposes one pipeline at a time. The app picks it from
    // the demod mode, so override the manifest's default IQ declaration.
    manifest.input = if audio_if {
        InputSpec::Audio {
            sample_rate_hz: 48_000.0,
            channels: 1,
        }
    } else {
        InputSpec::Iq {
            sample_rate_hz: 48_000.0,
            center_freq_hz: None,
        }
    };

    start_sidecar(
        event_tx,
        "SSDV",
        manifest,
        ssdv_overrides(output_dir, crc_inits, frame_header_len, audio_if),
    )
}

fn start_sidecar(
    event_tx: &EventSender,
    label: &str,
    manifest: Manifest,
    overrides: serde_json::Value,
) -> Option<(PluginSink, PluginHandle)> {
    let params = match manifest.resolve_params(&overrides) {
        Ok(params) => params,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "{label} plugin parameters: {error:#}"
            )));
            return None;
        }
    };

    let mut plugin = match SidecarPlugin::new(manifest) {
        Ok(plugin) => plugin,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "{label} plugin manifest: {error}"
            )));
            return None;
        }
    };
    if let Err(error) = plugin.start() {
        event_tx.best_effort(WorkerEvent::Error(format!("{label} plugin start: {error}")));
        return None;
    }
    if let Err(error) = plugin.configure(&params) {
        let _ = plugin.stop();
        event_tx.best_effort(WorkerEvent::Error(format!(
            "{label} plugin configure: {error}"
        )));
        return None;
    }

    let (sink, handle) = plugin_sink(Box::new(plugin));
    Some((sink, handle))
}

fn plugin_program(
    event_tx: &EventSender,
    label: &str,
    env_var: &str,
    binary: &str,
    discovered: Option<&DiscoveredPlugin>,
) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_var) {
        return Some(PathBuf::from(path));
    }
    if let Some(plugin) = discovered
        && let Some(program) = plugin.command.first()
    {
        return Some(PathBuf::from(program));
    }
    match executable_dir_binary(binary) {
        Some(path) => Some(path),
        None => {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "{label} plugin binary not found; build the workspace first"
            )));
            None
        }
    }
}

fn executable_dir_binary(binary: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let dir = executable.parent()?;
    let name = if cfg!(windows) {
        format!("{binary}.exe")
    } else {
        binary.to_string()
    };
    let path = dir.join(name);
    path.exists().then_some(path)
}

fn builtin_manifest(
    event_tx: &EventSender,
    label: &str,
    json: &str,
    program: &Path,
) -> Option<Manifest> {
    match Manifest::from_json(json) {
        Ok(mut manifest) => {
            manifest.command = vec![program.display().to_string()];
            Some(manifest)
        }
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!(
                "{label} built-in manifest: {error:#}"
            )));
            None
        }
    }
}

/// Parameters the GUI passes to the CW decoder today.
fn cw_overrides(tone_hz: f64) -> serde_json::Value {
    serde_json::json!({
        "tone_hz": tone_hz,
        // The live GUI AFC leaves a few Hz of residual offset; a wider
        // envelope keeps that from turning into keying ripple.
        "envelope_hz": 120.0,
        "start_at_signal": true,
    })
}

/// Parameters the GUI passes to the SSTV decoder today.
fn sstv_overrides(output_dir: &Path) -> serde_json::Value {
    serde_json::json!({
        "output_dir": output_dir.display().to_string(),
        "sample_rate_hz": 48_000,
    })
}

/// Parameters the GUI passes to the SSDV decoder today.
fn ssdv_overrides(
    output_dir: &Path,
    crc_inits: &[String],
    frame_header_len: usize,
    audio_if: bool,
) -> serde_json::Value {
    serde_json::json!({
        "output_dir": output_dir.display().to_string(),
        "input": if audio_if { "audio-if" } else { "iq" },
        "crc_inits": crc_inits,
        "frame_header_len": frame_header_len,
        "preview_interval_ms": SSDV_PREVIEW_INTERVAL_MS,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use midori_plugin::PROTOCOL_VERSION;

    /// A typo in `demod_modes` would silently pause a decoder in every mode.
    #[test]
    fn bundled_manifest_demod_modes_are_real_mode_keys() {
        let known: BTreeSet<&str> = midori_core::DemodMode::ALL
            .iter()
            .map(|mode| mode.config_key())
            .collect();
        for json in [CW_MANIFEST_JSON, SSTV_MANIFEST_JSON, SSDV_MANIFEST_JSON] {
            let manifest = midori_plugin::Manifest::from_json(json).unwrap();
            for mode in &manifest.demod_modes {
                assert!(
                    known.contains(mode.as_str()),
                    "{}: unknown demod mode '{mode}'",
                    manifest.name
                );
            }
        }
    }

    /// Each bundled manifest must parse, validate, and declare exactly the
    /// parameter names the worker overrides. That keeps a manifest edit from
    /// silently changing what the app thinks it can configure.
    #[test]
    fn bundled_manifests_cover_the_params_the_worker_passes() {
        let output_dir = Path::new("captures");
        let crc_inits = vec!["0x4EE4FDE1".to_string()];
        let cases: [(&str, &str, serde_json::Value); 3] = [
            ("cw", CW_MANIFEST_JSON, cw_overrides(700.0)),
            ("sstv", SSTV_MANIFEST_JSON, sstv_overrides(output_dir)),
            (
                "ssdv",
                SSDV_MANIFEST_JSON,
                ssdv_overrides(output_dir, &crc_inits, 5, false),
            ),
        ];

        for (name, json, overrides) in cases {
            let manifest =
                Manifest::from_json(json).unwrap_or_else(|error| panic!("{name}: {error:#}"));
            assert_eq!(manifest.name, name);
            assert_eq!(manifest.protocol, PROTOCOL_VERSION);
            assert!(
                !manifest.demod_modes.is_empty(),
                "{name} must declare the demod modes it supports"
            );
            assert!(manifest.accepts_demod(&manifest.demod_modes[0]));

            let override_keys: BTreeSet<&str> = overrides
                .as_object()
                .expect("overrides must be an object")
                .keys()
                .map(String::as_str)
                .collect();
            let param_names: BTreeSet<&str> = manifest
                .params
                .iter()
                .map(|param| param.name.as_str())
                .collect();
            assert_eq!(
                override_keys, param_names,
                "{name} manifest parameters drifted from the worker overrides"
            );

            manifest
                .resolve_params(&overrides)
                .unwrap_or_else(|error| panic!("{name} overrides: {error:#}"));
            manifest
                .resolve_params(&serde_json::json!({}))
                .unwrap_or_else(|error| panic!("{name} defaults: {error:#}"));
        }
    }

    #[test]
    fn ssdv_audio_if_changes_input_and_parameter() {
        let output_dir = Path::new("captures");
        let overrides = ssdv_overrides(output_dir, &[], 5, true);
        assert_eq!(overrides["input"], "audio-if");
        let overrides = ssdv_overrides(output_dir, &[], 5, false);
        assert_eq!(overrides["input"], "iq");
    }
}
