//! A tiny software canvas. Everything on screen is drawn here in low resolution
//! "art pixels", then scaled up by a whole number so edges stay crisp.

use super::font;
use super::sysfont::{OVERHANG, Run};

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0; w * h],
        }
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let (x0, y0) = (x.max(0) as usize, y.max(0) as usize);
        let x1 = ((x + w).max(0) as usize).min(self.w);
        let y1 = ((y + h).max(0) as usize).min(self.h);
        if x0 >= x1 {
            return;
        }
        for row in y0..y1 {
            self.px[row * self.w + x0..row * self.w + x1].fill(color);
        }
    }

    #[inline]
    pub fn set(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.px[y as usize * self.w + x as usize] = color;
        }
    }

    pub fn vgradient(&mut self, x: i32, y: i32, w: i32, h: i32, top: u32, bottom: u32) {
        for i in 0..h {
            let t = i as f32 / (h - 1).max(1) as f32;
            self.fill(x, y + i, w, 1, mix(top, bottom, t));
        }
    }

    /// Raised 3D edge, for buttons and panels.
    pub fn bevel(&mut self, x: i32, y: i32, w: i32, h: i32, hi: u32, lo: u32) {
        self.fill(x, y, w, 1, hi);
        self.fill(x, y, 1, h, hi);
        self.fill(x, y + h - 1, w, 1, lo);
        self.fill(x + w - 1, y, 1, h, lo);
    }

    pub fn text(&mut self, x: i32, y: i32, s: &str, color: u32) {
        self.text_between(x, y, i32::MIN, i32::MAX, s, color);
    }

    pub fn text_right(&mut self, right: i32, y: i32, s: &str, color: u32) {
        self.text(right - (font::width(s) - 1), y, s, color);
    }

    pub fn text_center(&mut self, cx: i32, y: i32, s: &str, color: u32) {
        self.text(cx - (font::width(s) - 1) / 2, y, s, color);
    }

    /// Text in a space `max` wide, ending in `~` when it doesn't all fit.
    pub fn text_fit(&mut self, x: i32, y: i32, max: i32, s: &str, color: u32) {
        if font::width(s) - 1 <= max {
            self.text(x, y, s, color);
        } else {
            let cut = ((max - font::ADVANCE) / font::ADVANCE) * font::ADVANCE;
            self.text_between(x, y, x, x + cut, s, color);
            self.text(x + cut, y, "~", color);
        }
    }

    /// Draws `s` from `x`, showing only the columns from `left` up to `right`.
    pub fn text_between(&mut self, x: i32, y: i32, left: i32, right: i32, s: &str, color: u32) {
        let mut cx = x;
        if s.is_ascii() {
            for ch in s.chars() {
                if cx >= right {
                    return;
                }
                self.glyph_cols(cx, y, left, right, font::glyph(ch), color);
                cx += font::ADVANCE;
            }
            return;
        }
        for piece in font::pieces(s) {
            if cx >= right {
                return;
            }
            match &piece {
                font::Piece::Glyph(cols) => self.glyph_cols(cx, y, left, right, *cols, color),
                font::Piece::Run(run) => self.blend_run(cx, y - OVERHANG, left, right, run, color),
            }
            cx += piece.advance();
        }
    }

    fn glyph_cols(&mut self, x: i32, y: i32, left: i32, right: i32, cols: [u8; 5], color: u32) {
        for (i, col) in cols.iter().enumerate() {
            let px = x + i as i32;
            if px < left || px >= right {
                continue;
            }
            for cy in 0..8 {
                if col & (1 << cy) != 0 {
                    self.set(px, y + cy, color);
                }
            }
        }
    }

    fn blend_run(&mut self, x: i32, top: i32, left: i32, right: i32, run: &Run, color: u32) {
        let width = run.width as usize;
        for (row, line) in run.alpha.chunks_exact(width.max(1)).enumerate() {
            let py = top + row as i32;
            if py < 0 || py as usize >= self.h {
                continue;
            }
            for (col, &alpha) in line.iter().enumerate() {
                let px = x + col as i32;
                if alpha == 0 || px < left || px >= right || px < 0 || px as usize >= self.w {
                    continue;
                }
                let at = py as usize * self.w + px as usize;
                self.px[at] = mix(self.px[at], color, alpha as f32 / 255.0);
            }
        }
    }

    /// Text scrolled by `offset` pixels and clipped to the box. `s` must wrap
    /// seamlessly, which the caller arranges by appending a separator.
    pub fn ticker(&mut self, x: i32, y: i32, w: i32, s: &str, offset: i32, color: u32) {
        let period = font::width(s);
        if period == 0 {
            return;
        }
        let mut cx = x - offset.rem_euclid(period);
        while cx < x + w {
            self.text_between(cx, y, x, x + w, s, color);
            cx += period;
        }
    }

    /// Seven-segment digit, 9 by 15. `on` is lit, `off` is the faint unlit
    /// segment an LCD shows. `None` draws only the unlit segments.
    pub fn seg_digit(&mut self, x: i32, y: i32, digit: Option<u8>, on: u32, off: u32) {
        const MAP: [u8; 10] = [
            0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110, 0b1101101, 0b1111101, 0b0000111,
            0b1111111, 0b1101111,
        ];
        let bits = digit.map_or(0, |d| MAP[d as usize % 10]);
        let c = |seg: u8| if bits & (1 << seg) != 0 { on } else { off };
        // a top, b upper right, c lower right, d bottom, e lower left,
        // f upper left, g middle.
        self.fill(x + 1, y, 7, 2, c(0));
        self.fill(x + 7, y + 1, 2, 6, c(1));
        self.fill(x + 7, y + 8, 2, 6, c(2));
        self.fill(x + 1, y + 13, 7, 2, c(3));
        self.fill(x, y + 8, 2, 6, c(4));
        self.fill(x, y + 1, 2, 6, c(5));
        self.fill(x + 1, y + 7, 7, 1, c(6));
    }

    /// Writes this canvas, scaled by a whole number `k`, into a plain buffer.
    /// Used for image export. On screen the GPU does this job instead.
    pub fn blit_scaled(&self, dst: &mut [u32], dst_w: usize, dst_h: usize, k: usize, back: u32) {
        dst.fill(back);
        let width = (self.w * k).min(dst_w);
        for y in 0..self.h {
            let top = y * k;
            if top >= dst_h {
                break;
            }
            let src = &self.px[y * self.w..(y + 1) * self.w];
            let row = &mut dst[top * dst_w..top * dst_w + width];
            for (chunk, &p) in row.chunks_mut(k).zip(src) {
                chunk.fill(p);
            }
            for r in 1..k {
                if top + r >= dst_h {
                    break;
                }
                let (head, tail) = dst.split_at_mut((top + r) * dst_w);
                tail[..width].copy_from_slice(&head[top * dst_w..top * dst_w + width]);
            }
        }
    }
}

pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |shift: u32| {
        let x = ((a >> shift) & 0xff) as f32;
        let y = ((b >> shift) & 0xff) as f32;
        ((x + (y - x) * t) as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}
