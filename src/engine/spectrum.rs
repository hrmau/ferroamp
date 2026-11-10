//! Turns the audio tap into the bars the main window draws.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

const TAP_FRAMES: usize = 2048;
pub const FFT_SIZE: usize = 1024;
pub const BARS: usize = 19;

/// Lock-free overwrite buffer: the audio callback writes, the UI reads the most
/// recent window. A torn read costs one slightly wrong sample in a visualiser.
pub struct Tap {
    /// Interleaved left and right.
    samples: [AtomicU32; TAP_FRAMES * 2],
    head: AtomicUsize,
}

impl Default for Tap {
    fn default() -> Self {
        Self {
            samples: std::array::from_fn(|_| AtomicU32::new(0)),
            head: AtomicUsize::new(0),
        }
    }
}

impl Tap {
    #[inline]
    pub fn push(&self, left: f32, right: f32) {
        let head = self.head.load(Ordering::Relaxed);
        let slot = head % TAP_FRAMES * 2;
        self.samples[slot].store(left.to_bits(), Ordering::Relaxed);
        self.samples[slot + 1].store(right.to_bits(), Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    fn frame(&self, index: usize) -> (f32, f32) {
        let slot = index % TAP_FRAMES * 2;
        (
            f32::from_bits(self.samples[slot].load(Ordering::Relaxed)),
            f32::from_bits(self.samples[slot + 1].load(Ordering::Relaxed)),
        )
    }

    /// The most recent samples mixed to mono, oldest first.
    pub fn read_mono(&self, out: &mut [f32]) {
        let start = self.head.load(Ordering::Acquire).wrapping_sub(out.len());
        for (i, slot) in out.iter_mut().enumerate() {
            let (l, r) = self.frame(start.wrapping_add(i));
            *slot = (l + r) * 0.5;
        }
    }

    /// The most recent samples per channel, oldest first.
    pub fn read_stereo(&self, left: &mut [f32], right: &mut [f32]) {
        let start = self.head.load(Ordering::Acquire).wrapping_sub(left.len());
        for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
            (*l, *r) = self.frame(start.wrapping_add(i));
        }
    }
}

pub struct Analyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Bar heights and falling peak caps, both 0..=1.
    pub bars: [f32; BARS],
    pub peaks: [f32; BARS],
    /// Linear energy in three broad bands, roughly 0..=1, for beat-driven visuals.
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
}

impl Default for Analyzer {
    fn default() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| {
                let x = std::f32::consts::TAU * i as f32 / (FFT_SIZE - 1) as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        Self {
            input: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            window,
            bars: [0.0; BARS],
            peaks: [0.0; BARS],
            bass: 0.0,
            mid: 0.0,
            treble: 0.0,
        }
    }
}

impl Analyzer {
    /// Captures the tap and moves the bars. `dt` is the seconds since last call.
    pub fn update(&mut self, tap: &Tap, rate: u32, dt: f32) {
        tap.read_mono(&mut self.input);
        self.analyse(rate, dt);
    }

