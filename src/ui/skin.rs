//! Classic `.wsz` skins: a zip of bitmaps with parts at fixed positions. None
//! ship with the app, so these only ever show art the user supplied. A skin
//! describes its own layout, which differs from the built-in look, so the
//! skinned windows have their own drawing and hit-testing here.

use std::collections::HashMap;
use std::path::Path;

use super::font;
use super::gfx::{Canvas, mix};
use super::panels::{
    EqCtl, EqView, ListStyle, MainCtl, MainView, PLAYLIST_H, PlCtl, PlaylistView, SCOPE_WIDTH, WIDTH,
    draw_jump_bar, draw_list,
};
use crate::engine::PlayState;
use crate::engine::eq::{BANDS, MAX_DB};
use crate::engine::spectrum::BARS;
use crate::playlist::Repeat;
use crate::settings::MainVis;

use super::panels::Rect;

/// Refuses absurd sizes in a damaged or hostile file.
const MAX_FILE: usize = 16 * 1024 * 1024;
const MAX_IMAGE_SIDE: i32 = 4096;

// ------------------------------------------------------------------ reading

/// A skin's files by lower-case name, wherever they sit inside it.
fn read_files(path: &Path) -> Result<HashMap<String, Vec<u8>>, String> {
    if path.is_dir() {
        let mut files = HashMap::new();
        let dir = std::fs::read_dir(path).map_err(|e| format!("cannot read the skin: {e}"))?;
        for entry in dir.flatten() {
            let meta = entry.metadata().map_err(|e| e.to_string())?;
            if meta.is_file() && meta.len() as usize <= MAX_FILE
                && let Ok(bytes) = std::fs::read(entry.path())
            {
                files.insert(entry.file_name().to_string_lossy().to_lowercase(), bytes);
            }
        }
        return Ok(files);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read the skin: {e}"))?;
    read_zip(&bytes).ok_or_else(|| "this is not a skin file".to_string())
}

fn u16_at(b: &[u8], at: usize) -> Option<usize> {
    let s = b.get(at..at + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]) as usize)
}

fn u32_at(b: &[u8], at: usize) -> Option<usize> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
}

/// Reads the top-level structure of a zip: its central directory, and each
/// file stored either as is or deflated.
fn read_zip(zip: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    const END_SIGNATURE: &[u8] = b"PK\x05\x06";
    let end = zip.windows(4).rposition(|w| w == END_SIGNATURE)?;
    let count = u16_at(zip, end + 10)?;
    let mut at = u32_at(zip, end + 16)?;
    let mut files = HashMap::new();
    for _ in 0..count {
        if zip.get(at..at + 4)? != b"PK\x01\x02" {
            return None;
        }
        let method = u16_at(zip, at + 10)?;
        let compressed = u32_at(zip, at + 20)?;
        let name_len = u16_at(zip, at + 28)?;
        let extra_len = u16_at(zip, at + 30)?;
        let comment_len = u16_at(zip, at + 32)?;
        let local = u32_at(zip, at + 42)?;
        let name = String::from_utf8_lossy(zip.get(at + 46..at + 46 + name_len)?).into_owned();
        at += 46 + name_len + extra_len + comment_len;

        let leaf = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_lowercase();
        if leaf.is_empty() || compressed > MAX_FILE {
            continue;
        }
        let start = local + 30 + u16_at(zip, local + 26)? + u16_at(zip, local + 28)?;
        let raw = zip.get(start..start + compressed)?;
        let data = match method {
            0 => raw.to_vec(),
            8 => match miniz_oxide::inflate::decompress_to_vec_with_limit(raw, MAX_FILE) {
                Ok(data) => data,
                Err(_) => continue,
            },
            _ => continue,
        };
        files.insert(leaf, data);
    }
    Some(files)
}

/// One bitmap of a skin. An absent one has no size and draws nothing.
#[derive(Clone, Default)]
pub struct Sheet {
    w: i32,
    h: i32,
    px: Vec<u32>,
}

impl Sheet {
    fn at(&self, x: i32, y: i32) -> Option<u32> {
        (x >= 0 && y >= 0 && x < self.w && y < self.h).then(|| self.px[(y * self.w + x) as usize])
    }
}

/// Windows BMP: 1, 4, 8, 24 or 32 bits, uncompressed or RLE8.
fn parse_bmp(b: &[u8]) -> Option<Sheet> {
    if b.get(..2)? != b"BM" {
        return None;
    }
    let data = u32_at(b, 10)?;
    let header = u32_at(b, 14)?;
    let old_header = header == 12;
    let (width, raw_height, bits, compression) = if old_header {
        (u16_at(b, 18)? as i32, u16_at(b, 20)? as i32, u16_at(b, 24)?, 0)
    } else {
        (u32_at(b, 18)? as i32, u32_at(b, 22)? as u32 as i32, u16_at(b, 28)?, u32_at(b, 30)?)
    };
    let (height, top_down) = (raw_height.abs(), raw_height < 0);
    if width <= 0 || height <= 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return None;
    }
    let colors = if old_header { 0 } else { u32_at(b, 46)? };
    let entry = if old_header { 3 } else { 4 };
    let table = 14 + header;
    let count = if bits <= 8 { if colors == 0 { 1 << bits } else { colors.min(256) } } else { 0 };
    let mut palette = vec![0u32; 256];
    for (i, slot) in palette.iter_mut().enumerate().take(count) {
        let c = b.get(table + i * entry..table + i * entry + 3)?;
        *slot = (c[2] as u32) << 16 | (c[1] as u32) << 8 | c[0] as u32;
    }
    let mut px = vec![0u32; (width * height) as usize];
    let row_at = |y: i32| -> usize { (if top_down { y } else { height - 1 - y } * width) as usize };

    if compression == 1 && bits == 8 {
        decode_rle8(b.get(data..)?, width, height, &palette, &mut |x, y, c| {
            px[row_at(y) + x as usize] = c;
        });
        return Some(Sheet { w: width, h: height, px });
    }
    if compression != 0 && !(compression == 3 && bits == 32) {
        return None;
    }
    let stride = (width as usize * bits).div_ceil(32) * 4;
    for y in 0..height {
        let row = b.get(data + y as usize * stride..data + (y as usize + 1) * stride)?;
        let out = &mut px[row_at(y)..row_at(y) + width as usize];
        for (x, slot) in out.iter_mut().enumerate() {
            *slot = match bits {
                1 => palette[((row[x / 8] >> (7 - x % 8)) & 1) as usize],
                4 => palette[((row[x / 2] >> (4 - 4 * (x % 2) as u8)) & 15) as usize],
                8 => palette[row[x] as usize],
                24 => (row[x * 3 + 2] as u32) << 16 | (row[x * 3 + 1] as u32) << 8 | row[x * 3] as u32,
                32 => (row[x * 4 + 2] as u32) << 16 | (row[x * 4 + 1] as u32) << 8 | row[x * 4] as u32,
                _ => return None,
            };
        }
    }
    Some(Sheet { w: width, h: height, px })
}

