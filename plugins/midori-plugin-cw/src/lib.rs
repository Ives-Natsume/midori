//! A small adaptive CW decoder.
//!
//! This is intentionally not a contest-grade decoder. It exists to validate
//! the sidecar boundary and to decode beacons: mix the audio to the selected
//! tone, envelope-detect it, adapt to the noise floor, and turn mark/space
//! durations into Morse characters.

use std::f64::consts::TAU;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MorseEvent {
    Char(char),
    Word,
}

pub struct MorseDecoder {
    sample_rate: f64,
    phase: f64,
    phase_inc: f64,
    lp_i: f64,
    lp_q: f64,
    lp_alpha: f64,
    noise: f64,
    keyed: bool,
    sample_index: u64,
    mark_start: u64,
    last_mark_end: u64,
    dot_samples: f64,
    symbols: String,
    word_emitted: bool,
}

impl MorseDecoder {
    pub fn new(sample_rate_hz: f64, tone_hz: f64) -> Self {
        let sample_rate_hz = sample_rate_hz.max(1_000.0);
        let tone_hz = tone_hz.clamp(100.0, sample_rate_hz * 0.45);
        Self {
            sample_rate: sample_rate_hz,
            phase: 0.0,
            phase_inc: TAU * tone_hz / sample_rate_hz,
            lp_i: 0.0,
            lp_q: 0.0,
            lp_alpha: 1.0 - (-TAU * 50.0 / sample_rate_hz).exp(),
            noise: 0.01,
            keyed: false,
            sample_index: 0,
            mark_start: 0,
            last_mark_end: 0,
            dot_samples: 0.060 * sample_rate_hz,
            symbols: String::new(),
            word_emitted: false,
        }
    }

    pub fn wpm(&self) -> f32 {
        (1200.0 / (self.dot_samples / self.sample_rate * 1000.0)) as f32
    }

    pub fn process(&mut self, samples: &[f32]) -> Vec<MorseEvent> {
        let mut events = Vec::new();

        for &sample in samples {
            let (sin, cos) = self.phase.sin_cos();
            let input = sample as f64;
            self.lp_i += self.lp_alpha * (input * cos - self.lp_i);
            self.lp_q += self.lp_alpha * (input * sin - self.lp_q);
            self.phase += self.phase_inc;
            if self.phase > TAU {
                self.phase -= TAU;
            }

            let envelope = (self.lp_i * self.lp_i + self.lp_q * self.lp_q).sqrt();
            let index = self.sample_index;
            let startup = (0.1 * self.sample_rate) as u64;
            if index < startup {
                self.noise = self.noise.min(envelope);
            } else if envelope < self.noise {
                self.noise += 0.01 * (envelope - self.noise);
            } else {
                self.noise += 0.00005 * (envelope - self.noise);
            }
            self.noise = self.noise.max(1e-5);

            let threshold = if self.keyed {
                self.noise * 1.8 + 1e-3
            } else {
                self.noise * 2.5 + 1e-3
            };
            let keyed_now = envelope > threshold;

            if keyed_now && !self.keyed {
                self.mark_start = index;
                self.keyed = true;
                self.word_emitted = false;
            } else if !keyed_now && self.keyed {
                let duration = index.saturating_sub(self.mark_start);
                self.keyed = false;
                self.last_mark_end = index;
                if duration > (0.01 * self.sample_rate) as u64
                    && duration < (1.5 * self.sample_rate) as u64
                {
                    let dot = self.dot_samples;
                    let (symbol, unit) = if (duration as f64) < 1.5 * dot {
                        ('.', duration as f64)
                    } else {
                        ('-', duration as f64 / 3.0)
                    };
                    self.dot_samples = (0.9 * dot + 0.1 * unit)
                        .clamp(0.015 * self.sample_rate, 0.25 * self.sample_rate);
                    self.symbols.push(symbol);
                }
            } else if !keyed_now {
                let gap = index.saturating_sub(self.last_mark_end) as f64;
                if gap > 2.5 * self.dot_samples && !self.symbols.is_empty() {
                    if let Some(character) = decode_symbols(&self.symbols) {
                        events.push(MorseEvent::Char(character));
                    }
                    self.symbols.clear();
                }
                if gap > 6.5 * self.dot_samples && !self.word_emitted {
                    events.push(MorseEvent::Word);
                    self.word_emitted = true;
                }
            }

            self.sample_index += 1;
        }

        events
    }
}

