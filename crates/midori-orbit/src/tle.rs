//! TLE parsing with checksum and epoch validation.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sgp4::Elements;

#[derive(Debug, Clone)]
pub struct Tle {
    pub name: Option<String>,
    pub norad_id: u32,
    pub line1: String,
    pub line2: String,
    pub epoch: DateTime<Utc>,
    pub elements: Elements,
}

impl Tle {
    /// Parse a 2-line or 3-line TLE. The name line is optional.
    pub fn parse(text: &str) -> Result<Self> {
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();

        let (name, line1, line2) = match lines.as_slice() {
            [line1, line2] if line1.starts_with('1') && line2.starts_with('2') => {
                (None, *line1, *line2)
            }
            [name, line1, line2] if line1.starts_with('1') && line2.starts_with('2') => {
                (Some((*name).to_string()), *line1, *line2)
            }
            _ => bail!("expected a two- or three-line TLE"),
        };

        validate_line(line1, 1)?;
        validate_line(line2, 2)?;

        let norad1 = line1[2..7]
            .trim()
            .parse::<u32>()
            .context("parsing line 1 NORAD ID")?;
        let norad2 = line2[2..7]
            .trim()
            .parse::<u32>()
            .context("parsing line 2 NORAD ID")?;
        if norad1 != norad2 {
            bail!("TLE NORAD IDs differ ({norad1} != {norad2})");
        }

        let epoch = parse_epoch(line1)?;
        let elements = Elements::from_tle(name.clone(), line1.as_bytes(), line2.as_bytes())
            .context("SGP4 rejected the TLE")?;

        Ok(Self {
            name,
            norad_id: norad1,
            line1: line1.to_string(),
            line2: line2.to_string(),
            epoch,
            elements,
        })
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn constants(&self) -> Result<sgp4::Constants> {
        sgp4::Constants::from_elements(&self.elements).context("building SGP4 constants")
    }
}

fn validate_line(line: &str, expected_number: u8) -> Result<()> {
    let bytes = line.as_bytes();
    if bytes.len() != 69 {
        bail!(
            "TLE line {expected_number} has length {}, expected 69",
            bytes.len()
        );
    }
    if bytes[0] != b'0' + expected_number {
        bail!("TLE line {expected_number} does not start with its line number");
    }
    let expected = checksum(line);
    let actual = bytes[68];
    if actual != b'0' + expected {
        bail!(
            "TLE line {expected_number} checksum mismatch (got {}, expected {expected})",
            actual as char
        );
    }
    Ok(())
}

fn checksum(line: &str) -> u8 {
    let mut sum = 0u32;
    for byte in line.bytes().take(68) {
        if byte.is_ascii_digit() {
            sum += (byte - b'0') as u32;
        } else if byte == b'-' {
            sum += 1;
        }
    }
    (sum % 10) as u8
}

fn parse_epoch(line1: &str) -> Result<DateTime<Utc>> {
    if line1.len() < 32 {
        bail!("TLE line 1 is too short for an epoch");
    }
    let field = &line1[18..32];
    let year2: i32 = field[0..2].parse().context("parsing TLE epoch year")?;
    let day: u32 = field[2..5].parse().context("parsing TLE epoch day")?;
    let fraction: f64 = format!("0{}", &field[5..])
        .parse()
        .context("parsing TLE epoch fraction")?;

    let year = if year2 >= 57 {
        1900 + year2
    } else {
        2000 + year2
    };
    let date = NaiveDate::from_yo_opt(year, day)
        .ok_or_else(|| anyhow::anyhow!("invalid TLE epoch day {day} for {year}"))?;
    let seconds = fraction * 86_400.0;
    let whole_seconds = seconds.floor() as i64;
    let nanos = ((seconds - whole_seconds as f64) * 1e9) as i64;
    let naive: NaiveDateTime = NaiveDateTime::new(date, NaiveTime::MIN)
        + Duration::seconds(whole_seconds)
        + Duration::nanoseconds(nanos);
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ISS, fetched from ARISS on 2026-10-18.
    const ISS: &str = "\
ISS (ZARYA)
1 25544U 98067A   26291.14461894  .00007503  00000-0  14456-3 0  9995
2 25544  51.6328  51.6259 0006780 277.7856  82.2363 15.48918166590814
";

    #[test]
    fn parses_iss_tle() {
        let tle = Tle::parse(ISS).unwrap();
        assert_eq!(tle.name.as_deref(), Some("ISS (ZARYA)"));
        assert_eq!(tle.norad_id, 25544);
        assert!((tle.elements.inclination - 51.6328).abs() < 1e-6);
        assert_eq!(tle.epoch.date_naive().to_string(), "2026-10-18");
        assert_eq!(tle.epoch.format("%H:%M:%S").to_string(), "03:28:15");
        assert!((tle.epoch.timestamp_subsec_millis() as i64 - 76).abs() <= 1);
    }

    #[test]
    fn rejects_bad_checksum() {
        let broken = ISS.replace("9995", "9996");
        assert!(Tle::parse(&broken).is_err());
    }
}
