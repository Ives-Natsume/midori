//! Minimal SigMF recording and replay.
//!
//! Only the subset needed by Midori is implemented: `ci16_le` and `cf32_le`
//! IQ data, a single capture segment, and a small `midori:*` extension for the
//! source's full-scale value. The metadata is plain SigMF JSON, so recordings
//! remain usable by other tools.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use midori_core::{Sample, SampleFormat, StreamSpec};
use num_complex::Complex32;
use serde_json::{Value, json};

fn with_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Accept both `capture` and `capture.sigmf` as base paths.
fn normalize_base(base: &Path) -> PathBuf {
    if base.extension().and_then(|ext| ext.to_str()) == Some("sigmf") {
        base.with_extension("")
    } else {
        base.to_path_buf()
    }
}

fn data_path(base: &Path) -> PathBuf {
    with_suffix(base, ".sigmf-data")
}

fn meta_path(base: &Path) -> PathBuf {
    with_suffix(base, ".sigmf-meta")
}

#[derive(Debug, Clone)]
pub struct SigMfInfo {
    pub base: PathBuf,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub datatype: String,
    pub full_scale: f64,
    pub sample_count: Option<u64>,
}

impl SigMfInfo {
    pub fn duration_seconds(&self) -> Option<f64> {
        self.sample_count.map(|n| n as f64 / self.sample_rate_hz)
    }
}

/// Streaming SigMF writer.
pub struct SigMfWriter {
    base: PathBuf,
    data: BufWriter<File>,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    full_scale: f64,
    datatype: &'static str,
    description: String,
    samples_written: u64,
}

/// Normalised float sources (WAV replay, channel baseband) use a full scale of
/// 1.0; integer ADC sources report the ADC's code full scale instead.
fn datatype_for_scale(full_scale: f64) -> &'static str {
    if full_scale <= 1.0 {
        "cf32_le"
    } else {
        "ci16_le"
    }
}

impl SigMfWriter {
    pub fn create(
        base: &Path,
        sample_rate_hz: f64,
        center_freq_hz: f64,
        full_scale: f64,
        description: &str,
    ) -> Result<Self> {
        let base = normalize_base(base);
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let data = File::create(data_path(&base))
            .with_context(|| format!("creating {}", data_path(&base).display()))?;

        let mut writer = Self {
            base: base.to_path_buf(),
            data: BufWriter::new(data),
            sample_rate_hz,
            center_freq_hz,
            full_scale,
            datatype: datatype_for_scale(full_scale),
            description: description.to_string(),
            samples_written: 0,
        };
        writer.write_meta()?;
        Ok(writer)
    }

    fn meta_json(&self) -> Value {
        json!({
            "global": {
                "core:datatype": self.datatype,
                "core:sample_rate": self.sample_rate_hz,
                "core:version": "1.2.0",
                "core:description": self.description,
                "core:recorder": "midori-cli",
                "midori:full_scale": self.full_scale,
                "midori:version": env!("CARGO_PKG_VERSION"),
                "midori:sample_count": self.samples_written,
            },
            "captures": [
                {
                    "core:sample_start": 0,
                    "core:frequency": self.center_freq_hz
                }
            ],
            "annotations": []
        })
    }

    fn write_meta(&mut self) -> Result<()> {
        let path = meta_path(&self.base);
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &self.meta_json())
            .context("serialising SigMF metadata")?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    /// Write normalised complex samples. Sources with a full scale above 1.0
    /// (integer ADC formats) are quantised to `ci16_le`; normalised float
    /// sources are stored losslessly as `cf32_le`.
    pub fn write_block(&mut self, samples: &[Sample]) -> Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 8);
        if self.datatype == "cf32_le" {
            for sample in samples {
                bytes.extend_from_slice(&sample.re.to_le_bytes());
                bytes.extend_from_slice(&sample.im.to_le_bytes());
            }
        } else {
            let scale = self.full_scale;
            for sample in samples {
                let i = (sample.re as f64 * scale)
                    .round()
                    .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                let q = (sample.im as f64 * scale)
                    .round()
                    .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                bytes.extend_from_slice(&i.to_le_bytes());
                bytes.extend_from_slice(&q.to_le_bytes());
            }
        }
        self.data.write_all(&bytes)?;
        self.samples_written += samples.len() as u64;
        Ok(())
    }

    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    pub fn finish(mut self) -> Result<()> {
        self.data.flush()?;
        self.write_meta()?;
        Ok(())
    }
}

impl Drop for SigMfWriter {
    fn drop(&mut self) {
        let _ = self.data.flush();
    }
}

/// Streaming SigMF reader.
pub struct SigMfReader {
    data: BufReader<File>,
    info: SigMfInfo,
    bytes_per_sample: usize,
}

