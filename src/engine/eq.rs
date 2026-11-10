//! 10-band graphic equaliser: a cascade of RBJ peaking biquads.
//!
//! The UI writes gains into `EqParams` (atomics). The audio callback owns an
//! `EqProcessor` and recomputes coefficients only when the generation counter
//! moves, so the callback never locks or allocates.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const BANDS: usize = 10;
pub const FREQS: [f32; BANDS] = [
    60.0, 170.0, 310.0, 600.0, 1000.0, 3000.0, 6000.0, 12000.0, 14000.0, 16000.0,
];
pub const MAX_DB: f32 = 12.0;
const Q: f32 = 1.4;

pub struct EqParams {
    enabled: AtomicBool,
    preamp_db: AtomicU32,
    gains_db: [AtomicU32; BANDS],
    generation: AtomicU32,
}

impl Default for EqParams {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            preamp_db: AtomicU32::new(0f32.to_bits()),
            gains_db: std::array::from_fn(|_| AtomicU32::new(0f32.to_bits())),
            generation: AtomicU32::new(1),
        }
    }
}

impl EqParams {
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.bump();
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
    pub fn set_preamp(&self, db: f32) {
        self.preamp_db
            .store(db.clamp(-MAX_DB, MAX_DB).to_bits(), Ordering::Relaxed);
        self.bump();
    }
    pub fn preamp(&self) -> f32 {
        f32::from_bits(self.preamp_db.load(Ordering::Relaxed))
    }
    pub fn set_gain(&self, band: usize, db: f32) {
        self.gains_db[band].store(db.clamp(-MAX_DB, MAX_DB).to_bits(), Ordering::Relaxed);
        self.bump();
    }
    pub fn gain(&self, band: usize) -> f32 {
        f32::from_bits(self.gains_db[band].load(Ordering::Relaxed))
    }
    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }
}

#[derive(Clone, Copy, Default)]
struct Coefs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

fn peaking(rate: f32, freq: f32, db: f32) -> Coefs {
    let a = 10f32.powf(db / 40.0);
    let w0 = std::f32::consts::TAU * freq / rate;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * Q);
    let a0 = 1.0 + alpha / a;
    Coefs {
        b0: (1.0 + alpha * a) / a0,
        b1: (-2.0 * cos) / a0,
        b2: (1.0 - alpha * a) / a0,
        a1: (-2.0 * cos) / a0,
        a2: (1.0 - alpha / a) / a0,
    }
}

/// Real-time half of the EQ. Owned by the audio callback.
pub struct EqProcessor {
    rate: f32,
    generation: u32,
    active: bool,
    preamp: f32,
    /// Bands at 0 dB and above Nyquist are skipped, so only `count` are live.
    coefs: [Coefs; BANDS],
    count: usize,
    /// Transposed direct form II state: [band][channel][z1, z2].
    state: [[[f32; 2]; 2]; BANDS],
}

impl EqProcessor {
    pub fn new(rate: u32) -> Self {
        Self {
            rate: rate as f32,
            generation: 0,
            active: false,
            preamp: 1.0,
            coefs: [Coefs::default(); BANDS],
            count: 0,
            state: [[[0.0; 2]; 2]; BANDS],
        }
    }

    /// Picks up new parameters if the UI changed anything. Allocation free.
    #[inline]
    pub fn sync(&mut self, params: &EqParams) {
        let generation = params.generation.load(Ordering::Acquire);
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        self.active = params.enabled();
        self.preamp = 10f32.powf(params.preamp() / 20.0);
        let mut count = 0;
        for (band, &freq) in FREQS.iter().enumerate() {
            let db = params.gain(band);
            if db.abs() >= 0.05 && freq < self.rate * 0.49 {
                self.coefs[count] = peaking(self.rate, freq, db);
                count += 1;
            }
        }
        self.count = count;
        // Slots are reassigned, so old state belongs to a different filter.
        // Zeroing can click very faintly mid-drag but cannot blow up.
        self.state = [[[0.0; 2]; 2]; BANDS];
    }

    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        if !self.active {
            return (l, r);
        }
        let mut out = [l * self.preamp, r * self.preamp];
        for band in 0..self.count {
            let c = self.coefs[band];
            let st = &mut self.state[band];
            for ch in 0..2 {
                let x = out[ch];
                let y = c.b0 * x + st[ch][0];
                st[ch][0] = c.b1 * x - c.a1 * y + st[ch][1];
                st[ch][1] = c.b2 * x - c.a2 * y;
                out[ch] = y;
            }
        }
        (out[0], out[1])
    }
}

/// Preset name and (preamp, ten band gains) in dB.
pub const PRESETS: &[(&str, f32, [f32; BANDS])] = &[
    ("FLAT", 0.0, [0.0; BANDS]),
    ("WARM", -1.0, [3.0, 3.0, 2.0, 1.0, 0.0, -1.0, -1.0, -1.0, -2.0, -2.0]),
    ("BRIGHT", -1.0, [-2.0, -2.0, -1.0, 0.0, 0.0, 1.0, 3.0, 4.0, 4.0, 4.0]),
    ("BASS BOOST", -4.0, [7.0, 6.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("LOUD", -4.0, [5.0, 4.0, 1.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0, 5.0]),
    ("VOCAL", -1.0, [-3.0, -2.0, -1.0, 2.0, 4.0, 4.0, 2.0, 0.0, -1.0, -2.0]),
    ("ACOUSTIC", -1.0, [3.0, 2.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 2.0, 1.0]),
    ("ELECTRONIC", -3.0, [6.0, 5.0, 1.0, 0.0, -2.0, 1.0, 1.0, 2.0, 5.0, 6.0]),
    ("LATE NIGHT", 0.0, [-4.0, -3.0, -1.0, 0.0, 1.0, 1.0, 0.0, -1.0, -3.0, -4.0]),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a sine through the EQ and returns its gain in dB after settling.
    fn gain_at(freq: f32, setup: impl Fn(&EqParams)) -> f32 {
        let rate = 48_000u32;
        let params = EqParams::default();
        params.set_enabled(true);
        setup(&params);
        let mut eq = EqProcessor::new(rate);
        eq.sync(&params);
        let n = rate as usize;
        let mut peak = 0f32;
        for i in 0..n {
            let x = (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin();
            let (y, _) = eq.process(x, x);
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        20.0 * peak.log10()
    }

    #[test]
    fn flat_is_transparent() {
        assert!(gain_at(1000.0, |_| {}).abs() < 0.01);
    }

    #[test]
    fn band_boost_hits_target_at_its_centre() {
        let db = gain_at(1000.0, |p| p.set_gain(4, 6.0));
        assert!((db - 6.0).abs() < 0.2, "got {db}");
    }

    #[test]
    fn band_cut_leaves_distant_frequencies_alone() {
        let db = gain_at(12_000.0, |p| p.set_gain(0, -12.0));
        assert!(db.abs() < 0.2, "got {db}");
    }

    #[test]
    fn preamp_scales_everything() {
        let db = gain_at(440.0, |p| p.set_preamp(-6.0));
        assert!((db + 6.0).abs() < 0.05, "got {db}");
    }

    #[test]
    fn disabled_bypasses_gains() {
        let params = EqParams::default();
        params.set_gain(4, 12.0);
        let mut eq = EqProcessor::new(48_000);
        eq.sync(&params);
        assert_eq!(eq.process(0.5, -0.5), (0.5, -0.5));
    }
}
