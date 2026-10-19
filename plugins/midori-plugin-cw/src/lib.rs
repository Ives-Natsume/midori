//! A small adaptive CW decoder.
//!
//! The decoder is deliberately beacon-oriented, not contest-grade: it mixes the
//! audio to the selected tone, tracks a moving noise/signal floor, debounces
//! the keying envelope, merges short dropouts inside a mark, and turns the
//! resulting mark/gap durations into Morse characters.
//!
//! The timing model follows the usual Morse ratios: 1 unit mark for a dot,
//! 3 units for a dash, 1 unit between elements, 3 units between characters,
//! and 7 units between words. Thresholds are slightly wider to tolerate fading
//! and operator timing.

use std::collections::VecDeque;
use std::f64::consts::TAU;

pub mod iq_wav;
pub mod offline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MorseEvent {
    Char(char),
    Word,
}

#[derive(Debug, Clone, Copy)]
struct PendingMark {
    duration: u64,
    end: u64,
}

pub struct MorseDecoder {
    sample_rate: f64,
    phase: f64,
    phase_inc: f64,
    lp_i: f64,
    lp_q: f64,
    lp_alpha: f64,

    noise: f64,
    signal: f64,
    on_threshold: f64,
    off_threshold: f64,
    block_sum: f64,
    block_count: usize,
    block_samples: usize,
    block_levels: VecDeque<f64>,

    keyed: bool,
    sample_index: u64,
    mark_start: u64,
    last_mark_end: u64,
    pending_mark: Option<PendingMark>,
    on_count: u64,
    off_count: u64,
    pending_start: u64,
    pending_end: u64,
    debounce_samples: u64,

    dot_samples: f64,
    dot_history: VecDeque<f64>,
    dot_locked: bool,
    symbols: String,
    pending_word: bool,
}

impl MorseDecoder {
    pub fn new(sample_rate_hz: f64, tone_hz: f64) -> Self {
        let sample_rate_hz = sample_rate_hz.max(1_000.0);
        let tone_hz = tone_hz.clamp(100.0, sample_rate_hz * 0.45);
        let block_samples = (0.005 * sample_rate_hz).round().max(16.0) as usize;
        let debounce_samples = (0.003 * sample_rate_hz).round().max(1.0) as u64;
        let dot_samples = 0.060 * sample_rate_hz;

        Self {
            sample_rate: sample_rate_hz,
            phase: 0.0,
            phase_inc: TAU * tone_hz / sample_rate_hz,
            lp_i: 0.0,
            lp_q: 0.0,
            // 25 Hz envelope bandwidth: enough for 20-30 WPM, much less
            // noisy than the original 50 Hz filter.
            lp_alpha: 1.0 - (-TAU * 50.0 / sample_rate_hz).exp(),
            noise: 0.01,
            signal: 0.2,
            on_threshold: 0.04,
            off_threshold: 0.025,
            block_sum: 0.0,
            block_count: 0,
            block_samples,
            block_levels: VecDeque::with_capacity(160),
            keyed: false,
            sample_index: 0,
            mark_start: 0,
            last_mark_end: 0,
            pending_mark: None,
            on_count: 0,
            off_count: 0,
            pending_start: 0,
            pending_end: 0,
            debounce_samples,
            dot_samples,
            dot_history: VecDeque::with_capacity(9),
            dot_locked: false,
            symbols: String::new(),
            pending_word: false,
        }
    }

    pub fn wpm(&self) -> f32 {
        (1200.0 / (self.dot_samples / self.sample_rate * 1000.0)) as f32
    }

    /// Current adaptive keying threshold, useful for diagnostics.
    pub fn threshold(&self) -> f32 {
        self.on_threshold as f32
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
            self.update_levels(envelope);
            self.update_keying(envelope, &mut events);
            self.sample_index += 1;
        }

