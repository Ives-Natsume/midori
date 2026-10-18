//! The live session: one source, N channels, N sinks.
//!
//! This is intentionally a fixed topology rather than a generic flowgraph.
//! A source block fans out to every sink as an IQ frame, and each channel adds
//! its own typed frames (audio, soft bits, bytes).

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use midori_core::{
    Channel, Frame, IqSource, Session, SessionStats, Sink, SourceConfig, StreamSpec,
};

pub struct LiveSession {
    config: SourceConfig,
    source: Box<dyn IqSource>,
    channels: Vec<Box<dyn Channel>>,
    sinks: Vec<Box<dyn Sink>>,
    spec: Option<StreamSpec>,
    seq: u64,
    stats: SessionStats,
    frames: Vec<Frame>,
}

impl LiveSession {
    pub fn new(
        config: SourceConfig,
        source: Box<dyn IqSource>,
        channels: Vec<Box<dyn Channel>>,
        sinks: Vec<Box<dyn Sink>>,
    ) -> Self {
        Self {
            config,
            source,
            channels,
            sinks,
            spec: None,
            seq: 0,
            stats: SessionStats::default(),
            frames: Vec::new(),
        }
    }

    pub fn current_spec(&self) -> Option<StreamSpec> {
        self.spec
    }

    pub fn set_frequency(&mut self, hz: f64) -> Result<()> {
        self.source.set_frequency(hz)?;
        if let Some(spec) = &mut self.spec {
            spec.center_freq_hz = hz;
        }
        Ok(())
    }

    pub fn set_gain(&mut self, db: f64) -> Result<()> {
        self.source.set_gain(db)
    }

    /// Start, loop until `stop` is set, then stop. Useful for tests and simple
    /// headless runs; the GUI drives `start`/`step`/`stop` directly so it can
    /// process control commands between blocks.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<()> {
        self.start()?;
        while !stop.load(Ordering::SeqCst) {
            if let Err(error) = self.step() {
                let _ = self.stop();
                return Err(error);
            }
        }
        self.stop()
    }
}

impl Session for LiveSession {
    fn spec(&self) -> Option<StreamSpec> {
        self.spec
    }

    fn start(&mut self) -> Result<()> {
        let spec = self.source.configure(self.config.clone())?;
        for channel in &mut self.channels {
            channel.configure(&spec)?;
        }
        self.source.start()?;
        self.spec = Some(spec);
        self.stats = SessionStats::default();
        self.seq = 0;
        Ok(())
    }

    fn step(&mut self) -> Result<SessionStats> {
        let block = self.source.read_block()?;
        self.spec = Some(block.spec);
        self.stats.blocks += 1;
        self.stats.samples += block.len() as u64;

        // Fan the raw IQ out first: recorder, waterfall, IQ plugins.
        let iq_frame = block.clone().into_frame(self.seq);
        self.seq += 1;
        for sink in &mut self.sinks {
            sink.on_frame(iq_frame.clone())?;
        }

        // Then run the digital channels.
        self.frames.clear();
        for channel in &mut self.channels {
            channel.process(&block, &mut self.frames)?;
        }

        let mut frames = std::mem::take(&mut self.frames);
        self.stats.frames += frames.len() as u64;
        for frame in frames.drain(..) {
            for sink in &mut self.sinks {
                sink.on_frame(frame.clone())?;
            }
        }
        self.frames = frames;

        Ok(self.stats)
    }

    fn stop(&mut self) -> Result<()> {
        for sink in &mut self.sinks {
            sink.flush()?;
        }
        self.source.stop()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use midori_core::{
        BlockFlags, Channel, DemodMode, FrameMeta, IqBlock, IqSource, Sample, SampleFormat,
        SourceCaps, StreamData, VfoConfig,
    };

    use super::*;

    struct MockSource {
        spec: StreamSpec,
        remaining: usize,
    }

    impl IqSource for MockSource {
        fn caps(&self) -> SourceCaps {
            SourceCaps {
                name: "mock".to_string(),
                min_freq_hz: 0.0,
                max_freq_hz: 1e9,
                min_sample_rate_hz: 1.0,
                max_sample_rate_hz: 10e6,
                gain_range_db: (0.0, 0.0),
                rf_ports: vec![],
            }
        }

        fn configure(&mut self, _config: SourceConfig) -> Result<StreamSpec> {
            Ok(self.spec)
        }

        fn start(&mut self) -> Result<()> {
            Ok(())
        }

        fn read_block(&mut self) -> Result<IqBlock> {
            if self.remaining == 0 {
                anyhow::bail!("no more blocks");
            }
            self.remaining -= 1;
            let samples: Arc<[Sample]> =
                Arc::from(vec![Sample::new(0.1, 0.1); 1024].into_boxed_slice());
            Ok(IqBlock {
                samples,
                spec: self.spec,
                start_sample: 0,
                timestamp_ns: None,
                flags: BlockFlags::default(),
            })
        }

        fn set_frequency(&mut self, _hz: f64) -> Result<()> {
            Ok(())
        }

        fn set_gain(&mut self, _db: f64) -> Result<()> {
            Ok(())
        }
    }

    struct TestChannel {
        config: VfoConfig,
    }

    impl Channel for TestChannel {
        fn config(&self) -> &VfoConfig {
            &self.config
        }

        fn process(&mut self, iq: &IqBlock, out: &mut Vec<Frame>) -> Result<()> {
            let meta = FrameMeta {
                seq: 0,
                start_sample: 0,
                timestamp_ns: iq.timestamp_ns,
                sample_rate_hz: 48_000.0,
                center_freq_hz: iq.spec.center_freq_hz,
                bandwidth_hz: 4_000.0,
                vfo_id: Some(self.config.id),
                flags: iq.flags,
            };
            out.push(Frame::new(
                meta,
                StreamData::Audio(Arc::from(vec![0.0f32; 480].into_boxed_slice())),
            ));
            Ok(())
        }
    }

    struct CountingSink {
        iq: Arc<AtomicUsize>,
        audio: Arc<AtomicUsize>,
    }

    impl Sink for CountingSink {
        fn on_frame(&mut self, frame: Frame) -> Result<()> {
            match frame.data {
                StreamData::Iq(_) => {
                    self.iq.fetch_add(1, Ordering::SeqCst);
                }
                StreamData::Audio(_) => {
                    self.audio.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn session_fans_out_iq_and_audio() {
        let spec = StreamSpec {
            sample_rate_hz: 2_400_000.0,
            center_freq_hz: 91_600_000.0,
            bandwidth_hz: 2_000_000.0,
            format: SampleFormat::Cf32Le,
            full_scale: 1.0,
        };
        let source = MockSource { spec, remaining: 2 };
        let channel = TestChannel {
            config: VfoConfig::new(1, 0.0, 200_000.0, DemodMode::Fm),
        };
        let iq_count = Arc::new(AtomicUsize::new(0));
        let audio_count = Arc::new(AtomicUsize::new(0));
        let sink = CountingSink {
            iq: Arc::clone(&iq_count),
            audio: Arc::clone(&audio_count),
        };

        let config = SourceConfig::default();
        let mut session = LiveSession::new(
            config,
            Box::new(source),
            vec![Box::new(channel)],
            vec![Box::new(sink)],
        );

        session.start().unwrap();
        let first = session.step().unwrap();
        let second = session.step().unwrap();
        session.stop().unwrap();

        assert_eq!(first.blocks, 1);
        assert_eq!(second.blocks, 2);
        assert_eq!(iq_count.load(Ordering::SeqCst), 2);
        assert_eq!(audio_count.load(Ordering::SeqCst), 2);
    }
}
