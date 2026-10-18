//! Doppler correction from SGP4 range rate.

use chrono::{DateTime, TimeZone, Utc};
use midori_core::DopplerSource;

use crate::pass::PassPredictor;

const C_KM_S: f64 = 299_792.458;

pub struct Sgp4Doppler {
    predictor: PassPredictor,
    downlink_hz: f64,
}

impl Sgp4Doppler {
    pub fn new(predictor: PassPredictor, downlink_hz: f64) -> Self {
        Self {
            predictor,
            downlink_hz,
        }
    }

    pub fn offset_at(&self, time: DateTime<Utc>) -> anyhow::Result<f64> {
        let angles = self.predictor.look_angles(time)?;
        // Positive range rate means receding, so the observed frequency is
        // lower than nominal.
        Ok(-self.downlink_hz * angles.range_rate_km_s / C_KM_S)
    }

    pub fn predictor(&self) -> &PassPredictor {
        &self.predictor
    }

    /// Change the downlink used to scale the range rate. Called when the user
    /// retunes the receiver while tracking.
    pub fn set_downlink_hz(&mut self, downlink_hz: f64) {
        self.downlink_hz = downlink_hz;
    }
}

impl DopplerSource for Sgp4Doppler {
    fn offset_hz(&self, utc_ns: u64) -> f64 {
        let Some(time) = utc_from_ns(utc_ns) else {
            return 0.0;
        };
        self.offset_at(time).unwrap_or(0.0)
    }

    fn rate_hz_s(&self, utc_ns: u64) -> f64 {
        let Some(time) = utc_from_ns(utc_ns) else {
            return 0.0;
        };
        let before = time - chrono::Duration::milliseconds(500);
        let after = time + chrono::Duration::milliseconds(500);
        let before = self.offset_at(before).unwrap_or(0.0);
        let after = self.offset_at(after).unwrap_or(0.0);
        after - before
    }

    fn set_nominal_hz(&mut self, hz: f64) {
        self.set_downlink_hz(hz);
    }
}

fn utc_from_ns(utc_ns: u64) -> Option<DateTime<Utc>> {
    let seconds = (utc_ns / 1_000_000_000) as i64;
    let nanos = (utc_ns % 1_000_000_000) as u32;
    Utc.timestamp_opt(seconds, nanos).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::GridSquare;
    use crate::tle::Tle;

    const ISS: &str = "\
ISS (ZARYA)
1 25544U 98067A   26291.14461894  .00007503  00000-0  14456-3 0  9995
2 25544  51.6328  51.6259 0006780 277.7856  82.2363 15.48918166590814
";

    #[test]
    fn doppler_is_bounded_for_iss() {
        let tle = Tle::parse(ISS).unwrap();
        let observer = GridSquare::parse("PM95").unwrap().observer(0.0);
        let predictor = PassPredictor::new(&tle, observer).unwrap();
        let doppler = Sgp4Doppler::new(predictor, 145_800_000.0);
        let offset = doppler.offset_at(tle.epoch).unwrap();
        assert!(offset.abs() < 10_000.0, "offset was {offset} Hz");
        let ns = tle.epoch.timestamp_nanos_opt().unwrap() as u64;
        let from_ns = DopplerSource::offset_hz(&doppler, ns);
        assert!((from_ns - offset).abs() < 1.0);
    }
}
