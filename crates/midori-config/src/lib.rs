//! Persistent application settings.
//!
//! The config is intentionally boring: TOML, human-editable, loaded once at
//! startup, and saved only when the user asks. Every field has a compiled
//! fallback so a missing section, a partial file, or a typo cannot prevent the
//! application from starting.

use std::fmt;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use midori_core::{DemodMode, GainMode};
use serde::{Deserialize, Serialize};

pub const DEFAULT_FILE: &str = "config.toml";
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Colormap {
    #[default]
    Viridis,
    Turbo,
    Grayscale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DecoderTab {
    Cw,
    Sstv,
    #[default]
    Passes,
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Pluto,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSettings {
    pub kind: SourceKind,
    pub uri: String,
    pub file_path: String,
    pub file_loop: bool,
    pub center_freq_hz: f64,
    pub sample_rate_hz: f64,
    pub bandwidth_hz: f64,
    pub gain_db: f64,
    pub gain_mode: GainMode,
    pub rf_port: String,
    pub buffer_samples: usize,
}

impl Default for SourceSettings {
    fn default() -> Self {
        Self {
            kind: SourceKind::Pluto,
            uri: "usb:".to_string(),
            file_path: String::new(),
            file_loop: false,
            center_freq_hz: 100_000_000.0,
            sample_rate_hz: 2_400_000.0,
            bandwidth_hz: 2_400_000.0,
            gain_db: 40.0,
            gain_mode: GainMode::Manual,
            rf_port: "A_BALANCED".to_string(),
            buffer_samples: 32_768,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DemodParams {
    pub bandwidth_hz: f64,
    pub channel_rate_hz: f64,
    pub audio_rate_hz: f64,
    pub audio_bandwidth_hz: f64,
    pub deemphasis_us: f64,
    pub bfo_hz: f64,
}

impl DemodParams {
    fn fm() -> Self {
        Self {
            bandwidth_hz: 200_000.0,
            channel_rate_hz: 240_000.0,
            audio_rate_hz: 48_000.0,
            audio_bandwidth_hz: 15_000.0,
            deemphasis_us: 50.0,
            bfo_hz: 700.0,
        }
    }

    fn am() -> Self {
        Self {
            bandwidth_hz: 6_000.0,
            channel_rate_hz: 48_000.0,
            audio_rate_hz: 48_000.0,
            audio_bandwidth_hz: 4_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        }
    }

    fn cw() -> Self {
        Self {
            bandwidth_hz: 500.0,
            channel_rate_hz: 48_000.0,
            audio_rate_hz: 48_000.0,
            audio_bandwidth_hz: 2_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        }
    }

    fn sstv() -> Self {
        Self {
            bandwidth_hz: 15_000.0,
            channel_rate_hz: 48_000.0,
            audio_rate_hz: 48_000.0,
            audio_bandwidth_hz: 3_000.0,
            deemphasis_us: 0.0,
            bfo_hz: 700.0,
        }
    }
}

impl Default for DemodParams {
    fn default() -> Self {
        Self::fm()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DemodSettings {
    pub mode: DemodMode,
    pub fm: DemodParams,
    pub am: DemodParams,
    pub cw: DemodParams,
    pub sstv: DemodParams,
}

impl Default for DemodSettings {
    fn default() -> Self {
        Self {
            mode: DemodMode::Fm,
            fm: DemodParams::fm(),
            am: DemodParams::am(),
            cw: DemodParams::cw(),
            sstv: DemodParams::sstv(),
        }
    }
}

impl DemodSettings {
    pub fn params(&self, mode: DemodMode) -> &DemodParams {
        match mode {
            DemodMode::Am => &self.am,
            DemodMode::Cw => &self.cw,
            DemodMode::Sstv => &self.sstv,
            _ => &self.fm,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoderSettings {
    pub cw_enabled: bool,
    pub sstv_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub spectrum_ratio: f32,
    pub decoder_dock_open: bool,
    pub decoder_tab: DecoderTab,
    pub left_panel_width: f32,
    pub right_panel_width: f32,
    pub decoder_dock_height: f32,
    pub theme: Theme,
    pub colormap: Colormap,
    pub db_min: f32,
    pub db_max: f32,
    pub fft_size: usize,
    pub waterfall_rows: usize,
    pub waterfall_width: usize,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            spectrum_ratio: 0.55,
            decoder_dock_open: true,
            decoder_tab: DecoderTab::Passes,
            left_panel_width: 280.0,
            right_panel_width: 320.0,
            decoder_dock_height: 170.0,
            theme: Theme::Dark,
            colormap: Colormap::Viridis,
            db_min: -110.0,
            db_max: -20.0,
            fft_size: 4_096,
            waterfall_rows: 200,
            waterfall_width: 1_024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SatelliteSettings {
    pub tle_path: String,
    pub grid: String,
    pub min_elevation_deg: f64,
    pub auto_predict: bool,
    /// Apply SGP4 Doppler correction to the digital VFO while connected.
    pub doppler_tracking: bool,
}

impl Default for SatelliteSettings {
    fn default() -> Self {
        Self {
            tle_path: String::new(),
            grid: String::new(),
            min_elevation_deg: 5.0,
            auto_predict: false,
            doppler_tracking: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub version: u32,
    pub source: SourceSettings,
    pub demod: DemodSettings,
    pub decoder: DecoderSettings,
    pub ui: UiSettings,
    pub satellite: SatelliteSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            source: SourceSettings::default(),
            demod: DemodSettings::default(),
            decoder: DecoderSettings::default(),
            ui: UiSettings::default(),
            satellite: SatelliteSettings::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub warning: Option<String>,
}

impl fmt::Display for LoadedConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.warning {
            Some(warning) => write!(formatter, "config warning: {warning}"),
            None => write!(formatter, "config loaded"),
        }
    }
}

impl AppConfig {
    /// Load `path`, falling back to compiled defaults on missing/invalid files.
    pub fn load(path: &Path) -> LoadedConfig {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return LoadedConfig {
                    config: Self::default(),
                    warning: None,
                };
            }
            Err(error) => {
                return LoadedConfig {
                    config: Self::default(),
                    warning: Some(format!("{}: {error}", path.display())),
                };
            }
        };

        match toml::from_str::<Self>(&text) {
            Ok(mut config) => {
                if config.version > CURRENT_VERSION {
                    return LoadedConfig {
                        config: Self::default(),
                        warning: Some(format!(
                            "{}: config version {} is newer than {}",
                            path.display(),
                            config.version,
                            CURRENT_VERSION
                        )),
                    };
                }
                config.version = CURRENT_VERSION;
                config.sanitize();
                LoadedConfig {
                    config,
                    warning: None,
                }
            }
            Err(error) => LoadedConfig {
                config: Self::default(),
                warning: Some(format!("{}: {error}", path.display())),
            },
        }
    }

    /// Atomically write the config as TOML.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }

        let text = toml::to_string_pretty(self).context("serialising config")?;
        let temp = path.with_extension("toml.tmp");
        fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("setting permissions on {}", temp.display()))?;
        }

        if fs::rename(&temp, path).is_err() {
            let _ = fs::remove_file(path);
            fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
        }
        Ok(())
    }

    /// Clamp hand-edited or stale values into ranges the UI can handle.
    pub fn sanitize(&mut self) {
        self.source.sample_rate_hz = self.source.sample_rate_hz.clamp(100_000.0, 61_440_000.0);
        self.source.bandwidth_hz = self.source.bandwidth_hz.clamp(100_000.0, 61_440_000.0);
        self.source.center_freq_hz = self.source.center_freq_hz.clamp(0.0, 6_000_000_000.0);
        self.source.gain_db = self.source.gain_db.clamp(-3.0, 71.0);
        self.source.buffer_samples = self.source.buffer_samples.clamp(1_024, 1_048_576);
        if self.source.uri.trim().is_empty() {
            self.source.uri = "usb:".to_string();
        }
        if self.source.rf_port.trim().is_empty() {
            self.source.rf_port = "A_BALANCED".to_string();
        }

        for params in [
            &mut self.demod.fm,
            &mut self.demod.am,
            &mut self.demod.cw,
            &mut self.demod.sstv,
        ] {
            params.bandwidth_hz = params.bandwidth_hz.clamp(100.0, 2_000_000.0);
            params.channel_rate_hz = params.channel_rate_hz.clamp(8_000.0, 61_440_000.0);
            params.audio_rate_hz = params.audio_rate_hz.clamp(8_000.0, 192_000.0);
            params.audio_bandwidth_hz = params
                .audio_bandwidth_hz
                .clamp(100.0, params.audio_rate_hz * 0.45);
            params.deemphasis_us = params.deemphasis_us.clamp(0.0, 500.0);
            params.bfo_hz = params.bfo_hz.clamp(100.0, 5_000.0);
        }

        self.ui.spectrum_ratio = self.ui.spectrum_ratio.clamp(0.2, 0.85);
        self.ui.left_panel_width = self.ui.left_panel_width.clamp(200.0, 500.0);
        self.ui.right_panel_width = self.ui.right_panel_width.clamp(220.0, 560.0);
        self.ui.decoder_dock_height = self.ui.decoder_dock_height.clamp(80.0, 500.0);
        self.ui.fft_size = self.ui.fft_size.clamp(256, 65_536);
        self.ui.waterfall_rows = self.ui.waterfall_rows.clamp(50, 1_000);
        self.ui.waterfall_width = self.ui.waterfall_width.clamp(256, 4_096);
        self.ui.db_min = self.ui.db_min.clamp(-200.0, -10.0);
        self.ui.db_max = self.ui.db_max.clamp(-100.0, 0.0);
        if self.ui.db_max <= self.ui.db_min + 10.0 {
            self.ui.db_min = -110.0;
            self.ui.db_max = -20.0;
        }

        self.satellite.min_elevation_deg = self.satellite.min_elevation_deg.clamp(0.0, 90.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("midori-config-{name}-{nanos}.toml"))
    }

    #[test]
    fn partial_config_uses_defaults() {
        let config: AppConfig = toml::from_str("[ui]\nspectrum_ratio = 0.6\n").unwrap();
        assert_eq!(config.source.center_freq_hz, 100_000_000.0);
        assert_eq!(config.ui.spectrum_ratio, 0.6);
        assert_eq!(config.ui.decoder_tab, DecoderTab::Passes);
        assert!(config.satellite.grid.is_empty());
    }

    #[test]
    fn invalid_config_falls_back() {
        let path = temp_path("invalid");
        fs::write(&path, "this is not toml = [").unwrap();
        let loaded = AppConfig::load(&path);
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.config.source.center_freq_hz, 100_000_000.0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_path("roundtrip");
        let mut config = AppConfig::default();
        config.source.center_freq_hz = 145_800_000.0;
        config.satellite.grid = "PM95".to_string();
        config.save(&path).unwrap();
        let loaded = AppConfig::load(&path);
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.config.source.center_freq_hz, 145_800_000.0);
        assert_eq!(loaded.config.satellite.grid, "PM95");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn clamps_out_of_range_values() {
        let mut config = AppConfig::default();
        config.source.gain_db = 200.0;
        config.ui.spectrum_ratio = 2.0;
        config.ui.db_min = -5.0;
        config.ui.db_max = -10.0;
        config.sanitize();
        assert_eq!(config.source.gain_db, 71.0);
        assert_eq!(config.ui.spectrum_ratio, 0.85);
        assert_eq!(config.ui.db_min, -110.0);
        assert_eq!(config.ui.db_max, -20.0);
    }
}
