//! Internet radio through the real decode pipeline, against a fake station on
//! localhost. Audio is generated with ffmpeg; skipped when it is missing.
//! `real_station` (ignored) plays a few seconds of an actual station.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferroamp::engine::pipeline::{Clip, Pipeline, Queue};
use ferroamp::engine::stream::StreamInfo;

const FREQ: f64 = 440.0;
const RATE: u32 = 44_100;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn encode(name: &str, seconds: f64, args: &[&str]) -> Option<Vec<u8>> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("radio");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let tone = format!("sine=frequency={FREQ}:sample_rate={RATE}:duration={seconds}");
    let ok = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "lavfi", "-i", &tone, "-ac", "2"])
        .args(args)
        .arg(&path)
        .status()
        .ok()?
        .success();
    ok.then(|| std::fs::read(&path).unwrap())
}

/// Splices ICY metadata into `audio` every `metaint` bytes, switching title
/// halfway through.
fn with_icy(audio: &[u8], metaint: usize, titles: [&str; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    let chunks: Vec<&[u8]> = audio.chunks(metaint).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        out.extend_from_slice(chunk);
        if chunk.len() < metaint {
            break;
        }
        let title = titles[usize::from(i >= chunks.len() / 2)];
        let mut block = format!("StreamTitle='{title}';StreamUrl='';").into_bytes();
        block.resize(block.len().div_ceil(16) * 16, 0);
        out.push((block.len() / 16) as u8);
        out.extend(block);
    }
    out
}

struct Station {
    port: u16,
}

struct Fixtures {
    mp3: Vec<u8>,
    ogg: Option<Vec<u8>>,
}

impl Station {
    fn start(fixtures: Fixtures) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fixtures = Arc::new(fixtures);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let fixtures = fixtures.clone();
                std::thread::spawn(move || serve(conn, port, &fixtures));
            }
        });
        Self { port }
    }

    fn url(&self, path: &str) -> PathBuf {
        PathBuf::from(format!("http://127.0.0.1:{}{path}", self.port))
    }
}

fn serve(mut conn: TcpStream, port: u16, f: &Fixtures) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    let mut icy = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        icy |= line.to_ascii_lowercase().starts_with("icy-metadata: 1");
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    let _ = match path.as_str() {
        "/live" => {
            assert!(icy, "the player should ask for song titles");
            let head = "ICY 200 OK\r\nicy-name:Test FM\r\nicy-br:128\r\nicy-metaint:4096\r\n\
                        content-type:audio/mpeg\r\n\r\n";
            conn.write_all(head.as_bytes()).and_then(|()| {
                conn.write_all(&with_icy(&f.mp3, 4096, ["Artist - First", "Artist - Second"]))
            })
        }
        "/redirect" => conn.write_all(b"HTTP/1.1 302 Found\r\nLocation: /tune.pls\r\nContent-Length: 0\r\n\r\n"),
        "/tune.pls" => {
            let body = format!("[playlist]\nNumberOfEntries=1\nFile1=http://127.0.0.1:{port}/chunked\n");
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/x-scpls\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            conn.write_all(head.as_bytes()).and_then(|()| conn.write_all(body.as_bytes()))
        }
        "/chunked" => {
            let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            for chunk in f.mp3.chunks(3001) {
                out.extend(format!("{:x}\r\n", chunk.len()).as_bytes());
                out.extend(chunk);
                out.extend(b"\r\n");
            }
            out.extend(b"0\r\n\r\n");
            conn.write_all(&out)
        }
        "/chained" => {
            let ogg = f.ogg.as_ref().unwrap();
            let head = "HTTP/1.0 200 OK\r\nContent-Type: application/ogg\r\n\r\n";
            conn.write_all(head.as_bytes())
                .and_then(|()| conn.write_all(ogg))
                .and_then(|()| conn.write_all(ogg))
        }
        "/hls" => {
            let body = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6,\nseg1.aac\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            conn.write_all(head.as_bytes()).and_then(|()| conn.write_all(body.as_bytes()))
        }
        _ => conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
    };
}

struct One(PathBuf);

impl Queue for One {
    fn next_after(&mut self, _: u64) -> Option<(u64, Clip)> {
        None
    }
    fn clip_of(&mut self, _: u64) -> Option<Clip> {
        Some(Clip::from(&self.0))
    }
}

struct Played {
    samples: Vec<f32>,
    infos: Vec<StreamInfo>,
    errors: Vec<String>,
    ended: bool,
}

