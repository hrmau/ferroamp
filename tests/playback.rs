//! Decode-path tests against real encoded files made with ffmpeg. Everything
//! here runs through the same `Pipeline` the app uses, without an audio device.
//! Skipped when ffmpeg is missing, since these files are generated, not stored.

use std::path::{Path, PathBuf};
use std::process::Command;

use ferroamp::engine::gain::ReplayGain;
use ferroamp::engine::pipeline::{Clip, Marker, Pipeline, Queue, Span};

const FREQ: f64 = 440.0;
const AMP: f64 = 0.5;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

/// Encodes `frames` of a continuous sine starting at `offset`, so that two
/// consecutive files form one unbroken wave.
fn encode(name: &str, rate: u32, offset: usize, frames: usize, args: &[&str]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let raw = dir.join(format!("{name}.raw"));
    let pcm: Vec<u8> = (offset..offset + frames)
        .flat_map(|i| {
            let s = (AMP * (std::f64::consts::TAU * FREQ * i as f64 / rate as f64).sin()) as f32;
            let mut b = s.to_le_bytes().to_vec();
            b.extend_from_slice(&s.to_le_bytes());
            b
        })
        .collect();
    std::fs::write(&raw, pcm).unwrap();
    let rate_s = rate.to_string();
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "f32le", "-ar", &rate_s, "-ac", "2", "-i"])
        .arg(&raw)
        .args(args)
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg failed for {name}");
    path
}

struct List(Vec<PathBuf>);

impl Queue for List {
    fn next_after(&mut self, track: u64) -> Option<(u64, Clip)> {
        let next = track as usize + 1;
        self.0.get(next).map(|p| (next as u64, Clip::from(p)))
    }
    fn clip_of(&mut self, track: u64) -> Option<Clip> {
        self.0.get(track as usize).map(Clip::from)
    }
}

fn drain(pipe: &mut Pipeline, queue: &mut dyn Queue) -> Vec<f32> {
    let mut out = Vec::new();
    while pipe.has_source() || !pipe.pending().is_empty() {
        out.extend_from_slice(pipe.pending());
        let n = pipe.pending().len();
        pipe.consume(n);
        pipe.step(queue);
    }
    out
}

fn play_all(files: &[PathBuf], device_rate: u32) -> (Vec<f32>, Vec<Marker>) {
    let mut pipe = Pipeline::new(device_rate);
    let mut queue = List(files.to_vec());
    pipe.start(0, &files[0], 0.0).unwrap();
    let out = drain(&mut pipe, &mut queue);
    (out, pipe.take_markers())
}

fn ideal(frame: usize, rate: u32) -> f32 {
    (AMP * (std::f64::consts::TAU * FREQ * frame as f64 / rate as f64).sin()) as f32
}

fn worst_error(samples: &[f32], rate: u32, from: usize, to: usize) -> f32 {
    (from..to)
        .map(|i| (samples[i * 2] - ideal(i, rate)).abs())
        .fold(0.0, f32::max)
}

#[test]
fn lossless_wav_pair_is_sample_exact_across_the_boundary() {
    if !have_ffmpeg() {
        return;
    }
    let a = encode("a.wav", 44_100, 0, 30_000, &["-c:a", "pcm_f32le"]);
    let b = encode("b.wav", 44_100, 30_000, 30_000, &["-c:a", "pcm_f32le"]);
    let (out, markers) = play_all(&[a, b], 44_100);
    assert_eq!(out.len() / 2, 60_000);
    assert!(worst_error(&out, 44_100, 0, 60_000) < 1e-6);
    assert_eq!(markers[1].seq, 60_000, "second track starts at frame 30000");
    assert_eq!(markers[1].track, 1);
}

