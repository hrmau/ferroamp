//! Drawing and hit-testing for the three windows. Pure functions of a view
//! struct, so the windows can also be rendered to images in tests.

use super::font::{self, ADVANCE};
use super::gfx::{Canvas, mix};
use crate::engine::PlayState;
use crate::engine::eq::{BANDS, MAX_DB, PRESETS};
use crate::engine::spectrum::BARS;
use crate::playlist::{Playlist, Repeat};
use crate::settings::MainVis;

pub const WIDTH: usize = 275;
pub const MAIN_H: usize = 116;
pub const EQ_H: usize = 116;
pub const PLAYLIST_H: usize = 232;
/// A window collapsed to a strip.
pub const SHADE_H: usize = 14;

const CHASSIS: u32 = 0x4d5058;
const CHASSIS_HI: u32 = 0x8a8e97;
const CHASSIS_LO: u32 = 0x1e2026;
const TITLE_TOP: u32 = 0x6f7c8f;
const TITLE_BOTTOM: u32 = 0x39445a;
const TITLE_IDLE_TOP: u32 = 0x62656d;
const TITLE_IDLE_BOTTOM: u32 = 0x45474e;
const LABEL: u32 = 0xdadde3;
const LCD: u32 = 0x040705;
const LCD_OFF: u32 = 0x0d2214;
const DISPLAY: u32 = 0x5cf07a;
const DISPLAY_DIM: u32 = 0x2d7f44;
const DISPLAY_ALT: u32 = 0xc4f2a6;
/// A button that is switched on.
const LIT: u32 = 0xffc04d;
const YELLOW: u32 = 0xf2e04a;
const RED: u32 = 0xe8503f;
const BUTTON: u32 = 0x62656e;
const BUTTON_DOWN: u32 = 0x35373d;
const SELECT: u32 = 0x1d3070;
const CURRENT: u32 = 0xf4fff4;

/// Level colour: green, through yellow, to red at the top.
fn heat(t: f32) -> u32 {
    if t < 0.6 {
        mix(DISPLAY, YELLOW, t / 0.6)
    } else {
        mix(YELLOW, RED, (t - 0.6) / 0.4)
    }
}

pub type Rect = (i32, i32, i32, i32);

fn inside(r: Rect, x: i32, y: i32) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

fn button(c: &mut Canvas, r: Rect, down: bool, lit: bool) {
    let (x, y, w, h) = r;
    if down {
        c.fill(x, y, w, h, BUTTON_DOWN);
        c.bevel(x, y, w, h, CHASSIS_LO, CHASSIS_HI);
    } else {
        c.vgradient(x, y, w, h, mix(BUTTON, 0xffffff, 0.06), BUTTON);
        c.bevel(x, y, w, h, CHASSIS_HI, CHASSIS_LO);
    }
    let _ = lit;
}

fn labelled(c: &mut Canvas, r: Rect, label: &str, down: bool, lit: bool) {
    button(c, r, down, lit);
    let color = if lit { LIT } else { LABEL };
    let dy = i32::from(down);
    c.text_center(r.0 + r.2 / 2, r.1 + (r.3 - 7) / 2 + dy, label, color);
}

fn well(c: &mut Canvas, r: Rect) {
    let (x, y, w, h) = r;
    c.fill(x, y, w, h, LCD);
    c.bevel(x - 1, y - 1, w + 2, h + 2, CHASSIS_LO, CHASSIS_HI);
}

fn title_bar(c: &mut Canvas, text: &str, focused: bool, close_down: bool, shade_down: bool) {
    let (top, bottom) = if focused {
        (TITLE_TOP, TITLE_BOTTOM)
    } else {
        (TITLE_IDLE_TOP, TITLE_IDLE_BOTTOM)
    };
    c.vgradient(1, 1, c.w as i32 - 2, 12, top, bottom);
    // Machined ribs either side of the name, each with a dark shadow.
    let cx = c.w as i32 / 2;
    let half = (text.chars().count() as i32 * ADVANCE) / 2 + 6;
    for line in [3, 5, 7, 9] {
        let (light, dark) = (mix(top, 0xffffff, 0.28), mix(bottom, 0x000000, 0.45));
        for (y, color) in [(line, light), (line + 1, dark)] {
            c.fill(5, y, cx - half - 5, 1, color);
            c.fill(cx + half, y, c.w as i32 - 32 - (cx + half), 1, color);
        }
    }
    let color = if focused { 0xe8f4f4 } else { LABEL };
    c.text_center(cx, 3, text, color);
    window_buttons(c, close_down, shade_down, false);
}

/// The two small buttons at the right of a title bar.
fn window_buttons(c: &mut Canvas, close_down: bool, shade_down: bool, shaded: bool) {
    let r = close_rect();
    button(c, r, close_down, false);
    for i in 0..5 {
        c.set(r.0 + 2 + i, r.1 + 2 + i, LABEL);
        c.set(r.0 + 6 - i, r.1 + 2 + i, LABEL);
    }
    let r = shade_rect();
    button(c, r, shade_down, false);
    if shaded {
        c.bevel(r.0 + 2, r.1 + 2, 5, 5, LABEL, LABEL);
    } else {
        c.fill(r.0 + 2, r.1 + 6, 5, 1, LABEL);
    }
}

fn close_rect() -> Rect {
    (WIDTH as i32 - 13, 3, 9, 9)
}

