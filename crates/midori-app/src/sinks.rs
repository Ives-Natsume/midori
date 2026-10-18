//! Non-audio sinks used by the GUI worker.

use anyhow::Result;
use crossbeam_channel::Sender;
use midori_core::{Frame, Sink, StreamData};
use midori_dsp::{SpectrumAnalyzer, fftshift, frequency_axis};

use crate::app::SpectrumFrame;

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