#[test]
fn flac_pair_is_gapless_to_within_sixteen_bit_rounding() {
    if !have_ffmpeg() {
        return;
    }
    let a = encode("a.flac", 44_100, 0, 30_000, &["-c:a", "flac", "-sample_fmt", "s16"]);
    let b = encode("b.flac", 44_100, 30_000, 30_000, &["-c:a", "flac", "-sample_fmt", "s16"]);
    let (out, _) = play_all(&[a, b], 44_100);
    assert_eq!(out.len() / 2, 60_000);
    assert!(worst_error(&out, 44_100, 0, 60_000) < 1.5 / 32768.0);
}

#[test]
fn resampled_album_stays_gapless_on_a_48k_device() {
    if !have_ffmpeg() {
        return;
    }
    let a = encode("r_a.wav", 44_100, 0, 44_100, &["-c:a", "pcm_f32le"]);
    let b = encode("r_b.wav", 44_100, 44_100, 44_100, &["-c:a", "pcm_f32le"]);
    let (out, markers) = play_all(&[a, b], 48_000);
    assert_eq!(out.len() / 2, 96_000);
    // Stays locked to the ideal 48 kHz wave straight through the boundary.
    let err = worst_error(&out, 48_000, 4_000, 92_000);
    assert!(err < 0.005, "worst error {err}");
    let boundary = markers[1].seq as i64 / 2;
    assert!((boundary - 48_000).abs() < 1200, "boundary at frame {boundary}");
}

#[test]
fn mp3_encoder_delay_and_padding_are_removed() {
    if !have_ffmpeg() {
        return;
    }
    let args = ["-c:a", "libmp3lame", "-b:a", "192k"];
    let a = encode("a.mp3", 44_100, 0, 44_100, &args);
    let b = encode("b.mp3", 44_100, 44_100, 44_100, &args);
    let (out, _) = play_all(&[a, b], 44_100);
    assert_eq!(out.len() / 2, 88_200, "exactly the encoded length");
    // Lossy, so allow codec error, but a stray gap or a shift would be huge.
    let err = worst_error(&out, 44_100, 2_000, 86_000);
    assert!(err < 0.1, "worst error {err}");
}

#[test]
fn vorbis_length_is_exact() {
    if !have_ffmpeg() {
        return;
    }
    let args = ["-c:a", "libvorbis", "-q:a", "6"];
    let a = encode("a.ogg", 44_100, 0, 44_100, &args);
    let b = encode("b.ogg", 44_100, 44_100, 44_100, &args);
    let (out, _) = play_all(&[a, b], 44_100);
    assert_eq!(out.len() / 2, 88_200);
}

#[test]
fn every_container_decodes_to_audio() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args) in [
        ("c.m4a", &["-c:a", "aac", "-b:a", "128k"][..]),
        ("c_alac.m4a", &["-c:a", "alac"][..]),
        ("c.aiff", &["-c:a", "pcm_s16be"][..]),
        ("c.mp2", &["-c:a", "mp2"][..]),
    ] {
        let f = encode(name, 44_100, 0, 44_100, args);
        let (out, _) = play_all(&[f], 44_100);
        let frames = out.len() / 2;
        assert!(
            (44_100..=47_000).contains(&frames),
            "{name}: decoded {frames} frames"
        );
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.4, "{name}: peak {peak}");
    }
}

#[test]
fn seek_lands_on_the_right_sample() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args) in [
        ("s.wav", &["-c:a", "pcm_f32le"][..]),
        ("s.flac", &["-c:a", "flac", "-sample_fmt", "s32"][..]),
    ] {
        let f = encode(name, 44_100, 0, 88_200, args);
        let mut pipe = Pipeline::new(44_100);
        let mut queue = List(vec![f.clone()]);
        pipe.start(0, &f, 0.0).unwrap();
        pipe.step(&mut queue);
        let seq = pipe.seek(1.0).unwrap().unwrap();
        assert_eq!(seq, pipe.produced(), "{name}: unconsumed audio was discarded");
        let out = drain(&mut pipe, &mut queue);
        assert_eq!(out.len() / 2, 44_100, "{name}: wrong length after seek");
        let err = (0..44_100)
            .map(|i| (out[i * 2] - ideal(i + 44_100, 44_100)).abs())
            .fold(0.0, f32::max);
        assert!(err < 2e-4, "{name}: worst error {err} after seek");
    }
}