/// Plays `url` like the engine does, stepping only when the stream is ready,
/// until `seconds` of audio arrive, the stream ends, or time runs out.
fn play(url: &Path, seconds: f64, limit: Duration) -> Played {
    let infos = Arc::new(Mutex::new(Vec::new()));
    let sink_infos = infos.clone();
    let mut pipe = Pipeline::new(RATE).with_live_sink(Arc::new(move |track, info| {
        assert_eq!(track, 7);
        sink_infos.lock().unwrap().push(info.clone());
    }));
    let mut queue = One(url.to_path_buf());
    pipe.start(7, url, 0.0).unwrap();
    let mut samples = Vec::new();
    let mut errors = Vec::new();
    let deadline = Instant::now() + limit;
    let want = (seconds * RATE as f64) as usize * 2;
    while pipe.has_source() && samples.len() < want && Instant::now() < deadline {
        if pipe.ready() {
            pipe.step(&mut queue);
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
        samples.extend_from_slice(pipe.pending());
        let n = pipe.pending().len();
        pipe.consume(n);
        errors.extend(pipe.take_errors());
    }
    let ended = !pipe.has_source();
    drop(pipe);
    let infos = infos.lock().unwrap().clone();
    Played {
        samples,
        infos,
        errors,
        ended,
    }
}

/// The tone's frequency from zero crossings of the left channel.
fn pitch(samples: &[f32]) -> f64 {
    let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
    let skip = left.len() / 10;
    let part = &left[skip..];
    let crossings = part.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    crossings as f64 / (part.len() as f64 / RATE as f64)
}

fn fixtures() -> Option<Fixtures> {
    if !have_ffmpeg() {
        return None;
    }
    let mp3 = encode("tone.mp3", 6.0, &["-c:a", "libmp3lame", "-b:a", "128k"])?;
    let ogg = encode("tone.ogg", 3.0, &["-c:a", "libvorbis", "-q:a", "3"]);
    Some(Fixtures { mp3, ogg })
}

#[test]
fn old_shoutcast_station_plays_with_song_titles() {
    let Some(f) = fixtures() else { return };
    let station = Station::start(f);
    let played = play(&station.url("/live"), 5.0, Duration::from_secs(20));
    assert!(played.errors.is_empty(), "{:?}", played.errors);
    assert!(played.samples.len() >= 5 * RATE as usize * 2, "got {} samples", played.samples.len());
    let hz = pitch(&played.samples);
    assert!((hz - FREQ).abs() < 3.0, "heard {hz} Hz");
    let last = played.infos.last().expect("station info");
    assert_eq!(last.station.as_deref(), Some("Test FM"));
    assert_eq!(last.kbps, Some(128));
    assert_eq!(last.rate, Some(RATE));
    assert_eq!(last.channels, Some(2));
    let titles: Vec<_> = played.infos.iter().filter_map(|i| i.title.clone()).collect();
    assert!(titles.contains(&"Artist - First".to_string()), "{titles:?}");
    assert!(titles.contains(&"Artist - Second".to_string()), "{titles:?}");
}

#[test]
fn redirect_to_a_pls_to_a_chunked_stream() {
    let Some(f) = fixtures() else { return };
    let station = Station::start(f);
    let played = play(&station.url("/redirect"), 4.0, Duration::from_secs(20));
    assert!(played.errors.is_empty(), "{:?}", played.errors);
    assert!(played.samples.len() >= 4 * RATE as usize * 2);
    let hz = pitch(&played.samples);
    assert!((hz - FREQ).abs() < 3.0, "heard {hz} Hz");
}

#[test]
fn ogg_station_keeps_playing_across_its_restarts() {
    let Some(f) = fixtures() else { return };
    if f.ogg.is_none() {
        return;
    }
    let station = Station::start(f);
    // Two 3 s Ogg streams back to back, as a station sends at a song change.
    let played = play(&station.url("/chained"), 5.5, Duration::from_secs(20));
    assert!(played.errors.is_empty(), "{:?}", played.errors);
    assert!(
        played.samples.len() >= (5.5 * RATE as f64) as usize * 2,
        "stopped after {:.2} s",
        played.samples.len() as f64 / 2.0 / RATE as f64
    );
}

#[test]
fn dead_and_unsupported_stations_fail_quickly_with_a_reason() {
    let Some(f) = fixtures() else { return };
    let station = Station::start(f);
    let started = Instant::now();
    let played = play(&station.url("/gone"), 1.0, Duration::from_secs(20));
    assert!(played.ended && played.samples.is_empty());
    assert!(played.errors[0].contains("404"), "{:?}", played.errors);

    let played = play(&station.url("/hls"), 1.0, Duration::from_secs(20));
    assert!(played.ended);
    assert!(played.errors[0].contains("HLS"), "{:?}", played.errors);

    // Nothing listening on this port at all.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let played = play(
        Path::new(&format!("http://127.0.0.1:{port}/x")),
        1.0,
        Duration::from_secs(20),
    );
    assert!(played.ended);
    assert!(played.errors[0].contains("cannot connect"), "{:?}", played.errors);
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
}

/// Plays a few seconds of a real station. Needs the internet:
/// `cargo test --release --test radio -- --ignored --nocapture`
#[test]
#[ignore]
fn real_station() {
    for url in [
        "https://somafm.com/groovesalad.pls",
        "http://stream.radioparadise.com/aac-128",
        "http://icecast.radiofrance.fr/fip-midfi.mp3",
        // HE-AAC ("AAC+"): plays, without its top octave.
        "https://ice2.somafm.com/groovesalad-64-aac",
    ] {
        let started = Instant::now();
        let played = play(Path::new(url), 4.0, Duration::from_secs(30));
        let secs = played.samples.len() as f64 / 2.0 / RATE as f64;
        let peak = played.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!(
            "{url}: {secs:.1} s in {:.1} s, peak {peak:.2}, errors {:?}\n  {:?}",
            started.elapsed().as_secs_f64(),
            played.errors,
            played.infos.last()
        );
        assert!(played.errors.is_empty());
        assert!(secs >= 4.0 && peak > 0.01);
    }
}
