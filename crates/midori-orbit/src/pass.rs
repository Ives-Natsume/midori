//! SGP4 propagation, observer look angles, and pass prediction.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use sgp4::{Constants, Elements};

use crate::grid::Observer;
use crate::tle::Tle;

const EARTH_RADIUS_KM: f64 = 6378.137;
const EARTH_FLATTENING: f64 = 1.0 / 298.257_223_563;

#[derive(Debug, Clone, Copy)]
pub struct LookAngles {
    pub azimuth_deg: f64,
    pub elevation_deg: f64,
    pub range_km: f64,
    pub range_rate_km_s: f64,
}

#[derive(Debug, Clone)]
pub struct Pass {
    pub aos: DateTime<Utc>,
    pub tca: DateTime<Utc>,
    pub los: DateTime<Utc>,
    pub max_elevation_deg: f64,
    pub aos_azimuth_deg: f64,
    pub los_azimuth_deg: f64,
}

pub struct PassPredictor {
    elements: Elements,
    constants: Constants,
    observer: Observer,
}

impl PassPredictor {
    pub fn new(tle: &Tle, observer: Observer) -> Result<Self> {
        let constants = tle.constants()?;
        Ok(Self {
            elements: tle.elements.clone(),
            constants,
            observer,
        })
    }

    pub fn observer(&self) -> Observer {
        self.observer
    }

    pub fn look_angles(&self, time: DateTime<Utc>) -> Result<LookAngles> {
        let sat = self.position_ecef(time)?;
        let obs = observer_ecef(self.observer);
        let (azimuth, elevation, range) =
            look(obs, sat, self.observer.lat_rad(), self.observer.lon_rad());

        let dt = Duration::milliseconds(500);
        let range_before = self.range_km(time - dt, obs)?;
        let range_after = self.range_km(time + dt, obs)?;
        let range_rate = (range_after - range_before) / 1.0;

        Ok(LookAngles {
            azimuth_deg: azimuth,
            elevation_deg: elevation,
            range_km: range,
            range_rate_km_s: range_rate,
        })
    }

    /// Predict passes in `[start, end]` above `min_elevation_deg`.
    pub fn predict(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        min_elevation_deg: f64,
    ) -> Result<Vec<Pass>> {
        if end <= start {
            bail!("pass prediction end must be after start");
        }
        let step = Duration::seconds(20);
        let mut passes = Vec::new();
        let mut time = start;
        let mut previous_time = start;
        let mut current: Option<PassState> = None;

        if self.elevation_deg(start)? >= min_elevation_deg {
            current = Some(PassState {
                aos: start,
                max_elevation: self.elevation_deg(start)?,
                max_time: start,
            });
        }

        while time < end {
            time += step;
            let elevation = self.elevation_deg(time)?;

            if elevation >= min_elevation_deg {
                if current.is_none() {
                    let aos = self.refine_crossing(previous_time, time, min_elevation_deg, true)?;
                    current = Some(PassState {
                        aos,
                        max_elevation: elevation,
                        max_time: time,
                    });
                }
                if let Some(state) = &mut current
                    && elevation > state.max_elevation
                {
                    state.max_elevation = elevation;
                    state.max_time = time;
                }
            } else if let Some(state) = current.take() {
                let los = self.refine_crossing(previous_time, time, min_elevation_deg, false)?;
                let tca = self.refine_tca(state.max_time)?;
                let max_elevation = self.elevation_deg(tca)?;
                let aos_az = self.look_angles(state.aos)?.azimuth_deg;
                let los_az = self.look_angles(los)?.azimuth_deg;
                passes.push(Pass {
                    aos: state.aos,
                    tca,
                    los,
                    max_elevation_deg: max_elevation,
                    aos_azimuth_deg: aos_az,
                    los_azimuth_deg: los_az,
                });
            }

            previous_time = time;
        }

        // Close a pass still in progress at the end of the window.
        if let Some(state) = current {
            let tca = self.refine_tca(state.max_time)?;
            let max_elevation = self.elevation_deg(tca)?;
            let aos_az = self.look_angles(state.aos)?.azimuth_deg;
            let los_az = self.look_angles(end)?.azimuth_deg;
            passes.push(Pass {
                aos: state.aos,
                tca,
                los: end,
                max_elevation_deg: max_elevation,
                aos_azimuth_deg: aos_az,
                los_azimuth_deg: los_az,
            });
        }

        Ok(passes)
    }

    fn elevation_deg(&self, time: DateTime<Utc>) -> Result<f64> {
        Ok(self.look_angles(time)?.elevation_deg)
    }

    fn position_ecef(&self, time: DateTime<Utc>) -> Result<[f64; 3]> {
        let minutes = self
            .elements
            .datetime_to_minutes_since_epoch(&time.naive_utc())
            .context("converting UTC to minutes since TLE epoch")?;
        let prediction = self
            .constants
            .propagate(minutes)
            .context("SGP4 propagation failed")?;
        Ok(teme_to_ecef(prediction.position, time))
    }

    fn range_km(&self, time: DateTime<Utc>, observer: [f64; 3]) -> Result<f64> {
        let sat = self.position_ecef(time)?;
        let dx = sat[0] - observer[0];
        let dy = sat[1] - observer[1];
        let dz = sat[2] - observer[2];
        Ok((dx * dx + dy * dy + dz * dz).sqrt())
    }

