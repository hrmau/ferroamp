//! The big visuals window: a feedback effects engine.
//!
//! Each frame starts from the previous one, pushed through a warp (zoom, spin,
//! ripple...) and faded slightly. New light is then drawn on top from the audio.
//! The warp smears it into trails, so simple shapes turn into rich images.
//!
//! The warp is only evaluated at a coarse grid of points and interpolated
//! across each cell, which is what makes per-pixel motion cheap on a CPU. Warp
//! strength and fade scale with the frame time, so motion stays the same speed
//! at any frame rate.

use std::f32::consts::{PI, TAU};

use super::gfx::Canvas;
use crate::engine::spectrum::BARS;

pub const WAVE_LEN: usize = 1024;
pub const PRESET_COUNT: usize = PRESETS.len();
const GRID_X: usize = 32;
const GRID_Y: usize = 20;
const AUTO_SWITCH_SECS: f32 = 30.0;
const BLEND_SECS: f32 = 2.0;
/// After this long without audio the image has faded out and drawing can stop.
const SETTLE_SECS: f32 = 6.0;
const NOTICE_SECS: f32 = 3.0;
const QUIET_SECS: f32 = 0.1;
/// Bass must climb this fast (per second) to count as a beat.
const BEAT_RISE: f32 = 0.5;
const MAX_SPARKS: usize = 256;
const MIN_HEIGHT: usize = 135;
const MAX_HEIGHT: usize = 360;
const MAX_WIDTH: usize = 640;

/// One frame's worth of analysed audio.
pub struct AudioFrame<'a> {
    pub left: &'a [f32],
    pub right: &'a [f32],
    pub bars: &'a [f32; BARS],
    pub bass: f32,
    pub mid: f32,
    pub treble: f32,
}

/// Slow-moving values that warps respond to.
#[derive(Clone, Copy)]
struct Ctx {
    t: f32,
    bass: f32,
    mid: f32,
    treble: f32,
    beat: f32,
}

struct Spark {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    color: u32,
}

/// What a preset's drawing function can see and touch. Coordinates are centred
/// with y running down the screen, and one unit is half the window's height.
struct Frame<'a> {
    px: &'a mut [u32],
    w: usize,
    h: usize,
    left: &'a [f32],
    right: &'a [f32],
    bars: &'a [f32; BARS],
    ctx: Ctx,
    dt: f32,
    hue: f32,
    just_beat: bool,
    sparks: &'a mut Vec<Spark>,
    rng: &'a mut u32,
}

struct Preset {
    name: &'static str,
    /// Share of red, green and blue kept per frame at 60 fps.
    keep: [f32; 3],
    /// Maps a point on screen to where its colour comes from in the last frame.
    warp: fn(&Ctx, f32, f32) -> (f32, f32),
    draw: fn(&mut Frame),
}

fn rotate(x: f32, y: f32, angle: f32) -> (f32, f32) {
    let (s, c) = angle.sin_cos();
    (x * c - y * s, x * s + y * c)
}

