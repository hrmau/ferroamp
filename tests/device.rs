//! Drives the real engine through the real CoreAudio output at zero volume.
//! Ignored by default because it needs an output device and takes a few seconds:
//!   cargo test --test device -- --ignored --test-threads=1

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferroamp::engine::pipeline::{Clip, Queue};
use ferroamp::engine::{Engine, PlayState};

struct List(Vec<PathBuf>);
impl Queue for List {
    fn next_after(&mut self, t: u64) -> Option<(u64, Clip)> {
        self.0.get(t as usize + 1).map(|p| (t + 1, Clip::from(p)))
    }
    fn clip_of(&mut self, t: u64) -> Option<Clip> {
        self.0.get(t as usize).map(Clip::from)
    }
}

fn tone(name: &str, secs: u32) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("device");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let source = format!("sine=frequency=440:duration={secs}");
    assert!(
        Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", &source])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    path
}

fn wait_until(engine: &Engine, limit: Duration, done: impl Fn(&ferroamp::engine::Status) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done(&engine.status()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
#[ignore = "needs an audio device"]
fn full_session_on_the_real_device() {
    let a = tone("a.mp3", 2);
    let b = tone("b.flac", 2);
    let engine = Engine::new(Box::new(List(vec![a.clone(), b])), Arc::new(|| {}));
    engine.set_volume(0.0);
    println!("device rate {}", engine.device_rate());

    engine.play(0, a.clone());
    let started = wait_until(&engine, Duration::from_secs(5), |s| s.state == PlayState::Playing);
    assert!(started, "not playing: {:?}, error: {:?}", engine.status(), engine.take_error());
    println!("device rate after open {}", engine.device_rate());

    // Time advances at real speed.
    let t0 = Instant::now();
    let p0 = engine.status().position;
    std::thread::sleep(Duration::from_millis(600));
    let advanced = engine.status().position - p0;
    let elapsed = t0.elapsed().as_secs_f64();
    println!("advanced {advanced:.3}s in {elapsed:.3}s");
    assert!((advanced - elapsed).abs() < 0.12, "clock is off: {advanced} vs {elapsed}");

    // Pause freezes the position and resume continues.
    engine.pause();
    assert!(wait_until(&engine, Duration::from_secs(1), |s| s.state == PlayState::Paused));
    std::thread::sleep(Duration::from_millis(100));
    let held = engine.status().position;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(engine.status().position, held, "position moved while paused");
    engine.resume();

    // Seeking moves the audible position without changing track.
    engine.seek(0, a, 1.5);
    assert!(
        wait_until(&engine, Duration::from_secs(2), |s| s.position >= 1.5 && s.track == Some(0)),
        "seek never took effect: {:?}",
        engine.status()
    );

    // The second track starts by itself at the end of the first...
    assert!(
        wait_until(&engine, Duration::from_secs(3), |s| s.track == Some(1)),
        "no gapless move to track 2: {:?}",
        engine.status()
    );
    let status = engine.status();
    println!("after boundary {status:?}");
    assert!(status.position < 0.25, "boundary reported late: {}", status.position);

    // ...and everything stops by itself at the end of the list.
    assert!(
        wait_until(&engine, Duration::from_secs(4), |s| s.state == PlayState::Stopped),
        "never finished: {:?}",
        engine.status()
    );
    assert!(engine.take_error().is_none());
}
