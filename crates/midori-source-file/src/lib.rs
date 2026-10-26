//! File-backed I/Q source for GUI playback and offline testing.
//!
//! Supports RIFF and RF64 WAV with interleaved signed 16/32-bit or f32 I/Q.
//! Playback is paced to real time by default so the waterfall, audio, and
//! sidecars behave like a live receiver.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use midori_core::{
    BlockFlags, IqBlock, IqSource, SampleFormat, SourceCaps, SourceConfig, StreamSpec,
};

pub mod wav;

pub use wav::{WavIqReader, WavIqSpec, WavSampleFormat};

pub struct WavIqSource {
    path: PathBuf,
    reader: WavIqReader,
    loop_file: bool,
    realtime: bool,
    finished: bool,
    spec: Option<StreamSpec>,
    buffer_samples: usize,
    start_sample: u64,
    first_timestamp_ns: Option<u64>,
    pace_anchor: Option<Instant>,
    paced_samples: u64,
}

impl WavIqSource {
    pub fn open(path: &Path, loop_file: bool) -> Result<Self> {
        let reader = WavIqReader::open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            reader,
            loop_file,
            realtime: true,
            finished: false,
            spec: None,
            buffer_samples: 32_768,
            start_sample: 0,
            first_timestamp_ns: None,
            pace_anchor: None,
            paced_samples: 0,
        })
    }

    pub fn set_realtime(&mut self, realtime: bool) {
        if self.realtime != realtime {
            self.realtime = realtime;
            self.reset_pacing();
        }
    }

    pub fn loop_file(&self) -> bool {
        self.loop_file
    }

    pub fn set_loop(&mut self, loop_file: bool) {
        self.loop_file = loop_file;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_spec(&self) -> WavIqSpec {
        self.reader.spec()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Restart the realtime playback schedule from the next block.
    fn reset_pacing(&mut self) {
        self.pace_anchor = None;
        self.paced_samples = 0;
    }
}

impl IqSource for WavIqSource {
    fn caps(&self) -> SourceCaps {
        let rate = self.reader.spec().sample_rate_hz;
        SourceCaps {
            name: format!("I/Q file ({})", self.path.display()),
            min_freq_hz: 0.0,
            max_freq_hz: 6_000_000_000.0,
            min_sample_rate_hz: rate,
            max_sample_rate_hz: rate,
            gain_range_db: (0.0, 0.0),
            rf_ports: Vec::new(),
        }
    }

    fn configure(&mut self, config: SourceConfig) -> Result<StreamSpec> {
        self.buffer_samples = config.buffer_samples.max(1);
        self.reader.rewind()?;
        self.start_sample = 0;
        self.first_timestamp_ns = None;
        self.finished = false;
        self.reset_pacing();
        let file = self.reader.spec();
        let spec = StreamSpec {
            sample_rate_hz: file.sample_rate_hz,
            center_freq_hz: config.center_freq_hz,
            bandwidth_hz: file.sample_rate_hz,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        self.spec = Some(spec);
        Ok(spec)
    }

    fn start(&mut self) -> Result<()> {
        self.reader.rewind()?;
        self.start_sample = 0;
        self.first_timestamp_ns = None;
        self.finished = false;
        self.reset_pacing();
        Ok(())
    }

    fn read_block(&mut self) -> Result<IqBlock> {
        if self.finished {
            bail!("end of file");
        }
        let spec = self
            .spec
            .ok_or_else(|| anyhow!("source is not configured"))?;
        let started = Instant::now();
        if self.realtime && self.pace_anchor.is_none() {
            self.pace_anchor = Some(started);
        }
        let mut samples = self.reader.read_iq(self.buffer_samples)?;

        if samples.is_empty() {
            if self.loop_file {
                self.reader.rewind()?;
                self.reset_pacing();
                if self.realtime {
                    self.pace_anchor = Some(Instant::now());
                }
                samples = self.reader.read_iq(self.buffer_samples)?;
            }
            if samples.is_empty() {
                self.finished = true;
                bail!("end of file");
            }
        }

        let frames = samples.len();
        if !self.loop_file && self.reader.remaining_frames() == 0 {
            self.finished = true;
        }

        let first = *self.first_timestamp_ns.get_or_insert_with(now_ns);
        let offset_ns = (self.start_sample as f64 * 1_000_000_000.0 / spec.sample_rate_hz) as u64;
        let timestamp_ns = Some(first.saturating_add(offset_ns));

        let block = IqBlock {
            samples: Arc::from(samples.into_boxed_slice()),
            spec,
            start_sample: self.start_sample,
            timestamp_ns,
            flags: BlockFlags::default(),
        };
        self.start_sample += frames as u64;

        if self.realtime {
            // Pace against an absolute schedule rather than sleeping for one
            // block at a time: consumer work between reads must not stretch
            // playback beyond real time.
            self.paced_samples += frames as u64;
            let anchor = self.pace_anchor.unwrap_or(started);
            let target =
                anchor + Duration::from_secs_f64(self.paced_samples as f64 / spec.sample_rate_hz);
            let now = Instant::now();
            if let Some(late) = now.checked_duration_since(target) {
                if late > MAX_PACING_LAG {
                    // The consumer stalled; restart the schedule instead of
                    // bursting back-to-back blocks to catch up.
                    self.pace_anchor = Some(now);
                    self.paced_samples = frames as u64;
                }
            } else {
                std::thread::sleep(target - now);
            }
        }

        Ok(block)
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_frequency(&mut self, hz: f64) -> Result<()> {
        if let Some(spec) = &mut self.spec {
            spec.center_freq_hz = hz;
        }
        Ok(())
    }

    fn set_gain(&mut self, _db: f64) -> Result<()> {
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Maximum amount a realtime playback schedule may slip before it is restarted
/// instead of trying to catch up with a burst of blocks.
const MAX_PACING_LAG: Duration = Duration::from_secs(1);

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_wav(frames: usize, rate: u32) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("midori-source-file-{nanos}.wav"));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for index in 0..frames {
            writer
                .write_sample((index as i16).wrapping_mul(100))
                .unwrap();
            writer
                .write_sample(-(index as i16).wrapping_mul(100))
                .unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn reads_to_end_and_reports_finished() {
        let path = temp_wav(8, 48_000);
        let mut source = WavIqSource::open(&path, false).unwrap();
        source.set_realtime(false);
        let config = SourceConfig {
            center_freq_hz: 437_393_500.0,
            buffer_samples: 4,
            ..SourceConfig::default()
        };
        let spec = source.configure(config).unwrap();
        assert_eq!(spec.sample_rate_hz, 48_000.0);
        assert_eq!(spec.center_freq_hz, 437_393_500.0);

        let first = source.read_block().unwrap();
        assert_eq!(first.len(), 4);
        assert!(!source.is_finished());
        let second = source.read_block().unwrap();
        assert_eq!(second.len(), 4);
        assert!(source.is_finished());
        assert!(source.read_block().is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn loops_at_end() {
        let path = temp_wav(8, 48_000);
        let mut source = WavIqSource::open(&path, true).unwrap();
        source.set_realtime(false);
        source
            .configure(SourceConfig {
                buffer_samples: 4,
                ..SourceConfig::default()
            })
            .unwrap();
        for _ in 0..3 {
            assert_eq!(source.read_block().unwrap().len(), 4);
        }
        assert!(!source.is_finished());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn realtime_playback_tracks_an_absolute_schedule() {
        let sample_rate = 8_000u32;
        let frames_per_block = 400usize;
        let blocks = 10usize;
        let path = temp_wav(frames_per_block * blocks, sample_rate);

        let mut source = WavIqSource::open(&path, false).unwrap();
        source.set_realtime(true);
        source
            .configure(SourceConfig {
                buffer_samples: frames_per_block,
                ..SourceConfig::default()
            })
            .unwrap();

        let started = Instant::now();
        for index in 0..blocks {
            let block = source.read_block().unwrap();
            assert_eq!(block.len(), frames_per_block);
            if index + 1 < blocks {
                // Simulate consumer work between reads.
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let elapsed = started.elapsed();

        let expected =
            Duration::from_secs_f64((frames_per_block * blocks) as f64 / sample_rate as f64);
        let tolerance = expected.mul_f64(0.15);
        assert!(
            elapsed >= expected - tolerance && elapsed <= expected + tolerance,
            "elapsed {elapsed:?}, expected about {expected:?}; \
             consumer time must not accumulate into playback drift"
        );
        let _ = std::fs::remove_file(path);
    }
}