const PRESETS: [Preset; 10] = [
    Preset {
        name: "Deep Drift",
        keep: [0.975, 0.975, 0.975],
        warp: |c, x, y| {
            let (x, y) = rotate(x, y, 0.006 + (c.t * 0.3).sin() * 0.012 + c.mid * 0.01);
            let zoom = 0.982 - c.bass * 0.03;
            (x * zoom, y * zoom)
        },
        draw: |f| wave_ring(f, 0.32 + f.ctx.beat * 0.12, 0.28, hsv(f.hue, 0.95, 0.55)),
    },
    Preset {
        name: "Whirlpool",
        keep: [0.97, 0.97, 0.975],
        warp: |c, x, y| {
            let r = (x * x + y * y).sqrt();
            let (x, y) = rotate(x, y, 0.018 / (r + 0.25) + c.bass * 0.03);
            (x * 0.988, y * 0.988)
        },
        draw: |f| {
            let offset = 0.35 + f.ctx.beat * 0.2;
            wave_line(f, offset, 0.3, hsv(f.hue, 0.9, 1.0));
            wave_line(f, -offset, 0.3, hsv(f.hue + 0.5, 0.9, 1.0));
        },
    },
    Preset {
        name: "Ripple Tank",
        keep: [0.97, 0.98, 0.988],
        warp: |c, x, y| {
            let r = (x * x + y * y).sqrt();
            let push = 0.982 + (r * 14.0 - c.t * 4.0).sin() * (0.008 + c.bass * 0.02);
            (x * push, y * push)
        },
        draw: |f| {
            wave_ring(f, 0.2 + f.ctx.bass * 0.35, 0.12, hsv(f.hue + 0.55, 0.8, 0.6));
            if f.just_beat {
                spray(f, 0.0, 0.0, 50, 1.0, hsv(f.hue + 0.2, 0.7, 1.0));
            }
            step_sparks(f);
        },
    },
    Preset {
        name: "Smoke Signal",
        keep: [0.982, 0.97, 0.955],
        warp: |c, x, y| {
            let sway = (y * 5.0 + c.t).sin() * 0.005 + (y * 11.0 - c.t * 1.7).sin() * 0.003;
            (x + sway, y + 0.009 + c.treble * 0.006)
        },
        draw: |f| {
            let color = hsv(f.hue * 0.2 + 0.03, 0.75, 1.0);
            wave_line(f, 0.9, 0.22, color);
            if f.just_beat {
                let x = rand_range(f.rng, -1.2, 1.2);
                spray(f, x, 0.9, 24, 0.5, hsv(f.hue * 0.2 + 0.08, 0.5, 1.0));
            }
            step_sparks(f);
        },
    },
    Preset {
        name: "Hall of Mirrors",
        keep: [0.975, 0.975, 0.975],
        warp: |c, x, y| {
            const SECTOR: f32 = TAU / 6.0;
            let r = (x * x + y * y).sqrt() * (0.985 - c.bass * 0.015);
            let a = (y.atan2(x) + c.t * 0.05).rem_euclid(SECTOR);
            let a = (a - SECTOR * 0.5).abs();
            let (s, co) = a.sin_cos();
            (r * co, r * s)
        },
        draw: |f| {
            wave_line(f, -0.2 + f.ctx.mid * 0.3, 0.35, hsv(f.hue, 0.8, 1.0));
            wave_ring(f, 0.55, 0.1, hsv(f.hue + 0.33, 0.8, 0.8));
        },
    },
    Preset {
        name: "Starburst",
        keep: [0.955, 0.96, 0.975],
        warp: |c, x, y| {
            let zoom = 0.955 - c.bass * 0.05;
            let (x, y) = rotate(x, y, 0.003);
            (x * zoom, y * zoom)
        },
        draw: |f| {
            let count = (f.ctx.treble * 40.0) as usize + 6;
            for _ in 0..count {
                let a = rand_range(f.rng, 0.0, TAU);
                let r = rand_range(f.rng, 0.02, 0.5);
                let color = hsv(f.hue + rand_range(f.rng, 0.0, 0.25), 0.55, 1.0);
                let (x, y) = to_px(f, a.cos() * r, a.sin() * r);
                line(f, (x, y), (x + 1, y), color);
            }
            if f.just_beat {
                wave_ring(f, 0.18, 0.1, hsv(f.hue + 0.5, 0.8, 0.9));
                spray(f, 0.0, 0.0, 90, 1.5, hsv(f.hue, 0.8, 1.0));
            }
            step_sparks(f);
        },
    },
    Preset {
        name: "Figure Skater",
        keep: [0.985, 0.98, 0.975],
        warp: |c, x, y| {
            let (x, y) = rotate(x, y, 0.004 + c.mid * 0.008);
            (x * 0.996, y * 0.996)
        },
        draw: wave_xy,
    },
    Preset {
        name: "Furnace",
        keep: [0.99, 0.945, 0.86],
        warp: |c, x, y| {
            let lick = (x * 9.0 + c.t * 2.0).sin() * 0.004 + (x * 23.0 - c.t * 3.0).sin() * 0.002;
            (x + lick, y + 0.016 + c.bass * 0.014)
        },
        draw: |f| {
            let (w, h) = (f.w as f32, f.h as f32);
            let width = w / BARS as f32;
            for bar in 0..BARS {
                let height = f.bars[bar] * h * 0.4;
                let x0 = (bar as f32 * width) as i32;
                let x1 = ((bar + 1) as f32 * width) as i32 - 1;
                for x in x0..x1 {
                    for y in (h - height) as i32..h as i32 {
                        add(f.px, f.w, f.h, x, y, 0x60_30_08);
                    }
                }
            }
        },
    },
    Preset {
        name: "Twin Vortex",
        keep: [0.988, 0.984, 0.99],
        warp: |c, x, y| {
            let orbit = 0.7 + c.bass * 0.2;
            let (ax, ay) = ((c.t * 0.4).cos() * orbit, (c.t * 0.53).sin() * 0.5);
            let twist = |x: &mut f32, y: &mut f32, cx: f32, cy: f32, dir: f32| {
                let (dx, dy) = (*x - cx, *y - cy);
                let d2 = dx * dx + dy * dy;
                let (rx, ry) = rotate(dx, dy, dir * 0.09 / (1.0 + d2 * 3.0));
                *x = cx + rx;
                *y = cy + ry;
            };
            let (mut px, mut py) = (x, y);
            twist(&mut px, &mut py, ax, ay, 1.0);
            twist(&mut px, &mut py, -ax, -ay, -1.0);
            (px * 0.988, py * 0.988)
        },
        draw: |f| {
            let (t, b) = (f.ctx.t, f.ctx.bass);
            let orbit = 0.7 + b * 0.2;
            let (ax, ay) = ((t * 0.4).cos() * orbit, (t * 0.53).sin() * 0.5);
            wave_ring_at(f, ax, ay, 0.16 + b * 0.2, 0.12, hsv(f.hue, 0.95, 0.6));
            wave_ring_at(f, -ax, -ay, 0.16 + b * 0.2, 0.12, hsv(f.hue + 0.5, 0.95, 0.6));
        },
    },
    Preset {
        name: "Heat Haze",
        keep: [0.97, 0.965, 0.97],
        warp: |c, x, y| {
            let a = (y * 7.0 + c.t * 1.3).sin() * (0.006 + c.mid * 0.012);
            let b = (x * 6.0 - c.t * 1.1).cos() * (0.006 + c.bass * 0.012);
            ((x + a) * 0.997, (y + b) * 0.997)
        },
        draw: |f| {
            let spread = 0.3 + f.ctx.beat * 0.4;
            for i in 0..3 {
                let at = (i as f32 - 1.0) * spread;
                wave_line(f, at, 0.2, hsv(f.hue + i as f32 * 0.12, 0.85, 0.9));
            }
        },
    },
];