fn shade_rect() -> Rect {
    (WIDTH as i32 - 24, 3, 9, 9)
}

/// The background of a collapsed window.
fn shade_strip(c: &mut Canvas, focused: bool) {
    chassis(c);
    let (top, bottom) = if focused {
        (TITLE_TOP, TITLE_BOTTOM)
    } else {
        (TITLE_IDLE_TOP, TITLE_IDLE_BOTTOM)
    };
    c.vgradient(1, 1, c.w as i32 - 2, 12, top, bottom);
}

fn chassis(c: &mut Canvas) {
    let (w, h) = (c.w as i32, c.h as i32);
    // A faint irregular grain reads as brushed metal at this size.
    for y in 0..h {
        let grain = [0.0, 0.05, -0.03, 0.02, 0.06, -0.04, 0.01][(y as usize * 5 + y as usize / 3) % 7];
        let color = if grain >= 0.0 {
            mix(CHASSIS, 0xffffff, grain)
        } else {
            mix(CHASSIS, 0x000000, -grain)
        };
        c.fill(0, y, w, 1, color);
    }
    c.bevel(0, 0, w, h, CHASSIS_HI, CHASSIS_LO);
}

fn format_time(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

// ---------------------------------------------------------------- main window

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainCtl {
    Close,
    Shade,
    /// The options button and clutter bar of a skin.
    Options,
    AlwaysOnTop,
    FileInfo,
    Prev,
    Play,
    Pause,
    Stop,
    Next,
    Open,
    Shuffle,
    Repeat,
    ToggleEq,
    TogglePlaylist,
    ToggleVisuals,
    Volume,
    Balance,
    Position,
    Time,
    Spectrum,
}

const TIME_R: Rect = (26, 21, 62, 18);
const SPECTRUM_R: Rect = (12, 42, BARS as i32 * 4, 17);
const TICKER_R: Rect = (133, 19, 134, 11);
const INFO_R: Rect = (133, 34, 134, 11);
const VOLUME_R: Rect = (152, 50, 52, 8);
const BALANCE_R: Rect = (229, 50, 38, 8);
const POSITION_R: Rect = (8, 68, 259, 10);
const TRANSPORT_Y: i32 = 86;
const PREV_R: Rect = (8, TRANSPORT_Y, 23, 20);
const PLAY_R: Rect = (32, TRANSPORT_Y, 23, 20);
const PAUSE_R: Rect = (56, TRANSPORT_Y, 23, 20);
const STOP_R: Rect = (80, TRANSPORT_Y, 23, 20);
const NEXT_R: Rect = (104, TRANSPORT_Y, 23, 20);
const OPEN_R: Rect = (132, TRANSPORT_Y, 21, 20);
const SHUFFLE_R: Rect = (156, TRANSPORT_Y + 3, 27, 14);
const REPEAT_R: Rect = (184, TRANSPORT_Y + 3, 27, 14);
const TOGGLE_EQ_R: Rect = (214, TRANSPORT_Y + 3, 16, 14);
const TOGGLE_PL_R: Rect = (231, TRANSPORT_Y + 3, 16, 14);
const TOGGLE_VIS_R: Rect = (248, TRANSPORT_Y + 3, 19, 14);
/// Samples of oscilloscope trace, one per pixel across the display.
pub const SCOPE_WIDTH: usize = BARS * 4;

pub fn hit_main(x: i32, y: i32) -> Option<MainCtl> {
    use MainCtl::*;
    [
        (close_rect(), Close),
        (shade_rect(), Shade),
        (PREV_R, Prev),
        (PLAY_R, Play),
        (PAUSE_R, Pause),
        (STOP_R, Stop),
        (NEXT_R, Next),
        (OPEN_R, Open),
        (SHUFFLE_R, Shuffle),
        (REPEAT_R, Repeat),
        (TOGGLE_EQ_R, ToggleEq),
        (TOGGLE_PL_R, TogglePlaylist),
        (TOGGLE_VIS_R, ToggleVisuals),
        (VOLUME_R, Volume),
        (BALANCE_R, Balance),
        (POSITION_R, Position),
        (TIME_R, Time),
        (SPECTRUM_R, Spectrum),
    ]
    .into_iter()
    .find(|(r, _)| inside(*r, x, y))
    .map(|(_, ctl)| ctl)
}

/// Slider value 0..=1 for a cursor position over the volume or balance track.
pub fn volume_from_x(x: i32) -> f32 {
    ((x - VOLUME_R.0) as f32 / (VOLUME_R.2 - 1) as f32).clamp(0.0, 1.0)
}
pub fn balance_from_x(x: i32) -> f32 {
    let v = ((x - BALANCE_R.0) as f32 / (BALANCE_R.2 - 1) as f32 * 2.0 - 1.0).clamp(-1.0, 1.0);
    // Sticky centre, because exactly centred is what people want.
    if v.abs() < 0.08 { 0.0 } else { v }
}
pub fn position_from_x(x: i32) -> f32 {
    ((x - POSITION_R.0) as f32 / (POSITION_R.2 - 1) as f32).clamp(0.0, 1.0)
}

/// What a skin shows in its separate bitrate, rate and channel displays.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TrackFacts {
    pub kbps: Option<u32>,
    pub khz: Option<u32>,
    pub channels: Option<u8>,
}