fn decode_rle8(data: &[u8], w: i32, h: i32, palette: &[u32], put: &mut dyn FnMut(i32, i32, u32)) {
    let (mut x, mut y, mut i) = (0, 0, 0);
    while i + 1 < data.len() && y < h {
        let (count, value) = (data[i] as i32, data[i + 1]);
        i += 2;
        if count > 0 {
            for _ in 0..count {
                if x < w {
                    put(x, y, palette[value as usize]);
                }
                x += 1;
            }
            continue;
        }
        match value {
            0 => (x, y) = (0, y + 1),
            1 => return,
            2 => {
                let (Some(&dx), Some(&dy)) = (data.get(i), data.get(i + 1)) else {
                    return;
                };
                x += dx as i32;
                y += dy as i32;
                i += 2;
            }
            run => {
                for _ in 0..run {
                    let Some(&v) = data.get(i) else {
                        return;
                    };
                    if x < w && y < h {
                        put(x, y, palette[v as usize]);
                    }
                    x += 1;
                    i += 1;
                }
                i += (run & 1) as usize;
            }
        }
    }
}

/// `#RRGGBB`, with or without the `#`.
fn parse_hex(text: &str) -> Option<u32> {
    let text = text.trim().trim_start_matches('#');
    (text.len() == 6).then(|| u32::from_str_radix(text, 16).ok()).flatten()
}

/// The 24 visualisation colours, one `r,g,b` per line.
fn parse_viscolor(text: &str) -> Option<[u32; 24]> {
    let mut colors = default_viscolor();
    let mut found = 0;
    for line in text.lines() {
        let body = line.split("//").next().unwrap_or_default();
        let parts: Vec<u32> = body
            .split(',')
            .filter_map(|p| p.split_whitespace().next()?.parse().ok())
            .collect();
        if parts.len() >= 3 && found < 24 {
            colors[found] = (parts[0].min(255) << 16) | (parts[1].min(255) << 8) | parts[2].min(255);
            found += 1;
        }
    }
    (found > 0).then_some(colors)
}

/// For a skin without its own: black, then red at the top down to green.
fn default_viscolor() -> [u32; 24] {
    let mut colors = [0u32; 24];
    colors[1] = 0x202020;
    for (i, slot) in colors[2..18].iter_mut().enumerate() {
        let t = i as f32 / 15.0;
        *slot = if t < 0.5 { mix(0xe8503f, 0xf2e04a, t * 2.0) } else { mix(0xf2e04a, 0x5cf07a, (t - 0.5) * 2.0) };
    }
    for (i, slot) in colors[18..23].iter_mut().enumerate() {
        *slot = mix(0xffffff, 0x5cf07a, i as f32 / 4.0);
    }
    colors[23] = 0xf0f0f0;
    colors
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ListColors {
    normal: u32,
    current: u32,
    background: u32,
    selected: u32,
}

fn parse_pledit(text: &str) -> ListColors {
    let mut colors = ListColors {
        normal: 0x00ff00,
        current: 0xffffff,
        background: 0x000000,
        selected: 0x0000c6,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(value) = parse_hex(value) else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "normal" => colors.normal = value,
            "current" => colors.current = value,
            "normalbg" => colors.background = value,
            "selectedbg" => colors.selected = value,
            _ => {}
        }
    }
    colors
}

pub struct Skin {
    pub name: String,
    main: Sheet,
    cbuttons: Sheet,
    titlebar: Sheet,
    numbers: Sheet,
    extended_numbers: bool,
    text: Sheet,
    text_color: u32,
    monoster: Sheet,
    playpaus: Sheet,
    posbar: Sheet,
    volume: Sheet,
    balance: Sheet,
    shufrep: Sheet,
    eqmain: Sheet,
    eq_ex: Sheet,
    pledit: Sheet,
    vis: [u32; 24],
    list: ListColors,
}

impl std::fmt::Debug for Skin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Skin({})", self.name)
    }
}

impl Skin {
    /// Loads a `.wsz`, a `.zip`, or a folder of skin files.
    pub fn load(path: &Path) -> Result<Self, String> {
        let files = read_files(path)?;
        let sheet = |name: &str| files.get(name).and_then(|b| parse_bmp(b)).unwrap_or_default();
        let main = sheet("main.bmp");
        if main.w == 0 {
            return Err("this is not a skin file".to_string());
        }
        let (numbers, extended_numbers) = match files.get("nums_ex.bmp").and_then(|b| parse_bmp(b)) {
            Some(extended) => (extended, true),
            None => (sheet("numbers.bmp"), false),
        };
        let volume = sheet("volume.bmp");
        let balance = files.get("balance.bmp").and_then(|b| parse_bmp(b)).unwrap_or_else(|| volume.clone());
        let text = sheet("text.bmp");
        let text_color = brightest_contrast(&text);
        let name = path
            .file_stem()
            .map_or_else(|| "Skin".to_string(), |s| s.to_string_lossy().into_owned());
        let viscolor = files
            .get("viscolor.txt")
            .and_then(|b| parse_viscolor(&crate::meta::decode_text(b)))
            .unwrap_or_else(default_viscolor);
        let list = parse_pledit(&files.get("pledit.txt").map(|b| crate::meta::decode_text(b)).unwrap_or_default());
        Ok(Self {
            name,
            main,
            cbuttons: sheet("cbuttons.bmp"),
            titlebar: sheet("titlebar.bmp"),
            numbers,
            extended_numbers,
            text,
            text_color,
            monoster: sheet("monoster.bmp"),
            playpaus: sheet("playpaus.bmp"),
            posbar: sheet("posbar.bmp"),
            volume,
            balance,
            shufrep: sheet("shufrep.bmp"),
            eqmain: sheet("eqmain.bmp"),
            eq_ex: sheet("eq_ex.bmp"),
            pledit: sheet("pledit.bmp"),
            vis: viscolor,
            list,
        })
    }

