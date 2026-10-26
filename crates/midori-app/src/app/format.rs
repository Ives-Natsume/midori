//! Presentation and formatting helpers.

use std::path::Path;

use eframe::egui;

use midori_config::{Colormap, Theme};
use midori_core::DemodMode;

pub(crate) fn apply_theme(ctx: &egui::Context, theme: Theme) {
    let mut visuals = match theme {
        Theme::Dark => {
            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = egui::Color32::from_rgb(16, 20, 26);
            visuals.window_fill = egui::Color32::from_rgb(20, 25, 32);
            visuals.extreme_bg_color = egui::Color32::from_rgb(10, 13, 18);
            visuals.faint_bg_color = egui::Color32::from_rgb(24, 30, 38);
            visuals.selection.bg_fill = egui::Color32::from_rgb(30, 120, 180);
            visuals
        }
        Theme::Light => egui::Visuals::light(),
    };
    if theme == Theme::Light {
        visuals.selection.bg_fill = egui::Color32::from_rgb(30, 120, 180);
    }
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    ctx.set_style(style);
}

pub(crate) fn format_bandwidth(hz: f64) -> String {
    if hz >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

pub(crate) fn format_demod(mode: DemodMode) -> &'static str {
    match mode {
        DemodMode::Fm => "FM",
        DemodMode::Am => "AM",
        DemodMode::Cw => "CW",
        DemodMode::Sstv => "SSTV",
        DemodMode::Bpsk => "BPSK",
        DemodMode::BpskIf => "BPSK IF",
        DemodMode::Usb => "USB",
        DemodMode::Lsb => "LSB",
    }
}

/// Extract a centre frequency from names like `recording_437393500Hz.wav`.
pub(crate) fn frequency_from_path(path: &str) -> Option<f64> {
    let stem = Path::new(path).file_stem()?.to_str()?;
    let tail = stem.rsplit('_').next()?;
    let digits = tail.strip_suffix("Hz")?;
    let hz = digits.parse::<f64>().ok()?;
    (hz > 0.0).then_some(hz / 1e6)
}

pub(crate) fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}
pub(crate) fn downsample(input: &[f32], width: usize) -> Vec<f32> {
    if input.len() == width || input.is_empty() {
        return input.to_vec();
    }
    (0..width)
        .map(|index| {
            let start = index * input.len() / width;
            let end = (((index + 1) * input.len()) / width)
                .max(start + 1)
                .min(input.len());
            let sum: f32 = input[start..end].iter().sum();
            sum / (end - start) as f32
        })
        .collect()
}

/// Map a dBFS value to a viridis-like colour.
#[inline]
pub(crate) fn db_to_color(db: f32, min: f32, max: f32, colormap: Colormap) -> egui::Color32 {
    let t = ((db - min) / (max - min)).clamp(0.0, 1.0);
    match colormap {
        Colormap::Grayscale => {
            let value = (t * 255.0) as u8;
            egui::Color32::from_rgb(value, value, value)
        }
        Colormap::Turbo => turbo_color(t),
        Colormap::Viridis => viridis_color(t),
    }
}

pub(crate) fn viridis_color(t: f32) -> egui::Color32 {
    let (r, g, b) = if t < 0.25 {
        let s = t / 0.25;
        (
            lerp(68.0, 59.0, s),
            lerp(1.0, 82.0, s),
            lerp(84.0, 139.0, s),
        )
    } else if t < 0.5 {
        let s = (t - 0.25) / 0.25;
        (
            lerp(59.0, 33.0, s),
            lerp(82.0, 145.0, s),
            lerp(139.0, 140.0, s),
        )
    } else if t < 0.75 {
        let s = (t - 0.5) / 0.25;
        (
            lerp(33.0, 94.0, s),
            lerp(145.0, 201.0, s),
            lerp(140.0, 97.0, s),
        )
    } else {
        let s = (t - 0.75) / 0.25;
        (
            lerp(94.0, 253.0, s),
            lerp(201.0, 231.0, s),
            lerp(97.0, 37.0, s),
        )
    };
    egui::Color32::from_rgb(r as u8, g as u8, b as u8)
}

pub(crate) fn turbo_color(t: f32) -> egui::Color32 {
    const STOPS: [(f32, [f32; 3]); 5] = [
        (0.0, [48.0, 18.0, 59.0]),
        (0.25, [70.0, 107.0, 227.0]),
        (0.5, [62.0, 188.0, 115.0]),
        (0.75, [245.0, 187.0, 68.0]),
        (1.0, [180.0, 4.0, 38.0]),
    ];

    for pair in STOPS.windows(2) {
        let (start, start_color) = pair[0];
        let (end, end_color) = pair[1];
        if t <= end {
            let local = ((t - start) / (end - start)).clamp(0.0, 1.0);
            let red = lerp(start_color[0], end_color[0], local);
            let green = lerp(start_color[1], end_color[1], local);
            let blue = lerp(start_color[2], end_color[2], local);
            return egui::Color32::from_rgb(red as u8, green as u8, blue as u8);
        }
    }
    egui::Color32::from_rgb(180, 4, 38)
}

#[inline]
pub(crate) fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_centre_from_recording_filename() {
        assert_eq!(
            frequency_from_path("data/BOTAN_TLM_CW_437393500Hz.wav"),
            Some(437.3935)
        );
        assert_eq!(frequency_from_path("no-frequency.wav"), None);
    }
}