pub struct MainView<'a> {
    pub focused: bool,
    pub pressed: Option<MainCtl>,
    pub state: PlayState,
    pub position: f64,
    pub duration: Option<f64>,
    pub seek_preview: Option<f32>,
    pub show_remaining: bool,
    pub ticker: &'a str,
    pub ticker_offset: i32,
    pub ticker_alert: bool,
    pub info: &'a str,
    pub facts: TrackFacts,
    pub always_on_top: bool,
    pub volume: f32,
    pub balance: f32,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub eq_shown: bool,
    pub playlist_shown: bool,
    pub visuals_shown: bool,
    pub main_vis: MainVis,
    pub bars: &'a [f32; BARS],
    pub peaks: &'a [f32; BARS],
    pub scope: &'a [f32; SCOPE_WIDTH],
}

pub fn draw_main(c: &mut Canvas, v: &MainView) {
    chassis(c);
    title_bar(
        c,
        "FERROAMP",
        v.focused,
        v.pressed == Some(MainCtl::Close),
        v.pressed == Some(MainCtl::Shade),
    );

    // Time and spectrum display.
    well(c, (8, 18, 120, 44));
    draw_state_icon(c, 13, 24, v.state);
    draw_time(c, v);
    draw_spectrum(c, v);
    draw_unit_labels(c, v);

    // Track name and stream facts.
    well(c, TICKER_R);
    let color = if v.ticker_alert { RED } else { DISPLAY };
    c.ticker(
        TICKER_R.0 + 2,
        TICKER_R.1 + 2,
        TICKER_R.2 - 4,
        v.ticker,
        v.ticker_offset,
        color,
    );
    well(c, INFO_R);
    c.text(INFO_R.0 + 2, INFO_R.1 + 2, v.info, DISPLAY_ALT);

    // Volume and balance.
    c.text(132, 51, "VOL", LABEL);
    draw_hslider(c, VOLUME_R, v.volume, v.pressed == Some(MainCtl::Volume));
    c.text(208, 51, "BAL", LABEL);
    draw_hslider(
        c,
        BALANCE_R,
        (v.balance + 1.0) * 0.5,
        v.pressed == Some(MainCtl::Balance),
    );

    // Position bar: the groove fills with amber as the track plays.
    let frac = v.seek_preview.map(|p| p as f64).unwrap_or_else(|| {
        v.duration
            .filter(|d| *d > 0.0)
            .map_or(0.0, |d| (v.position / d).clamp(0.0, 1.0))
    }) as f32;
    well(c, POSITION_R);
    let (px, py, pw, ph) = POSITION_R;
    if v.duration.is_some() {
        let fill = (frac * (pw - 2) as f32) as i32;
        c.vgradient(px + 1, py + 2, fill, ph - 4, DISPLAY, DISPLAY_DIM);
        let knob = (px + fill - 5).clamp(px, px + pw - 12);
        let r = (knob, py, 12, ph);
        button(c, r, v.pressed == Some(MainCtl::Position), false);
        c.fill(knob + 5, py + 2, 2, ph - 4, LABEL);
    }

    // Transport.
    let pressed = |ctl| v.pressed == Some(ctl);
    for (r, ctl, icon) in [
        (PREV_R, MainCtl::Prev, Icon::Prev),
        (PLAY_R, MainCtl::Play, Icon::Play),
        (PAUSE_R, MainCtl::Pause, Icon::Pause),
        (STOP_R, MainCtl::Stop, Icon::Stop),
        (NEXT_R, MainCtl::Next, Icon::Next),
        (OPEN_R, MainCtl::Open, Icon::Eject),
    ] {
        button(c, r, pressed(ctl), false);
        let dy = i32::from(pressed(ctl));
        draw_icon(c, r.0 + r.2 / 2, r.1 + r.3 / 2 + dy, icon, LABEL);
    }
    labelled(c, SHUFFLE_R, "SHUF", pressed(MainCtl::Shuffle), v.shuffle);
    let repeat = if v.repeat == Repeat::One { "REP1" } else { "REPT" };
    labelled(c, REPEAT_R, repeat, pressed(MainCtl::Repeat), v.repeat != Repeat::Off);
    labelled(c, TOGGLE_EQ_R, "EQ", pressed(MainCtl::ToggleEq), v.eq_shown);
    labelled(
        c,
        TOGGLE_PL_R,
        "PL",
        pressed(MainCtl::TogglePlaylist),
        v.playlist_shown,
    );
    labelled(
        c,
        TOGGLE_VIS_R,
        "VIS",
        pressed(MainCtl::ToggleVisuals),
        v.visuals_shown,
    );
}

const SHADE_TICKER_R: Rect = (52, 2, 114, 10);
/// Room for a title in the collapsed main window.
pub const SHADE_TICKER_ROOM: i32 = SHADE_TICKER_R.2 - 4;
const MINI_Y: i32 = 3;
const MINI_BUTTONS: [(MainCtl, i32); 5] = [
    (MainCtl::Prev, 170),
    (MainCtl::Play, 181),
    (MainCtl::Pause, 192),
    (MainCtl::Stop, 203),
    (MainCtl::Next, 214),
];

