//! Leading-silence gate for the CW decoder.
//!
//! A recording can start with seconds of channel noise. Letting that noise
//! reach the Morse timing estimator corrupts the first characters. This sink
//! buffers a short pre-roll and only starts forwarding audio once the level
//! jumps to a keyed signal, so the decoder starts with the first mark just
//! like the offline decoder does.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use midori_core::{Frame, Sink, StreamData};

pub struct CwStartGateSink<S> {
    inner: S,
    open: bool,
    pre_roll_limit: usize,
    pre_roll: VecDeque<f32>,
    levels: VecDeque<f32>,
    high_frames: u8,
}

impl<S> CwStartGateSink<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            open: false,
            pre_roll_limit: 0,
            pre_roll: VecDeque::new(),
            levels: VecDeque::new(),
            high_frames: 0,
        }
    }
}

impl<S: Sink> Sink for CwStartGateSink<S> {
    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        if self.open {
            return self.inner.on_frame(frame);
        }

        let StreamData::Audio(samples) = &frame.data else {
            return Ok(());
        };
        if samples.is_empty() {
            return Ok(());
        }

        if self.pre_roll_limit == 0 {
            let sample_rate = frame.meta.sample_rate_hz.max(1_000.0);
            self.pre_roll_limit = (0.10 * sample_rate).round() as usize;
        }

        self.pre_roll.extend(samples.iter().copied());
        while self.pre_roll.len() > self.pre_roll_limit {
            self.pre_roll.pop_front();
        }

        let sum: f64 = samples
            .iter()
            .map(|&sample| (sample as f64) * (sample as f64))
            .sum();
        let rms = (sum / samples.len() as f64).sqrt() as f32;
        self.levels.push_back(rms);
        while self.levels.len() > 24 {
            self.levels.pop_front();
        }

        // Wait for a little history so the noise floor is meaningful. A keyed
        // mark then stands clearly above that floor for at least two frames.
        if self.levels.len() < 8 {
            return Ok(());
        }
        let mut sorted: Vec<f32> = self.levels.iter().copied().collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let floor = sorted[sorted.len() / 10].max(1e-6);
        if rms > floor * 1.8 + 1e-5 {
            self.high_frames = self.high_frames.saturating_add(1);
        } else {
            self.high_frames = 0;
        }
        if self.high_frames < 2 {
            return Ok(());
        }

        self.open = true;
        let audio: Vec<f32> = self.pre_roll.drain(..).collect();
        self.pre_roll.clear();
        self.levels.clear();
        let meta = frame.meta;
        self.inner
            .on_frame(Frame::new(meta, StreamData::Audio(Arc::from(audio))))
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midori_core::{BlockFlags, FrameMeta};

    #[derive(Default)]
    struct Collect {
        frames: Vec<Frame>,
    }

    impl Sink for Collect {
        fn on_frame(&mut self, frame: Frame) -> Result<()> {
            self.frames.push(frame);
            Ok(())
        }
    }

    fn frame(samples: &[f32], seq: u64) -> Frame {
        Frame::new(
            FrameMeta {
                seq,
                start_sample: 0,
                timestamp_ns: None,
                sample_rate_hz: 48_000.0,
                center_freq_hz: 0.0,
                nominal_freq_hz: 0.0,
                bandwidth_hz: 2_000.0,
                vfo_id: Some(0),
                doppler_hz: 0.0,
                doppler_rate_hz_s: 0.0,
                flags: BlockFlags::default(),
            },
            StreamData::Audio(Arc::from(samples.to_vec())),
        )
    }

    #[test]
    fn drops_leading_noise_then_forwards_signal() {
        let mut gate = CwStartGateSink::new(Collect::default());
        for seq in 0..20 {
            gate.on_frame(frame(&[0.001; 480], seq)).unwrap();
        }
        assert!(gate.inner.frames.is_empty());

        gate.on_frame(frame(&[0.05; 480], 20)).unwrap();
        gate.on_frame(frame(&[0.05; 480], 21)).unwrap();
        assert_eq!(gate.inner.frames.len(), 1);
        let StreamData::Audio(samples) = &gate.inner.frames[0].data else {
            panic!("expected audio");
        };
        assert!(samples.iter().any(|sample| *sample > 0.04));
    }

    #[test]
    fn opens_on_a_signal_that_starts_immediately() {
        let mut gate = CwStartGateSink::new(Collect::default());
        for seq in 0..6 {
            let samples = if seq % 2 == 0 { 0.05 } else { 0.005 };
            gate.on_frame(frame(&[samples; 480], seq)).unwrap();
        }
        for seq in 6..10 {
            gate.on_frame(frame(&[0.05; 480], seq)).unwrap();
        }
        assert!(!gate.inner.frames.is_empty());
    }
}