    // ---------------------------------------------------------------- text

    fn glyph_cell(&self, ch: char) -> Option<(i32, i32)> {
        let c = font::fold(ch).to_ascii_lowercase();
        Some(match c {
            'a'..='z' => (0, c as i32 - 'a' as i32),
            '0'..='9' => (1, c as i32 - '0' as i32),
            '"' => (0, 26),
            '@' => (0, 27),
            ' ' => (0, 30),
            '\u{2026}' => (1, 10),
            '.' => (1, 11),
            ':' => (1, 12),
            '(' => (1, 13),
            ')' => (1, 14),
            '-' => (1, 15),
            '\'' => (1, 16),
            '!' => (1, 17),
            '_' => (1, 18),
            '+' => (1, 19),
            '\\' => (1, 20),
            '/' => (1, 21),
            '[' | '{' | '<' => (1, 22),
            ']' | '}' | '>' => (1, 23),
            '^' => (1, 24),
            '&' => (1, 25),
            '%' => (1, 26),
            ',' => (1, 27),
            '=' => (1, 28),
            '$' => (1, 29),
            '#' => (1, 30),
            '?' => (2, 3),
            '*' => (2, 4),
            _ => return None,
        })
    }

    /// Letters from the skin's own font. Anything it has no letter for is
    /// drawn the usual way, in the skin's text colour.
    fn text(&self, c: &mut Canvas, x: i32, y: i32, left: i32, right: i32, s: &str) {
        let chars: Vec<char> = s.chars().collect();
        let (mut cx, mut i) = (x, 0);
        while i < chars.len() && cx < right {
            if let (Some((row, col)), true) = (self.glyph_cell(chars[i]), self.text.w > 0) {
                self.blit_clipped(c, (col * 5, row * 6, 5, 6), cx, y, left, right);
                cx += 5;
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && (self.glyph_cell(chars[i]).is_none() || self.text.w == 0) {
                i += 1;
            }
            let other: String = chars[start..i].iter().collect();
            c.text_between(cx, y - 1, left, right, &other, self.text_color);
            cx += font::width(&other);
        }
    }

    fn blit_clipped(&self, c: &mut Canvas, (sx, sy, w, h): Rect, dx: i32, dy: i32, left: i32, right: i32) {
        for row in 0..h {
            for col in 0..w {
                let px = dx + col;
                if px >= left && px < right
                    && let Some(color) = self.text.at(sx + col, sy + row)
                {
                    c.set(px, dy + row, color);
                }
            }
        }
    }

    /// How wide `s` is in the skin's font, for deciding whether it scrolls.
    pub fn text_width(&self, s: &str) -> i32 {
        s.chars()
            .map(|ch| {
                if self.glyph_cell(ch).is_some() {
                    5
                } else {
                    font::width(&ch.to_string())
                }
            })
            .sum()
    }
}

fn brightest_contrast(text: &Sheet) -> u32 {
    let Some(back) = text.at(0, 0) else {
        return 0x00ff00;
    };
    let distance = |c: u32| {
        let part = |shift: u32| (((c >> shift) & 255) as i32 - ((back >> shift) & 255) as i32).abs();
        part(16) + part(8) + part(0)
    };
    text.px.iter().copied().max_by_key(|&c| distance(c)).unwrap_or(0x00ff00)
}

/// Copies a part of a sheet onto the canvas. Parts of a smaller sheet than the
/// format calls for just draw nothing.
fn put(c: &mut Canvas, sheet: &Sheet, (sx, sy, w, h): Rect, dx: i32, dy: i32) {
    for row in 0..h {
        for col in 0..w {
            if let Some(color) = sheet.at(sx + col, sy + row) {
                c.set(dx + col, dy + row, color);
            }
        }
    }
}

fn inside(r: Rect, x: i32, y: i32) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

const CLOSE_R: Rect = (264, 3, 9, 9);
const SHADE_R: Rect = (254, 3, 9, 9);

// -------------------------------------------------------------- main window

const OPTIONS_R: Rect = (6, 3, 9, 9);
const CLUTTER: (i32, i32) = (10, 22);
const CLUTTER_O: Rect = (10, 25, 8, 8);
const CLUTTER_A: Rect = (10, 33, 8, 7);
const CLUTTER_I: Rect = (10, 40, 8, 7);
const CLUTTER_V: Rect = (10, 55, 8, 7);
const VIS: Rect = (24, 43, 76, 16);
const TIME_R: Rect = (39, 26, 59, 13);
const MARQUEE: Rect = (111, 27, 154, 6);
/// Room for a title in the skin's display.
pub const TICKER_ROOM: i32 = MARQUEE.2;
const VOLUME_R: Rect = (107, 57, 68, 13);
const BALANCE_R: Rect = (177, 57, 38, 13);
const POSITION_R: Rect = (16, 72, 248, 10);
const THUMB_W: i32 = 14;
const POS_THUMB_W: i32 = 29;
const TRANSPORT: [(MainCtl, Rect, i32); 6] = [
    (MainCtl::Prev, (16, 88, 23, 18), 0),
    (MainCtl::Play, (39, 88, 23, 18), 23),
    (MainCtl::Pause, (62, 88, 23, 18), 46),
    (MainCtl::Stop, (85, 88, 23, 18), 69),
    (MainCtl::Next, (108, 88, 22, 18), 92),
    (MainCtl::Open, (136, 89, 22, 16), 114),
];
const SHUFFLE_R: Rect = (164, 89, 47, 15);
const REPEAT_R: Rect = (210, 89, 28, 15);
const EQ_BUTTON_R: Rect = (219, 58, 23, 12);
const PL_BUTTON_R: Rect = (242, 58, 23, 12);

pub fn hit_main(x: i32, y: i32) -> Option<MainCtl> {
    use MainCtl::*;
    let fixed = [
        (CLOSE_R, Close),
        (SHADE_R, Shade),
        (OPTIONS_R, Options),
        (CLUTTER_O, Options),
        (CLUTTER_A, AlwaysOnTop),
        (CLUTTER_I, FileInfo),
        (CLUTTER_V, ToggleVisuals),
        (SHUFFLE_R, Shuffle),
        (REPEAT_R, Repeat),
        (EQ_BUTTON_R, ToggleEq),
        (PL_BUTTON_R, TogglePlaylist),
        (VOLUME_R, Volume),
        (BALANCE_R, Balance),
        (POSITION_R, Position),
        (TIME_R, Time),
        (VIS, Spectrum),
    ];
    TRANSPORT
        .iter()
        .map(|(ctl, r, _)| (*r, *ctl))
        .chain(fixed)
        .find(|(r, _)| inside(*r, x, y))
        .map(|(_, ctl)| ctl)
}

pub fn volume_from_x(x: i32) -> f32 {
    ((x - VOLUME_R.0 - THUMB_W / 2) as f32 / (VOLUME_R.2 - THUMB_W) as f32).clamp(0.0, 1.0)
}

pub fn balance_from_x(x: i32) -> f32 {
    let v = ((x - BALANCE_R.0 - THUMB_W / 2) as f32 / (BALANCE_R.2 - THUMB_W) as f32 * 2.0 - 1.0).clamp(-1.0, 1.0);
    if v.abs() < 0.08 { 0.0 } else { v }
}

pub fn position_from_x(x: i32) -> f32 {
    ((x - POSITION_R.0 - POS_THUMB_W / 2) as f32 / (POSITION_R.2 - POS_THUMB_W) as f32).clamp(0.0, 1.0)
}

impl Skin {
    pub fn draw_main(&self, c: &mut Canvas, v: &MainView) {
        put(c, &self.main, (0, 0, 275, 116), 0, 0);
        put(c, &self.titlebar, (27, if v.focused { 0 } else { 15 }, 275, 14), 0, 0);
        put(c, &self.titlebar, (304, 0, 8, 43), CLUTTER.0, CLUTTER.1);
        if v.always_on_top {
            put(c, &self.titlebar, (312, 55, 8, 7), CLUTTER_A.0, CLUTTER_A.1);
        }
        let pressed = |ctl| v.pressed == Some(ctl);
        if pressed(MainCtl::Close) {
            put(c, &self.titlebar, (18, 9, 9, 9), CLOSE_R.0, CLOSE_R.1);
        }
        if pressed(MainCtl::Shade) {
            put(c, &self.titlebar, (9, 18, 9, 9), SHADE_R.0, SHADE_R.1);
        }
        if pressed(MainCtl::Options) {
            put(c, &self.titlebar, (0, 9, 9, 9), OPTIONS_R.0, OPTIONS_R.1);
        }

        let indicator = match v.state {
            PlayState::Playing => 0,
            PlayState::Paused => 9,
            PlayState::Stopped => 18,
        };
        put(c, &self.playpaus, (indicator, 0, 9, 9), 26, 28);
        self.draw_digits(c, v);
        self.draw_vis(c, v);

        self.draw_marquee(c, v);
        self.draw_facts(c, v);

        // A slider's track is one of 28 frames, filling up with the value.
        let frame = |value: f32| (value.clamp(0.0, 1.0) * 27.0).round() as i32 * 15;
        put(c, &self.volume, (0, frame(v.volume), 68, 13), VOLUME_R.0, VOLUME_R.1);
        let knob = VOLUME_R.0 + (v.volume * (VOLUME_R.2 - THUMB_W) as f32) as i32;
        let sx = if pressed(MainCtl::Volume) { 0 } else { 15 };
        put(c, &self.volume, (sx, 422, THUMB_W, 11), knob, VOLUME_R.1 + 1);

        put(c, &self.balance, (9, frame(v.balance.abs()), 38, 13), BALANCE_R.0, BALANCE_R.1);
        let knob = BALANCE_R.0 + ((v.balance + 1.0) * 0.5 * (BALANCE_R.2 - THUMB_W) as f32) as i32;
        let sx = if pressed(MainCtl::Balance) { 0 } else { 15 };
        put(c, &self.balance, (sx, 422, THUMB_W, 11), knob, BALANCE_R.1 + 1);

        put(c, &self.posbar, (0, 0, 248, 10), POSITION_R.0, POSITION_R.1);
        if let Some(duration) = v.duration.filter(|d| *d > 0.0) {
            let fraction = v.seek_preview.map_or((v.position / duration).clamp(0.0, 1.0), |p| p as f64);
            let knob = POSITION_R.0 + (fraction * (POSITION_R.2 - POS_THUMB_W) as f64) as i32;
            let sx = if pressed(MainCtl::Position) { 278 } else { 248 };
            put(c, &self.posbar, (sx, 0, POS_THUMB_W, 10), knob, POSITION_R.1);
        }

        for (ctl, r, sx) in TRANSPORT {
            let sy = if pressed(ctl) { r.3 } else { 0 };
            put(c, &self.cbuttons, (sx, sy, r.2, r.3), r.0, r.1);
        }

        let toggle = |on: bool, down: bool| i32::from(on) * 30 + i32::from(down) * 15;
        put(c, &self.shufrep, (28, toggle(v.shuffle, pressed(MainCtl::Shuffle)), 47, 15), SHUFFLE_R.0, SHUFFLE_R.1);
        let repeat = toggle(v.repeat != Repeat::Off, pressed(MainCtl::Repeat));
        put(c, &self.shufrep, (0, repeat, 28, 15), REPEAT_R.0, REPEAT_R.1);
        if v.repeat == Repeat::One {
            self.text(c, REPEAT_R.0 + 20, REPEAT_R.1 + 5, 0, WIDTH as i32, "1");
        }
        self.draw_window_toggle(c, 0, 46, v.eq_shown, pressed(MainCtl::ToggleEq), EQ_BUTTON_R);
        self.draw_window_toggle(c, 23, 69, v.playlist_shown, pressed(MainCtl::TogglePlaylist), PL_BUTTON_R);
    }