pub struct Visualizer {
    /// What goes on screen: the image plus any text over it.
    pub canvas: Canvas,
    front: Vec<u32>,
    back: Vec<u32>,
    grid: Vec<(f32, f32)>,
    w: usize,
    h: usize,
    preset: usize,
    from_preset: usize,
    blend: f32,
    since_switch: f32,
    t: f32,
    hue: f32,
    bass: f32,
    mid: f32,
    treble: f32,
    beat: f32,
    bass_average: f32,
    last_bass: f32,
    beat_cooldown: f32,
    quiet: f32,
    sparks: Vec<Spark>,
    rng: u32,
    notice: Option<(String, f32)>,
}

impl Visualizer {
    pub fn new(window_w: u32, window_h: u32, preset: usize, seed: u32) -> Self {
        let preset = preset % PRESETS.len();
        let mut v = Self {
            canvas: Canvas::new(0, 0),
            front: Vec::new(),
            back: Vec::new(),
            grid: vec![(0.0, 0.0); (GRID_X + 1) * (GRID_Y + 1)],
            w: 0,
            h: 0,
            preset,
            from_preset: preset,
            blend: 1.0,
            since_switch: 0.0,
            t: 0.0,
            hue: 0.0,
            bass: 0.0,
            mid: 0.0,
            treble: 0.0,
            beat: 0.0,
            bass_average: 0.0,
            last_bass: 0.0,
            beat_cooldown: 0.0,
            quiet: SETTLE_SECS,
            sparks: Vec::with_capacity(MAX_SPARKS),
            rng: seed | 1,
            notice: None,
        };
        v.resize(window_w, window_h);
        v
    }

