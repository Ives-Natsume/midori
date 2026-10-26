//! Satellite pass planning.

use std::path::Path;

use chrono::Utc;
use midori_config::DecoderTab;
use midori_orbit::{GridSquare, Pass, PassPredictor, Tle};

use crate::app::MidoriApp;

impl MidoriApp {
    pub(crate) fn predict_passes(&mut self) {
        let tle = match Tle::from_file(Path::new(&self.tle_path)) {
            Ok(tle) => tle,
            Err(error) => {
                self.pass_status = format!("TLE error: {error}");
                self.push_log(self.pass_status.clone());
                return;
            }
        };
        let grid = match GridSquare::parse(&self.grid_input) {
            Ok(grid) => grid,
            Err(error) => {
                self.pass_status = format!("grid error: {error}");
                self.push_log(self.pass_status.clone());
                return;
            }
        };
        let min_elevation = match self.min_elev_input.parse::<f64>() {
            Ok(value) if (0.0..=90.0).contains(&value) => value,
            _ => {
                self.pass_status = "minimum elevation must be 0..90".to_string();
                return;
            }
        };

        let observer = grid.observer(0.0);
        let predictor = match PassPredictor::new(&tle, observer) {
            Ok(predictor) => predictor,
            Err(error) => {
                self.pass_status = format!("SGP4 error: {error}");
                self.push_log(self.pass_status.clone());
                return;
            }
        };

        let now = Utc::now();
        match predictor.predict(now, now + chrono::Duration::hours(24), min_elevation) {
            Ok(passes) => {
                self.satellite_name = tle
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("NORAD {}", tle.norad_id));
                self.pass_status = format!(
                    "{} passes in the next 24 h | grid {} | min el {:.1}°",
                    passes.len(),
                    self.grid_input.to_uppercase(),
                    min_elevation
                );
                self.passes = passes;
                self.decoder_tab = DecoderTab::Passes;
                self.decoder_dock_open = true;
                self.push_log(format!("predicted {} passes", self.passes.len()));
            }
            Err(error) => {
                self.pass_status = format!("prediction error: {error}");
                self.push_log(self.pass_status.clone());
            }
        }
    }

    pub(crate) fn next_pass(&self) -> Option<&Pass> {
        let now = Utc::now();
        self.passes.iter().find(|pass| pass.los > now)
    }
}