    fn draw_window_toggle(&self, c: &mut Canvas, up_x: i32, down_x: i32, on: bool, down: bool, r: Rect) {
        let sx = if down { down_x } else { up_x };
        put(c, &self.shufrep, (sx, if on { 73 } else { 61 }, 23, 12), r.0, r.1);
    }

    fn draw_digits(&self, c: &mut Canvas, v: &MainView) {
        let shown = match (v.seek_preview, v.duration) {
            (Some(p), Some(d)) => Some(p as f64 * d),
            _ => (v.state != PlayState::Stopped).then_some(v.position),
        };
        let remaining = v.show_remaining && v.duration.is_some();
        let shown = shown.map(|p| match (remaining, v.duration) {
            (true, Some(d)) => (d - p).max(0.0),
            _ => p,
        });
        let Some(secs) = shown else {
            return;
        };
        if remaining {
            if self.extended_numbers {
                put(c, &self.numbers, (99, 0, 9, 13), 38, 26);
            } else {
                put(c, &self.numbers, (20, 6, 5, 1), 38, 32);
            }
        }
        let total = secs as u64;
        let (a, b) = if total >= 6000 { (total / 3600, (total / 60) % 60) } else { (total / 60, total % 60) };
        let digits = [(a / 10 % 10) as i32, (a % 10) as i32, (b / 10) as i32, (b % 10) as i32];
        for (digit, x) in digits.into_iter().zip([48, 60, 78, 90]) {
            put(c, &self.numbers, (digit * 9, 0, 9, 13), x, 26);
        }
    }