    /// Picks an internal resolution for a window's size in physical pixels. The
    /// GPU scales the result up, and a soft image suits this kind of visual.
    pub fn resize(&mut self, window_w: u32, window_h: u32) {
        let aspect = window_w.max(1) as f32 / window_h.max(1) as f32;
        let mut h = ((window_h / 2) as usize).clamp(MIN_HEIGHT, MAX_HEIGHT);
        let mut w = (h as f32 * aspect) as usize;
        if w > MAX_WIDTH {
            w = MAX_WIDTH;
            h = ((w as f32 / aspect) as usize).clamp(16, MAX_HEIGHT);
        }
        let w = w.max(16);
        if (w, h) == (self.w, self.h) {
            return;
        }
        (self.w, self.h) = (w, h);
        self.front = vec![0; w * h];
        self.back = vec![0; w * h];
        self.canvas = Canvas::new(w, h);
    }

    pub fn preset(&self) -> usize {
        self.preset
    }

    pub fn next_preset(&mut self) {
        self.switch_to((self.preset + 1) % PRESETS.len());
    }

    pub fn switch_to(&mut self, preset: usize) {
        self.from_preset = self.preset;
        self.preset = preset % PRESETS.len();
        self.blend = 0.0;
        self.since_switch = 0.0;
        self.notice = Some((PRESETS[self.preset].name.to_string(), NOTICE_SECS));
    }

    /// Shows a line of text for a few seconds, such as a new track's title.
    pub fn announce(&mut self, text: String) {
        self.notice = Some((text, NOTICE_SECS));
    }

    /// True once silence has faded the picture out and no text is left on it.
    pub fn is_settled(&self) -> bool {
        self.quiet >= SETTLE_SECS && self.notice.is_none()
    }

