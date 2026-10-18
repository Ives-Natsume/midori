//! CoreAudio/WASAPI/ALSA output via cpal.
//!
//! The audio callback only pops pre-allocated `Arc<[f32]>` blocks from a
//! bounded channel; it never allocates or performs DSP.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Sender, TryRecvError, bounded};
use midori_core::{Frame, Sink, StreamData};

pub struct AudioOutput {
    _stream: cpal::Stream,
    sample_rate_hz: u32,
    channels: u16,
}

pub struct AudioProducer {
    tx: Sender<Arc<[f32]>>,
    sample_rate_hz: u32,
}

impl AudioOutput {
    pub fn open() -> Result<(Self, AudioProducer)> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no default audio output device")?;
        let supported = device
            .default_output_config()
            .context("could not query default audio output config")?;
        if supported.sample_format() != cpal::SampleFormat::F32 {
            bail!(
                "default output format is {:?}, only f32 is supported",
                supported.sample_format()
            );
        }

        let config = supported.config();
        let sample_rate_hz = config.sample_rate.0;
        let channels = config.channels;
        let (tx, rx) = bounded::<Arc<[f32]>>(8);

        let mut current: Option<Arc<[f32]>> = None;
        let mut cursor = 0usize;
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _info| {
                    for frame in data.chunks_mut(channels as usize) {
                        let mut value = 0.0;
                        loop {
                            if let Some(block) = current.as_ref()
                                && cursor < block.len()
                            {
                                value = block[cursor];
                                cursor += 1;
                                break;
                            }
                            match rx.try_recv() {
                                Ok(block) => {
                                    current = Some(block);
                                    cursor = 0;
                                }
                                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                                    current = None;
                                    cursor = 0;
                                    break;
                                }
                            }
                        }
                        for sample in frame.iter_mut() {
                            *sample = value;
                        }
                    }
                },
                |error| eprintln!("[audio] stream error: {error}"),
                None,
            )
            .context("building audio output stream")?;

        stream.play().context("starting audio output stream")?;
        Ok((
            Self {
                _stream: stream,
                sample_rate_hz,
                channels,
            },
            AudioProducer { tx, sample_rate_hz },
        ))
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

impl AudioProducer {
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    pub fn push(&mut self, samples: Arc<[f32]>) {
        // If the audio thread is behind, drop the block instead of blocking
        // the SDR worker. A small glitch is better than a growing latency.
        let _ = self.tx.try_send(samples);
    }
}

pub struct AudioSink {
    producer: AudioProducer,
    frames: u64,
}

impl AudioSink {
    pub fn new(producer: AudioProducer) -> Self {
        Self {
            producer,
            frames: 0,
        }
    }
}

impl Sink for AudioSink {
    fn on_frame(&mut self, frame: Frame) -> anyhow::Result<()> {
        if (frame.meta.sample_rate_hz - self.producer.sample_rate_hz() as f64).abs() > 1.0 {
            return Ok(());
        }
        if let StreamData::Audio(samples) = frame.data {
            if self.frames == 0 {
                eprintln!(
                    "[audio] first frame: {} samples at {:.0} Hz",
                    samples.len(),
                    frame.meta.sample_rate_hz
                );
            }
            self.frames += 1;
            self.producer.push(samples);
        }
        Ok(())
    }
}