    fn analyse(&mut self, rate: u32, dt: f32) {
        for (s, w) in self.input.iter_mut().zip(&self.window) {
            *s *= w;
        }
        if self
            .fft
            .process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch)
            .is_err()
        {
            return;
        }
        let bin_hz = rate as f32 / FFT_SIZE as f32;
        let (lo, hi) = (50f32, 16_000f32.min(rate as f32 * 0.5));
        // Hann coherent gain is 0.5, so a full-scale sine sits at N/4.
        let reference = FFT_SIZE as f32 * 0.25;
        let band = |spectrum: &[Complex<f32>], from: f32, to: f32| {
            let start = ((from / bin_hz) as usize).min(spectrum.len() - 1);
            let end = ((to / bin_hz) as usize).clamp(start + 1, spectrum.len());
            let sum: f32 = spectrum[start..end].iter().map(|c| c.norm_sqr()).sum();
            ((sum / (end - start) as f32).sqrt() / reference).min(1.0)
        };
        self.bass = band(&self.spectrum, 30.0, 180.0);
        // Higher bands hold far less energy, so they get lifted to a similar range.
        self.mid = (band(&self.spectrum, 180.0, 2_000.0) * 3.0).min(1.0);
        self.treble = (band(&self.spectrum, 2_000.0, 14_000.0) * 8.0).min(1.0);
        for bar in 0..BARS {
            let edge = |b: f32| lo * (hi / lo).powf(b / BARS as f32);
            let start = (edge(bar as f32) / bin_hz) as usize;
            let end = ((edge(bar as f32 + 1.0) / bin_hz).ceil() as usize)
                .max(start + 1)
                .min(self.spectrum.len());
            let peak = self.spectrum[start.min(end - 1)..end]
                .iter()
                .map(|c| c.norm_sqr())
                .fold(0f32, f32::max)
                .sqrt();
            let db = 20.0 * (peak / reference).max(1e-6).log10();
            let level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            // Fast attack, gravity on the way down, slower for the caps.
            let fallen = self.bars[bar] - dt * 2.2;
            self.bars[bar] = level.max(fallen).max(0.0);
            let cap = self.peaks[bar] - dt * 0.55;
            self.peaks[bar] = self.bars[bar].max(cap).max(0.0);
        }
    }

    /// Lets the display settle to nothing when audio stops.
    pub fn decay(&mut self, dt: f32) -> bool {
        (self.bass, self.mid, self.treble) = (0.0, 0.0, 0.0);
        let mut moving = false;
        for bar in 0..BARS {
            self.bars[bar] = (self.bars[bar] - dt * 2.2).max(0.0);
            self.peaks[bar] = (self.peaks[bar] - dt * 0.55).max(self.bars[bar]);
            moving |= self.peaks[bar] > 0.0;
        }
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars_for_sine(freq: f32) -> [f32; BARS] {
        let tap = Tap::default();
        for i in 0..TAP_FRAMES {
            let s = (std::f32::consts::TAU * freq * i as f32 / 44_100.0).sin();
            tap.push(s, s);
        }
        let mut a = Analyzer::default();
        a.update(&tap, 44_100, 1.0);
        a.bars
    }

    #[test]
    fn low_tone_lights_low_bars_only() {
        let bars = bars_for_sine(200.0);
        assert!(bars[..6].iter().cloned().fold(0.0, f32::max) > 0.9);
        assert!(bars[14..].iter().cloned().fold(0.0, f32::max) < 0.5);
    }

    #[test]
    fn high_tone_lights_high_bars_only() {
        let bars = bars_for_sine(9_000.0);
        assert!(bars[13..].iter().cloned().fold(0.0, f32::max) > 0.9);
        assert!(bars[..6].iter().cloned().fold(0.0, f32::max) < 0.5);
    }

    #[test]
    fn stereo_channels_stay_separate_and_in_order() {
        let tap = Tap::default();
        for i in 0..5000 {
            tap.push(i as f32, -(i as f32));
        }
        let (mut l, mut r) = ([0.0; 3], [0.0; 3]);
        tap.read_stereo(&mut l, &mut r);
        assert_eq!(l, [4997.0, 4998.0, 4999.0]);
        assert_eq!(r, [-4997.0, -4998.0, -4999.0]);
        let mut mono = [1.0; 2];
        tap.read_mono(&mut mono);
        assert_eq!(mono, [0.0, 0.0]);
    }

    #[test]
    fn bands_separate_low_from_high() {
        let tone = |freq: f32| {
            let tap = Tap::default();
            for i in 0..TAP_FRAMES {
                let s = (std::f32::consts::TAU * freq * i as f32 / 44_100.0).sin();
                tap.push(s, s);
            }
            let mut a = Analyzer::default();
            a.update(&tap, 44_100, 0.03);
            a
        };
        let low = tone(80.0);
        assert!(low.bass > 0.2 && low.bass > low.treble * 4.0, "{} {}", low.bass, low.treble);
        let high = tone(6_000.0);
        assert!(high.treble > 0.2 && high.treble > high.bass * 4.0, "{} {}", high.treble, high.bass);
    }

    #[test]
    fn silence_is_empty_and_decay_finishes() {
        let tap = Tap::default();
        let mut a = Analyzer::default();
        a.update(&tap, 44_100, 0.1);
        assert_eq!(a.bars, [0.0; BARS]);
        a.bars = [1.0; BARS];
        a.peaks = [1.0; BARS];
        let mut frames = 0;
        while a.decay(0.1) {
            frames += 1;
            assert!(frames < 100);
        }
    }
}