    fn draw_vis(&self, c: &mut Canvas, v: &MainView) {
        let (x, y, w, h) = VIS;
        c.fill(x, y, w, h, self.vis[0]);
        for row in (1..h).step_by(2) {
            for col in (1..w).step_by(2) {
                c.set(x + col, y + row, self.vis[1]);
            }
        }
        match v.main_vis {
            MainVis::Off => {}
            MainVis::Spectrum => {
                for bar in 0..BARS {
                    let bx = x + bar as i32 * 4;
                    let height = (v.bars[bar] * h as f32).round() as i32;
                    for row in 0..height {
                        // Colours are listed from the top of the scale down.
                        let index = 17 - (row * 15 / (h - 1)).min(15) as usize;
                        c.fill(bx, y + h - 1 - row, 3, 1, self.vis[index]);
                    }
                    if v.peaks[bar] > 0.02 {
                        let row = ((v.peaks[bar] * h as f32) as i32).min(h - 1);
                        c.fill(bx, y + h - 1 - row, 3, 1, self.vis[23]);
                    }
                }
            }
            MainVis::Scope => {
                let row_of = |s: f32| ((0.5 - s.clamp(-1.0, 1.0) * 0.5) * (h - 1) as f32).round() as i32;
                let mut last = row_of(v.scope[0]);
                for (i, &sample) in v.scope.iter().enumerate().take(SCOPE_WIDTH) {
                    let row = row_of(sample);
                    let (from, to) = (row.min(last), row.max(last));
                    let index = 18 + (((row - h / 2).unsigned_abs() as usize * 5) / (h as usize / 2 + 1)).min(4);
                    c.fill(x + i as i32, y + from, 1, to - from + 1, self.vis[index]);
                    last = row;
                }
            }
        }
    }

    fn draw_marquee(&self, c: &mut Canvas, v: &MainView) {
        let (x, y, w, _) = MARQUEE;
        let period = self.text_width(v.ticker);
        if period == 0 {
            return;
        }
        let mut cx = x - v.ticker_offset.rem_euclid(period);
        while cx < x + w {
            self.text(c, cx, y, x, x + w, v.ticker);
            cx += period;
            if period <= w && !v.ticker.ends_with(' ') {
                break;
            }
        }
    }

    fn draw_facts(&self, c: &mut Canvas, v: &MainView) {
        if let Some(kbps) = v.facts.kbps {
            let text = format!("{kbps:>3}");
            self.text(c, 111, 43, 0, WIDTH as i32, &text[text.len().saturating_sub(3)..]);
        }
        if let Some(khz) = v.facts.khz {
            let text = format!("{khz:>2}");
            self.text(c, 156, 43, 0, WIDTH as i32, &text[text.len().saturating_sub(2)..]);
        }
        let lit = |on: bool| if on { 0 } else { 12 };
        let channels = v.facts.channels;
        put(c, &self.monoster, (29, lit(channels == Some(1)), 27, 12), 212, 41);
        put(c, &self.monoster, (0, lit(channels.is_some_and(|n| n >= 2)), 29, 12), 239, 41);
    }