pub fn hit_main_shade(x: i32, y: i32) -> Option<MainCtl> {
    if inside(close_rect(), x, y) {
        return Some(MainCtl::Close);
    }
    if inside(shade_rect(), x, y) {
        return Some(MainCtl::Shade);
    }
    MINI_BUTTONS
        .iter()
        .find(|(_, bx)| inside((*bx, MINI_Y, 9, 9), x, y))
        .map(|(ctl, _)| *ctl)
}

/// The main window collapsed: the time, the title and small transport buttons.
pub fn draw_main_shade(c: &mut Canvas, v: &MainView) {
    shade_strip(c, v.focused);
    if v.state != PlayState::Stopped {
        let shown = match (v.show_remaining, v.duration) {
            (true, Some(d)) => format!("-{}", format_time((d - v.position).max(0.0))),
            _ => format_time(v.position),
        };
        c.text(6, 3, &shown, DISPLAY);
    }
    well(c, SHADE_TICKER_R);
    let color = if v.ticker_alert { RED } else { DISPLAY };
    let (x, y, w, _) = SHADE_TICKER_R;
    c.ticker(x + 2, y + 1, w - 4, v.ticker, v.ticker_offset, color);
    for (ctl, bx) in MINI_BUTTONS {
        let down = v.pressed == Some(ctl);
        button(c, (bx, MINI_Y, 9, 9), down, false);
        draw_mini_icon(c, bx + 2, MINI_Y + 2, ctl);
    }
    window_buttons(
        c,
        v.pressed == Some(MainCtl::Close),
        v.pressed == Some(MainCtl::Shade),
        true,
    );
}

/// A 5 by 5 transport symbol.
fn draw_mini_icon(c: &mut Canvas, x: i32, y: i32, ctl: MainCtl) {
    match ctl {
        MainCtl::Play => {
            for i in 0..3 {
                c.fill(x + 1 + i, y + i, 1, 5 - 2 * i, LABEL);
            }
        }
        MainCtl::Pause => {
            c.fill(x, y, 2, 5, LABEL);
            c.fill(x + 3, y, 2, 5, LABEL);
        }
        MainCtl::Stop => c.fill(x, y, 5, 5, LABEL),
        MainCtl::Prev => {
            c.fill(x, y, 1, 5, LABEL);
            for i in 0..3 {
                c.fill(x + 4 - i, y + i, 1, 5 - 2 * i, LABEL);
            }
        }
        MainCtl::Next => {
            c.fill(x + 4, y, 1, 5, LABEL);
            for i in 0..3 {
                c.fill(x + i, y + i, 1, 5 - 2 * i, LABEL);
            }
        }
        _ => {}
    }
}

fn draw_unit_labels(c: &mut Canvas, v: &MainView) {
    if v.show_remaining {
        c.text(92, 24, "LEFT", DISPLAY_DIM);
    } else {
        c.text(92, 24, "TIME", DISPLAY_DIM);
    }
    let text = match v.state {
        PlayState::Playing => "PLAY",
        PlayState::Paused => "HOLD",
        PlayState::Stopped => "IDLE",
    };
    c.text(92, 49, text, DISPLAY_DIM);
}

fn draw_state_icon(c: &mut Canvas, x: i32, y: i32, state: PlayState) {
    match state {
        PlayState::Playing => {
            for i in 0..5 {
                c.fill(x + i, y + i, 1, 11 - 2 * i, DISPLAY_ALT);
            }
        }
        PlayState::Paused => {
            c.fill(x, y, 3, 11, DISPLAY);
            c.fill(x + 5, y, 3, 11, DISPLAY);
        }
        PlayState::Stopped => c.fill(x, y + 1, 9, 9, DISPLAY_DIM),
    }
}

fn draw_time(c: &mut Canvas, v: &MainView) {
    let (x, y) = (TIME_R.0, TIME_R.1 + 1);
    let shown = match (v.seek_preview, v.duration) {
        (Some(p), Some(d)) => Some(p as f64 * d),
        _ => (v.state != PlayState::Stopped).then_some(v.position),
    };
    let shown = shown.map(|p| match (v.show_remaining, v.duration) {
        (true, Some(d)) => (d - p).max(0.0),
        _ => p,
    });
    if v.show_remaining && shown.is_some() {
        c.fill(x - 1, y + 7, 4, 1, DISPLAY);
    }
    let digits = shown.map(|secs| {
        let total = secs as u64;
        // Long tracks wrap to hours and minutes on the same four digits.
        let (a, b) = if total >= 6000 {
            (total / 3600, (total / 60) % 60)
        } else {
            (total / 60, total % 60)
        };
        [(a / 10 % 10) as u8, (a % 10) as u8, (b / 10) as u8, (b % 10) as u8]
    });
    let at = |i: usize| digits.map(|d| d[i]);
    let on = DISPLAY;
    c.seg_digit(x + 4, y, at(0), on, LCD_OFF);
    c.seg_digit(x + 15, y, at(1), on, LCD_OFF);
    c.fill(x + 27, y + 4, 2, 2, if digits.is_some() { on } else { LCD_OFF });
    c.fill(x + 27, y + 9, 2, 2, if digits.is_some() { on } else { LCD_OFF });
    c.seg_digit(x + 31, y, at(2), on, LCD_OFF);
    c.seg_digit(x + 42, y, at(3), on, LCD_OFF);
}

