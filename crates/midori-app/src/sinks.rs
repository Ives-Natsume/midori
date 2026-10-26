//! Non-audio sinks used by the GUI worker.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Sender;
use midori_core::{Frame, Sink, StreamData};
use midori_dsp::{OffsetScan, SpectrumAnalyzer, fftshift, frequency_axis};

use crate::app::SpectrumFrame;

/// Shared state for the GUI auto-tune scan.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) enum AutoTuneState {
    #[default]
    Idle,
    Scanning,
    Done(Option<f64>),
}

/// Collects IQ for a few seconds and reports the strongest narrowband peak.
pub(crate) struct AutoTuneSink {
    state: Arc<Mutex<AutoTuneState>>,
    previous: AutoTuneState,
    scan: Option<OffsetScan>,
    samples: f64,
}

impl AutoTuneSink {
    pub(crate) fn new(state: Arc<Mutex<AutoTuneState>>) -> Self {
        Self {
            state,
            previous: AutoTuneState::Idle,
            scan: None,
            samples: 0.0,
        }
    }
}

impl Sink for AutoTuneSink {
    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        // A BPSK channel also emits IQ, but at the channel rate and with a VFO
        // id. Scanning it would look for carriers inside the selected channel
        // instead of across the wideband capture.
        if frame.meta.vfo_id.is_some() {
            return Ok(());
        }

        let mut state = self.state.lock().unwrap();
        let scanning = matches!(*state, AutoTuneState::Scanning);
        if scanning && !matches!(self.previous, AutoTuneState::Scanning) {
            self.scan = None;
            self.samples = 0.0;
        }
        self.previous = *state;

        if !scanning {
            return Ok(());
        }

        let StreamData::Iq(samples) = frame.data else {
            return Ok(());
        };
        if self.scan.is_none() {
            let scan_range = (frame.meta.sample_rate_hz * 0.45).min(100_000.0);
            self.scan = Some(OffsetScan::new(
                frame.meta.sample_rate_hz,
                32_768,
                scan_range,
            ));
            self.samples = 0.0;
        }

        if let Some(scan) = self.scan.as_mut() {
            scan.process(&samples);
        }
        self.samples += samples.len() as f64;
        if self.samples / frame.meta.sample_rate_hz >= 4.0 {
            let result = self.scan.as_ref().and_then(OffsetScan::result);
            self.scan = None;
            self.samples = 0.0;
            *state = AutoTuneState::Done(result);
        }
        Ok(())
    }
}

/// Turns IQ frames into the compact spectrum frames consumed by egui.
pub struct SpectrumSink {
    analyzer: SpectrumAnalyzer,
    tx: Sender<SpectrumFrame>,
}

impl SpectrumSink {
    pub fn new(fft_size: usize, tx: Sender<SpectrumFrame>) -> Self {
        Self {
            analyzer: SpectrumAnalyzer::new(fft_size),
            tx,
        }
    }
}

impl Sink for SpectrumSink {
    fn on_frame(&mut self, frame: Frame) -> Result<()> {
        let meta = frame.meta;
        // Ignore channel IQ (BPSK baseband): the waterfall shows the wideband
        // source block only.
        if meta.vfo_id.is_some() {
            return Ok(());
        }
        let StreamData::Iq(samples) = frame.data else {
            return Ok(());
        };

        self.analyzer.process(&samples);
        if self.analyzer.frames() == 0 {
            return Ok(());
        }

        let spectrum_db = fftshift(&self.analyzer.spectrum_db());
        let frequencies_mhz =
            frequency_axis(meta.center_freq_hz, meta.sample_rate_hz, spectrum_db.len())
                .into_iter()
                .map(|hz| (hz / 1e6) as f32)
                .collect();

        let _ = self.tx.try_send(SpectrumFrame {
            spectrum_db,
            frequencies_mhz,
            center_freq_hz: meta.center_freq_hz,
            sample_rate_hz: meta.sample_rate_hz,
        });
        self.analyzer.reset();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use midori_core::{BlockFlags, FrameMeta, Sample};

    use super::*;

    fn meta(vfo_id: Option<u32>, sample_rate_hz: f64) -> FrameMeta {
        FrameMeta {
            seq: 0,
            start_sample: 0,
            timestamp_ns: None,
            sample_rate_hz,
            center_freq_hz: 100_000_000.0,
            nominal_freq_hz: 100_000_000.0,
            bandwidth_hz: sample_rate_hz,
            vfo_id,
            doppler_hz: 0.0,
            doppler_rate_hz_s: 0.0,
            flags: BlockFlags::default(),
        }
    }

    fn iq(len: usize) -> Arc<[Sample]> {
        Arc::from(vec![Sample::new(0.0, 0.0); len].into_boxed_slice())
    }

    #[test]
    fn spectrum_sink_ignores_channel_iq() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut sink = SpectrumSink::new(64, tx);

        sink.on_frame(Frame::new(meta(Some(1), 48_000.0), StreamData::Iq(iq(64))))
            .unwrap();
        assert_eq!(sink.analyzer.frames(), 0, "channel IQ must not be analysed");
        assert!(rx.try_recv().is_err(), "no spectrum frame expected");

        // The wideband source frame still produces a spectrum frame.
        sink.on_frame(Frame::new(meta(None, 48_000.0), StreamData::Iq(iq(64))))
            .unwrap();
        assert!(rx.try_recv().is_ok(), "wideband IQ must be analysed");
    }

    #[test]
    fn auto_tune_sink_ignores_channel_iq() {
        let state = Arc::new(Mutex::new(AutoTuneState::Scanning));
        let mut sink = AutoTuneSink::new(Arc::clone(&state));

        // More than four seconds of channel IQ; if it were counted the scan
        // would finish immediately.
        for _ in 0..3 {
            sink.on_frame(Frame::new(
                meta(Some(1), 10_000.0),
                StreamData::Iq(iq(20_000)),
            ))
            .unwrap();
        }
        assert_eq!(*state.lock().unwrap(), AutoTuneState::Scanning);

        // Four seconds of wideband IQ completes the scan.
        sink.on_frame(Frame::new(meta(None, 10_000.0), StreamData::Iq(iq(40_001))))
            .unwrap();
        assert!(matches!(*state.lock().unwrap(), AutoTuneState::Done(_)));
    }
}
