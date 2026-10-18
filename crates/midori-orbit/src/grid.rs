//! Maidenhead grid locators and observer coordinates.

use anyhow::{Result, bail};

/// Centre of a Maidenhead locator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridSquare {
    pub lat_deg: f64,
    pub lon_deg: f64,
}

impl GridSquare {
    /// Parse 4, 6, or 8 character Maidenhead locators (case-insensitive).
    pub fn parse(locator: &str) -> Result<Self> {
        let locator = locator.trim();
        if ![4, 6, 8].contains(&locator.len()) {
            bail!("Maidenhead locator must be 4, 6, or 8 characters");
        }
        if !locator.is_ascii() {
            bail!("Maidenhead locator must be ASCII");
        }

        let bytes = locator.as_bytes();
        let mut lon = 0.0;
        let mut lat = 0.0;

        // Field: 20° x 10°
        let lon_field = field_index(bytes[0])?;
        let lat_field = field_index(bytes[1])?;
        lon += lon_field as f64 * 20.0 - 180.0;
        lat += lat_field as f64 * 10.0 - 90.0;

        // Square: 2° x 1°
        let lon_square = digit_index(bytes[2])?;
        let lat_square = digit_index(bytes[3])?;
        lon += lon_square as f64 * 2.0;
        lat += lat_square as f64 * 1.0;

        if locator.len() >= 6 {
            // Subsquare: 5' x 2.5'
            let lon_sub = subsquare_index(bytes[4])?;
            let lat_sub = subsquare_index(bytes[5])?;
            lon += lon_sub as f64 * (1.0 / 12.0);
            lat += lat_sub as f64 * (1.0 / 24.0);
        }

        if locator.len() >= 8 {
            // Extended square: 0.5' x 0.25'
            let lon_ext = digit_index(bytes[6])?;
            let lat_ext = digit_index(bytes[7])?;
            lon += lon_ext as f64 * (1.0 / 120.0);
            lat += lat_ext as f64 * (1.0 / 240.0);
        }

        // Centre of the final cell.
        let (lon_width, lat_width) = match locator.len() {
            4 => (2.0, 1.0),
            6 => (1.0 / 12.0, 1.0 / 24.0),
            _ => (1.0 / 120.0, 1.0 / 240.0),
        };
        Ok(Self {
            lat_deg: lat + lat_width / 2.0,
            lon_deg: lon + lon_width / 2.0,
        })
    }

    pub fn observer(&self, altitude_m: f64) -> Observer {
        Observer {
            lat_deg: self.lat_deg,
            lon_deg: self.lon_deg,
            altitude_m,
        }
    }
}

/// Ground station coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observer {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub altitude_m: f64,
}

impl Observer {
    pub fn new(lat_deg: f64, lon_deg: f64, altitude_m: f64) -> Result<Self> {
        if !(-90.0..=90.0).contains(&lat_deg) {
            bail!("latitude must be in [-90, 90]");
        }
        if !(-180.0..=180.0).contains(&lon_deg) {
            bail!("longitude must be in [-180, 180]");
        }
        Ok(Self {
            lat_deg,
            lon_deg,
            altitude_m,
        })
    }

    pub fn from_grid(grid: &GridSquare, altitude_m: f64) -> Self {
        grid.observer(altitude_m)
    }

    pub fn lat_rad(&self) -> f64 {
        self.lat_deg.to_radians()
    }

    pub fn lon_rad(&self) -> f64 {
        self.lon_deg.to_radians()
    }
}

fn field_index(byte: u8) -> Result<u8> {
    let byte = byte.to_ascii_uppercase();
    if (b'A'..=b'R').contains(&byte) {
        Ok(byte - b'A')
    } else {
        bail!("invalid Maidenhead field character '{}'", byte as char);
    }
}

fn digit_index(byte: u8) -> Result<u8> {
    if byte.is_ascii_digit() {
        Ok(byte - b'0')
    } else {
        bail!("invalid Maidenhead digit '{}'", byte as char);
    }
}

fn subsquare_index(byte: u8) -> Result<u8> {
    let byte = byte.to_ascii_uppercase();
    if (b'A'..=b'X').contains(&byte) {
        Ok(byte - b'A')
    } else {
        bail!("invalid Maidenhead subsquare character '{}'", byte as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tokyo_grid() {
        let grid = GridSquare::parse("PM95").unwrap();
        assert!((grid.lat_deg - 35.5).abs() < 1e-6);
        assert!((grid.lon_deg - 139.0).abs() < 1e-6);
    }

    #[test]
    fn parses_six_char_grid() {
        let grid = GridSquare::parse("PM95xr").unwrap();
        assert!((grid.lat_deg - 35.729_166_67).abs() < 1e-6);
        assert!((grid.lon_deg - 139.958_333_33).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_locator() {
        assert!(GridSquare::parse("PM95k").is_err());
        assert!(GridSquare::parse("ZZ99zz").is_err());
    }
}