fn draw_spectrum(c: &mut Canvas, v: &MainView) {
    let (x, y, _, h) = SPECTRUM_R;
    match v.main_vis {
        MainVis::Off => return,
        MainVis::Scope => {
            draw_scope(c, v);
            return;
        }
        MainVis::Spectrum => {}
    }
    for bar in 0..BARS {
        let bx = x + bar as i32 * 4;
        let height = (v.bars[bar] * h as f32).round() as i32;
        for row in 0..height {
            let t = row as f32 / (h - 1) as f32;
            let color = heat(t);
            c.fill(bx, y + h - 1 - row, 3, 1, color);
        }
        if v.peaks[bar] > 0.02 {
            let peak_row = ((v.peaks[bar] * h as f32) as i32).min(h - 1);
            c.fill(bx, y + h - 1 - peak_row, 3, 1, 0xf2f2e6);
        }
    }
}

fn draw_scope(c: &mut Canvas, v: &MainView) {
    let (x, y, _, h) = SPECTRUM_R;
    let row_of = |sample: f32| {
        let t = 0.5 - sample.clamp(-1.0, 1.0) * 0.5;
        (t * (h - 1) as f32).round() as i32
    };
    let mut last = row_of(v.scope[0]);
    for (i, &sample) in v.scope.iter().enumerate() {
        let row = row_of(sample);
        let (from, to) = (row.min(last), row.max(last));
        // Colour runs with distance from the centre line, like the bars' heat.
        let t = ((row - h / 2).abs() as f32 / (h / 2) as f32).clamp(0.0, 1.0);
        let color = heat(t);
        c.fill(x + i as i32, y + from, 1, to - from + 1, color);
        last = row;
    }
}

fn draw_hslider(c: &mut Canvas, r: Rect, value: f32, down: bool) {
    let (x, y, w, h) = r;
    c.fill(x, y + h / 2 - 1, w, 2, LCD);
    c.bevel(x, y + h / 2 - 2, w, 4, CHASSIS_LO, CHASSIS_HI);
    let fill = (value * (w - 2) as f32) as i32;
    c.fill(x + 1, y + h / 2 - 1, fill, 2, DISPLAY_DIM);
    let knob = x + (value * (w - 7) as f32) as i32;
    button(c, (knob, y, 7, h), down, false);
    c.fill(knob + 3, y + 2, 1, h - 4, LABEL);
}

#[derive(Clone, Copy)]
enum Icon {
    Prev,
    Play,
    Pause,
    Stop,
    Next,
    Eject,
}

fn draw_icon(c: &mut Canvas, cx: i32, cy: i32, icon: Icon, color: u32) {
    let triangle_right = |c: &mut Canvas, x: i32| {
        for i in 0..5 {
            c.fill(x + i, cy - 4 + i, 1, 9 - 2 * i, color);
        }
    };
    let triangle_left = |c: &mut Canvas, x: i32| {
        for i in 0..5 {
            c.fill(x + 4 - i, cy - 4 + i, 1, 9 - 2 * i, color);
        }
    };
    match icon {
        Icon::Play => triangle_right(c, cx - 2),
        Icon::Pause => {
            c.fill(cx - 4, cy - 4, 3, 9, color);
            c.fill(cx + 1, cy - 4, 3, 9, color);
        }
        Icon::Stop => c.fill(cx - 4, cy - 4, 9, 9, color),
        Icon::Prev => {
            c.fill(cx - 6, cy - 4, 2, 9, color);
            triangle_left(c, cx - 3);
            triangle_left(c, cx + 2);
        }
        Icon::Next => {
            c.fill(cx + 5, cy - 4, 2, 9, color);
            triangle_right(c, cx - 6);
            triangle_right(c, cx - 1);
        }
        Icon::Eject => {
            for i in 0..5 {
                c.fill(cx - i, cy - 5 + i, 2 * i + 1, 1, color);
            }
            c.fill(cx - 4, cy + 2, 9, 2, color);
        }
    }
}

// ------------------------------------------------------------- equaliser window

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqCtl {
    Close,
    Shade,
    Power,
    Preset,
    Preamp,
    Band(usize),
}

const EQ_POWER_R: Rect = (8, 18, 24, 14);
const EQ_PRESET_R: Rect = (35, 18, 38, 14);
const EQ_NAME_R: Rect = (77, 19, 73, 12);
const EQ_CURVE_R: Rect = (155, 19, 112, 12);
const SLIDER_TOP: i32 = 37;
const SLIDER_H: i32 = 60;
const PREAMP_X: i32 = 13;
const BAND_X0: i32 = 58;
const BAND_STEP: i32 = 21;
const SLIDER_W: i32 = 11;

pub fn hit_eq(x: i32, y: i32) -> Option<EqCtl> {
    if inside(close_rect(), x, y) {
        return Some(EqCtl::Close);
    }
    if inside(shade_rect(), x, y) {
        return Some(EqCtl::Shade);
    }
    if inside(EQ_POWER_R, x, y) {
        return Some(EqCtl::Power);
    }
    if inside(EQ_PRESET_R, x, y) || inside(EQ_NAME_R, x, y) {
        return Some(EqCtl::Preset);
    }
    if inside((PREAMP_X, SLIDER_TOP, SLIDER_W, SLIDER_H), x, y) {
        return Some(EqCtl::Preamp);
    }
    (0..BANDS)
        .find(|&b| {
            let bx = BAND_X0 + b as i32 * BAND_STEP;
            inside((bx - 3, SLIDER_TOP, SLIDER_W + 6, SLIDER_H), x, y)
        })
        .map(EqCtl::Band)
}