    /// Advances one frame. `dt` is the seconds since the previous one.
    pub fn step(&mut self, dt: f32, audio: &AudioFrame) {
        let dt = dt.clamp(0.001, 0.05);
        self.update_levels(dt, audio);
        self.t += dt;
        self.hue = (self.hue + dt * 0.04 + self.beat * dt * 0.1).fract();

        let just_beat = self.detect_beat(dt);
        self.since_switch += dt;
        if self.since_switch > AUTO_SWITCH_SECS && just_beat {
            let jump = 1 + (self.random() as usize % (PRESETS.len() - 1));
            self.switch_to((self.preset + jump) % PRESETS.len());
        }
        self.blend = (self.blend + dt / BLEND_SECS).min(1.0);

        self.warp(dt);
        std::mem::swap(&mut self.front, &mut self.back);

        // In silence nothing new is drawn, so the picture fades away by itself
        // instead of leaving the flat outline of a waveform glowing.
        if self.quiet > QUIET_SECS {
            self.sparks.clear();
            self.canvas.px.copy_from_slice(&self.front);
            self.draw_notice(dt);
            return;
        }

        let ctx = self.ctx();
        let mut frame = Frame {
            px: &mut self.front,
            w: self.w,
            h: self.h,
            left: audio.left,
            right: audio.right,
            bars: audio.bars,
            ctx,
            dt,
            hue: self.hue,
            just_beat,
            sparks: &mut self.sparks,
            rng: &mut self.rng,
        };
        (PRESETS[self.preset].draw)(&mut frame);

        self.canvas.px.copy_from_slice(&self.front);
        self.draw_notice(dt);
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            t: self.t,
            bass: self.bass,
            mid: self.mid,
            treble: self.treble,
            beat: self.beat,
        }
    }

    fn update_levels(&mut self, dt: f32, audio: &AudioFrame) {
        let clean = |v: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let follow = |current: f32, target: f32, rate: f32| {
            current + (target - current) * (1.0 - (-dt * rate).exp())
        };
        self.bass = follow(self.bass, clean(audio.bass), 18.0);
        self.mid = follow(self.mid, clean(audio.mid), 12.0);
        self.treble = follow(self.treble, clean(audio.treble), 12.0);
        let loudest = audio
            .left
            .iter()
            .chain(audio.right)
            .fold(0f32, |m, s| m.max(s.abs()));
        self.quiet = if loudest > 1e-4 { 0.0 } else { self.quiet + dt };
    }

    /// A beat is bass jumping well above its own recent average, which adapts
    /// to quiet and loud music alike. It must also still be climbing, or one
    /// sound starting from silence would read as several beats in a row.
    fn detect_beat(&mut self, dt: f32) -> bool {
        self.beat *= (-dt * 5.0).exp();
        self.beat_cooldown -= dt;
        let rising = (self.bass - self.last_bass) / dt > BEAT_RISE;
        self.last_bass = self.bass;
        let hit = rising
            && self.bass > self.bass_average * 1.35
            && self.bass > 0.04
            && self.beat_cooldown <= 0.0;
        if hit {
            self.beat = 1.0;
            self.beat_cooldown = 0.2;
        }
        self.bass_average += (self.bass - self.bass_average) * (1.0 - (-dt * 1.5).exp());
        hit
    }

    fn random(&mut self) -> u32 {
        next_random(&mut self.rng)
    }

    fn warp(&mut self, dt: f32) {
        let (w, h) = (self.w, self.h);
        let ctx = self.ctx();
        let from = &PRESETS[self.from_preset];
        let to = &PRESETS[self.preset];
        let blend = self.blend;
        let half_h = h as f32 * 0.5;
        let half_w = w as f32 * 0.5;
        let strength = dt * 60.0;

        // Where each grid point's colour comes from, in source pixels.
        for gy in 0..=GRID_Y {
            let y = (gy as f32 / GRID_Y as f32 * h as f32 - half_h) / half_h;
            for gx in 0..=GRID_X {
                let x = (gx as f32 / GRID_X as f32 * w as f32 - half_w) / half_h;
                let (mut sx, mut sy) = (to.warp)(&ctx, x, y);
                if blend < 1.0 {
                    let (ox, oy) = (from.warp)(&ctx, x, y);
                    sx = ox + (sx - ox) * blend;
                    sy = oy + (sy - oy) * blend;
                }
                let sx = x + (sx - x) * strength;
                let sy = y + (sy - y) * strength;
                self.grid[gy * (GRID_X + 1) + gx] = (sx * half_h + half_w, sy * half_h + half_h);
            }
        }

        // Fade per channel as an 8-bit fixed point factor.
        let keep: [u32; 3] = std::array::from_fn(|i| {
            let k = from.keep[i] + (to.keep[i] - from.keep[i]) * blend;
            (k.powf(strength) * 256.0) as u32
        });

        let max_u = ((w - 1) as i64) << 16;
        let max_v = ((h - 1) as i64) << 16;
        let front = &self.front;
        let mut row_u = [0f32; GRID_X + 1];
        let mut row_v = [0f32; GRID_X + 1];
        for (py, out) in self.back.chunks_exact_mut(w).enumerate() {
            let g = py as f32 / h as f32 * GRID_Y as f32;
            let j = (g as usize).min(GRID_Y - 1);
            let fy = g - j as f32;
            for i in 0..=GRID_X {
                let a = self.grid[j * (GRID_X + 1) + i];
                let b = self.grid[(j + 1) * (GRID_X + 1) + i];
                row_u[i] = a.0 + (b.0 - a.0) * fy;
                row_v[i] = a.1 + (b.1 - a.1) * fy;
            }
            for i in 0..GRID_X {
                let x0 = i * w / GRID_X;
                let x1 = (i + 1) * w / GRID_X;
                let span = (x1 - x0).max(1) as f32;
                let mut u = (row_u[i] * 65536.0) as i64;
                let mut v = (row_v[i] * 65536.0) as i64;
                let du = ((row_u[i + 1] - row_u[i]) / span * 65536.0) as i64;
                let dv = ((row_v[i + 1] - row_v[i]) / span * 65536.0) as i64;
                for slot in &mut out[x0..x1] {
                    *slot = fade(sample(front, w, h, u.clamp(0, max_u), v.clamp(0, max_v)), keep);
                    u += du;
                    v += dv;
                }
            }
        }
    }

    fn draw_notice(&mut self, dt: f32) {
        let Some((text, left)) = &mut self.notice else {
            return;
        };
        *left -= dt;
        if *left <= 0.0 {
            self.notice = None;
            return;
        }
        // Easing out gives a soft exit, and text goes on the display copy only,
        // so it never enters the feedback loop and smears.
        let v = (*left * 2.0).min(1.0);
        let color = hsv(0.0, 0.0, v);
        let y = self.h as i32 - 14;
        let max_chars = (self.w as i32 / super::font::ADVANCE - 2).max(1) as usize;
        let clipped: String = text.chars().take(max_chars).collect();
        self.canvas.text(7, y + 1, &clipped, 0);
        self.canvas.text(6, y, &clipped, color);
    }
}