#[test]
fn lossy_seek_is_close() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args) in [
        ("s.mp3", &["-c:a", "libmp3lame", "-b:a", "192k"][..]),
        ("s.ogg", &["-c:a", "libvorbis", "-q:a", "6"][..]),
    ] {
        let f = encode(name, 44_100, 0, 88_200, args);
        let mut pipe = Pipeline::new(44_100);
        let mut queue = List(vec![f.clone()]);
        pipe.start(0, &f, 1.0).unwrap();
        let out = drain(&mut pipe, &mut queue);
        let frames = out.len() / 2;
        assert!(
            (44_100i64 - frames as i64).abs() < 2,
            "{name}: {frames} frames after seek"
        );
        let err = (2_000..42_000)
            .map(|i| (out[i * 2] - ideal(i + 44_100, 44_100)).abs())
            .fold(0.0, f32::max);
        assert!(err < 0.1, "{name}: worst error {err} after seek");
    }
}

#[test]
fn broken_track_is_skipped_and_reported() {
    if !have_ffmpeg() {
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let good = encode("g.wav", 44_100, 0, 1000, &["-c:a", "pcm_f32le"]);
    let bad = dir.join("bad.mp3");
    std::fs::write(&bad, b"this is not audio at all").unwrap();
    let (out, markers) = play_all(&[good.clone(), bad, good], 44_100);
    assert_eq!(out.len() / 2, 2000);
    assert_eq!(markers.iter().map(|m| m.track).collect::<Vec<_>>(), [0, 2]);
}

#[test]
fn tags_and_stream_facts_are_read() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args) in [
        ("t.mp3", &["-c:a", "libmp3lame", "-b:a", "128k"][..]),
        ("t.ogg", &["-c:a", "libvorbis"][..]),
        ("t.flac", &["-c:a", "flac"][..]),
        ("t.m4a", &["-c:a", "aac"][..]),
    ] {
        let mut full = args.to_vec();
        full.extend(["-metadata", "title=Żółw", "-metadata", "artist=Łukasz"]);
        let f = encode(name, 44_100, 0, 88_200, &full);
        let info = ferroamp::meta::read(&f).unwrap();
        assert_eq!(info.title.as_deref(), Some("Żółw"), "{name}");
        assert_eq!(info.artist.as_deref(), Some("Łukasz"), "{name}");
        assert_eq!(info.sample_rate, Some(44_100), "{name}");
        let d = info.duration.expect(name);
        assert!((d - 2.0).abs() < 0.1, "{name}: duration {d}");
    }
}

/// A queue of parts of files, like the tracks of a CUE sheet.
struct Clips {
    clips: Vec<Clip>,
    gapless: bool,
}

impl Queue for Clips {
    fn next_after(&mut self, track: u64) -> Option<(u64, Clip)> {
        let next = track as usize + 1;
        self.clips.get(next).map(|c| (next as u64, c.clone()))
    }
    fn clip_of(&mut self, track: u64) -> Option<Clip> {
        self.clips.get(track as usize).cloned()
    }
    fn keeps_gapless(&mut self, _: u64, _: u64) -> bool {
        self.gapless
    }
}

fn part(path: &Path, start: f64, end: Option<f64>) -> Clip {
    Clip {
        path: path.to_path_buf(),
        span: Some(Span { start, end }),
    }
}

fn play_clips(clips: Vec<Clip>, rate: u32, crossfade_ms: u32, gapless: bool) -> (Vec<f32>, Vec<Marker>) {
    let mut pipe = Pipeline::new(rate);
    pipe.mixing().set_crossfade_ms(crossfade_ms);
    let first = clips[0].clone();
    let mut queue = Clips { clips, gapless };
    pipe.start(0, first, 0.0).unwrap();
    let out = drain(&mut pipe, &mut queue);
    (out, pipe.take_markers())
}