    pub fn draw_main_shade(&self, c: &mut Canvas, v: &MainView) {
        put(c, &self.titlebar, (27, if v.focused { 29 } else { 42 }, 275, 14), 0, 0);
        put(c, &self.titlebar, (0, 27, 9, 9), SHADE_R.0, SHADE_R.1);
        if v.pressed == Some(MainCtl::Close) {
            put(c, &self.titlebar, (18, 9, 9, 9), CLOSE_R.0, CLOSE_R.1);
        }
        if v.state != PlayState::Stopped {
            let total = v.position as u64;
            let text = format!("{:>2}:{:02}", (total / 60).min(99), total % 60);
            self.text(c, 127, 4, 0, WIDTH as i32, &text);
        }
        put(c, &self.titlebar, (0, 36, 17, 7), 226, 4);
        if let Some(duration) = v.duration.filter(|d| *d > 0.0) {
            let x = 226 + ((v.position / duration).clamp(0.0, 1.0) * 14.0) as i32;
            put(c, &self.titlebar, (20, 36, 3, 7), x, 4);
        }
    }
}

pub fn hit_main_shade(x: i32, y: i32) -> Option<MainCtl> {
    use MainCtl::*;
    [
        (CLOSE_R, Close),
        (SHADE_R, Shade),
        ((169, 2, 7, 10), Prev),
        ((176, 2, 10, 10), Play),
        ((186, 2, 9, 10), Pause),
        ((195, 2, 9, 10), Stop),
        ((204, 2, 10, 10), Next),
        ((215, 2, 10, 10), Open),
    ]
    .into_iter()
    .find(|(r, _)| inside(*r, x, y))
    .map(|(_, ctl)| ctl)
}

// ---------------------------------------------------------------- equaliser

const EQ_ON_R: Rect = (14, 18, 26, 12);
const EQ_PRESETS_R: Rect = (217, 18, 44, 12);
const EQ_GRAPH: Rect = (86, 17, 113, 19);
const EQ_SLIDER_TOP: i32 = 38;
const EQ_SLIDER_W: i32 = 14;
const EQ_SLIDER_H: i32 = 63;
const EQ_THUMB: i32 = 11;
const EQ_PREAMP_X: i32 = 21;
const EQ_BAND_X0: i32 = 78;
const EQ_BAND_STEP: i32 = 18;

pub fn hit_eq(x: i32, y: i32) -> Option<EqCtl> {
    if inside(CLOSE_R, x, y) {
        return Some(EqCtl::Close);
    }
    if inside(SHADE_R, x, y) {
        return Some(EqCtl::Shade);
    }
    if inside(EQ_ON_R, x, y) {
        return Some(EqCtl::Power);
    }
    if inside(EQ_PRESETS_R, x, y) {
        return Some(EqCtl::Preset);
    }
    let slider = |sx: i32| inside((sx, EQ_SLIDER_TOP, EQ_SLIDER_W, EQ_SLIDER_H), x, y);
    if slider(EQ_PREAMP_X) {
        return Some(EqCtl::Preamp);
    }
    (0..BANDS).find(|&b| slider(EQ_BAND_X0 + b as i32 * EQ_BAND_STEP)).map(EqCtl::Band)
}

pub fn hit_eq_shade(x: i32, y: i32) -> Option<EqCtl> {
    if inside(CLOSE_R, x, y) {
        Some(EqCtl::Close)
    } else if inside(SHADE_R, x, y) {
        Some(EqCtl::Shade)
    } else {
        None
    }
}

pub fn db_from_y(y: i32) -> f32 {
    let t = ((y - EQ_SLIDER_TOP - EQ_THUMB / 2) as f32 / (EQ_SLIDER_H - EQ_THUMB) as f32).clamp(0.0, 1.0);
    let db = MAX_DB * (1.0 - 2.0 * t);
    if db.abs() < 0.8 { 0.0 } else { db }
}

impl Skin {
    pub fn draw_eq(&self, c: &mut Canvas, v: &EqView) {
        put(c, &self.eqmain, (0, 0, 275, 116), 0, 0);
        put(c, &self.eqmain, (0, if v.focused { 134 } else { 149 }, 275, 14), 0, 0);
        if v.pressed == Some(EqCtl::Close) {
            put(c, &self.eqmain, (0, 125, 9, 9), CLOSE_R.0, CLOSE_R.1);
        }
        let down = v.pressed == Some(EqCtl::Power);
        let sx = match (v.enabled, down) {
            (false, false) => 10,
            (false, true) => 128,
            (true, false) => 69,
            (true, true) => 187,
        };
        put(c, &self.eqmain, (sx, 119, EQ_ON_R.2, EQ_ON_R.3), EQ_ON_R.0, EQ_ON_R.1);
        // The auto button has no function here, so it always shows as off.
        put(c, &self.eqmain, (36, 119, 32, 12), 40, 18);
        let sy = if v.pressed == Some(EqCtl::Preset) { 176 } else { 164 };
        put(c, &self.eqmain, (224, sy, 44, 12), EQ_PRESETS_R.0, EQ_PRESETS_R.1);
        self.draw_graph(c, v);
        self.draw_eq_slider(c, EQ_PREAMP_X, v.preamp, v.pressed == Some(EqCtl::Preamp));
        for (b, &gain) in v.gains.iter().enumerate() {
            let x = EQ_BAND_X0 + b as i32 * EQ_BAND_STEP;
            self.draw_eq_slider(c, x, gain, v.pressed == Some(EqCtl::Band(b)));
        }
    }

    fn draw_eq_slider(&self, c: &mut Canvas, x: i32, db: f32, down: bool) {
        let t = ((db + MAX_DB) / (2.0 * MAX_DB)).clamp(0.0, 1.0);
        let frame = (t * 27.0).round() as i32;
        let area = (13 + (frame % 14) * 15, 164 + (frame / 14) * 65, EQ_SLIDER_W, EQ_SLIDER_H);
        put(c, &self.eqmain, area, x, EQ_SLIDER_TOP);
        let knob = EQ_SLIDER_TOP + ((1.0 - t) * (EQ_SLIDER_H - EQ_THUMB) as f32) as i32;
        put(c, &self.eqmain, (0, if down { 176 } else { 164 }, EQ_THUMB, EQ_THUMB), x + 1, knob);
    }

    fn draw_graph(&self, c: &mut Canvas, v: &EqView) {
        let (x, y, w, h) = EQ_GRAPH;
        put(c, &self.eqmain, (0, 294, w, h), x, y);
        let row_of = |db: f32| ((0.5 - db / (MAX_DB * 2.0)).clamp(0.0, 1.0) * (h - 1) as f32) as i32;
        put(c, &self.eqmain, (0, 314, w, 1), x, y + row_of(v.preamp));
        let mut last = None;
        for px in 0..w {
            let pos = px as f32 / (w - 1) as f32 * (BANDS - 1) as f32;
            let i = (pos as usize).min(BANDS - 2);
            let f = pos - i as f32;
            let f = f * f * (3.0 - 2.0 * f);
            let row = row_of(v.gains[i] + (v.gains[i + 1] - v.gains[i]) * f);
            let (from, to) = match last {
                Some(l) if l < row => (l + 1, row),
                Some(l) if l > row => (row, l - 1),
                _ => (row, row),
            };
            for r in from..=to {
                if let Some(color) = self.eqmain.at(115, 294 + r) {
                    c.set(x + px, y + r, color);
                }
            }
            last = Some(row);
        }
    }

