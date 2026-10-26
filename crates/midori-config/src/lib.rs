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
    Ssdv,
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
            // A 500 Hz filter is fragile with a few tens of Hz of residual
            // tuning error and a drifting satellite carrier. 2 kHz still
            // rejects nearby signals but keeps the keying sidebands intact.
            bandwidth_hz: 2_000.0,
            channel_rate_hz: 240_000.0,
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

    fn bpsk() -> Self {
        Self {
            // A 9k6 BPSK link with RRC roll-off 0.35. A 30 kHz channel keeps
            // both sidebands and the FLL pull-in range; the output rate is
            // the 48 kHz IQ stream the data decoder expects.
            bandwidth_hz: 30_000.0,
            channel_rate_hz: 48_000.0,
            audio_rate_hz: 48_000.0,
            audio_bandwidth_hz: 30_000.0,
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
    #[serde(default = "default_fm_params")]
    pub fm: DemodParams,
    #[serde(default = "default_am_params")]
    pub am: DemodParams,
    #[serde(default = "default_cw_params")]
    pub cw: DemodParams,
    #[serde(default = "default_sstv_params")]
    pub sstv: DemodParams,
    #[serde(default = "default_bpsk_params")]
    pub bpsk: DemodParams,
}

fn default_fm_params() -> DemodParams {
    DemodParams::fm()
}

fn default_am_params() -> DemodParams {
    DemodParams::am()
}

fn default_cw_params() -> DemodParams {
    DemodParams::cw()
}

fn default_sstv_params() -> DemodParams {
    DemodParams::sstv()
}

fn default_bpsk_params() -> DemodParams {
    DemodParams::bpsk()
}

impl Default for DemodSettings {
    fn default() -> Self {
        Self {
            mode: DemodMode::Fm,
            fm: DemodParams::fm(),
            am: DemodParams::am(),
            cw: DemodParams::cw(),
            sstv: DemodParams::sstv(),
            bpsk: DemodParams::bpsk(),
        }
    }
}

impl DemodSettings {
    /// Parameters for an implemented mode. Returns `None` for reserved modes
    /// (USB/LSB), which must not silently reuse another mode's settings.
    pub fn params(&self, mode: DemodMode) -> Option<&DemodParams> {
        match mode {
            DemodMode::Am => Some(&self.am),
            DemodMode::Fm => Some(&self.fm),
            DemodMode::Cw => Some(&self.cw),
            DemodMode::Sstv => Some(&self.sstv),
            DemodMode::Bpsk | DemodMode::BpskIf => Some(&self.bpsk),
            DemodMode::Usb | DemodMode::Lsb => None,
        }
    }

    /// Mutable counterpart of [`Self::params`].
    pub fn params_mut(&mut self, mode: DemodMode) -> Option<&mut DemodParams> {
        match mode {
            DemodMode::Am => Some(&mut self.am),
            DemodMode::Fm => Some(&mut self.fm),
            DemodMode::Cw => Some(&mut self.cw),
            DemodMode::Sstv => Some(&mut self.sstv),
            DemodMode::Bpsk | DemodMode::BpskIf => Some(&mut self.bpsk),
            DemodMode::Usb | DemodMode::Lsb => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoderSettings {
    pub cw_enabled: bool,
    /// Keep the CW carrier centred on the BFO with a real-time AFC.
    pub cw_afc: bool,
    pub sstv_enabled: bool,
    pub ssdv_enabled: bool,
    /// CRC32 initial values accepted for SSDV packets. The polynomial and
    /// final XOR are fixed; different SSDV implementations use different
    /// initial values.
    pub ssdv_crc_inits: Vec<String>,
    /// Bytes before the 218-byte SSDV packet inside a 223-byte CCSDS frame.
    pub ssdv_frame_header_len: usize,
}

impl Default for DecoderSettings {
    fn default() -> Self {
        Self {
            cw_enabled: false,
            cw_afc: true,
            sstv_enabled: false,
            ssdv_enabled: false,
            ssdv_crc_inits: vec!["0xFFFFFFFF".to_string()],
            ssdv_frame_header_len: 5,
        }
    }
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
    /// Crop the waterfall to the spectrum plot's visible frequency range.
    /// When false the waterfall always shows the full captured band and marks
    /// the spectrum view with boundary lines.
    pub waterfall_follow_spectrum: bool,
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
            waterfall_follow_spectrum: true,
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

    /// Clamp hand-edited, stale, or non-finite values into ranges the UI and
    /// engine can handle.
    pub fn sanitize(&mut self) {
        let source_defaults = SourceSettings::default();
        self.source.sample_rate_hz = clamp_or(
            self.source.sample_rate_hz,
            100_000.0,
            61_440_000.0,
            source_defaults.sample_rate_hz,
        );
        self.source.bandwidth_hz = clamp_or(
            self.source.bandwidth_hz,
            100_000.0,
            61_440_000.0,
            source_defaults.bandwidth_hz,
        );
        self.source.center_freq_hz = clamp_or(
            self.source.center_freq_hz,
            0.0,
            6_000_000_000.0,
            source_defaults.center_freq_hz,
        );
        self.source.gain_db = clamp_or(self.source.gain_db, -3.0, 71.0, source_defaults.gain_db);
        self.source.buffer_samples = self.source.buffer_samples.clamp(1_024, 1_048_576);
        if self.source.uri.trim().is_empty() {
            self.source.uri = "usb:".to_string();
        }
        if self.source.rf_port.trim().is_empty() {
            self.source.rf_port = "A_BALANCED".to_string();
        }

        // USB/LSB are reserved but not implemented. Keep them in the config
        // schema and serde surface, but never start a session with one.
        if !self.demod.mode.is_implemented() {
            self.demod.mode = DemodMode::Fm;
        }

        let source_rate_hz = self.source.sample_rate_hz;
        sanitize_params(
            &mut self.demod.fm,
            &DemodParams::fm(),
            source_rate_hz,
            false,
        );
        sanitize_params(
            &mut self.demod.am,
            &DemodParams::am(),
            source_rate_hz,
            false,
        );
        sanitize_params(
            &mut self.demod.cw,
            &DemodParams::cw(),
            source_rate_hz,
            false,
        );
        sanitize_params(
            &mut self.demod.sstv,
            &DemodParams::sstv(),
            source_rate_hz,
            false,
        );
        sanitize_params(
            &mut self.demod.bpsk,
            &DemodParams::bpsk(),
            source_rate_hz,
            true,
        );

        let ui_defaults = UiSettings::default();
        self.ui.spectrum_ratio = clamp_or_f32(
            self.ui.spectrum_ratio,
            0.2,
            0.85,
            ui_defaults.spectrum_ratio,
        );
        self.ui.left_panel_width = clamp_or_f32(
            self.ui.left_panel_width,
            200.0,
            500.0,
            ui_defaults.left_panel_width,
        );
        self.ui.right_panel_width = clamp_or_f32(
            self.ui.right_panel_width,
            220.0,
            560.0,
            ui_defaults.right_panel_width,
        );
        self.ui.decoder_dock_height = clamp_or_f32(
            self.ui.decoder_dock_height,
            80.0,
            500.0,
            ui_defaults.decoder_dock_height,
        );
        self.ui.fft_size = self
            .ui
            .fft_size
            .clamp(256, 65_536)
            .next_power_of_two()
            .min(65_536);
        self.ui.waterfall_rows = self.ui.waterfall_rows.clamp(50, 1_000);
        self.ui.waterfall_width = self.ui.waterfall_width.clamp(256, 4_096);
        self.ui.db_min = clamp_or_f32(self.ui.db_min, -200.0, -10.0, ui_defaults.db_min);
        self.ui.db_max = clamp_or_f32(self.ui.db_max, -100.0, 0.0, ui_defaults.db_max);
        if self.ui.db_max <= self.ui.db_min + 10.0 {
            self.ui.db_min = ui_defaults.db_min;
            self.ui.db_max = ui_defaults.db_max;
        }

        self.satellite.min_elevation_deg = clamp_or(
            self.satellite.min_elevation_deg,
            0.0,
            90.0,
            SatelliteSettings::default().min_elevation_deg,
        );
    }
}

/// Clamp a value, substituting `default` when it is NaN or infinite.
fn clamp_or(value: f64, min: f64, max: f64, default: f64) -> f64 {
    let value = if value.is_finite() { value } else { default };
    value.clamp(min, max)
}

/// `clamp_or` for UI values.
fn clamp_or_f32(value: f32, min: f32, max: f32, default: f32) -> f32 {
    let value = if value.is_finite() { value } else { default };
    value.clamp(min, max)
}

fn sanitize_params(
    params: &mut DemodParams,
    defaults: &DemodParams,
    source_rate_hz: f64,
    data_channel: bool,
) {
    params.bandwidth_hz = clamp_or(
        params.bandwidth_hz,
        100.0,
        2_000_000.0,
        defaults.bandwidth_hz,
    );
    params.channel_rate_hz = clamp_or(
        params.channel_rate_hz,
        8_000.0,
        61_440_000.0,
        defaults.channel_rate_hz,
    );
    params.audio_rate_hz = clamp_or(
        params.audio_rate_hz,
        8_000.0,
        192_000.0,
        defaults.audio_rate_hz,
    );

    // A channel or filter cannot be wider than the captured stream.
    params.bandwidth_hz = params.bandwidth_hz.min(source_rate_hz);
    params.channel_rate_hz = params.channel_rate_hz.min(source_rate_hz);

    // Audio modes must leave room for the resampler. Data modes use this field
    // for the complex output bandwidth, which is bounded by the channel rate.
    let max_bandwidth_hz = if data_channel {
        params.channel_rate_hz
    } else {
        params.audio_rate_hz * 0.45
    };
    params.audio_bandwidth_hz = clamp_or(
        params.audio_bandwidth_hz,
        100.0,
        max_bandwidth_hz,
        defaults.audio_bandwidth_hz,
    );
    params.deemphasis_us = clamp_or(params.deemphasis_us, 0.0, 500.0, defaults.deemphasis_us);
    params.bfo_hz = clamp_or(params.bfo_hz, 100.0, 5_000.0, defaults.bfo_hz);
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
        assert!(config.decoder.cw_afc);
        assert!(config.ui.waterfall_follow_spectrum);
        assert!(config.satellite.grid.is_empty());
        assert_eq!(config.demod.bpsk.bandwidth_hz, 30_000.0);
        assert_eq!(config.demod.bpsk.audio_rate_hz, 48_000.0);
        assert!(!config.decoder.ssdv_enabled);
        assert_eq!(config.decoder.ssdv_crc_inits, vec!["0xFFFFFFFF"]);
        assert_eq!(config.decoder.ssdv_frame_header_len, 5);
        assert_eq!(config.ui.decoder_tab, DecoderTab::Passes);
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

    #[test]
    fn example_config_loads_and_sanitizes_cleanly() {
        let text = include_str!("../../../config.example.toml");
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.demod.mode, DemodMode::Fm);
        assert_eq!(
            parsed.demod.params(DemodMode::Bpsk).unwrap().bandwidth_hz,
            30_000.0
        );

        let path = temp_path("example");
        fs::write(&path, text).unwrap();
        let loaded = AppConfig::load(&path);
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert_eq!(loaded.config.demod.mode, DemodMode::Fm);
        // BPSK's data-channel bandwidth survives sanitize unchanged.
        assert_eq!(
            loaded.config.demod.bpsk.audio_bandwidth_hz,
            DemodParams::bpsk().audio_bandwidth_hz
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reserved_modes_have_no_params() {
        let settings = DemodSettings::default();
        assert!(settings.params(DemodMode::Usb).is_none());
        assert!(settings.params(DemodMode::Lsb).is_none());
        for mode in DemodMode::implemented() {
            assert!(settings.params(mode).is_some(), "{mode:?}");
        }
    }

    #[test]
    fn reserved_mode_falls_back_to_fm() {
        let mut config = AppConfig::default();
        config.demod.mode = DemodMode::Usb;
        config.sanitize();
        assert_eq!(config.demod.mode, DemodMode::Fm);
    }

    #[test]
    fn sanitize_covers_bpsk() {
        let mut config = AppConfig::default();
        config.demod.bpsk.bandwidth_hz = 9_000_000.0;
        config.demod.bpsk.channel_rate_hz = 9_000_000.0;
        config.demod.bpsk.audio_bandwidth_hz = 100_000_000.0;
        config.demod.bpsk.deemphasis_us = 9_000.0;
        config.demod.bpsk.bfo_hz = 0.0;
        config.sanitize();
        assert_eq!(config.demod.bpsk.bandwidth_hz, 2_000_000.0);
        assert_eq!(
            config.demod.bpsk.channel_rate_hz,
            config.source.sample_rate_hz
        );
        assert_eq!(
            config.demod.bpsk.audio_bandwidth_hz,
            config.demod.bpsk.channel_rate_hz
        );
        assert_eq!(config.demod.bpsk.deemphasis_us, 500.0);
        assert_eq!(config.demod.bpsk.bfo_hz, 100.0);
    }

    #[test]
    fn demod_widths_do_not_exceed_the_source_rate() {
        let mut config = AppConfig::default();
        config.source.sample_rate_hz = 100_000.0;
        config.demod.fm.bandwidth_hz = 200_000.0;
        config.demod.fm.channel_rate_hz = 240_000.0;
        config.sanitize();
        assert_eq!(config.demod.fm.bandwidth_hz, 100_000.0);
        assert_eq!(config.demod.fm.channel_rate_hz, 100_000.0);
    }

    #[test]
    fn fft_size_is_a_power_of_two_in_range() {
        for (input, expected) in [
            (0, 256),
            (1_000, 1_024),
            (4_096, 4_096),
            (5_000, 8_192),
            (70_000, 65_536),
        ] {
            let mut config = AppConfig::default();
            config.ui.fft_size = input;
            config.sanitize();
            assert_eq!(config.ui.fft_size, expected);
            assert!(config.ui.fft_size.is_power_of_two());
        }
    }

    #[test]
    fn non_finite_values_fall_back_to_defaults() {
        let defaults = AppConfig::default();
        let mut config = AppConfig::default();
        config.source.sample_rate_hz = f64::NAN;
        config.source.gain_db = f64::INFINITY;
        config.demod.fm.bandwidth_hz = f64::NAN;
        config.demod.bpsk.audio_bandwidth_hz = f64::NEG_INFINITY;
        config.ui.spectrum_ratio = f32::NAN;
        config.ui.db_min = f32::NEG_INFINITY;
        config.satellite.min_elevation_deg = f64::NAN;
        config.sanitize();

        assert_eq!(config.source.sample_rate_hz, defaults.source.sample_rate_hz);
        assert_eq!(config.source.gain_db, defaults.source.gain_db);
        assert_eq!(config.demod.fm.bandwidth_hz, DemodParams::fm().bandwidth_hz);
        assert_eq!(
            config.demod.bpsk.audio_bandwidth_hz,
            DemodParams::bpsk().audio_bandwidth_hz
        );
        assert_eq!(config.ui.spectrum_ratio, defaults.ui.spectrum_ratio);
        assert_eq!(config.ui.db_min, defaults.ui.db_min);
        assert_eq!(
            config.satellite.min_elevation_deg,
            defaults.satellite.min_elevation_deg
        );
    }
}
