//! Minimal RIFF/RF64 WAV reader for interleaved I/Q recordings.
//!
//! `hound` does not understand RF64, which is what many SDR recorders write
//! once a file exceeds 4 GiB. This parser handles the small subset Midori
//! needs: PCM 16/32-bit and IEEE float, one or two channels.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};
use midori_core::Sample;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavSampleFormat {
    Int,
    Float,
}

#[derive(Debug, Clone, Copy)]
pub struct WavIqSpec {
    pub sample_rate_hz: f64,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub sample_format: WavSampleFormat,
    pub frames: u64,
}

pub struct WavIqReader {
    reader: BufReader<File>,
    spec: WavIqSpec,
    block_align: usize,
    data_offset: u64,
    data_len: u64,
    frames_read: u64,
}

impl WavIqReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut reader = BufReader::new(file);

        let mut header = [0u8; 12];
        reader
            .read_exact(&mut header)
            .context("reading WAV header")?;
        let is_rf64 = &header[0..4] == b"RF64";
        let is_riff = &header[0..4] == b"RIFF";
        if !is_rf64 && !is_riff {
            bail!("not a RIFF/RF64 WAV file");
        }
        if &header[8..12] != b"WAVE" {
            bail!("WAV header is missing the WAVE form");
        }

        let mut ds64_data_size = None;
        let mut fmt: Option<FmtChunk> = None;
        let mut data_offset = None;
        let mut data_size = None;
        let mut file_size = file_size(&mut reader)?;

        loop {
            let mut chunk_header = [0u8; 8];
            if reader.read_exact(&mut chunk_header).is_err() {
                break;
            }
            let id = &chunk_header[0..4];
            let size = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap()) as u64;

            match id {
                b"ds64" => {
                    let ds64 = read_ds64(&mut reader, size)?;
                    ds64_data_size = Some(ds64.data_size);
                    file_size = file_size.max(ds64.riff_size + 8);
                    skip_padding(&mut reader, size)?;
                }
                b"fmt " => {
                    let mut bytes = vec![0u8; size as usize];
                    reader.read_exact(&mut bytes).context("reading fmt chunk")?;
                    fmt = Some(parse_fmt(&bytes)?);
                    skip_padding(&mut reader, size)?;
                }
                b"data" => {
                    let actual_size = if size == u32::MAX as u64 {
                        ds64_data_size.unwrap_or(0)
                    } else {
                        size
                    };
                    data_offset = Some(reader.stream_position()?);
                    data_size = Some(actual_size);
                    // Do not read the payload; seek over it if possible.
                    if actual_size > 0 {
                        let target = reader
                            .stream_position()?
                            .saturating_add(actual_size)
                            .min(file_size);
                        reader.seek(SeekFrom::Start(target))?;
                    }
                }
                _ => {
                    let target = reader
                        .stream_position()?
                        .saturating_add(size)
                        .min(file_size);
                    reader.seek(SeekFrom::Start(target))?;
                }
            }

            if data_offset.is_some() && fmt.is_some() {
                // The data chunk is normally last; the fmt chunk must be seen
                // first. Break once both are known.
                if id == b"data" {
                    break;
                }
            }
        }

        let fmt = fmt.context("WAV has no fmt chunk")?;
        let data_offset = data_offset.context("WAV has no data chunk")?;
        let mut data_len = data_size.context("WAV has no data chunk size")?;
        if fmt.channels == 0 {
            bail!("WAV has zero channels");
        }
        if fmt.sample_rate_hz <= 0.0 {
            bail!("WAV has an invalid sample rate");
        }
        if data_offset + data_len > file_size && file_size > 0 {
            // Truncated recordings are common; read what exists.
            data_len = file_size.saturating_sub(data_offset);
        }

        let block_align = (fmt.channels as usize) * (fmt.bits_per_sample as usize / 8);
        if block_align == 0 {
            bail!("WAV has an invalid block alignment");
        }
        let frames = data_len / block_align as u64;

        reader.seek(SeekFrom::Start(data_offset))?;

        Ok(Self {
            reader,
            spec: WavIqSpec {
                sample_rate_hz: fmt.sample_rate_hz,
                channels: fmt.channels,
                bits_per_sample: fmt.bits_per_sample,
                sample_format: fmt.sample_format,
                frames,
            },
            block_align,
            data_offset,
            data_len,
            frames_read: 0,
        })
    }

    pub fn spec(&self) -> WavIqSpec {
        self.spec
    }

    /// Read up to `max_frames` complex samples. Mono files are treated as I
    /// with Q=0; stereo files are I/Q.
    pub fn read_iq(&mut self, max_frames: usize) -> Result<Vec<Sample>> {
        let remaining_frames = self.spec.frames.saturating_sub(self.frames_read);
        let frames = remaining_frames.min(max_frames as u64) as usize;
        if frames == 0 {
            return Ok(Vec::new());
        }

        let mut bytes = vec![0u8; frames * self.block_align];
        self.reader
            .read_exact(&mut bytes)
            .context("reading WAV data")?;
        self.frames_read += frames as u64;

        let mut samples = Vec::with_capacity(frames);
        match (self.spec.sample_format, self.spec.bits_per_sample) {
            (WavSampleFormat::Int, 16) => {
                for frame in bytes.chunks_exact(self.block_align) {
                    let i = i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0;
                    let q = if self.spec.channels >= 2 {
                        i16::from_le_bytes([frame[2], frame[3]]) as f32 / 32768.0
                    } else {
                        0.0
                    };
                    samples.push(Sample::new(i, q));
                }
            }
            (WavSampleFormat::Int, 32) => {
                for frame in bytes.chunks_exact(self.block_align) {
                    let i = i32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as f32
                        / 2_147_483_648.0;
                    let q = if self.spec.channels >= 2 {
                        i32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) as f32
                            / 2_147_483_648.0
                    } else {
                        0.0
                    };
                    samples.push(Sample::new(i, q));
                }
            }
            (WavSampleFormat::Float, 32) => {
                for frame in bytes.chunks_exact(self.block_align) {
                    let i = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
                    let q = if self.spec.channels >= 2 {
                        f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]])
                    } else {
                        0.0
                    };
                    samples.push(Sample::new(i, q));
                }
            }
            (format, bits) => bail!("unsupported WAV format {format:?} / {bits} bits"),
        }

        Ok(samples)
    }

    pub fn data_offset(&self) -> u64 {
        self.data_offset
    }

    pub fn data_len(&self) -> u64 {
        self.data_len
    }
}