/// dB for a cursor height. Sticky around zero, so flat is easy to get back to.
pub fn db_from_y(y: i32) -> f32 {
    let t = ((y - SLIDER_TOP - 3) as f32 / (SLIDER_H - 7) as f32).clamp(0.0, 1.0);
    let db = MAX_DB * (1.0 - 2.0 * t);
    if db.abs() < 0.8 { 0.0 } else { db }
}

pub struct EqView<'a> {
    pub focused: bool,
    pub pressed: Option<EqCtl>,
    pub enabled: bool,
    pub preset_name: &'a str,
    pub preamp: f32,
    pub gains: [f32; BANDS],
}

pub fn draw_eq(c: &mut Canvas, v: &EqView) {
    chassis(c);
    title_bar(
        c,
        "EQUALIZER",
        v.focused,
        v.pressed == Some(EqCtl::Close),
        v.pressed == Some(EqCtl::Shade),
    );
    labelled(c, EQ_POWER_R, "ON", v.pressed == Some(EqCtl::Power), v.enabled);
    labelled(c, EQ_PRESET_R, "PRESET", v.pressed == Some(EqCtl::Preset), false);
    well(c, EQ_NAME_R);
    c.text(EQ_NAME_R.0 + 2, EQ_NAME_R.1 + 3, v.preset_name, DISPLAY);
    well(c, EQ_CURVE_R);
    draw_curve(c, v);

    draw_vslider(c, PREAMP_X, v.preamp, v.pressed == Some(EqCtl::Preamp), v.enabled);
    c.text_center(PREAMP_X + SLIDER_W / 2, 100, "PRE", LABEL);
    c.text_right(52, SLIDER_TOP, "+12", LABEL);
    c.text_right(52, SLIDER_TOP + SLIDER_H / 2 - 3, "0", LABEL);
    c.text_right(52, SLIDER_TOP + SLIDER_H - 7, "-12", LABEL);

    const NAMES: [&str; BANDS] = ["60", "170", "310", "600", "1K", "3K", "6K", "12K", "14K", "16K"];
    for (b, name) in NAMES.iter().enumerate() {
        let x = BAND_X0 + b as i32 * BAND_STEP;
        draw_vslider(c, x, v.gains[b], v.pressed == Some(EqCtl::Band(b)), v.enabled);
        c.text_center(x + SLIDER_W / 2, 100, name, LABEL);
    }
}

pub fn hit_eq_shade(x: i32, y: i32) -> Option<EqCtl> {
    if inside(close_rect(), x, y) {
        Some(EqCtl::Close)
    } else if inside(shade_rect(), x, y) {
        Some(EqCtl::Shade)
    } else {
        None
    }
}

pub fn draw_eq_shade(c: &mut Canvas, v: &EqView) {
    shade_strip(c, v.focused);
    let color = if v.focused { 0xe8f4f4 } else { LABEL };
    c.text(6, 3, "EQUALIZER", color);
    let state = if v.enabled { v.preset_name } else { "OFF" };
    c.text(72, 3, state, DISPLAY);
    window_buttons(
        c,
        v.pressed == Some(EqCtl::Close),
        v.pressed == Some(EqCtl::Shade),
        true,
    );
}

fn draw_vslider(c: &mut Canvas, x: i32, db: f32, down: bool, enabled: bool) {
    let y = SLIDER_TOP;
    let cx = x + SLIDER_W / 2;
    c.fill(cx - 1, y, 3, SLIDER_H, LCD);
    c.bevel(cx - 2, y, 5, SLIDER_H, CHASSIS_LO, CHASSIS_HI);
    let t = (1.0 - db / MAX_DB) * 0.5;
    let knob = y + (t * (SLIDER_H - 7) as f32) as i32;
    let mid = y + (SLIDER_H - 7) / 2 + 3;
    let color = if enabled { DISPLAY } else { DISPLAY_DIM };
    let (a, b) = if knob + 3 < mid { (knob + 3, mid) } else { (mid, knob + 3) };
    c.fill(cx - 1, a, 3, b - a, color);
    button(c, (x, knob, SLIDER_W, 7), down, false);
    c.fill(x + 2, knob + 3, SLIDER_W - 4, 1, LABEL);
}

fn draw_curve(c: &mut Canvas, v: &EqView) {
    let (x, y, w, h) = EQ_CURVE_R;
    c.fill(x, y + h / 2, w, 1, LCD_OFF);
    let color = if v.enabled { DISPLAY_ALT } else { DISPLAY_DIM };
    let mut last = None;
    for px in 0..w {
        let pos = px as f32 / (w - 1) as f32 * (BANDS - 1) as f32;
        let i = (pos as usize).min(BANDS - 2);
        let f = pos - i as f32;
        // Smoothstep between sliders reads as a curve rather than a zigzag.
        let f = f * f * (3.0 - 2.0 * f);
        let db = v.gains[i] + (v.gains[i + 1] - v.gains[i]) * f + v.preamp;
        let row = y + ((0.5 - db / (MAX_DB * 2.0)).clamp(0.0, 1.0) * (h - 1) as f32) as i32;
        let (from, to) = match last {
            Some(l) if l < row => (l + 1, row),
            Some(l) if l > row => (row, l - 1),
            _ => (row, row),
        };
        c.fill(x + px, from, 1, to - from + 1, color);
        last = Some(row);
    }
}