#[test]
fn cue_tracks_of_one_file_join_sample_exactly() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args, limit) in [
        ("cue.wav", &["-c:a", "pcm_f32le"][..], 1e-6),
        ("cue.flac", &["-c:a", "flac", "-sample_fmt", "s16"][..], 1.5 / 32768.0),
    ] {
        let f = encode(name, 44_100, 0, 132_300, args);
        let clips = vec![part(&f, 0.0, Some(1.0)), part(&f, 1.0, Some(2.0)), part(&f, 2.0, None)];
        let (out, markers) = play_clips(clips, 44_100, 0, false);
        assert_eq!(out.len() / 2, 132_300, "{name}");
        assert!(worst_error(&out, 44_100, 0, 132_300) < limit, "{name}");
        assert_eq!(markers.iter().map(|m| m.seq).collect::<Vec<_>>(), [0, 88_200, 176_400], "{name}");
        assert!((markers[1].duration.unwrap() - 1.0).abs() < 1e-9, "{name}");
        assert!((markers[2].duration.unwrap() - 1.0).abs() < 0.01, "{name}: to the end of the file");
    }
}

#[test]
fn mp3_cue_tracks_stay_continuous_through_one_decoder() {
    if !have_ffmpeg() {
        return;
    }
    let f = encode("cue.mp3", 44_100, 0, 88_200, &["-c:a", "libmp3lame", "-b:a", "192k"]);
    let (out, markers) = play_clips(vec![part(&f, 0.0, Some(1.0)), part(&f, 1.0, None)], 44_100, 0, false);
    assert_eq!(out.len() / 2, 88_200);
    assert_eq!(markers[1].seq, 88_200);
    let err = worst_error(&out, 44_100, 2_000, 86_000);
    assert!(err < 0.1, "worst error {err}");
}

#[test]
fn a_cue_track_starts_plays_and_seeks_inside_its_part() {
    if !have_ffmpeg() {
        return;
    }
    let f = encode("cue_seek.flac", 44_100, 0, 132_300, &["-c:a", "flac", "-sample_fmt", "s32"]);
    let clip = part(&f, 1.0, Some(2.0));
    let mut pipe = Pipeline::new(44_100);
    let mut queue = Clips { clips: vec![clip.clone()], gapless: false };
    pipe.start(0, clip.clone(), 0.0).unwrap();
    pipe.step(&mut queue);
    pipe.seek(0.5).unwrap().unwrap();
    let out = drain(&mut pipe, &mut queue);
    assert_eq!(out.len() / 2, 22_050, "from halfway through the part to its end");
    let err = (0..22_050).map(|i| (out[i * 2] - ideal(i + 66_150, 44_100)).abs()).fold(0.0, f32::max);
    assert!(err < 2e-4, "worst error {err}");
}

#[test]
fn crossfade_overlaps_tracks_by_exactly_its_length() {
    if !have_ffmpeg() {
        return;
    }
    let args = ["-c:a", "pcm_f32le"];
    let a = encode("x_a.wav", 44_100, 0, 44_100, &args);
    let b = encode("x_b.wav", 44_100, 10_000, 44_100, &args);
    let (out, markers) = play_all_crossfading(&[a, b], 44_100, 200);
    let fade = 8_820;
    assert_eq!(out.len() / 2, 88_200 - fade);
    let start = 44_100 - fade;
    assert_eq!(markers[1].seq as usize, start * 2, "the next track shows when it is first heard");
    for i in [0, 1000, fade / 2, fade - 1] {
        let angle = (i * 2) as f32 / (fade * 2) as f32 * std::f32::consts::FRAC_PI_2;
        let want = ideal(start + i, 44_100) * angle.cos() + ideal(10_000 + i, 44_100) * angle.sin();
        assert!((out[(start + i) * 2] - want).abs() < 1e-4, "frame {i} of the fade");
    }
    let after = (start + fade..out.len() / 2)
        .map(|i| (out[i * 2] - ideal(10_000 + i - start, 44_100)).abs())
        .fold(0.0, f32::max);
    assert!(after < 1e-6, "the new track plays clean afterwards: {after}");
    assert!(worst_error(&out, 44_100, 0, start) < 1e-6, "and so did the old one before");
}