    pub fn draw_eq_shade(&self, c: &mut Canvas, v: &EqView) {
        if self.eq_ex.w > 0 {
            put(c, &self.eq_ex, (0, if v.focused { 0 } else { 15 }, 275, 14), 0, 0);
        } else {
            put(c, &self.eqmain, (0, if v.focused { 134 } else { 149 }, 275, 14), 0, 0);
        }
    }
}

// ----------------------------------------------------------------- playlist

const TOP: i32 = 20;
const BOTTOM: i32 = 38;
const LEFT: i32 = 12;
const RIGHT: i32 = 20;
const ROW_H: i32 = 9;
const LIST_H: i32 = PLAYLIST_H as i32 - TOP - BOTTOM;
/// How many rows fit between a skin's frame.
pub const ROWS: usize = (LIST_H / ROW_H) as usize;
const LIST_R: Rect = (LEFT, TOP, WIDTH as i32 - LEFT - RIGHT, ROWS as i32 * ROW_H);
const SCROLL_R: Rect = (WIDTH as i32 - 15, TOP, 8, LIST_H);
const HANDLE_H: i32 = 18;
const BUTTON_Y: i32 = PLAYLIST_H as i32 - 30;
const MENUS: [(PlCtl, i32); 5] = [
    (PlCtl::MenuAdd, 14),
    (PlCtl::MenuRemove, 43),
    (PlCtl::MenuSelect, 72),
    (PlCtl::MenuMisc, 101),
    (PlCtl::MenuList, WIDTH as i32 - 44),
];

pub fn hit_playlist(x: i32, y: i32, scroll: usize) -> Option<PlCtl> {
    if inside(CLOSE_R, x, y) {
        return Some(PlCtl::Close);
    }
    if inside(SHADE_R, x, y) {
        return Some(PlCtl::Shade);
    }
    if inside(LIST_R, x, y) {
        return Some(PlCtl::Row(scroll + ((y - TOP) / ROW_H) as usize));
    }
    if inside(SCROLL_R, x, y) {
        return Some(PlCtl::Scrollbar);
    }
    MENUS
        .iter()
        .find(|(_, bx)| inside((*bx, BUTTON_Y, 22, 18), x, y))
        .map(|(ctl, _)| *ctl)
}

pub fn hit_playlist_shade(x: i32, y: i32) -> Option<PlCtl> {
    if inside(CLOSE_R, x, y) {
        Some(PlCtl::Close)
    } else if inside(SHADE_R, x, y) {
        Some(PlCtl::Shade)
    } else {
        None
    }
}

pub fn row_at_y(y: i32, scroll: usize) -> isize {
    scroll as isize + (y - TOP).div_euclid(ROW_H) as isize
}

pub fn scroll_from_y(y: i32, total: usize) -> usize {
    let t = ((y - SCROLL_R.1 - HANDLE_H / 2) as f32 / (SCROLL_R.3 - HANDLE_H) as f32).clamp(0.0, 1.0);
    (t * total.saturating_sub(ROWS) as f32).round() as usize
}

impl Skin {
    pub fn draw_playlist(&self, c: &mut Canvas, v: &PlaylistView) {
        let width = WIDTH as i32;
        let y = if v.focused { 0 } else { 21 };
        put(c, &self.pledit, (0, y, 25, 20), 0, 0);
        let title_x = (width - 100) / 2;
        let mut x = 25;
        while x < width - 25 {
            put(c, &self.pledit, (127, y, 25, 20), x, 0);
            x += 25;
        }
        put(c, &self.pledit, (26, y, 100, 20), title_x, 0);
        put(c, &self.pledit, (153, y, 25, 20), width - 25, 0);
        let mut row = TOP;
        while row < TOP + LIST_H {
            put(c, &self.pledit, (0, 42, 12, 29), 0, row);
            put(c, &self.pledit, (31, 42, 20, 29), width - RIGHT, row);
            row += 29;
        }
        // The frame's side tiles can overrun into the corners below them.
        put(c, &self.pledit, (0, 72, 125, 38), 0, PLAYLIST_H as i32 - BOTTOM);
        put(c, &self.pledit, (126, 72, 150, 38), 125, PLAYLIST_H as i32 - BOTTOM);
        c.fill(LEFT, TOP, LIST_R.2, LIST_H, self.list.background);

        if v.pressed == Some(PlCtl::Close) {
            put(c, &self.pledit, (52, 42, 9, 9), CLOSE_R.0, CLOSE_R.1);
        }
        if v.pressed == Some(PlCtl::Shade) {
            put(c, &self.pledit, (62, 42, 9, 9), SHADE_R.0, SHADE_R.1);
        }
        draw_list(
            c,
            v,
            &ListStyle {
                rect: LIST_R,
                rows: ROWS,
                row_h: ROW_H,
                normal: self.list.normal,
                current: self.list.current,
                selected: self.list.selected,
                badge: self.list.current,
            },
        );

        let pl = v.playlist;
        if let Some(jump) = &v.jump {
            draw_jump_bar(c, jump, pl.entries.len(), (LEFT, BUTTON_Y + 3, 206, 12), WIDTH as i32 - RIGHT);
        }
        let total = v.jump.as_ref().map_or(pl.entries.len(), |j| j.matches.len());
        let t = if total > ROWS { v.scroll as f32 / (total - ROWS) as f32 } else { 0.0 };
        let handle = TOP + (t * (LIST_H - HANDLE_H) as f32) as i32;
        let sx = if v.pressed == Some(PlCtl::Scrollbar) { 61 } else { 52 };
        put(c, &self.pledit, (sx, 53, 8, HANDLE_H), SCROLL_R.0, handle);

        let secs = pl.total_seconds() as u64;
        let time = if secs >= 3600 {
            format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
        } else {
            format!("{}:{:02}", secs / 60, secs % 60)
        };
        if v.jump.is_none() {
            let summary = format!("{}/{time}", pl.entries.len());
            self.text(c, 132, PLAYLIST_H as i32 - BOTTOM + 10, 0, WIDTH as i32, &summary);
        }
    }