pub fn preset_name(index: usize) -> &'static str {
    PRESETS.get(index).map_or("CUSTOM", |p| p.0)
}

// -------------------------------------------------------------- playlist window

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlCtl {
    Close,
    Shade,
    Row(usize),
    Scrollbar,
    Add,
    Folder,
    Url,
    Remove,
    Clear,
    Sort,
    Save,
    /// The five menu buttons along the bottom of a skinned playlist.
    MenuAdd,
    MenuRemove,
    MenuSelect,
    MenuMisc,
    MenuList,
}

pub const ROWS: usize = 21;
const ROW_H: i32 = 9;
const LIST_R: Rect = (8, 18, 250, ROWS as i32 * ROW_H + 1);
const SCROLL_R: Rect = (260, 18, 7, ROWS as i32 * ROW_H + 1);
const PL_BUTTONS_Y: i32 = 212;

const PL_BUTTONS: [(PlCtl, &str, i32, i32); 7] = [
    (PlCtl::Add, "ADD", 8, 24),
    (PlCtl::Folder, "DIR", 33, 24),
    (PlCtl::Url, "URL", 58, 24),
    (PlCtl::Remove, "REM", 83, 24),
    (PlCtl::Clear, "CLR", 108, 24),
    (PlCtl::Sort, "SORT", 133, 29),
    (PlCtl::Save, "SAVE", 163, 29),
];

pub fn hit_playlist(x: i32, y: i32, scroll: usize) -> Option<PlCtl> {
    if inside(close_rect(), x, y) {
        return Some(PlCtl::Close);
    }
    if inside(shade_rect(), x, y) {
        return Some(PlCtl::Shade);
    }
    if inside(LIST_R, x, y) {
        return Some(PlCtl::Row(scroll + ((y - LIST_R.1) / ROW_H) as usize));
    }
    if inside(SCROLL_R, x, y) {
        return Some(PlCtl::Scrollbar);
    }
    PL_BUTTONS
        .iter()
        .find(|(_, _, bx, w)| inside((*bx, PL_BUTTONS_Y, *w, 14), x, y))
        .map(|(ctl, ..)| *ctl)
}

pub fn row_at_y(y: i32, scroll: usize) -> isize {
    scroll as isize + ((y - LIST_R.1).div_euclid(ROW_H)) as isize
}

pub fn scroll_from_y(y: i32, total: usize) -> usize {
    let t = ((y - SCROLL_R.1) as f32 / SCROLL_R.3 as f32).clamp(0.0, 1.0);
    (t * total.saturating_sub(ROWS) as f32).round() as usize
}

pub fn hit_playlist_shade(x: i32, y: i32) -> Option<PlCtl> {
    if inside(close_rect(), x, y) {
        Some(PlCtl::Close)
    } else if inside(shade_rect(), x, y) {
        Some(PlCtl::Shade)
    } else {
        None
    }
}

/// The search box that takes over the list while it is open.
pub struct JumpView<'a> {
    pub query: &'a str,
    pub matches: &'a [u64],
    pub cursor: usize,
}

pub struct PlaylistView<'a> {
    pub focused: bool,
    pub pressed: Option<PlCtl>,
    pub playlist: &'a Playlist,
    pub scroll: usize,
    pub jump: Option<JumpView<'a>>,
}

/// The playlist collapsed: what is playing, and for how long.
pub fn draw_playlist_shade(c: &mut Canvas, v: &PlaylistView) {
    shade_strip(c, v.focused);
    let pl = v.playlist;
    let current = pl.current.and_then(|id| pl.get(id));
    let title = current.map_or_else(|| "PLAYLIST".to_string(), |e| e.display_title());
    let time = current.and_then(|e| e.info.duration).map(format_time).unwrap_or_default();
    let color = if v.focused { 0xe8f4f4 } else { LABEL };
    c.text_right(WIDTH as i32 - 30, 3, &time, color);
    let room = WIDTH as i32 - 36 - font::width(&time) - 6;
    c.text_fit(6, 3, room, &title, color);
    window_buttons(
        c,
        v.pressed == Some(PlCtl::Close),
        v.pressed == Some(PlCtl::Shade),
        true,
    );
}

/// Where the track list goes and what colours it uses.
pub struct ListStyle {
    pub rect: Rect,
    pub rows: usize,
    pub row_h: i32,
    pub normal: u32,
    pub current: u32,
    pub selected: u32,
    pub badge: u32,
}