        events
    }

    /// Flush a partially received character. Useful when a sidecar shuts down
    /// or the caller knows the transmission ended.
    pub fn flush(&mut self) -> Vec<MorseEvent> {
        let mut events = Vec::new();
        if self.keyed {
            self.keyed = false;
            self.finish_mark(self.sample_index);
        }
        if let Some(mark) = self.pending_mark.take() {
            self.finalize_mark(mark.duration);
        }
        self.emit_character(&mut events);
        events
    }

    fn update_levels(&mut self, envelope: f64) {
        self.block_sum += envelope;
        self.block_count += 1;
        if self.block_count < self.block_samples {
            return;
        }

        let level = self.block_sum / self.block_count as f64;
        self.block_sum = 0.0;
        self.block_count = 0;
        if self.block_levels.len() >= 160 {
            self.block_levels.pop_front();
        }
        self.block_levels.push_back(level);
        if self.block_levels.len() < 8 {
            return;
        }

        let mut sorted: Vec<f64> = self.block_levels.iter().copied().collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let low = percentile(&sorted, 0.25);
        let high = percentile(&sorted, 0.90);
        self.noise = self.noise.max(low * 0.5);
        self.signal = self.signal.min(high);
        // Keep the estimates useful across AGC and fading changes.
        self.noise = self.noise * 0.9 + low * 0.1;
        self.signal = self.signal * 0.9 + high * 0.1;

        let span = (self.signal - self.noise).max(1e-4);
        self.on_threshold = self.noise + 0.36 * span;
        self.off_threshold = self.noise + 0.22 * span;
        self.on_threshold = self.on_threshold.max(1e-4);
        self.off_threshold = self.off_threshold.max(5e-5);
    }

    fn update_keying(&mut self, envelope: f64, events: &mut Vec<MorseEvent>) {
        let index = self.sample_index;

        if envelope > self.on_threshold {
            self.on_count += 1;
            self.off_count = 0;
            if !self.keyed {
                if self.on_count == 1 {
                    self.pending_start = index;
                }
                if self.on_count >= self.debounce_samples {
                    self.keyed = true;
                    self.mark_start = self.pending_start;
                    if self.pending_word {
                        self.emit_character(events);
                        events.push(MorseEvent::Word);
                        self.pending_word = false;
                    }
                }
            }
        } else if envelope < self.off_threshold {
            self.off_count += 1;
            self.on_count = 0;
            if self.keyed {
                if self.off_count == 1 {
                    self.pending_end = index;
                }
                if self.off_count >= self.debounce_samples {
                    self.keyed = false;
                    self.finish_mark(self.pending_end);
                }
            }
        } else {
            // Hysteresis band: hold the current state and counters.
        }

        self.update_gaps(index, events);
    }

    fn finish_mark(&mut self, mark_end: u64) {
        let duration = mark_end.saturating_sub(self.mark_start);
        if let Some(mut pending) = self.pending_mark.take() {
            let gap = self.mark_start.saturating_sub(pending.end);
            if gap < self.merge_gap_samples() {
                // A dropout inside a dash (fading, AGC, detector noise):
                // treat the two fragments as one mark.
                pending.duration += gap + duration;
                pending.end = mark_end;
                self.pending_mark = Some(pending);
            } else {
                self.finalize_mark(pending.duration);
                self.pending_mark = Some(PendingMark {
                    duration,
                    end: mark_end,
                });
            }
        } else {
            self.pending_mark = Some(PendingMark {
                duration,
                end: mark_end,
            });
        }
    }

    fn update_gaps(&mut self, index: u64, events: &mut Vec<MorseEvent>) {
        if self.keyed {
            return;
        }

        if let Some(pending) = self.pending_mark {
            let gap = index.saturating_sub(pending.end);
            if gap >= self.merge_gap_samples() {
                self.finalize_mark(pending.duration);
                self.pending_mark = None;
                self.last_mark_end = pending.end;
            } else {
                return;
            }
        }

        let gap = index.saturating_sub(self.last_mark_end) as f64;
        let dot = self.dot_samples;
        if gap > 2.2 * dot {
            self.emit_character(events);
        }
        if gap > 6.0 * dot {
            self.pending_word = true;
        }
    }

    fn finalize_mark(&mut self, duration: u64) {
        let dot = self.dot_samples;
        let duration = duration as f64;
        let min_mark = (0.5 * dot).max(0.010 * self.sample_rate);
        let max_mark = 8.0 * dot;
        if duration < min_mark || duration > max_mark {
            return;
        }

        if duration < 1.9 * dot {
            self.symbols.push('.');
            if !self.dot_locked {
                self.dot_history.push_back(duration);
                if self.dot_history.len() >= 8 {
                    let mut sorted: Vec<f64> = self.dot_history.iter().copied().collect();
                    sorted.sort_by(|a, b| a.total_cmp(b));
                    self.dot_samples = percentile(&sorted, 0.50)
                        .clamp(0.018 * self.sample_rate, 0.150 * self.sample_rate);
                    self.dot_locked = true;
                }
            } else if duration > 0.80 * dot && duration < 1.20 * dot {
                // Once locked, adapt only slowly. Short marks caused by fading
                // must not drag the estimate toward 40-50 WPM.
                self.dot_samples = (0.95 * dot + 0.05 * duration)
                    .clamp(0.018 * self.sample_rate, 0.150 * self.sample_rate);
            }
        } else {
            self.symbols.push('-');
        }
    }

    fn emit_character(&mut self, events: &mut Vec<MorseEvent>) {
        if self.symbols.is_empty() {
            return;
        }
        if let Some(character) = decode_symbols(&self.symbols) {
            events.push(MorseEvent::Char(character));
        }
        self.symbols.clear();
    }

    fn merge_gap_samples(&self) -> u64 {
        ((0.60 * self.dot_samples) as u64)
            .max((0.002 * self.sample_rate) as u64)
            .max(1)
    }
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
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

    fn decode_text(sample_rate: f64, tone_hz: f64, audio: &[f32]) -> String {
        let mut decoder = MorseDecoder::new(sample_rate, tone_hz);
        let mut text = String::new();
        for block in audio.chunks(512) {
            for event in decoder.process(block) {
                match event {
                    MorseEvent::Char(character) => text.push(character),
                    MorseEvent::Word => text.push(' '),
                }
            }
        }
        for event in decoder.flush() {
            match event {
                MorseEvent::Char(character) => text.push(character),
                MorseEvent::Word => text.push(' '),
            }
        }
        text
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

        let text = decode_text(sample_rate, tone_hz, &audio);
        assert_eq!(text, "CQ");
    }

    #[test]
    fn decodes_hex_beacon_with_words() {
        let sample_rate = 48_000.0;
        let tone_hz = 700.0;
        let mut audio = Vec::new();
        for (index, word) in ["BOTAN", "JS1YPT", "S9", "0123456789ABCDEF"]
            .iter()
            .enumerate()
        {
            if index > 0 {
                audio.extend(silence(sample_rate, 7.0 * 60.0));
            }
            for (character_index, character) in word.chars().enumerate() {
                if character_index > 0 {
                    audio.extend(silence(sample_rate, 3.0 * 60.0));
                }
                let marks = encode_symbol(character).expect("morse character");
                push_marks(&mut audio, sample_rate, tone_hz, marks);
            }
        }
        audio.extend(silence(sample_rate, 500.0));

        let text = decode_text(sample_rate, tone_hz, &audio);
        assert_eq!(text, "BOTAN JS1YPT S9 0123456789ABCDEF");
    }

    #[test]
    fn decodes_with_a_short_fade_inside_a_dash() {
        let sample_rate = 48_000.0;
        let tone_hz = 700.0;
        let mut audio = Vec::new();
        // One dash, split by an 8 ms dropout. It must still decode as T.
        let dash = tone(sample_rate, tone_hz, 180.0, 0.5);
        let split = dash.len() / 2;
        audio.extend_from_slice(&dash[..split]);
        audio.extend(silence(sample_rate, 8.0));
        audio.extend_from_slice(&dash[split..]);
        audio.extend(silence(sample_rate, 500.0));

        let text = decode_text(sample_rate, tone_hz, &audio);
        assert_eq!(text, "T");
    }

    fn encode_symbol(character: char) -> Option<&'static str> {
        Some(match character {
            'A' => ".-",
            'B' => "-...",
            'C' => "-.-.",
            'D' => "-..",
            'E' => ".",
            'F' => "..-.",
            'G' => "--.",
            'H' => "....",
            'I' => "..",
            'J' => ".---",
            'K' => "-.-",
            'L' => ".-..",
            'M' => "--",
            'N' => "-.",
            'O' => "---",
            'P' => ".--.",
            'Q' => "--.-",
            'R' => ".-.",
            'S' => "...",
            'T' => "-",
            'U' => "..-",
            'V' => "...-",
            'W' => ".--",
            'X' => "-..-",
            'Y' => "-.--",
            'Z' => "--..",
            '0' => "-----",
            '1' => ".----",
            '2' => "..---",
            '3' => "...--",
            '4' => "....-",
            '5' => ".....",
            '6' => "-....",
            '7' => "--...",
            '8' => "---..",
            '9' => "----.",
            _ => return None,
        })
    }
}