    fn refine_crossing(
        &self,
        before: DateTime<Utc>,
        after: DateTime<Utc>,
        threshold_deg: f64,
        rising: bool,
    ) -> Result<DateTime<Utc>> {
        let mut low = before;
        let mut high = after;
        for _ in 0..20 {
            let mid = low + (high - low) / 2;
            let elevation = self.elevation_deg(mid)?;
            let above = elevation >= threshold_deg;
            if above == rising {
                high = mid;
            } else {
                low = mid;
            }
        }
        Ok(low + (high - low) / 2)
    }

    fn refine_tca(&self, around: DateTime<Utc>) -> Result<DateTime<Utc>> {
        let mut best = around;
        let mut best_elevation = self.elevation_deg(around)?;
        for seconds in -30..=30 {
            let time = around + Duration::seconds(seconds);
            let elevation = self.elevation_deg(time)?;
            if elevation > best_elevation {
                best_elevation = elevation;
                best = time;
            }
        }
        Ok(best)
    }
}

struct PassState {
    aos: DateTime<Utc>,
    max_elevation: f64,
    max_time: DateTime<Utc>,
}

fn observer_ecef(observer: Observer) -> [f64; 3] {
    let lat = observer.lat_rad();
    let lon = observer.lon_rad();
    let altitude_km = observer.altitude_m / 1000.0;
    let e2 = EARTH_FLATTENING * (2.0 - EARTH_FLATTENING);
    let n = EARTH_RADIUS_KM / (1.0 - e2 * lat.sin().powi(2)).sqrt();
    [
        (n + altitude_km) * lat.cos() * lon.cos(),
        (n + altitude_km) * lat.cos() * lon.sin(),
        (n * (1.0 - e2) + altitude_km) * lat.sin(),
    ]
}

fn teme_to_ecef(position: [f64; 3], time: DateTime<Utc>) -> [f64; 3] {
    let theta = gmst_rad(time);
    let (sin, cos) = theta.sin_cos();
    [
        position[0] * cos + position[1] * sin,
        -position[0] * sin + position[1] * cos,
        position[2],
    ]
}

fn gmst_rad(time: DateTime<Utc>) -> f64 {
    let seconds = time.timestamp() as f64 + time.timestamp_subsec_nanos() as f64 / 1e9;
    let jd = seconds / 86_400.0 + 2_440_587.5;
    let t = (jd - 2_451_545.0) / 36_525.0;
    let degrees = 280.460_618_37 + 360.985_647_366_29 * (jd - 2_451_545.0) + 0.000_387_933 * t * t
        - t * t * t / 38_710_000.0;
    degrees.rem_euclid(360.0).to_radians()
}

fn look(observer: [f64; 3], satellite: [f64; 3], lat: f64, lon: f64) -> (f64, f64, f64) {
    let rx = satellite[0] - observer[0];
    let ry = satellite[1] - observer[1];
    let rz = satellite[2] - observer[2];
    let range = (rx * rx + ry * ry + rz * rz).sqrt();

    let (lon_sin, lon_cos) = lon.sin_cos();
    let (lat_sin, lat_cos) = lat.sin_cos();
    let east = [-lon_sin, lon_cos, 0.0];
    let north = [-lat_sin * lon_cos, -lat_sin * lon_sin, lat_cos];
    let up = [lat_cos * lon_cos, lat_cos * lon_sin, lat_sin];

    let east_dot = rx * east[0] + ry * east[1] + rz * east[2];
    let north_dot = rx * north[0] + ry * north[1] + rz * north[2];
    let up_dot = rx * up[0] + ry * up[1] + rz * up[2];

    let azimuth = east_dot.atan2(north_dot).to_degrees().rem_euclid(360.0);
    let elevation = (up_dot / range).clamp(-1.0, 1.0).asin().to_degrees();
    (azimuth, elevation, range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::GridSquare;

    const ISS: &str = "\
ISS (ZARYA)
1 25544U 98067A   26291.14461894  .00007503  00000-0  14456-3 0  9995
2 25544  51.6328  51.6259 0006780 277.7856  82.2363 15.48918166590814
";

    #[test]
    fn look_angles_are_plausible() {
        let tle = Tle::parse(ISS).unwrap();
        let observer = GridSquare::parse("PM95").unwrap().observer(0.0);
        let predictor = PassPredictor::new(&tle, observer).unwrap();
        let angles = predictor.look_angles(tle.epoch).unwrap();
        assert!((-90.0..=90.0).contains(&angles.elevation_deg));
        assert!((0.0..360.0).contains(&angles.azimuth_deg));
        assert!(angles.range_km > 300.0 && angles.range_km < 20_000.0);
    }

    #[test]
    fn predicts_iss_passes_from_tokyo() {
        let tle = Tle::parse(ISS).unwrap();
        let observer = GridSquare::parse("PM95").unwrap().observer(0.0);
        let predictor = PassPredictor::new(&tle, observer).unwrap();
        let passes = predictor
            .predict(tle.epoch, tle.epoch + Duration::hours(24), 0.0)
            .unwrap();
        assert!(!passes.is_empty(), "expected at least one ISS pass");
        for pass in &passes {
            assert!(pass.aos <= pass.tca && pass.tca <= pass.los);
            assert!(pass.max_elevation_deg >= 0.0);
            assert!((0.0..360.0).contains(&pass.aos_azimuth_deg));
            assert!((0.0..360.0).contains(&pass.los_azimuth_deg));
        }
    }
}