fn decode_symbols(symbols: &str) -> Option<char> {
    Some(match symbols {
        ".-" => 'A',
        "-..." => 'B',
        "-.-." => 'C',
        "-.." => 'D',
        "." => 'E',
        "..-." => 'F',
        "--." => 'G',
        "...." => 'H',
        ".." => 'I',
        ".---" => 'J',
        "-.-" => 'K',
        ".-.." => 'L',
        "--" => 'M',
        "-." => 'N',
        "---" => 'O',
        ".--." => 'P',
        "--.-" => 'Q',
        ".-." => 'R',
        "..." => 'S',
        "-" => 'T',
        "..-" => 'U',
        "...-" => 'V',
        ".--" => 'W',
        "-..-" => 'X',
        "-.--" => 'Y',
        "--.." => 'Z',
        "-----" => '0',
        ".----" => '1',
        "..---" => '2',
        "...--" => '3',
        "....-" => '4',
        "....." => '5',
        "-...." => '6',
        "--..." => '7',
        "---.." => '8',
        "----." => '9',
        ".-.-.-" => '.',
        "--..--" => ',',
        "..--.." => '?',
        "-..-." => '/',
        "-...-" => '=',
        ".-.-." => '+',
        "-....-" => '-',
        "---..." => ':',
        ".----." => '\'',
        ".-..-." => '"',
        ".--.-." => '@',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(sample_rate: f64, tone_hz: f64, ms: f64, amplitude: f32) -> Vec<f32> {
        let count = (sample_rate * ms / 1000.0) as usize;
        (0..count)
            .map(|i| {
                let phase = TAU * tone_hz * i as f64 / sample_rate;
                amplitude * phase.sin() as f32
            })
            .collect()
    }

    fn silence(sample_rate: f64, ms: f64) -> Vec<f32> {
        vec![0.0; (sample_rate * ms / 1000.0) as usize]
    }

    fn push_marks(audio: &mut Vec<f32>, sample_rate: f64, tone_hz: f64, marks: &str) {
        let dot_ms = 60.0;
        let mut first = true;
        for symbol in marks.chars() {
            if !first {
                audio.extend(silence(sample_rate, dot_ms));
            }
            first = false;
            match symbol {
                '.' => audio.extend(tone(sample_rate, tone_hz, dot_ms, 0.5)),
                '-' => audio.extend(tone(sample_rate, tone_hz, dot_ms * 3.0, 0.5)),
                _ => {}
            }
        }
    }

    #[test]
    fn decodes_cq() {
        let sample_rate = 48_000.0;
        let tone_hz = 700.0;
        let mut audio = Vec::new();
        push_marks(&mut audio, sample_rate, tone_hz, "-.-.");
        audio.extend(silence(sample_rate, 180.0));
        push_marks(&mut audio, sample_rate, tone_hz, "--.-");
        audio.extend(silence(sample_rate, 500.0));

        let mut decoder = MorseDecoder::new(sample_rate, tone_hz);
        let mut text = String::new();
        for block in audio.chunks(512) {
            for event in decoder.process(block) {
                if let MorseEvent::Char(character) = event {
                    text.push(character);
                }
            }
        }

        assert_eq!(text, "CQ");
        assert!((decoder.wpm() - 20.0).abs() < 5.0, "wpm {}", decoder.wpm());
    }
}
