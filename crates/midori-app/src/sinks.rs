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