#[derive(Debug, Clone, Copy)]
struct FmtChunk {
    channels: u16,
    sample_rate_hz: f64,
    bits_per_sample: u16,
    sample_format: WavSampleFormat,
}

#[derive(Debug, Clone, Copy)]
struct Ds64 {
    riff_size: u64,
    data_size: u64,
}

fn parse_fmt(bytes: &[u8]) -> Result<FmtChunk> {
    if bytes.len() < 16 {
        bail!("fmt chunk is too short");
    }
    let mut audio_format = u16::from_le_bytes([bytes[0], bytes[1]]);
    let channels = u16::from_le_bytes([bytes[2], bytes[3]]);
    let sample_rate = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let bits = u16::from_le_bytes([bytes[14], bytes[15]]);

    if audio_format == 0xFFFE {
        if bytes.len() < 40 {
            bail!("extensible fmt chunk is too short");
        }
        audio_format = u16::from_le_bytes([bytes[24], bytes[25]]);
    }

    let sample_format = match audio_format {
        1 => WavSampleFormat::Int,
        3 => WavSampleFormat::Float,
        other => bail!("unsupported WAV format tag {other}"),
    };

    Ok(FmtChunk {
        channels,
        sample_rate_hz: sample_rate as f64,
        bits_per_sample: bits,
        sample_format,
    })
}

fn read_ds64<R: Read>(reader: &mut R, size: u64) -> Result<Ds64> {
    if size < 24 {
        bail!("ds64 chunk is too short");
    }
    let mut bytes = vec![0u8; size as usize];
    reader
        .read_exact(&mut bytes)
        .context("reading ds64 chunk")?;
    let riff_size = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let data_size = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    Ok(Ds64 {
        riff_size,
        data_size,
    })
}

fn skip_padding<R: Read + Seek>(reader: &mut R, size: u64) -> Result<()> {
    if size % 2 == 1 {
        let mut padding = [0u8; 1];
        let _ = reader.read_exact(&mut padding);
    }
    Ok(())
}

fn file_size<R: Read + Seek>(reader: &mut R) -> Result<u64> {
    let current = reader.stream_position()?;
    let size = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(current))?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_small_pcm_wav() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("midori-iq-wav-{nanos}.wav"));
        {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            writer.write_sample(10_000i16).unwrap();
            writer.write_sample(-10_000i16).unwrap();
            writer.write_sample(0i16).unwrap();
            writer.write_sample(0i16).unwrap();
            writer.finalize().unwrap();
        }

        let mut reader = WavIqReader::open(&path).unwrap();
        assert_eq!(reader.spec().sample_rate_hz, 48_000.0);
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().frames, 2);
        let samples = reader.read_iq(10).unwrap();
        assert_eq!(samples.len(), 2);
        assert!((samples[0].re - 10_000.0 / 32768.0).abs() < 1e-6);
        assert!((samples[0].im + 10_000.0 / 32768.0).abs() < 1e-6);
        let _ = std::fs::remove_file(path);
    }
}
