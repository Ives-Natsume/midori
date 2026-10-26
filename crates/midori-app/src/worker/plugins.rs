//! Plugin path lookup, manifests, and sidecar start/configure helpers.

use std::path::{Path, PathBuf};

use midori_engine::{PluginHandle, PluginSink, plugin_sink};
use midori_plugin::{InputSpec, Manifest, OutputKind, PROTOCOL_VERSION, PluginHost, SidecarPlugin};

use crate::worker::events::{EventSender, WorkerEvent};

pub(super) fn cw_plugin_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MIDORI_CW_PLUGIN") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) {
        "midori-plugin-cw.exe"
    } else {
        "midori-plugin-cw"
    };
    let path = dir.join(name);
    path.exists().then_some(path)
}

pub(super) fn start_cw_plugin(
    event_tx: &EventSender,
    tone_hz: f64,
) -> Option<(PluginSink, PluginHandle)> {
    let Some(path) = cw_plugin_path() else {
        event_tx.best_effort(WorkerEvent::Error(
            "CW plugin binary not found; build the workspace first".to_string(),
        ));
        return None;
    };

    let manifest = Manifest {
        name: "cw".to_string(),
        version: "0.1.0".to_string(),
        protocol: PROTOCOL_VERSION,
        command: vec![path.display().to_string()],
        input: InputSpec::Audio {
            sample_rate_hz: 48_000.0,
            channels: 1,
        },
        outputs: vec![OutputKind::Display, OutputKind::Packets],
        params: vec![],
    };

    let mut plugin = match SidecarPlugin::new(manifest) {
        Ok(plugin) => plugin,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!("CW plugin manifest: {error}")));
            return None;
        }
    };
    if let Err(error) = plugin.start() {
        event_tx.best_effort(WorkerEvent::Error(format!("CW plugin start: {error}")));
        return None;
    }
    if let Err(error) = plugin.configure(&serde_json::json!({
        "tone_hz": tone_hz,
        // The live GUI AFC leaves a few Hz of residual offset; a wider
        // envelope keeps that from turning into keying ripple.
        "envelope_hz": 120.0,
        "start_at_signal": true,
    })) {
        let _ = plugin.stop();
        event_tx.best_effort(WorkerEvent::Error(format!("CW plugin configure: {error}")));
        return None;
    }

    let (sink, handle) = plugin_sink(Box::new(plugin));
    Some((sink, handle))
}

pub(super) fn sstv_plugin_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MIDORI_SSTV_PLUGIN") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) {
        "midori-plugin-sstv.exe"
    } else {
        "midori-plugin-sstv"
    };
    let path = dir.join(name);
    path.exists().then_some(path)
}

pub(super) fn start_sstv_plugin(
    event_tx: &EventSender,
    output_dir: &Path,
) -> Option<(PluginSink, PluginHandle)> {
    let Some(path) = sstv_plugin_path() else {
        event_tx.best_effort(WorkerEvent::Error(
            "SSTV plugin binary not found; build the workspace first".to_string(),
        ));
        return None;
    };

    let manifest = Manifest {
        name: "sstv".to_string(),
        version: "0.1.0".to_string(),
        protocol: PROTOCOL_VERSION,
        command: vec![path.display().to_string()],
        input: InputSpec::Audio {
            sample_rate_hz: 48_000.0,
            channels: 1,
        },
        outputs: vec![OutputKind::Display],
        params: vec![],
    };

    let mut plugin = match SidecarPlugin::new(manifest) {
        Ok(plugin) => plugin,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!("SSTV plugin manifest: {error}")));
            return None;
        }
    };
    if let Err(error) = plugin.start() {
        event_tx.best_effort(WorkerEvent::Error(format!("SSTV plugin start: {error}")));
        return None;
    }
    if let Err(error) = plugin.configure(&serde_json::json!({
        "output_dir": output_dir.display().to_string(),
    })) {
        let _ = plugin.stop();
        event_tx.best_effort(WorkerEvent::Error(format!(
            "SSTV plugin configure: {error}"
        )));
        return None;
    }

    let (sink, handle) = plugin_sink(Box::new(plugin));
    Some((sink, handle))
}

pub(super) fn ssdv_plugin_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MIDORI_SSDV_PLUGIN") {
        return Some(PathBuf::from(path));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) {
        "midori-plugin-ssdv.exe"
    } else {
        "midori-plugin-ssdv"
    };
    let path = dir.join(name);
    path.exists().then_some(path)
}

pub(super) fn start_ssdv_plugin(
    event_tx: &EventSender,
    output_dir: &Path,
    crc_inits: &[String],
    frame_header_len: usize,
    audio_if: bool,
) -> Option<(PluginSink, PluginHandle)> {
    let Some(path) = ssdv_plugin_path() else {
        event_tx.best_effort(WorkerEvent::Error(
            "SSDV plugin binary not found; build the workspace first".to_string(),
        ));
        return None;
    };

    let input = if audio_if {
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
    let manifest = Manifest {
        name: "ssdv".to_string(),
        version: "0.1.0".to_string(),
        protocol: PROTOCOL_VERSION,
        command: vec![path.display().to_string()],
        input,
        outputs: vec![OutputKind::Display],
        params: vec![],
    };

    let mut plugin = match SidecarPlugin::new(manifest) {
        Ok(plugin) => plugin,
        Err(error) => {
            event_tx.best_effort(WorkerEvent::Error(format!("SSDV plugin manifest: {error}")));
            return None;
        }
    };
    if let Err(error) = plugin.start() {
        event_tx.best_effort(WorkerEvent::Error(format!("SSDV plugin start: {error}")));
        return None;
    }
    if let Err(error) = plugin.configure(&serde_json::json!({
        "output_dir": output_dir.display().to_string(),
        "input": if audio_if { "audio-if" } else { "iq" },
        "crc_inits": crc_inits,
        "frame_header_len": frame_header_len,
    })) {
        let _ = plugin.stop();
        event_tx.best_effort(WorkerEvent::Error(format!(
            "SSDV plugin configure: {error}"
        )));
        return None;
    }

    let (sink, handle) = plugin_sink(Box::new(plugin));
    Some((sink, handle))
}