/// Bilinear lookup. The two blends work on red with blue, then green, at once.
#[inline]
fn sample(px: &[u32], w: usize, h: usize, u: i64, v: i64) -> u32 {
    let (x, y) = ((u >> 16) as usize, (v >> 16) as usize);
    let fx = ((u >> 8) & 0xff) as u32;
    let fy = ((v >> 8) & 0xff) as u32;
    let x1 = (x + 1).min(w - 1);
    let y1 = (y + 1).min(h - 1);
    let top = lerp_rgb(px[y * w + x], px[y * w + x1], fx);
    let bottom = lerp_rgb(px[y1 * w + x], px[y1 * w + x1], fx);
    lerp_rgb(top, bottom, fy)
}

#[inline]
fn lerp_rgb(a: u32, b: u32, t: u32) -> u32 {
    let rb = (a & 0xff00ff) * (256 - t) + (b & 0xff00ff) * t;
    let g = (a & 0x00ff00) * (256 - t) + (b & 0x00ff00) * t;
    ((rb >> 8) & 0xff00ff) | ((g >> 8) & 0x00ff00)
}

/// Rounding down means every lit pixel reaches true black in the end.
#[inline]
fn fade(p: u32, keep: [u32; 3]) -> u32 {
    let r = (((p >> 16) & 255) * keep[0]) >> 8;
    let g = (((p >> 8) & 255) * keep[1]) >> 8;
    let b = ((p & 255) * keep[2]) >> 8;
    (r << 16) | (g << 8) | b
}

#[inline]
fn add(px: &mut [u32], w: usize, h: usize, x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
        return;
    }
    let slot = &mut px[y as usize * w + x as usize];
    let p = *slot;
    let r = (((p >> 16) & 255) + ((color >> 16) & 255)).min(255);
    let g = (((p >> 8) & 255) + ((color >> 8) & 255)).min(255);
    let b = ((p & 255) + (color & 255)).min(255);
    *slot = (r << 16) | (g << 8) | b;
}

pub fn hsv(h: f32, s: f32, v: f32) -> u32 {
    let h = h.rem_euclid(1.0) * 6.0;
    let i = h as u32;
    let f = h - i as f32;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
    let (r, g, b) = match i {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0) as u32;
    (byte(r) << 16) | (byte(g) << 8) | byte(b)
}

fn next_random(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

fn rand_range(rng: &mut u32, from: f32, to: f32) -> f32 {
    from + (next_random(rng) as f32 / u32::MAX as f32) * (to - from)
}

/// Audio values can be anything in a damaged file. Keep shapes on the screen.
#[inline]
fn clean(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.5, 1.5)
    } else {
        0.0
    }
}

fn to_px(f: &Frame, x: f32, y: f32) -> (i32, i32) {
    let half_h = f.h as f32 * 0.5;
    ((x * half_h + f.w as f32 * 0.5) as i32, ((y + 1.0) * half_h) as i32)
}

fn plot(f: &mut Frame, x: f32, y: f32, color: u32) {
    let (px, py) = to_px(f, x, y);
    add(f.px, f.w, f.h, px, py, color);
}

/// Additive line, two pixels thick for presence at low resolution.
fn line(f: &mut Frame, from: (i32, i32), to: (i32, i32), color: u32) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let steps = dx.abs().max(dy.abs()).clamp(1, 2048);
    let steep = dy.abs() > dx.abs();
    for i in 0..=steps {
        let x = from.0 + dx * i / steps;
        let y = from.1 + dy * i / steps;
        add(f.px, f.w, f.h, x, y, color);
        if steep {
            add(f.px, f.w, f.h, x + 1, y, color);
        } else {
            add(f.px, f.w, f.h, x, y + 1, color);
        }
    }
}

fn wave_line(f: &mut Frame, at_y: f32, amplitude: f32, color: u32) {
    const POINTS: usize = 192;
    let half_w = f.w as f32 / f.h as f32;
    let mut last = None;
    for i in 0..POINTS {
        let t = i as f32 / (POINTS - 1) as f32;
        let s = clean(f.left[(t * (f.left.len() - 1) as f32) as usize]);
        // Pinned at the edges, so the line reads as a string being plucked.
        let taper = (t * PI).sin();
        let point = to_px(f, (t * 2.0 - 1.0) * half_w, at_y + s * amplitude * taper);
        if let Some(prev) = last {
            line(f, prev, point, color);
        }
        last = Some(point);
    }
}