/// The rows of the playlist, or of the search results while searching.
pub fn draw_list(c: &mut Canvas, v: &PlaylistView, style: &ListStyle) {
    let pl = v.playlist;
    let (lx, ly, lw, _) = style.rect;
    let visible: Vec<(usize, &crate::playlist::Entry)> = match &v.jump {
        Some(jump) => jump
            .matches
            .iter()
            .skip(v.scroll)
            .take(style.rows)
            .filter_map(|&id| Some((pl.index_of(id)?, pl.get(id)?)))
            .collect(),
        None => pl.entries.iter().enumerate().skip(v.scroll).take(style.rows).collect(),
    };
    for (row, (i, e)) in visible.into_iter().enumerate() {
        let y = ly + row as i32 * style.row_h;
        let highlighted = match &v.jump {
            Some(jump) => jump.cursor == v.scroll + row,
            None => pl.selected.contains(&e.id),
        };
        if highlighted {
            c.fill(lx, y, lw, style.row_h, style.selected);
        }
        let color = if e.failed {
            RED
        } else if pl.current == Some(e.id) {
            style.current
        } else {
            style.normal
        };
        let time = e.info.duration.map(format_time).unwrap_or_default();
        let mut right = lx + lw - 3;
        c.text_right(right, y + 1, &time, color);
        right -= font::width(&time);
        if let Some(place) = pl.queue_position(e.id) {
            let badge = format!("[{place}]");
            c.text_right(right - 3, y + 1, &badge, style.badge);
            right -= font::width(&badge) + 3;
        }
        c.text_fit(lx + 2, y + 1, right - lx - 2 - ADVANCE, &format!("{}. {}", i + 1, e.display_title()), color);
    }
}

pub fn draw_playlist(c: &mut Canvas, v: &PlaylistView) {
    chassis(c);
    title_bar(
        c,
        "PLAYLIST",
        v.focused,
        v.pressed == Some(PlCtl::Close),
        v.pressed == Some(PlCtl::Shade),
    );
    well(c, LIST_R);
    well(c, SCROLL_R);

    let pl = v.playlist;
    let (lx, ly, lw, _) = LIST_R;
    if pl.entries.is_empty() && v.jump.is_none() {
        c.text_center(lx + lw / 2, ly + 72, "Drop files or folders here", DISPLAY_DIM);
        c.text_center(lx + lw / 2, ly + 84, "or press L to open", DISPLAY_DIM);
        c.text_center(lx + lw / 2, ly + 102, "Radio: URL button, or", DISPLAY_DIM);
        c.text_center(lx + lw / 2, ly + 114, "paste a stream address", DISPLAY_DIM);
    }
    draw_list(
        c,
        v,
        &ListStyle {
            rect: LIST_R,
            rows: ROWS,
            row_h: ROW_H,
            normal: DISPLAY,
            current: CURRENT,
            selected: SELECT,
            badge: DISPLAY_ALT,
        },
    );

    // Scroll thumb.
    let (sx, sy, sw, sh) = SCROLL_R;
    let total = pl.entries.len();
    let rows_total = v.jump.as_ref().map_or(total, |j| j.matches.len());
    if rows_total > ROWS {
        let thumb = ((ROWS as f32 / rows_total as f32) * sh as f32).max(10.0) as i32;
        let t = v.scroll as f32 / (rows_total - ROWS) as f32;
        let ty = sy + (t * (sh - thumb) as f32) as i32;
        button(c, (sx, ty, sw, thumb), v.pressed == Some(PlCtl::Scrollbar), false);
    }

    if let Some(jump) = &v.jump {
        draw_jump_bar(c, jump, total, (8, PL_BUTTONS_Y + 1, 206, 12), 266);
        return;
    }
    for (ctl, label, bx, w) in PL_BUTTONS {
        labelled(c, (bx, PL_BUTTONS_Y, w, 14), label, v.pressed == Some(ctl), false);
    }
    // Drop the track count when the total time alone needs the room.
    let time = format_time(pl.total_seconds());
    let full = format!("{total} / {time}");
    let room = ((266 - 195) / ADVANCE) as usize;
    let summary = if full.chars().count() <= room { full } else { time };
    c.text_right(266, PL_BUTTONS_Y + 3, &summary, DISPLAY_ALT);
}

/// The search box, drawn over whatever is at the bottom of the window.
pub fn draw_jump_bar(c: &mut Canvas, jump: &JumpView, total: usize, bar: Rect, summary_right: i32) {
    well(c, bar);
    c.text(bar.0 + 2, bar.1 + 2, "JUMP", DISPLAY_ALT);
    let start = bar.0 + 30;
    let room = bar.2 - 32 - ADVANCE;
    // Keep the end of a long query, where the typing is, in view.
    let width = font::width(jump.query);
    let x = if width > room { start + room - width } else { start };
    c.text_between(x, bar.1 + 2, start, start + room, jump.query, DISPLAY);
    c.fill((x + width).max(start), bar.1 + 2, 5, 7, DISPLAY_DIM);
    let found = jump.matches.len();
    let summary = if found == total { format!("{total}") } else { format!("{found}/{total}") };
    c.text_right(summary_right, bar.1 + 2, &summary, DISPLAY_ALT);
}

/// The application icon: pixel art scaled to the requested size.
pub fn draw_app_icon() -> Canvas {
    const LEVELS: [i32; 7] = [7, 13, 19, 15, 21, 11, 16];
    let mut c = Canvas::new(32, 32);
    c.fill(2, 2, 28, 28, CHASSIS);
    c.bevel(2, 2, 28, 28, CHASSIS_HI, CHASSIS_LO);
    c.fill(4, 4, 24, 24, LCD);
    for (i, &level) in LEVELS.iter().enumerate() {
        let x = 6 + i as i32 * 3;
        for row in 0..level {
            let t = row as f32 / 21.0;
            let color = heat(t);
            c.fill(x, 26 - row, 2, 1, color);
        }
    }
    c
}