fn play_all_crossfading(files: &[PathBuf], rate: u32, ms: u32) -> (Vec<f32>, Vec<Marker>) {
    let clips = files.iter().map(Clip::from).collect();
    play_clips(clips, rate, ms, false)
}

#[test]
fn crossfade_keeps_the_end_of_the_last_track_and_albums_seamless() {
    if !have_ffmpeg() {
        return;
    }
    let args = ["-c:a", "pcm_f32le"];
    let a = encode("y_a.wav", 44_100, 0, 30_000, &args);
    let b = encode("y_b.wav", 44_100, 30_000, 30_000, &args);
    let clips: Vec<Clip> = [&a, &b].map(Clip::from).to_vec();
    let (out, markers) = play_clips(clips, 44_100, 300, true);
    assert_eq!(out.len() / 2, 60_000, "held-back audio is all let out");
    assert!(worst_error(&out, 44_100, 0, 60_000) < 1e-6);
    assert_eq!(markers[1].seq, 60_000);
}

#[test]
fn crossfade_never_swallows_a_short_track() {
    if !have_ffmpeg() {
        return;
    }
    let args = ["-c:a", "pcm_f32le"];
    let long = encode("z_long.wav", 44_100, 0, 88_200, &args);
    let short = encode("z_short.wav", 44_100, 0, 4_410, &args);
    let (out, _) = play_all_crossfading(&[long, short], 44_100, 2_000);
    assert_eq!(out.len() / 2, 88_200 + 4_410 - 2_205, "half of the short track at most");
}

#[test]
fn crossfade_across_sample_rates() {
    if !have_ffmpeg() {
        return;
    }
    let args = ["-c:a", "pcm_f32le"];
    let a = encode("r_x_a.wav", 44_100, 0, 44_100, &args);
    let b = encode("r_x_b.wav", 48_000, 0, 48_000, &args);
    let (out, markers) = play_all_crossfading(&[a, b], 48_000, 250);
    let frames = out.len() as i64 / 2;
    assert!((frames - (96_000 - 12_000)).abs() < 64, "{frames} frames");
    assert_eq!(markers[1].seq as i64 / 2, frames - 48_000);
}

#[test]
fn replay_gain_comes_from_the_tags_of_every_format() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args) in [
        ("g.flac", &["-c:a", "flac"][..]),
        ("g.mp3", &["-c:a", "libmp3lame", "-b:a", "192k"][..]),
        ("g.ogg", &["-c:a", "libvorbis", "-q:a", "6"][..]),
    ] {
        let mut full = args.to_vec();
        full.extend(["-metadata", "REPLAYGAIN_TRACK_GAIN=-6.02 dB", "-metadata", "REPLAYGAIN_ALBUM_GAIN=-12.04 dB"]);
        let f = encode(name, 44_100, 0, 44_100, &full);
        let peak_with = |mode| {
            let mut pipe = Pipeline::new(44_100);
            pipe.mixing().set_replay_gain(mode);
            let mut queue = List(vec![f.clone()]);
            pipe.start(0, &f, 0.0).unwrap();
            drain(&mut pipe, &mut queue).iter().skip(8_000).fold(0f32, |m, s| m.max(s.abs()))
        };
        let (off, track, album) = (peak_with(ReplayGain::Off), peak_with(ReplayGain::Track), peak_with(ReplayGain::Album));
        assert!((track / off - 0.5).abs() < 0.02, "{name}: track {track} against {off}");
        assert!((album / off - 0.25).abs() < 0.02, "{name}: album {album} against {off}");
        let report = ferroamp::meta::report(&f, None).unwrap();
        assert!(report.contains("ReplayGain track: -6.02 dB"), "{name}: {report}");
    }
}