impl SigMfReader {
    pub fn open(base: &Path) -> Result<Self> {
        let base = normalize_base(base);
        let path = meta_path(&base);
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let meta: Value =
            serde_json::from_reader(file).with_context(|| format!("parsing {}", path.display()))?;

        let global = meta
            .get("global")
            .ok_or_else(|| anyhow::anyhow!("SigMF metadata has no 'global' object"))?;
        let datatype = global
            .get("core:datatype")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("SigMF metadata has no core:datatype"))?
            .to_string();
        let sample_rate_hz = global
            .get("core:sample_rate")
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow::anyhow!("SigMF metadata has no core:sample_rate"))?;
        let center_freq_hz = meta
            .get("captures")
            .and_then(Value::as_array)
            .and_then(|captures| captures.first())
            .and_then(|capture| capture.get("core:frequency"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let default_scale = if datatype.starts_with("ci16") {
            32768.0
        } else {
            1.0
        };
        let full_scale = global
            .get("midori:full_scale")
            .and_then(Value::as_f64)
            .unwrap_or(default_scale);
        let sample_count = global.get("midori:sample_count").and_then(Value::as_u64);

        let bytes_per_sample = match datatype.as_str() {
            "ci16_le" => 4,
            "cf32_le" => 8,
            other => bail!("unsupported SigMF datatype '{other}'"),
        };

        let data = File::open(data_path(&base))
            .with_context(|| format!("opening {}", data_path(&base).display()))?;

        Ok(Self {
            data: BufReader::new(data),
            info: SigMfInfo {
                base,
                sample_rate_hz,
                center_freq_hz,
                datatype,
                full_scale,
                sample_count,
            },
            bytes_per_sample,
        })
    }

    pub fn info(&self) -> &SigMfInfo {
        &self.info
    }

    pub fn spec(&self) -> StreamSpec {
        StreamSpec {
            sample_rate_hz: self.info.sample_rate_hz,
            center_freq_hz: self.info.center_freq_hz,
            bandwidth_hz: self.info.sample_rate_hz,
            format: if self.info.datatype == "cf32_le" {
                SampleFormat::Cf32Le
            } else {
                SampleFormat::Ci16Le
            },
            full_scale: self.info.full_scale,
        }
    }

    /// Read up to `max_samples` samples. Returns an empty vector at EOF.
    pub fn read_block(&mut self, max_samples: usize) -> Result<Vec<Sample>> {
        if max_samples == 0 {
            return Ok(Vec::new());
        }
        let mut bytes = vec![0u8; max_samples * self.bytes_per_sample];
        let mut filled = 0;
        while filled < bytes.len() {
            let n = self.data.read(&mut bytes[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        let complete = filled / self.bytes_per_sample;
        bytes.truncate(complete * self.bytes_per_sample);

        let mut samples = Vec::with_capacity(complete);
        match self.info.datatype.as_str() {
            "ci16_le" => {
                let scale = self.info.full_scale as f32;
                for chunk in bytes.chunks_exact(4) {
                    let i = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / scale;
                    let q = i16::from_le_bytes([chunk[2], chunk[3]]) as f32 / scale;
                    samples.push(Complex32::new(i, q));
                }
            }
            "cf32_le" => {
                for chunk in bytes.chunks_exact(8) {
                    let i = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    let q = f32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                    samples.push(Complex32::new(i, q));
                }
            }
            other => bail!("unsupported SigMF datatype '{other}'"),
        }
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("midori-sigmf-{name}-{nanos}"))
    }

    #[test]
    fn roundtrip_ci16() {
        let base = temp_base("roundtrip");
        let input: Vec<Sample> = (0..1024)
            .map(|i| {
                let x = i as f32 / 1024.0;
                Complex32::new(x.sin(), (2.0 * x).cos())
            })
            .collect();

        let mut writer =
            SigMfWriter::create(&base, 2_084_000.0, 120_000_000.0, 2048.0, "test").unwrap();
        writer.write_block(&input).unwrap();
        writer.finish().unwrap();

        let mut reader = SigMfReader::open(&base).unwrap();
        assert_eq!(reader.info().sample_rate_hz, 2_084_000.0);
        assert_eq!(reader.info().center_freq_hz, 120_000_000.0);
        assert_eq!(reader.info().datatype, "ci16_le");
        assert_eq!(reader.info().sample_count, Some(1024));
        assert!((reader.info().duration_seconds().unwrap() - 1024.0 / 2_084_000.0).abs() < 1e-12);
        let output = reader.read_block(2048).unwrap();
        assert_eq!(output.len(), input.len());
        for (a, b) in input.iter().zip(output.iter()) {
            assert!((a.re - b.re).abs() < 1.5 / 2048.0);
            assert!((a.im - b.im).abs() < 1.5 / 2048.0);
        }
        assert!(reader.read_block(2048).unwrap().is_empty());

        let _ = std::fs::remove_file(data_path(&base));
        let _ = std::fs::remove_file(meta_path(&base));
    }

    #[test]
    fn roundtrip_normalised_float() {
        let base = temp_base("roundtrip-cf32");
        let input = vec![
            Complex32::new(0.25, 0.125),
            Complex32::new(-0.5, -0.75),
            Complex32::new(0.001, -0.001),
        ];

        let mut writer = SigMfWriter::create(&base, 48_000.0, 14_074_000.0, 1.0, "test").unwrap();
        writer.write_block(&input).unwrap();
        assert_eq!(writer.samples_written(), 3);
        writer.finish().unwrap();

        let mut reader = SigMfReader::open(&base).unwrap();
        assert_eq!(reader.info().datatype, "cf32_le");
        assert_eq!(reader.info().sample_count, Some(3));
        assert!((reader.info().duration_seconds().unwrap() - 3.0 / 48_000.0).abs() < 1e-12);
        assert_eq!(reader.spec().format, SampleFormat::Cf32Le);

        let output = reader.read_block(16).unwrap();
        assert_eq!(output.len(), input.len());
        for (a, b) in input.iter().zip(output.iter()) {
            assert!((a.re - b.re).abs() < 1e-4, "{a} != {b}");
            assert!((a.im - b.im).abs() < 1e-4, "{a} != {b}");
        }
        assert!(reader.read_block(16).unwrap().is_empty());

        let _ = std::fs::remove_file(data_path(&base));
        let _ = std::fs::remove_file(meta_path(&base));
    }
}
