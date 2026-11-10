//! Renders each window to a PNG in target/ui-snapshots, so the look can be
//! checked without launching the app. Also regenerates the app icon source.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use ferroamp::engine::PlayState;
use ferroamp::engine::eq::{BANDS, PRESETS};
use ferroamp::engine::spectrum::BARS;
use ferroamp::meta::TrackInfo;
use ferroamp::playlist::{Playlist, Repeat};
use ferroamp::settings::MainVis;
use ferroamp::ui::gfx::Canvas;
use ferroamp::ui::panels::*;
use ferroamp::ui::skin::{self, Skin};
use ferroamp::ui::visuals::{AudioFrame, PRESET_COUNT, Visualizer, WAVE_LEN};

fn write_png(canvas: &Canvas, scale: usize, path: &Path) {
    let (w, h) = (canvas.w * scale, canvas.h * scale);
    let mut buf = vec![0u32; w * h];
    canvas.blit_scaled(&mut buf, w, h, scale, 0);
    let mut enc = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    let bytes: Vec<u8> = buf
        .iter()
        .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8])
        .collect();
    writer.write_image_data(&bytes).unwrap();
}

fn out(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn bars() -> ([f32; BARS], [f32; BARS]) {
    let mut bars = [0.0; BARS];
    let mut peaks = [0.0; BARS];
    for i in 0..BARS {
        bars[i] = 0.95 - i as f32 * 0.04 + ((i * 7) % 5) as f32 * 0.05;
        peaks[i] = (bars[i] + 0.12).min(1.0);
    }
    (bars, peaks)
}

#[test]
fn main_window_playing() {
    let (b, p) = bars();
    let mut c = Canvas::new(WIDTH, MAIN_H);
    draw_main(
        &mut c,
        &MainView {
            focused: true,
            pressed: None,
            state: PlayState::Playing,
            position: 127.0,
            duration: Some(263.0),
            seek_preview: None,
            show_remaining: false,
            ticker: "1. Kraftklub - Zolw (4:23)  ***  ",
            ticker_offset: 0,
            ticker_alert: false,
            info: "320 KBPS 44 KHZ STEREO",
            facts: TrackFacts::default(),
            always_on_top: false,
            volume: 0.8,
            balance: 0.0,
            shuffle: true,
            repeat: Repeat::Off,
            eq_shown: true,
            playlist_shown: true,
            visuals_shown: true,
            main_vis: MainVis::Spectrum,
            bars: &b,
            peaks: &p,
            scope: &[0.0; SCOPE_WIDTH],
        },
    );
    write_png(&c, 3, &out("main_playing.png"));
}

#[test]
fn main_window_idle() {
    let z = [0.0; BARS];
    let mut c = Canvas::new(WIDTH, MAIN_H);
    draw_main(
        &mut c,
        &MainView {
            focused: false,
            pressed: Some(MainCtl::Play),
            state: PlayState::Stopped,
            position: 0.0,
            duration: None,
            seek_preview: None,
            show_remaining: false,
            ticker: "ferroamp - drop some music on me",
            ticker_offset: 0,
            ticker_alert: false,
            info: "",
            facts: TrackFacts::default(),
            always_on_top: false,
            volume: 0.5,
            balance: -0.5,
            shuffle: false,
            repeat: Repeat::All,
            eq_shown: false,
            playlist_shown: false,
            visuals_shown: false,
            main_vis: MainVis::Spectrum,
            bars: &z,
            peaks: &z,
            scope: &[0.0; SCOPE_WIDTH],
        },
    );
    write_png(&c, 3, &out("main_idle.png"));
}

#[test]
fn equaliser_window() {
    let (name, preamp, gains) = PRESETS[7];
    assert_eq!(gains.len(), BANDS);
    let mut c = Canvas::new(WIDTH, EQ_H);
    draw_eq(
        &mut c,
        &EqView {
            focused: true,
            pressed: None,
            enabled: true,
            preset_name: name,
            preamp,
            gains,
        },
    );
    write_png(&c, 3, &out("eq.png"));
}

#[test]
fn playlist_window() {
    let mut pl = Playlist::new(1);
    let added = pl.add_paths((0..30).map(|i| PathBuf::from(format!("/m/track {i}.mp3"))));
    let titles = [
        "Blue Monday", "Żółta łódź na jeziorze", "Very Long Title That Certainly Will Not Fit In This Row At All",
    ];
    for (i, (id, _)) in added.iter().enumerate() {
        let e = pl.get_mut(*id).unwrap();
        e.scanned = true;
        e.info = TrackInfo {
            title: Some(titles[i % 3].to_string()),
            artist: Some("Some Artist".into()),
            duration: Some(180.0 + i as f64 * 11.0),
            ..TrackInfo::default()
        };
    }
    pl.entries[6].failed = true;
    // Two stations: one playing a song, one not yet heard from.
    let stations = pl.add_paths([
        PathBuf::from("https://ice.example.com/groove-128"),
        PathBuf::from("http://radio.example/live"),
    ]);
    let station = pl.get_mut(stations[0].0).unwrap();
    station.info.station = Some("Groove FM".into());
    station.info.title = Some("DJ Someone - Late Night".into());
    let moved: Vec<_> = pl.entries.drain(30..).collect();
    for (i, e) in moved.into_iter().enumerate() {
        pl.entries.insert(8 + i, e);
    }
    pl.selected = vec![added[4].0, added[5].0];
    let mut c = Canvas::new(WIDTH, PLAYLIST_H);
    draw_playlist(
        &mut c,
        &PlaylistView {
            focused: true,
            pressed: None,
            playlist: &pl,
            scroll: 0,
            jump: None,
        },
    );
    write_png(&c, 3, &out("playlist.png"));

    let empty = Playlist::new(1);
    let mut c = Canvas::new(WIDTH, PLAYLIST_H);
    draw_playlist(
        &mut c,
        &PlaylistView {
            focused: false,
            pressed: None,
            playlist: &empty,
            scroll: 0,
            jump: None,
        },
    );
    write_png(&c, 3, &out("playlist_empty.png"));
}

#[test]
fn collapsed_windows_and_other_scripts() {
    let (b, p) = bars();
    let mut pl = Playlist::new(1);
    let titles = [
        ("\u{41a}\u{438}\u{43d}\u{43e}", "\u{41f}\u{0435}\u{0440}\u{0435}\u{043c}\u{0435}\u{043d}"),
        ("\u{5b87}\u{591a}\u{7530}\u{30d2}\u{30ab}\u{30eb}", "\u{521d}\u{604b}"),
        ("Some Artist", "\u{3042}\u{308a}\u{304c}\u{3068}\u{3046} and more"),
        ("\u{BC29}\u{D0C4}\u{C18C}\u{B144}\u{B2E8}", "\u{BD04}\u{B0A0}"),
        ("Bj\u{f6}rk", "J\u{f3}ga"),
    ];
    let added = pl.add_paths((0..5).map(|i| PathBuf::from(format!("/m/t{i}.mp3"))));
    for (i, (id, _)) in added.iter().enumerate() {
        let e = pl.get_mut(*id).unwrap();
        e.info = TrackInfo {
            artist: Some(titles[i].0.into()),
            title: Some(titles[i].1.into()),
            duration: Some(200.0 + i as f64),
            ..TrackInfo::default()
        };
    }
    pl.set_current(Some(added[1].0));
    pl.toggle_queued(added[3].0);
    pl.toggle_queued(added[0].0);

    let mut c = Canvas::new(WIDTH, PLAYLIST_H);
    draw_playlist(&mut c, &PlaylistView { focused: true, pressed: None, playlist: &pl, scroll: 0, jump: None });
    write_png(&c, 4, &out("playlist_scripts.png"));

    let matches = pl.search("o");
    let mut c = Canvas::new(WIDTH, PLAYLIST_H);
    draw_playlist(
        &mut c,
        &PlaylistView {
            focused: true,
            pressed: None,
            playlist: &pl,
            scroll: 0,
            jump: Some(JumpView { query: "bjo", matches: &matches, cursor: 0 }),
        },
    );
    write_png(&c, 3, &out("playlist_jump.png"));

    let view = MainView {
        focused: true,
        pressed: None,
        state: PlayState::Playing,
        position: 127.0,
        duration: Some(263.0),
        seek_preview: None,
        show_remaining: false,
        ticker: "2. \u{5b87}\u{591a}\u{7530}\u{30d2}\u{30ab}\u{30eb} - \u{521d}\u{604b} (3:21)  ***  ",
        ticker_offset: 0,
        ticker_alert: false,
        info: "320 KBPS 44 KHZ STEREO",
        facts: TrackFacts::default(),
        always_on_top: false,
        volume: 0.8,
        balance: 0.0,
        shuffle: false,
        repeat: Repeat::One,
        eq_shown: true,
        playlist_shown: true,
        visuals_shown: false,
        main_vis: MainVis::Spectrum,
        bars: &b,
        peaks: &p,
        scope: &[0.0; SCOPE_WIDTH],
    };
    let mut c = Canvas::new(WIDTH, MAIN_H);
    draw_main(&mut c, &view);
    write_png(&c, 3, &out("main_scripts.png"));

    let mut sheet = Canvas::new(WIDTH, SHADE_H * 3);
    let mut strip = Canvas::new(WIDTH, SHADE_H);
    draw_main_shade(&mut strip, &view);
    sheet.px[..strip.px.len()].copy_from_slice(&strip.px);
    draw_eq_shade(
        &mut strip,
        &EqView { focused: true, pressed: None, enabled: true, preset_name: "ROCK", preamp: 0.0, gains: [0.0; BANDS] },
    );
    sheet.px[strip.px.len()..strip.px.len() * 2].copy_from_slice(&strip.px);
    draw_playlist_shade(&mut strip, &PlaylistView { focused: false, pressed: None, playlist: &pl, scroll: 0, jump: None });
    sheet.px[strip.px.len() * 2..].copy_from_slice(&strip.px);
    write_png(&sheet, 3, &out("shade_strips.png"));
}

#[test]
fn collapsed_windows_answer_where_they_are_drawn() {
    assert_eq!(hit_main_shade(265, 6), Some(MainCtl::Close));
    assert_eq!(hit_main_shade(255, 6), Some(MainCtl::Shade));
    assert_eq!(hit_main_shade(185, 7), Some(MainCtl::Play));
    assert_eq!(hit_main_shade(100, 7), None, "the rest drags the window");
    assert_eq!(hit_eq_shade(255, 6), Some(EqCtl::Shade));
    assert_eq!(hit_playlist_shade(255, 6), Some(PlCtl::Shade));
    assert_eq!(hit_main(255, 6), Some(MainCtl::Shade));
    assert_eq!(hit_eq(255, 6), Some(EqCtl::Shade));
    assert_eq!(hit_playlist(255, 6, 0), Some(PlCtl::Shade));
}

#[test]
fn every_control_is_hittable_where_it_is_drawn() {
    assert_eq!(hit_main(40, 94), Some(MainCtl::Play));
    assert_eq!(hit_main(140, 94), Some(MainCtl::Open));
    assert_eq!(hit_main(100, 72), Some(MainCtl::Position));
    assert_eq!(hit_main(170, 53), Some(MainCtl::Volume));
    assert_eq!(hit_main(265, 6), Some(MainCtl::Close));
    assert_eq!(hit_main(60, 5), None, "title bar drags the window");
    assert_eq!(hit_eq(20, 22), Some(EqCtl::Power));
    assert_eq!(hit_eq(62, 60), Some(EqCtl::Band(0)));
    assert_eq!(hit_eq(62 + 21 * 9, 60), Some(EqCtl::Band(9)));
    assert_eq!(hit_playlist(50, 18, 5), Some(PlCtl::Row(5)));
    assert_eq!(hit_playlist(50, 18 + 9, 5), Some(PlCtl::Row(6)));
    assert_eq!(hit_playlist(70, 218, 0), Some(PlCtl::Url));
    assert_eq!(hit_playlist(175, 218, 0), Some(PlCtl::Save));
}

#[test]
fn regenerate_app_icon_source() {
    let icon = draw_app_icon();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icon.png");
    write_png(&icon, 32, &path);
}

#[test]
fn main_window_oscilloscope() {
    let (b, p) = bars();
    let scope: [f32; SCOPE_WIDTH] = std::array::from_fn(|i| {
        let t = i as f32 / 76.0 * std::f32::consts::TAU;
        (t * 2.0).sin() * 0.6 + (t * 9.0).sin() * 0.25
    });
    let mut c = Canvas::new(WIDTH, MAIN_H);
    draw_main(
        &mut c,
        &MainView {
            focused: true,
            pressed: None,
            state: PlayState::Playing,
            position: 75.0,
            duration: Some(263.0),
            seek_preview: None,
            show_remaining: false,
            ticker: "2. Test - Oscilloscope",
            ticker_offset: 0,
            ticker_alert: false,
            info: "192 KBPS 44 KHZ STEREO",
            facts: TrackFacts::default(),
            always_on_top: false,
            volume: 0.7,
            balance: 0.0,
            shuffle: false,
            repeat: Repeat::Off,
            eq_shown: true,
            playlist_shown: true,
            visuals_shown: true,
            main_vis: MainVis::Scope,
            bars: &b,
            peaks: &p,
            scope: &scope,
        },
    );
    write_png(&c, 3, &out("main_scope.png"));
}

/// Plays a synthetic track through a preset, as the app would at 60 fps.
fn render_preset(preset: usize, frames: usize, v: &mut Visualizer) {
    use std::f32::consts::TAU;
    let dt = 1.0 / 60.0;
    let mut bars_now = [0.0f32; BARS];
    let _ = preset;
    for frame in 0..frames {
        let t = frame as f32 * dt;
        let kick = (1.0 - t.rem_euclid(0.5) * 5.0).max(0.0);
        let chord = (t * 0.25).floor() as usize % 3;
        let note = [220.0, 277.0, 330.0][chord];
        let left: Vec<f32> = (0..WAVE_LEN)
            .map(|i| {
                let x = t + i as f32 / 44_100.0;
                (x * note * TAU).sin() * 0.3
                    + (x * note * 3.01 * TAU).sin() * 0.15
                    + (x * 70.0 * TAU).sin() * kick * 0.5
            })
            .collect();
        let right: Vec<f32> = (0..WAVE_LEN)
            .map(|i| {
                let x = t + i as f32 / 44_100.0;
                (x * note * 1.5 * TAU).cos() * 0.3
                    + (x * note * 2.0 * TAU).sin() * 0.12
                    + (x * 70.0 * TAU).sin() * kick * 0.5
            })
            .collect();
        for (i, bar) in bars_now.iter_mut().enumerate() {
            let shape = ((i as f32 * 0.7 + t * 2.0).sin() * 0.25 + 0.45) * (1.0 - i as f32 / 26.0);
            *bar = (shape + if i < 4 { kick * 0.5 } else { 0.0 }).clamp(0.0, 1.0);
        }
        v.step(
            dt,
            &AudioFrame {
                left: &left,
                right: &right,
                bars: &bars_now,
                bass: kick * 0.7,
                mid: 0.35 + (t * 1.3).sin() * 0.15,
                treble: 0.3 + (t * 5.0).sin().abs() * 0.3,
            },
        );
    }
}

#[test]
fn visuals_contact_sheet() {
    const COLS: usize = 5;
    let (tw, th) = (320, 180);
    let rows = PRESET_COUNT.div_ceil(COLS);
    let mut sheet = Canvas::new(COLS * tw, rows * th);
    for preset in 0..PRESET_COUNT {
        let mut v = Visualizer::new(tw as u32 * 2, th as u32 * 2, preset, 12345);
        render_preset(preset, 300, &mut v);
        let (cx, cy) = ((preset % COLS) * tw, (preset / COLS) * th);
        for y in 0..th.min(v.canvas.h) {
            for x in 0..tw.min(v.canvas.w) {
                sheet.px[(cy + y) * sheet.w + cx + x] = v.canvas.px[y * v.canvas.w + x];
            }
        }
    }
    write_png(&sheet, 1, &out("visuals_presets.png"));
}

/// Prints the frame cost at the largest internal resolution. Run with:
///   cargo test --release --test render_ui frame_cost -- --ignored --nocapture
#[test]
#[ignore = "timing only"]
fn frame_cost() {
    for preset in 0..PRESET_COUNT {
        let mut v = Visualizer::new(3840, 2160, preset, 12345);
        render_preset(preset, 60, &mut v);
        let start = std::time::Instant::now();
        render_preset(preset, 240, &mut v);
        let per_frame = start.elapsed().as_secs_f64() * 1000.0 / 240.0;
        println!(
            "preset {preset}: {per_frame:.2} ms/frame at {}x{}, including test audio synthesis",
            v.canvas.w, v.canvas.h
        );
    }
}

/// A 24-bit bitmap file, written bottom row first as most tools do.
fn bmp(w: usize, h: usize, px: &[u32]) -> Vec<u8> {
    let stride = (w * 3).div_ceil(4) * 4;
    let mut out = b"BM".to_vec();
    out.extend(((54 + stride * h) as u32).to_le_bytes());
    out.extend([0; 4]);
    out.extend(54u32.to_le_bytes());
    out.extend(40u32.to_le_bytes());
    out.extend((w as i32).to_le_bytes());
    out.extend((h as i32).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(24u16.to_le_bytes());
    out.extend([0; 24]);
    for row in px.chunks(w).rev() {
        let mut bytes: Vec<u8> = row.iter().flat_map(|c| [*c as u8, (c >> 8) as u8, (c >> 16) as u8]).collect();
        bytes.resize(stride, 0);
        out.extend(bytes);
    }
    out
}

/// A skin in which each part of the format is one flat, known colour. If a
/// part is taken from the wrong place, or drawn in the wrong place, it shows.
fn test_pattern_skin() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("skin");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    type Part = (usize, usize, usize, usize, u32);
    let sheet = |name: &str, w: usize, h: usize, back: u32, parts: &[Part]| {
        let mut px = vec![back; w * h];
        for &(x, y, pw, ph, color) in parts {
            for row in y..y + ph {
                px[row * w + x..row * w + x + pw].fill(color);
            }
        }
        std::fs::write(dir.join(name), bmp(w, h, &px)).unwrap();
    };
    sheet("main.bmp", 275, 116, 0x303040, &[]);
    sheet("titlebar.bmp", 344, 87, 0x111111, &[(27, 0, 275, 14, 0x3050c0), (27, 15, 275, 14, 0x505070), (27, 29, 275, 13, 0x4060d0), (27, 42, 275, 14, 0x606080), (304, 0, 8, 43, 0x202020)]);
    sheet("cbuttons.bmp", 136, 36, 0x000000, &[(0, 0, 23, 18, 0x101080), (23, 0, 23, 18, 0x108010), (23, 18, 23, 18, 0x10f010), (46, 0, 23, 18, 0x801010), (69, 0, 23, 18, 0x808010), (92, 0, 22, 18, 0x801080), (114, 0, 22, 16, 0x108080)]);
    let mut digits: Vec<Part> = (0..10).map(|d| (d * 9, 0, 9, 13, 0x002000 + d as u32 * 0x001800)).collect();
    digits.push((20, 6, 5, 1, 0xffffff));
    sheet("numbers.bmp", 99, 13, 0x000000, &digits);
    let mut letters: Vec<Part> = (0..26).map(|c| (c * 5, 0, 5, 6, 0x00c000)).collect();
    letters.extend((0..31).map(|c| (c * 5, 6, 5, 6, 0x00a000)));
    sheet("text.bmp", 155, 18, 0x000000, &letters);
    sheet("playpaus.bmp", 42, 9, 0x000000, &[(0, 0, 9, 9, 0x00ff00), (9, 0, 9, 9, 0xffff00), (18, 0, 9, 9, 0xff0000)]);
    sheet("monoster.bmp", 56, 24, 0x000000, &[(0, 0, 29, 12, 0x00ff80), (0, 12, 29, 12, 0x004020), (29, 0, 27, 12, 0x00ff81), (29, 12, 27, 12, 0x004021)]);
    sheet("posbar.bmp", 307, 10, 0x202020, &[(0, 0, 248, 10, 0x202030), (248, 0, 29, 10, 0xc0c0c0), (278, 0, 29, 10, 0xffffff)]);
    let frames: Vec<Part> = (0..28).map(|f| (0, f * 15, 68, 13, (f as u32 * 8) << 16)).chain([(15, 422, 14, 11, 0xd0d0d0), (0, 422, 14, 11, 0xf0f0f0)]).collect();
    sheet("volume.bmp", 68, 433, 0x000000, &frames);
    sheet("shufrep.bmp", 92, 85, 0x000000, &[(28, 0, 47, 15, 0x202050), (28, 30, 47, 15, 0x5050f0), (0, 0, 28, 15, 0x205020), (0, 30, 28, 15, 0x50f050), (0, 61, 23, 12, 0x402020), (0, 73, 23, 12, 0xf08080), (23, 61, 23, 12, 0x204040), (23, 73, 23, 12, 0x80f0f0)]);
    let mut eq: Vec<Part> = vec![(0, 0, 275, 116, 0x283038), (0, 134, 275, 14, 0x3050c0), (0, 149, 275, 14, 0x505070), (10, 119, 26, 12, 0x303030), (69, 119, 26, 12, 0x30f030), (36, 119, 32, 12, 0x404040), (224, 164, 44, 12, 0x505050), (0, 164, 11, 11, 0xd0d0d0), (0, 294, 113, 19, 0x101010), (115, 294, 1, 19, 0xffff00), (0, 314, 113, 1, 0x00ffff)];
    for f in 0..28 {
        eq.push((13 + (f % 14) * 15, 164 + (f / 14) * 65, 14, 63, 0x000040 + f as u32 * 8));
    }
    sheet("eqmain.bmp", 275, 315, 0x000000, &eq);
    sheet("pledit.bmp", 280, 186, 0x000000, &[(0, 0, 25, 20, 0x3050c0), (26, 0, 100, 20, 0x3060d0), (127, 0, 25, 20, 0x3070e0), (153, 0, 25, 20, 0x3080f0), (0, 21, 25, 20, 0x505070), (26, 21, 100, 20, 0x505080), (127, 21, 25, 20, 0x505090), (153, 21, 25, 20, 0x5050a0), (0, 42, 12, 29, 0x607060), (31, 42, 20, 29, 0x708070), (52, 53, 8, 18, 0xe0e0e0), (0, 72, 125, 38, 0x806040), (126, 72, 150, 38, 0x906050)]);
    std::fs::write(dir.join("pledit.txt"), "[Text]\r\nNormal=#00FF00\r\nCurrent=#FFFFFF\r\nNormalBG=#101018\r\nSelectedBG=#0000C6\r\n").unwrap();
    dir
}

#[test]
fn a_skin_puts_every_part_where_the_format_says() {
    let (b, p) = bars();
    let skin = Skin::load(&test_pattern_skin()).unwrap();
    let at = |c: &Canvas, x: usize, y: usize| c.px[y * c.w + x];
    let view = |pressed, ticker| MainView {
        focused: true,
        pressed,
        state: PlayState::Playing,
        position: 83.0,
        duration: Some(200.0),
        seek_preview: None,
        show_remaining: false,
        ticker,
        ticker_offset: 0,
        ticker_alert: false,
        info: "",
        facts: TrackFacts { kbps: Some(128), khz: Some(44), channels: Some(2) },
        always_on_top: false,
        volume: 1.0,
        balance: 0.0,
        shuffle: true,
        repeat: Repeat::Off,
        eq_shown: true,
        playlist_shown: false,
        visuals_shown: false,
        main_vis: MainVis::Spectrum,
        bars: &b,
        peaks: &p,
        scope: &[0.0; SCOPE_WIDTH],
    };

    let mut c = Canvas::new(WIDTH, MAIN_H);
    skin.draw_main(&mut c, &view(None, "ab \u{65e5}\u{672c}"));
    write_png(&c, 3, &out("skin_main.png"));
    assert_eq!(at(&c, 100, 5), 0x3050c0, "focused title bar");
    assert_eq!(at(&c, 45, 90), 0x108010, "play button");
    assert_eq!(at(&c, 70, 90), 0x801010, "pause button");
    assert_eq!(at(&c, 140, 92), 0x108080, "eject button");
    assert_eq!(at(&c, 170, 92), 0x5050f0, "shuffle lit");
    assert_eq!(at(&c, 215, 92), 0x205020, "repeat unlit");
    assert_eq!(at(&c, 222, 60), 0xf08080, "EQ button lit");
    assert_eq!(at(&c, 245, 60), 0x204040, "playlist button unlit");
    assert_eq!(at(&c, 28, 30), 0x00ff00, "play indicator");
    assert_eq!(at(&c, 50, 30), 0x002000, "0 tens of minutes");
    assert_eq!(at(&c, 62, 30), 0x002000 + 0x001800, "1 minute");
    assert_eq!(at(&c, 80, 30), 0x002000 + 2 * 0x001800, "23 seconds, tens");
    assert_eq!(at(&c, 92, 30), 0x002000 + 3 * 0x001800, "23 seconds, units");
    assert_eq!(at(&c, 112, 28), 0x00c000, "first letter of the title");
    assert_eq!(at(&c, 112, 44), 0x00a000, "bitrate digits");
    assert_eq!(at(&c, 245, 45), 0x00ff80, "stereo lit");
    assert_eq!(at(&c, 215, 45), 0x004021, "mono unlit");
    assert_eq!(at(&c, 110, 60), (27u32 * 8) << 16, "volume track, last frame");
    assert_eq!(at(&c, 170, 62), 0xd0d0d0, "volume thumb at the far end");
    assert_eq!(at(&c, 20, 77), 0x202030, "position track");
    let knob = 16 + (83.0 / 200.0 * 219.0) as usize;
    assert_eq!(at(&c, knob + 3, 77), 0xc0c0c0, "position thumb");

    skin.draw_main(&mut c, &view(Some(MainCtl::Play), "x"));
    assert_eq!(at(&c, 45, 90), 0x10f010, "play button held down");

    let mut strip = Canvas::new(WIDTH, SHADE_H);
    skin.draw_main_shade(&mut strip, &view(None, ""));
    assert_eq!(at(&strip, 100, 2), 0x4060d0, "collapsed title bar");

    let mut gains = [0.0; BANDS];
    gains[0] = 12.0;
    gains[9] = -12.0;
    let eq = EqView { focused: true, pressed: None, enabled: true, preset_name: "", preamp: 0.0, gains };
    let mut c = Canvas::new(WIDTH, EQ_H);
    skin.draw_eq(&mut c, &eq);
    write_png(&c, 3, &out("skin_eq.png"));
    assert_eq!(at(&c, 100, 5), 0x3050c0, "EQ title bar");
    assert_eq!(at(&c, 20, 22), 0x30f030, "EQ lit");
    assert_eq!(at(&c, 80, 95), 0x000040 + 27 * 8, "top band uses the last frame");
    assert_eq!(at(&c, 242, 40), 0x000040, "bottom band uses the first frame");
    assert_eq!(at(&c, 80, 42), 0xd0d0d0, "thumb at the top for +12");
    assert_eq!(at(&c, 90, 18), 0x101010, "graph background");

    let mut pl = Playlist::new(1);
    let added = pl.add_paths((0..40).map(|i| PathBuf::from(format!("/m/track {i}.mp3"))));
    pl.set_current(Some(added[1].0));
    pl.selected = vec![added[2].0];
    let list = |jump| PlaylistView { focused: true, pressed: None, playlist: &pl, scroll: 0, jump };
    let mut c = Canvas::new(WIDTH, PLAYLIST_H);
    skin.draw_playlist(&mut c, &list(None));
    write_png(&c, 3, &out("skin_playlist.png"));
    assert_eq!(at(&c, 5, 5), 0x3050c0, "top left corner");
    assert_eq!(at(&c, 137, 5), 0x3060d0, "title in the middle");
    assert_eq!(at(&c, 268, 5), 0x3080f0, "top right corner");
    assert_eq!(at(&c, 5, 100), 0x607060, "left side");
    assert_eq!(at(&c, 257, 100), 0x708070, "right side");
    assert_eq!(at(&c, 10, 200), 0x806040, "bottom left");
    assert_eq!(at(&c, 200, 225), 0x906050, "bottom right");
    assert_eq!(at(&c, 263, 22), 0xe0e0e0, "scroll handle at the top");
    assert_eq!(at(&c, 200, 25), 0x101018, "list background from pledit.txt");
    assert_eq!(at(&c, 200, 39), 0x0000c6, "third row selected");
}

#[test]
fn a_skin_answers_clicks_where_the_format_draws_controls() {
    assert_eq!(skin::hit_main(45, 90), Some(MainCtl::Play));
    assert_eq!(skin::hit_main(140, 92), Some(MainCtl::Open));
    assert_eq!(skin::hit_main(120, 60), Some(MainCtl::Volume));
    assert_eq!(skin::hit_main(190, 60), Some(MainCtl::Balance));
    assert_eq!(skin::hit_main(100, 75), Some(MainCtl::Position));
    assert_eq!(skin::hit_main(267, 6), Some(MainCtl::Close));
    assert_eq!(skin::hit_main(257, 6), Some(MainCtl::Shade));
    assert_eq!(skin::hit_main(13, 58), Some(MainCtl::ToggleVisuals));
    assert_eq!(skin::hit_main(100, 5), None, "the title bar drags the window");
    assert_eq!(skin::hit_eq(20, 22), Some(EqCtl::Power));
    assert_eq!(skin::hit_eq(82, 60), Some(EqCtl::Band(0)));
    assert_eq!(skin::hit_eq(82 + 18 * 9, 60), Some(EqCtl::Band(9)));
    assert_eq!(skin::hit_playlist(50, 20, 5), Some(PlCtl::Row(5)));
    assert_eq!(skin::hit_playlist(50, 29, 5), Some(PlCtl::Row(6)));
    assert_eq!(skin::hit_playlist(20, 210, 0), Some(PlCtl::MenuAdd));
    assert_eq!(skin::hit_playlist(240, 210, 0), Some(PlCtl::MenuList));
    assert_eq!(skin::hit_playlist(263, 100, 0), Some(PlCtl::Scrollbar));
    assert_eq!(skin::volume_from_x(107), 0.0);
    assert_eq!(skin::volume_from_x(175), 1.0);
    assert_eq!(skin::balance_from_x(196), 0.0, "sticks to the centre");
}

#[test]
fn something_that_is_not_a_skin_is_refused() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("notskin");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("music.zip");
    std::fs::write(&file, b"this is not a zip file").unwrap();
    assert!(Skin::load(&file).is_err());
    assert!(Skin::load(&dir.join("missing.wsz")).is_err());
    assert!(Skin::load(&dir).is_err(), "a folder without main.bmp");
}
