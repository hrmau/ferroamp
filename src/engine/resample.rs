//! Streaming wrapper around rubato's FFT resampler for interleaved stereo.
//!
//! The instance is deliberately kept alive across consecutive tracks at the
//! same source rate, so a 44.1 kHz album on a 48 kHz device stays gapless.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler as _};

const CHUNK: usize = 1024;

pub struct Resampler {
    fft: Fft<f32>,
    ratio: f64,
    input: Vec<f32>,
    output: Vec<f32>,
    /// Output frames still to drop: the filter's startup delay.
    skip: usize,
    frames_in: u64,
    frames_out: u64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Result<Self, String> {
        let fft = Fft::<f32>::new(from as usize, to as usize, CHUNK, 2, FixedSync::Input)
            .map_err(|e| format!("resampler: {e}"))?;
        let skip = fft.output_delay();
        let output = vec![0.0; fft.output_frames_max() * 2];
        Ok(Self {
            fft,
            ratio: to as f64 / from as f64,
            input: Vec::with_capacity(CHUNK * 4),
            output,
            skip,
            frames_in: 0,
            frames_out: 0,
        })
    }

    /// Output frames between the next input frame and its audible result.
    pub fn latency_frames(&self) -> usize {
        if self.frames_in == 0 {
            return 0;
        }
        self.fft.output_delay() + ((self.input.len() / 2) as f64 * self.ratio) as usize
    }

    pub fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) {
        self.input.extend_from_slice(samples);
        self.frames_in += (samples.len() / 2) as u64;
        let need = self.fft.input_frames_next() * 2;
        let mut pos = 0;
        while self.input.len() - pos >= need {
            let chunk: &[f32] = &self.input[pos..pos + need];
            Self::run(
                &mut self.fft,
                chunk,
                need / 2,
                &mut self.output,
                &mut self.skip,
                &mut self.frames_out,
                u64::MAX,
                out,
            );
            pos += need;
        }
        self.input.drain(..pos);
    }

    /// Drains the filter at the end of a stream and readies it for a new one.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let target = (self.frames_in as f64 * self.ratio).round() as u64;
        let need = self.fft.input_frames_next() * 2;
        self.input.resize(need, 0.0);
        let silence = vec![0.0f32; need];
        let mut first = true;
        // The bound only guards against a pathological filter delay.
        for _ in 0..64 {
            if self.frames_out >= target {
                break;
            }
            let chunk: &[f32] = if first { &self.input } else { &silence };
            Self::run(
                &mut self.fft,
                chunk,
                need / 2,
                &mut self.output,
                &mut self.skip,
                &mut self.frames_out,
                target,
                out,
            );
            first = false;
        }
        self.reset();
    }

    /// Forgets all buffered audio, for a seek or a hard track change.
    pub fn reset(&mut self) {
        self.fft.reset();
        self.input.clear();
        self.skip = self.fft.output_delay();
        self.frames_in = 0;
        self.frames_out = 0;
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        fft: &mut Fft<f32>,
        chunk: &[f32],
        frames: usize,
        output: &mut [f32],
        skip: &mut usize,
        frames_out: &mut u64,
        limit: u64,
        out: &mut Vec<f32>,
    ) {
        let cap = output.len() / 2;
        let (Ok(input), Ok(mut dst)) = (
            InterleavedSlice::new(chunk, 2, frames),
            InterleavedSlice::new_mut(output, 2, cap),
        ) else {
            return;
        };
        let Ok((_, produced)) = fft.process_into_buffer(&input, &mut dst, None::<&Indexing>) else {
            return;
        };
        let dropped = produced.min(*skip);
        *skip -= dropped;
        let keep = ((produced - dropped) as u64).min(limit.saturating_sub(*frames_out)) as usize;
        *frames_out += keep as u64;
        out.extend_from_slice(&output[dropped * 2..(dropped + keep) * 2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (std::f64::consts::TAU * freq * i as f64 / rate as f64).sin() as f32;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn output_length_tracks_ratio_exactly() {
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let mut out = Vec::new();
        for chunk in sine(44_100, 440.0, 44_100).chunks(1234) {
            r.push(chunk, &mut out);
        }
        r.flush(&mut out);
        assert_eq!(out.len() / 2, 48_000);
    }

    #[test]
    fn sine_stays_in_phase_across_the_conversion() {
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let mut out = Vec::new();
        r.push(&sine(44_100, 440.0, 44_100), &mut out);
        r.flush(&mut out);
        let want = sine(48_000, 440.0, 48_000);
        // Ignore the edges, where the filter sees the signal start and stop.
        let worst = out[4000..90_000]
            .iter()
            .zip(&want[4000..90_000])
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 0.01, "worst sample error {worst}");
    }
}
