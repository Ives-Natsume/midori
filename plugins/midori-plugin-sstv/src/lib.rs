//! SSTV decoding session used by the `midori-plugin-sstv` sidecar.
//!
//! The heavy lifting is done by [`slowrx`], a pure-Rust port of slowrx. This
//! module only adapts its events to Midori's sidecar protocol: progress,
//! status, and a PNG image at completion.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use slowrx::{SstvDecoder, SstvEvent, SstvImage, SstvMode};

/// A completed image ready for the UI or disk.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub mode: SstvMode,
    pub mode_name: String,
    pub short_name: String,
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
    pub path: Option<PathBuf>,
}

/// Adapter event produced while processing audio.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    VisDetected {
        mode: SstvMode,
        mode_name: String,
        header_shift_hz: f64,
    },
    UnknownVis {
        code: u8,
        header_shift_hz: f64,
    },
    Progress {
        mode: SstvMode,
        mode_name: String,
        line: u32,
        total_lines: u32,
    },
    Image(DecodedImage),
}

/// Streaming SSTV decoder at a caller-provided audio sample rate.
pub struct SstvSession {
    decoder: SstvDecoder,
    sample_rate_hz: u32,
    output_dir: Option<PathBuf>,
    image_seq: u64,
}

impl SstvSession {
    pub fn new(sample_rate_hz: u32) -> Result<Self> {
        Ok(Self {
            decoder: SstvDecoder::new(sample_rate_hz)
                .with_context(|| format!("creating SSTV decoder at {sample_rate_hz} Hz"))?,
            sample_rate_hz,
            output_dir: None,
            image_seq: 0,
        })
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Directory where completed images are written. `None` disables saving.
    pub fn set_output_dir(&mut self, dir: Option<PathBuf>) {
        self.output_dir = dir;
    }

    /// Rebuild the decoder after a sample-rate change. Acquisition state is
    /// intentionally discarded because the audio timeline changed.
    pub fn set_sample_rate(&mut self, sample_rate_hz: u32) -> Result<()> {
        if sample_rate_hz == self.sample_rate_hz {
            return Ok(());
        }
        self.decoder = SstvDecoder::new(sample_rate_hz)
            .with_context(|| format!("creating SSTV decoder at {sample_rate_hz} Hz"))?;
        self.sample_rate_hz = sample_rate_hz;
        Ok(())
    }

    /// Feed mono audio. Completed images are encoded as PNG.
    pub fn process(&mut self, samples: &[f32]) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        for event in self.decoder.process(samples) {
            match event {
                SstvEvent::VisDetected {
                    mode,
                    hedr_shift_hz,
                    ..
                } => events.push(SessionEvent::VisDetected {
                    mode,
                    mode_name: mode_name(mode),
                    header_shift_hz: hedr_shift_hz,
                }),
                SstvEvent::UnknownVis {
                    code,
                    hedr_shift_hz: header_shift_hz,
                    ..
                } => events.push(SessionEvent::UnknownVis {
                    code,
                    header_shift_hz,
                }),
                SstvEvent::LineDecoded {
                    mode, line_index, ..
                } => {
                    // Roughly four updates per second on a PD-120 image,
                    // without flooding the sidecar JSON pipe.
                    if line_index % 8 == 0 || line_index + 1 == image_lines(mode) {
                        events.push(SessionEvent::Progress {
                            mode,
                            mode_name: mode_name(mode),
                            line: line_index + 1,
                            total_lines: image_lines(mode),
                        });
                    }
                }
                SstvEvent::ImageComplete { image, .. } => {
                    if let Some(decoded) = self.finish_image(image) {
                        events.push(SessionEvent::Image(decoded));
                    }
                }
                _ => {}
            }
        }
        events
    }

    fn finish_image(&mut self, image: SstvImage) -> Option<DecodedImage> {
        let png = match encode_png(&image) {
            Ok(png) => png,
            Err(error) => {
                eprintln!("[sstv] PNG encode failed: {error}");
                return None;
            }
        };

        self.image_seq += 1;
        let mode_name = mode_name(image.mode);
        let short_name = short_name(image.mode);
        let path = self.output_dir.as_ref().and_then(|dir| {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let path = dir.join(format!(
                "sstv_{stamp}_{:03}_{short_name}.png",
                self.image_seq
            ));
            match write_image(&path, &png) {
                Ok(()) => Some(path),
                Err(error) => {
                    eprintln!("[sstv] could not write {}: {error}", path.display());
                    None
                }
            }
        });

        Some(DecodedImage {
            mode: image.mode,
            mode_name,
            short_name,
            width: image.width,
            height: image.height,
            png,
            path,
        })
    }
}

pub fn mode_name(mode: SstvMode) -> String {
    slowrx::modespec::for_mode(mode).name.to_string()
}

pub fn short_name(mode: SstvMode) -> String {
    slowrx::modespec::for_mode(mode).short_name.to_string()
}

pub fn image_lines(mode: SstvMode) -> u32 {
    slowrx::modespec::for_mode(mode).image_lines
}

/// Encode an RGB image as a PNG byte stream.
pub fn encode_png(image: &SstvImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("writing PNG header")?;
        let pixels: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        writer
            .write_image_data(&pixels)
            .context("writing PNG pixels")?;
    }
    Ok(bytes)
}