fn wave_ring(f: &mut Frame, radius: f32, amplitude: f32, color: u32) {
    wave_ring_at(f, 0.0, 0.0, radius, amplitude, color);
}

fn wave_ring_at(f: &mut Frame, cx: f32, cy: f32, radius: f32, amplitude: f32, color: u32) {
    const POINTS: usize = 160;
    let mut first = None;
    let mut last = None;
    for i in 0..POINTS {
        let t = i as f32 / POINTS as f32;
        // Mirrored through the buffer so that the two ends of the ring meet.
        let m = 1.0 - (t * 2.0 - 1.0).abs();
        let s = clean(f.right[(m * (f.right.len() - 1) as f32) as usize]);
        let (sin, cos) = (t * TAU).sin_cos();
        let r = radius + s * amplitude;
        let point = to_px(f, cx + cos * r, cy + sin * r);
        if let Some(prev) = last {
            line(f, prev, point, color);
        } else {
            first = Some(point);
        }
        last = Some(point);
    }
    if let (Some(a), Some(b)) = (last, first) {
        line(f, a, b, color);
    }
}

/// Left against right. Wide stereo draws loops, and mono draws a line.
fn wave_xy(f: &mut Frame) {
    let color = hsv(f.hue, 0.95, 0.3);
    let scale = 0.8 + f.ctx.beat * 0.35;
    let n = f.left.len().min(f.right.len());
    let step = (n / 400).max(1);
    let spin = f.ctx.t * 0.3;
    let mut last = None;
    for i in (n / 4..n).step_by(step) {
        let (x, y) = rotate(clean(f.left[i]) * scale, clean(f.right[i]) * scale, spin);
        let point = to_px(f, x, y);
        if let Some(prev) = last {
            line(f, prev, point, color);
        }
        last = Some(point);
    }
}

fn spray(f: &mut Frame, x: f32, y: f32, count: usize, speed: f32, color: u32) {
    for _ in 0..count {
        if f.sparks.len() >= MAX_SPARKS {
            return;
        }
        let a = rand_range(f.rng, 0.0, TAU);
        let v = rand_range(f.rng, 0.3, 1.0) * speed;
        let life = rand_range(f.rng, 0.3, 0.9);
        f.sparks.push(Spark {
            x,
            y,
            vx: a.cos() * v,
            vy: a.sin() * v,
            life,
            color,
        });
    }
}