    pub fn draw_playlist_shade(&self, c: &mut Canvas, v: &PlaylistView) {
        let width = WIDTH as i32;
        put(c, &self.pledit, (72, 42, 25, 14), 0, 0);
        let mut x = 25;
        while x < width - 50 {
            put(c, &self.pledit, (72, 57, 25, 14), x, 0);
            x += 25;
        }
        put(c, &self.pledit, (99, if v.focused { 42 } else { 57 }, 50, 14), width - 50, 0);
        let pl = v.playlist;
        let current = pl.current.and_then(|id| pl.get(id));
        let time = current.and_then(|e| e.info.duration).map_or(String::new(), |d| {
            let s = d.round() as u64;
            format!("{}:{:02}", s / 60, s % 60)
        });
        let time_x = width - 30 - self.text_width(&time);
        self.text(c, time_x, 4, 0, WIDTH as i32, &time);
        let title = current.map_or_else(String::new, |e| e.display_title());
        self.text(c, 5, 4, 5, time_x - 5, &title);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A 24-bit bitmap, bottom row first, as most tools write them.
    pub fn bmp24(w: i32, h: i32, color: impl Fn(i32, i32) -> u32) -> Vec<u8> {
        let stride = ((w * 3 + 3) / 4 * 4) as usize;
        let mut out = b"BM".to_vec();
        out.extend(((54 + stride * h as usize) as u32).to_le_bytes());
        out.extend([0; 4]);
        out.extend(54u32.to_le_bytes());
        out.extend(40u32.to_le_bytes());
        out.extend(w.to_le_bytes());
        out.extend(h.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(24u16.to_le_bytes());
        out.extend([0; 24]);
        for y in (0..h).rev() {
            let mut row = Vec::new();
            for x in 0..w {
                let c = color(x, y);
                row.extend([c as u8, (c >> 8) as u8, (c >> 16) as u8]);
            }
            row.resize(stride, 0);
            out.extend(row);
        }
        out
    }

    fn zip(files: &[(&str, Vec<u8>, bool)]) -> Vec<u8> {
        let (mut body, mut central) = (Vec::new(), Vec::new());
        for (name, data, deflate) in files {
            let stored = if *deflate { miniz_oxide::deflate::compress_to_vec(data, 6) } else { data.clone() };
            let method: u16 = if *deflate { 8 } else { 0 };
            let offset = body.len() as u32;
            body.extend(b"PK\x03\x04");
            body.extend([20, 0, 0, 0]);
            body.extend(method.to_le_bytes());
            body.extend([0; 8]);
            body.extend((stored.len() as u32).to_le_bytes());
            body.extend((data.len() as u32).to_le_bytes());
            body.extend((name.len() as u16).to_le_bytes());
            body.extend([0, 0]);
            body.extend(name.as_bytes());
            body.extend(&stored);
            central.extend(b"PK\x01\x02");
            central.extend([20, 0, 20, 0, 0, 0]);
            central.extend(method.to_le_bytes());
            central.extend([0; 8]);
            central.extend((stored.len() as u32).to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0; 12]);
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let start = body.len() as u32;
        let size = central.len() as u32;
        body.extend(central);
        body.extend(b"PK\x05\x06");
        body.extend([0; 4]);
        body.extend((files.len() as u16).to_le_bytes());
        body.extend((files.len() as u16).to_le_bytes());
        body.extend(size.to_le_bytes());
        body.extend(start.to_le_bytes());
        body.extend([0, 0]);
        body
    }

    #[test]
    fn zip_entries_come_out_stored_or_deflated_and_in_folders() {
        let files = read_zip(&zip(&[
            ("Skin/MAIN.BMP", b"hello hello hello".to_vec(), true),
            ("viscolor.txt", b"0,0,0".to_vec(), false),
        ]))
        .unwrap();
        assert_eq!(files["main.bmp"], b"hello hello hello");
        assert_eq!(files["viscolor.txt"], b"0,0,0");
        assert!(read_zip(b"not a zip").is_none());
    }

    #[test]
    fn bitmaps_decode_in_the_right_orientation() {
        let sheet = parse_bmp(&bmp24(3, 2, |x, y| ((x as u32) << 16) | y as u32)).unwrap();
        assert_eq!((sheet.w, sheet.h), (3, 2));
        assert_eq!(sheet.at(2, 0), Some(0x020000));
        assert_eq!(sheet.at(0, 1), Some(0x000001));
    }

    #[test]
    fn eight_bit_and_rle_bitmaps_use_the_palette() {
        let header = |compression: u32, data: &[u8]| {
            let mut out = b"BM".to_vec();
            out.extend([0; 8]);
            out.extend(62u32.to_le_bytes());
            out.extend(40u32.to_le_bytes());
            out.extend(2i32.to_le_bytes());
            out.extend(2i32.to_le_bytes());
            out.extend(1u16.to_le_bytes());
            out.extend(8u16.to_le_bytes());
            out.extend(compression.to_le_bytes());
            out.extend([0; 12]);
            out.extend(2u32.to_le_bytes());
            out.extend([0; 4]);
            out.extend([0xff, 0, 0, 0, 0, 0, 0xff, 0]);
            out.extend(data);
            out
        };
        let plain = parse_bmp(&header(0, &[0, 1, 0, 0, 1, 1, 0, 0])).unwrap();
        assert_eq!(plain.at(0, 1), Some(0x0000ff), "bottom row is stored first");
        assert_eq!(plain.at(1, 1), Some(0xff0000));
        assert_eq!(plain.at(0, 0), Some(0xff0000));
        let rle = parse_bmp(&header(1, &[2, 1, 0, 0, 2, 0, 0, 1])).unwrap();
        assert_eq!(rle.at(0, 1), Some(0xff0000));
        assert_eq!(rle.at(1, 0), Some(0x0000ff));
    }

    #[test]
    fn damaged_files_are_refused_rather_than_panicking() {
        let good = bmp24(4, 4, |_, _| 0);
        for cut in [0, 2, 20, 40, 54, 60] {
            assert!(parse_bmp(&good[..cut]).is_none(), "cut at {cut}");
        }
        let mut huge = good.clone();
        huge[18..22].copy_from_slice(&100_000i32.to_le_bytes());
        assert!(parse_bmp(&huge).is_none());
    }

    #[test]
    fn colour_files_parse_with_comments_and_defaults() {
        let vis = parse_viscolor("0,0,0, // background\n24,33,41,\n").unwrap();
        assert_eq!(vis[1], 0x182129);
        let list = parse_pledit("[Text]\r\nNormal=#00FF00\r\nCurrent=#ffffff\r\nNormalBG=#101010\r\nSelectedBG=#0000C6\r\nFont=Arial\r\n");
        assert_eq!(list.background, 0x101010);
        assert_eq!(list.selected, 0x0000c6);
    }

    #[test]
    fn skin_text_finds_every_letter_of_its_font() {
        let skin = Skin {
            name: String::new(),
            main: Sheet::default(),
            cbuttons: Sheet::default(),
            titlebar: Sheet::default(),
            numbers: Sheet::default(),
            extended_numbers: false,
            text: Sheet::default(),
            text_color: 0,
            monoster: Sheet::default(),
            playpaus: Sheet::default(),
            posbar: Sheet::default(),
            volume: Sheet::default(),
            balance: Sheet::default(),
            shufrep: Sheet::default(),
            eqmain: Sheet::default(),
            eq_ex: Sheet::default(),
            pledit: Sheet::default(),
            vis: default_viscolor(),
            list: parse_pledit(""),
        };
        assert_eq!(skin.glyph_cell('Z'), Some((0, 25)));
        assert_eq!(skin.glyph_cell('\u{17b}'), Some((0, 25)), "accents fold");
        assert_eq!(skin.glyph_cell('7'), Some((1, 7)));
        assert_eq!(skin.glyph_cell('#'), Some((1, 30)));
        assert_eq!(skin.glyph_cell('\u{65e5}'), None);
        assert_eq!(skin.text_width("abc"), 15);
    }
}