fn write_image(path: &Path, png: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, png).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use rasterwave::{EncodeOptions, Rgb, RgbImage, SstvEncoder, SstvMode as EncoderMode};

    use super::*;

    fn test_image(width: u32, height: u32) -> RgbImage {
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.push(Rgb::new(
                    (x * 255 / width.max(1)) as u8,
                    (y * 255 / height.max(1)) as u8,
                    ((x + y) * 127 / (width + height).max(1)) as u8,
                ));
            }
        }
        RgbImage::new(width, height, pixels).unwrap()
    }

    fn encode(mode: EncoderMode, sample_rate: u32) -> Vec<f32> {
        let spec = mode.spec();
        let image = test_image(spec.width, spec.height);
        let mut encoder =
            SstvEncoder::new(image, mode, sample_rate, EncodeOptions::default()).unwrap();
        let mut audio = Vec::new();
        let mut buffer = vec![0.0f32; 4096];
        while !encoder.is_finished() {
            let written = encoder.read_samples(&mut buffer);
            audio.extend_from_slice(&buffer[..written]);
        }
        audio
    }

    fn decode(
        audio: &[f32],
        sample_rate: u32,
        chunk: usize,
    ) -> (Vec<SessionEvent>, Option<DecodedImage>) {
        let mut session = SstvSession::new(sample_rate).unwrap();
        let mut events = Vec::new();
        let mut image = None;
        for block in audio.chunks(chunk) {
            for event in session.process(block) {
                if let SessionEvent::Image(decoded) = &event {
                    image = Some(decoded.clone());
                }
                events.push(event);
            }
        }
        (events, image)
    }

    #[test]
    fn detects_pd120_vis() {
        let audio = encode(EncoderMode::Pd120, 48_000);
        let mut session = SstvSession::new(48_000).unwrap();
        let mut modes = Vec::new();
        for block in audio.chunks(4_800).take(40) {
            for event in session.process(block) {
                if let SessionEvent::VisDetected { mode, .. } = event {
                    modes.push(mode);
                }
            }
        }
        assert_eq!(modes, vec![SstvMode::Pd120]);
    }

    #[test]
    fn detects_robot36_vis() {
        let audio = encode(EncoderMode::Robot36, 48_000);
        let mut session = SstvSession::new(48_000).unwrap();
        let mut modes = Vec::new();
        for block in audio.chunks(4_800).take(40) {
            for event in session.process(block) {
                if let SessionEvent::VisDetected { mode, .. } = event {
                    modes.push(mode);
                }
            }
        }
        assert_eq!(modes, vec![SstvMode::Robot36]);
    }

    #[test]
    fn encodes_png_with_mode_dimensions() {
        let mut image = SstvImage::new(SstvMode::Robot36, 2, 2);
        image.put_pixel(0, 0, [255, 0, 0]);
        image.put_pixel(1, 1, [0, 255, 0]);
        let png = encode_png(&image).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert_eq!(mode_name(SstvMode::Robot36), "Robot 36");
        assert_eq!(short_name(SstvMode::Robot36), "robot36");
        assert_eq!(image_lines(SstvMode::Robot36), 240);
    }

    // Full image decode is CPU-heavy in debug builds (slowrx runs a per-pixel
    // FFT). Keep these ignored for the default workspace test run and execute
    // them with:
    //
    //   cargo test -p midori-plugin-sstv --release -- --ignored
    //
    #[test]
    #[ignore = "full SSTV decode is slow in debug; run with --release -- --ignored"]
    fn decodes_pd120_at_48khz() {
        let audio = encode(EncoderMode::Pd120, 48_000);
        let (events, image) = decode(&audio, 48_000, 4800);
        let image = image.expect("PD120 image");
        assert_eq!((image.width, image.height), (640, 496));
        assert!(image.mode_name.contains("PD-120"));
        assert!(image.png.starts_with(b"\x89PNG"));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::VisDetected { mode, .. } if *mode == SstvMode::Pd120
        )));
    }

    #[test]
    #[ignore = "full SSTV decode is slow in debug; run with --release -- --ignored"]
    fn decodes_robot36_at_48khz() {
        let audio = encode(EncoderMode::Robot36, 48_000);
        let (_, image) = decode(&audio, 48_000, 4800);
        let image = image.expect("Robot 36 image");
        assert_eq!((image.width, image.height), (320, 240));
        assert!(image.mode_name.contains("Robot 36"));
    }

    #[test]
    #[ignore = "full SSTV decode is slow in debug; run with --release -- --ignored"]
    fn decodes_robot36_at_44khz() {
        let audio = encode(EncoderMode::Robot36, 44_100);
        let (_, image) = decode(&audio, 44_100, 4_410);
        let image = image.expect("Robot 36 image at 44.1 kHz");
        assert_eq!((image.width, image.height), (320, 240));
    }

    #[test]
    #[ignore = "full SSTV decode is slow in debug; run with --release -- --ignored"]
    fn decodes_pd180_and_martin1_at_48khz() {
        for (mode, width, height) in [
            (EncoderMode::Pd180, 640, 496),
            (EncoderMode::Martin1, 320, 256),
        ] {
            let audio = encode(mode, 48_000);
            let (_, image) = decode(&audio, 48_000, 4800);
            let image = image.unwrap_or_else(|| panic!("no image for {mode:?}"));
            assert_eq!((image.width, image.height), (width, height));
        }
    }

    #[test]
    #[ignore = "full SSTV decode is slow in debug; run with --release -- --ignored"]
    fn decodes_with_agc_and_odd_chunk_boundaries() {
        let audio = encode(EncoderMode::Robot36, 48_000);
        let mut audio = audio;
        midori_dsp::AudioAgc::new().process(&mut audio);
        for chunk in [1_024, 4_096, 44_100] {
            let (_, image) = decode(&audio, 48_000, chunk);
            assert!(image.is_some(), "no image at chunk size {chunk}");
        }
    }
}