fn step_sparks(f: &mut Frame) {
    let dt = f.dt;
    let mut sparks = std::mem::take(f.sparks);
    sparks.retain_mut(|s| {
        s.x += s.vx * dt;
        s.y += s.vy * dt;
        s.life -= dt;
        s.life > 0.0
    });
    for s in &sparks {
        plot(f, s.x, s.y, s.color);
    }
    *f.sparks = sparks;
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    /// Four-on-the-floor kick with a stereo melody over it.
    fn music(frame: usize) -> (Vec<f32>, Vec<f32>, f32, f32, f32) {
        let t = frame as f32 * DT;
        let since_kick = t.rem_euclid(0.5);
        let bass = (1.0 - since_kick * 5.0).max(0.0);
        let left: Vec<f32> = (0..WAVE_LEN)
            .map(|i| {
                let x = t + i as f32 / 44_100.0;
                (x * 660.0 * TAU).sin() * 0.4 + (x * 90.0 * TAU).sin() * bass * 0.6
            })
            .collect();
        let right: Vec<f32> = (0..WAVE_LEN)
            .map(|i| {
                let x = t + i as f32 / 44_100.0;
                (x * 990.0 * TAU).cos() * 0.4 + (x * 90.0 * TAU).sin() * bass * 0.6
            })
            .collect();
        (left, right, bass, 0.4, 0.2 + (t * 3.0).sin().abs() * 0.4)
    }

    fn run(v: &mut Visualizer, frames: std::ops::Range<usize>) {
        let bars = [0.5; BARS];
        for i in frames {
            let (left, right, bass, mid, treble) = music(i);
            v.step(
                DT,
                &AudioFrame {
                    left: &left,
                    right: &right,
                    bars: &bars,
                    bass,
                    mid,
                    treble,
                },
            );
        }
    }

    fn silence(v: &mut Visualizer, frames: usize) {
        let z = vec![0.0; WAVE_LEN];
        let bars = [0.0; BARS];
        for _ in 0..frames {
            v.step(
                DT,
                &AudioFrame {
                    left: &z,
                    right: &z,
                    bars: &bars,
                    bass: 0.0,
                    mid: 0.0,
                    treble: 0.0,
                },
            );
        }
    }

    fn lit(px: &[u32]) -> usize {
        px.iter().filter(|&&p| p != 0).count()
    }

    #[test]
    fn every_preset_draws_something_with_music() {
        for preset in 0..PRESETS.len() {
            let mut v = Visualizer::new(960, 540, preset, 7);
            run(&mut v, 0..120);
            let share = lit(&v.front) as f32 / v.front.len() as f32;
            assert!(share > 0.01, "{} lit only {share}", PRESETS[preset].name);
        }
    }

    #[test]
    fn every_preset_fades_to_true_black_before_drawing_stops() {
        for preset in 0..PRESETS.len() {
            let mut v = Visualizer::new(960, 540, preset, 7);
            run(&mut v, 0..120);
            let mut frames = 0;
            v.notice = None;
            while !v.is_settled() {
                silence(&mut v, 1);
                frames += 1;
                assert!(frames < 2000, "{} never settled", PRESETS[preset].name);
            }
            assert_eq!(lit(&v.canvas.px), 0, "{} left light behind", PRESETS[preset].name);
        }
    }

    #[test]
    fn beats_follow_the_kick_not_a_steady_tone() {
        let mut v = Visualizer::new(960, 540, 0, 7);
        let z = vec![0.1; WAVE_LEN];
        let bars = [0.0; BARS];
        let count = |v: &mut Visualizer, frames: usize, level: &dyn Fn(usize) -> f32| {
            let mut beats = 0;
            for i in 0..frames {
                v.step(
                    DT,
                    &AudioFrame {
                        left: &z,
                        right: &z,
                        bars: &bars,
                        bass: level(i),
                        mid: 0.0,
                        treble: 0.0,
                    },
                );
                // The envelope is exactly 1 only on the frame a beat lands.
                if v.beat == 1.0 {
                    beats += 1;
                }
            }
            beats
        };
        let settle = count(&mut v, 300, &|_| 0.5);
        assert!(settle <= 2, "a steady tone made {settle} beats");
        let steady = count(&mut v, 300, &|_| 0.5);
        assert_eq!(steady, 0, "a steady tone kept beating");
        // Eight kicks in four seconds, at 120 bpm.
        let kicks = count(&mut v, 240, &|i| if i % 30 < 6 { 0.9 } else { 0.1 });
        assert!((6..=10).contains(&kicks), "{kicks} beats from 8 kicks");
    }

    #[test]
    fn odd_sizes_and_damaged_audio_cannot_break_it() {
        let bad = vec![f32::NAN, f32::INFINITY, -1e30, 1e30];
        let bars = [f32::NAN; BARS];
        for (w, h) in [(1, 1), (3000, 2), (2, 3000), (401, 233), (5120, 2880)] {
            for preset in 0..PRESETS.len() {
                let mut v = Visualizer::new(w, h, preset, 7);
                for _ in 0..30 {
                    v.step(
                        DT,
                        &AudioFrame {
                            left: &bad,
                            right: &bad,
                            bars: &bars,
                            bass: f32::NAN,
                            mid: f32::INFINITY,
                            treble: -3.0,
                        },
                    );
                }
                assert!(v.w * v.h <= MAX_WIDTH * MAX_HEIGHT);
            }
        }
    }

    #[test]
    fn switching_presets_mid_blend_and_resizing_are_safe() {
        let mut v = Visualizer::new(960, 540, 0, 7);
        for i in 0..PRESETS.len() * 2 {
            run(&mut v, i * 10..i * 10 + 10);
            v.next_preset();
            v.resize(960 + i as u32 * 37, 540);
        }
    }

    #[test]
    fn auto_switch_waits_for_a_beat_after_its_time_is_up() {
        let mut v = Visualizer::new(320, 180, 0, 7);
        run(&mut v, 0..(AUTO_SWITCH_SECS as usize - 1) * 60);
        assert_eq!(v.preset(), 0, "switched early");
        run(&mut v, 2000..2000 + 3 * 60);
        assert_ne!(v.preset(), 0, "never switched");
    }
}
