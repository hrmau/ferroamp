//! Playback engine.
//!
//! Three parties cooperate without locks on the real-time path:
//!  * the engine thread decodes ahead into a lock-free ring buffer,
//!  * the CoreAudio callback drains it, applying EQ and volume,
//!  * the UI reads atomics and sends commands.
//!
//! All audio positions derive from one monotonic sample counter. Markers say
//! what starts at which count, so a gapless boundary and a seek are the same
//! mechanism, and the UI always shows what is audible rather than what has been
//! decoded (up to `RING_SECONDS` earlier).

pub mod eq;
pub mod gain;
pub mod http;
pub mod pipeline;
pub mod resample;
pub mod source;
pub mod spectrum;
pub mod stream;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eq::{EqParams, EqProcessor};
use gain::ReplayGain;
use pipeline::{Clip, LiveSink, Marker, Mixing, Pipeline, Queue};
use spectrum::Tap;
use stream::StreamInfo;

const RING_SECONDS: f32 = 2.0;
/// How long to sleep once the ring is full. Short enough never to underrun,
/// long enough that the decoder works in rare bursts and the CPU can idle.
const REFILL_WAIT: Duration = Duration::from_millis(400);
const DRAIN_POLL: Duration = Duration::from_millis(40);
/// How often to look again while a station's buffer refills.
const NET_POLL: Duration = Duration::from_millis(40);
/// Fade in after a discontinuity so seeks and track jumps do not click.
const DECLICK_FRAMES: u32 = 256;
/// A large device buffer means far fewer wake-ups than the default.
const PREFERRED_BUFFER_FRAMES: u32 = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    pub state: PlayState,
    pub track: Option<u64>,
    pub position: f64,
    pub duration: Option<f64>,
    pub source_rate: u32,
}

enum Cmd {
    Play { track: u64, clip: Clip, at: f64 },
    Seek { track: u64, clip: Clip, at: f64 },
    Pause,
    Resume,
    Stop,
    OutputLost,
    Quit,
}

struct Shared {
    /// Samples the callback has taken from the ring, including skipped ones.
    read_total: AtomicU64,
    /// Samples before this sequence number are stale and must not be played.
    skip_until: AtomicU64,
    volume: AtomicU32,
    balance: AtomicU32,
    device_rate: AtomicU32,
    state: AtomicU8,
    markers: Mutex<VecDeque<Marker>>,
    tap: Tap,
    tap_enabled: AtomicBool,
    eq: EqParams,
    mixing: Arc<Mixing>,
    last_error: Mutex<Option<String>>,
    /// The latest a station has said about itself, for the track it belongs to.
    live: Mutex<Option<(u64, StreamInfo)>>,
    live_generation: AtomicU64,
}

pub struct Engine {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
}

impl Engine {
    /// `notify` is called from the engine thread when the UI should re-check
    /// status, such as when playback ends. It must not block.
    pub fn new(queue: Box<dyn Queue>, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        let shared = Arc::new(Shared {
            read_total: AtomicU64::new(0),
            skip_until: AtomicU64::new(0),
            volume: AtomicU32::new(0.8f32.to_bits()),
            balance: AtomicU32::new(0f32.to_bits()),
            device_rate: AtomicU32::new(44_100),
            state: AtomicU8::new(encode(PlayState::Stopped)),
            markers: Mutex::new(VecDeque::new()),
            tap: Tap::default(),
            tap_enabled: AtomicBool::new(true),
            eq: EqParams::default(),
            mixing: Arc::default(),
            last_error: Mutex::new(None),
            live: Mutex::new(None),
            live_generation: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let thread = Worker {
            rx,
            tx: tx.clone(),
            shared: shared.clone(),
            queue,
            notify,
            output: None,
            pipeline: Pipeline::new(44_100),
        };
        let thread = Worker {
            pipeline: thread.new_pipeline(44_100),
            ..thread
        };
        std::thread::Builder::new()
            .name("ferroamp-engine".into())
            .spawn(move || thread.run())
            .expect("spawn engine thread");
        Self { tx, shared }
    }

    pub fn play(&self, track: u64, clip: impl Into<Clip>) {
        self.play_at(track, clip, 0.0);
    }
    pub fn play_at(&self, track: u64, clip: impl Into<Clip>, at: f64) {
        let clip = clip.into();
        let _ = self.tx.send(Cmd::Play { track, clip, at });
    }
    /// `track` and `clip` identify what is audible. With read-ahead the decoder
    /// may already have moved on, in which case that track is reopened.
    pub fn seek(&self, track: u64, clip: impl Into<Clip>, at: f64) {
        let clip = clip.into();
        let _ = self.tx.send(Cmd::Seek { track, clip, at });
    }
    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }
    pub fn resume(&self) {
        let _ = self.tx.send(Cmd::Resume);
    }
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared
            .volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    pub fn set_balance(&self, balance: f32) {
        self.shared
            .balance
            .store(balance.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    /// Overlap between tracks, or 0 for none. Takes effect from the next join.
    pub fn set_crossfade(&self, secs: u32) {
        self.shared.mixing.set_crossfade_ms(secs * 1000);
    }
    pub fn set_replay_gain(&self, mode: ReplayGain) {
        self.shared.mixing.set_replay_gain(mode);
    }
    pub fn eq(&self) -> &EqParams {
        &self.shared.eq
    }
    pub fn tap(&self) -> &Tap {
        &self.shared.tap
    }
    pub fn set_tap_enabled(&self, on: bool) {
        self.shared.tap_enabled.store(on, Ordering::Relaxed);
    }
    pub fn device_rate(&self) -> u32 {
        self.shared.device_rate.load(Ordering::Relaxed)
    }
    pub fn take_error(&self) -> Option<String> {
        self.shared.last_error.lock().ok()?.take()
    }

    /// What a station last said about itself, if that changed since `seen`.
    pub fn take_live(&self, seen: &mut u64) -> Option<(u64, StreamInfo)> {
        let generation = self.shared.live_generation.load(Ordering::Acquire);
        if generation == *seen {
            return None;
        }
        *seen = generation;
        self.shared.live.lock().ok()?.clone()
    }

    /// What is audible right now.
    pub fn status(&self) -> Status {
        let state = decode(self.shared.state.load(Ordering::Acquire));
        let read = self.shared.read_total.load(Ordering::Acquire);
        let rate = self.shared.device_rate.load(Ordering::Relaxed).max(1);
        let markers = self.shared.markers.lock().expect("markers lock");
        match current_marker(&markers, read) {
            Some(m) if state != PlayState::Stopped => Status {
                state,
                track: Some(m.track),
                position: m.start_secs + (read.saturating_sub(m.seq) / 2) as f64 / rate as f64,
                duration: m.duration,
                source_rate: m.source_rate,
            },
            _ => Status {
                state,
                track: None,
                position: 0.0,
                duration: None,
                source_rate: 0,
            },
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
    }
}

fn current_marker(markers: &VecDeque<Marker>, read: u64) -> Option<&Marker> {
    markers.iter().rev().find(|m| m.seq <= read)
}

fn encode(s: PlayState) -> u8 {
    match s {
        PlayState::Stopped => 0,
        PlayState::Playing => 1,
        PlayState::Paused => 2,
    }
}

fn decode(v: u8) -> PlayState {
    match v {
        1 => PlayState::Playing,
        2 => PlayState::Paused,
        _ => PlayState::Stopped,
    }
}

struct Output {
    stream: cpal::Stream,
    producer: rtrb::Producer<f32>,
    playing: bool,
}

struct Worker {
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    queue: Box<dyn Queue>,
    notify: Arc<dyn Fn() + Send + Sync>,
    output: Option<Output>,
    pipeline: Pipeline,
}

enum Wait {
    None,
    For(Duration),
    Forever,
}

impl Worker {
    fn run(mut self) {
        let mut wait = Wait::Forever;
        loop {
            let cmd = match wait {
                Wait::None => match self.rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                },
                Wait::For(d) => match self.rx.recv_timeout(d) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                Wait::Forever => match self.rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
            };
            if let Some(cmd) = cmd {
                if matches!(cmd, Cmd::Quit) {
                    return;
                }
                self.handle(cmd);
            }
            wait = self.work();
        }
    }

    fn state(&self) -> PlayState {
        decode(self.shared.state.load(Ordering::Acquire))
    }

    fn new_pipeline(&self, rate: u32) -> Pipeline {
        let shared = self.shared.clone();
        let notify = self.notify.clone();
        let sink: LiveSink = Arc::new(move |track, info| {
            if let Ok(mut live) = shared.live.lock() {
                *live = Some((track, info.clone()));
            }
            shared.live_generation.fetch_add(1, Ordering::AcqRel);
            notify();
        });
        Pipeline::new(rate)
            .with_live_sink(sink)
            .with_mixing(self.shared.mixing.clone())
    }

    fn set_state(&self, state: PlayState) {
        self.shared.state.store(encode(state), Ordering::Release);
        (self.notify)();
    }

    fn report(&self, msg: String) {
        if let Ok(mut slot) = self.shared.last_error.lock() {
            *slot = Some(msg);
        }
        (self.notify)();
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play { track, clip, at } => self.play(track, &clip, at),
            Cmd::Seek { track, clip, at } => {
                let live = self.pipeline.current_track() == Some(track) && self.pipeline.is_live();
                if self.state() == PlayState::Stopped || live {
                    return;
                }
                let boundary = if self.pipeline.current_track() == Some(track) {
                    self.pipeline.seek(at)
                } else {
                    None
                };
                match boundary.unwrap_or_else(|| self.pipeline.start(track, &clip, at)) {
                    Ok(seq) => self.shared.skip_until.store(seq, Ordering::Release),
                    Err(e) => self.report(e),
                }
                self.publish_markers();
                (self.notify)();
            }
            Cmd::Pause => {
                if self.state() == PlayState::Playing {
                    if let Some(o) = &mut self.output {
                        let _ = o.stream.pause();
                        o.playing = false;
                    }
                    self.set_state(PlayState::Paused);
                }
            }
            Cmd::Resume => {
                if self.state() == PlayState::Paused {
                    self.start_stream();
                    self.set_state(PlayState::Playing);
                }
            }
            Cmd::Stop => self.stop(),
            Cmd::OutputLost => self.recover_output(),
            Cmd::Quit => {}
        }
    }

    fn play(&mut self, track: u64, clip: &Clip, at: f64) {
        if self.output.is_none()
            && let Err(e) = self.open_output()
        {
            self.report(format!("no audio output: {e}"));
            return;
        }
        match self.pipeline.start(track, clip, at) {
            Ok(seq) => {
                self.shared.skip_until.store(seq, Ordering::Release);
                self.publish_markers();
                self.start_stream();
                self.set_state(PlayState::Playing);
            }
            Err(e) => {
                self.queue.mark_failed(track, &e);
                self.report(e);
            }
        }
    }

    fn stop(&mut self) {
        self.output = None;
        self.pipeline.stop();
        self.reset_counters();
        self.set_state(PlayState::Stopped);
    }

    fn reset_counters(&mut self) {
        let rate = self.shared.device_rate.load(Ordering::Relaxed);
        self.pipeline = self.new_pipeline(rate);
        self.shared.read_total.store(0, Ordering::Release);
        self.shared.skip_until.store(0, Ordering::Release);
        self.shared.markers.lock().expect("markers lock").clear();
    }

    fn start_stream(&mut self) {
        if let Some(o) = &mut self.output
            && !o.playing
        {
            match o.stream.play() {
                Ok(()) => o.playing = true,
                Err(e) => {
                    self.report(format!("audio output failed: {e}"));
                }
            }
        }
    }

    /// The device disappeared. Rebuild on the new default and carry on from
    /// the audible position.
    fn recover_output(&mut self) {
        let status = self.position_snapshot();
        let was_playing = self.state() == PlayState::Playing;
        self.output = None;
        self.pipeline.stop();
        self.reset_counters();
        let Some((track, position)) = status else {
            self.set_state(PlayState::Stopped);
            return;
        };
        let Some(clip) = self.queue.clip_of(track) else {
            self.set_state(PlayState::Stopped);
            return;
        };
        if let Err(e) = self.open_output() {
            self.report(format!("no audio output: {e}"));
            self.set_state(PlayState::Stopped);
            return;
        }
        match self.pipeline.start(track, clip, position) {
            Ok(_) => {
                self.publish_markers();
                if was_playing {
                    self.start_stream();
                }
                (self.notify)();
            }
            Err(e) => {
                self.report(e);
                self.set_state(PlayState::Stopped);
            }
        }
    }

    fn position_snapshot(&self) -> Option<(u64, f64)> {
        let read = self.shared.read_total.load(Ordering::Acquire);
        let rate = self.shared.device_rate.load(Ordering::Relaxed).max(1);
        let markers = self.shared.markers.lock().ok()?;
        let m = current_marker(&markers, read)?;
        Some((
            m.track,
            m.start_secs + (read.saturating_sub(m.seq) / 2) as f64 / rate as f64,
        ))
    }

    fn publish_markers(&mut self) {
        let fresh = self.pipeline.take_markers();
        if fresh.is_empty() {
            return;
        }
        let read = self.shared.read_total.load(Ordering::Acquire);
        let mut markers = self.shared.markers.lock().expect("markers lock");
        // A marker at a boundary replaces anything queued beyond it: that audio
        // was skipped and will never be heard.
        for m in fresh {
            while markers.back().is_some_and(|last| last.seq >= m.seq) {
                markers.pop_back();
            }
            markers.push_back(m);
        }
        // Keep the marker in effect now, drop the ones before it.
        while markers.len() > 1 && markers[1].seq <= read {
            markers.pop_front();
        }
    }

    /// One unit of background work. Returns how long the thread may then sleep.
    fn work(&mut self) -> Wait {
        let paused = self.state() == PlayState::Paused;
        if self.output.is_none() {
            return Wait::Forever;
        }
        if !self.pipeline.pending().is_empty() {
            self.push_to_ring();
            if !self.pipeline.pending().is_empty() {
                // Ring full. While paused nothing will drain it, so truly sleep.
                return if paused {
                    Wait::Forever
                } else {
                    Wait::For(REFILL_WAIT)
                };
            }
        }
        if self.pipeline.has_source() {
            // A station still buffering: look again shortly rather than block
            // this thread, which also has to answer the transport buttons.
            if !self.pipeline.ready() {
                return if paused {
                    Wait::Forever
                } else {
                    Wait::For(NET_POLL)
                };
            }
            self.pipeline.step(self.queue.as_mut());
            self.publish_markers();
            for e in self.pipeline.take_errors() {
                self.report(e);
            }
            return Wait::None;
        }
        if paused {
            return Wait::Forever;
        }
        // All decoded. Finish once the callback has played everything.
        let read = self.shared.read_total.load(Ordering::Acquire);
        if read >= self.pipeline.produced() {
            self.stop();
            return Wait::Forever;
        }
        Wait::For(DRAIN_POLL)
    }

    fn push_to_ring(&mut self) {
        let Some(output) = &mut self.output else {
            return;
        };
        let pending = self.pipeline.pending();
        // Keep frames whole so left and right can never swap.
        let n = pending.len().min(output.producer.slots()) & !1;
        if n == 0 {
            return;
        }
        if let Ok(chunk) = output.producer.write_chunk_uninit(n) {
            chunk.fill_from_iter(pending[..n].iter().copied());
        }
        self.pipeline.consume(n);
    }

    fn open_output(&mut self) -> Result<(), String> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("no default output device")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        if supported.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!(
                "unsupported device format {}",
                supported.sample_format()
            ));
        }
        let rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let buffer_size = match supported.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                cpal::BufferSize::Fixed(PREFERRED_BUFFER_FRAMES.clamp(*min, *max))
            }
            _ => cpal::BufferSize::Default,
        };
        let config = cpal::StreamConfig {
            channels: channels as u16,
            sample_rate: rate,
            buffer_size,
        };

        self.shared.device_rate.store(rate, Ordering::Relaxed);
        self.reset_counters();

        let capacity = (rate as f32 * RING_SECONDS) as usize * 2;
        let (producer, consumer) = rtrb::RingBuffer::<f32>::new(capacity);
        let mut callback = Callback {
            consumer,
            shared: self.shared.clone(),
            eq: EqProcessor::new(rate),
            read_total: 0,
            declick: 0,
            gain_l: 0.0,
            gain_r: 0.0,
            channels,
        };
        let tx = self.tx.clone();
        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| callback.fill(data),
                move |err: cpal::Error| match err.kind() {
                    // Rerouted automatically by cpal; nothing to do.
                    cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::Xrun => {}
                    cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated => {
                        let _ = tx.send(Cmd::OutputLost);
                    }
                    _ => eprintln!("ferroamp: audio stream: {err}"),
                },
                None,
            )
            .map_err(|e| e.to_string())?;
        self.output = Some(Output {
            stream,
            producer,
            playing: false,
        });
        Ok(())
    }
}

/// Real-time thread state. No locks, no allocation, no I/O in `fill`.
struct Callback {
    consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    eq: EqProcessor,
    read_total: u64,
    declick: u32,
    gain_l: f32,
    gain_r: f32,
    channels: usize,
}

impl Callback {
    fn fill(&mut self, data: &mut [f32]) {
        data.fill(0.0);
        let shared = &*self.shared;

        // Throw away anything queued before the latest seek or track jump. If
        // the ring is holding less than that, the rest is still on its way.
        let skip_until = shared.skip_until.load(Ordering::Acquire);
        if self.read_total < skip_until {
            let n = (skip_until - self.read_total).min(self.consumer.slots() as u64) as usize;
            if let Ok(chunk) = self.consumer.read_chunk(n) {
                chunk.commit_all();
            }
            self.read_total += n as u64;
            self.declick = DECLICK_FRAMES;
        }
        if self.read_total < skip_until {
            shared.read_total.store(self.read_total, Ordering::Release);
            return;
        }

        self.eq.sync(&shared.eq);
        let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
        let balance = f32::from_bits(shared.balance.load(Ordering::Relaxed));
        // Perceptual taper: the slider's midpoint sounds like about half volume.
        let level = volume * volume;
        let target_l = level * (1.0 - balance).min(1.0);
        let target_r = level * (1.0 + balance).min(1.0);
        let tap_on = shared.tap_enabled.load(Ordering::Relaxed);

        let frames = data.len() / self.channels.max(1);
        let take = (frames * 2).min(self.consumer.slots()) & !1;
        let Ok(chunk) = self.consumer.read_chunk(take) else {
            return;
        };
        let (a, b) = chunk.as_slices();
        let mut samples = a.iter().chain(b.iter()).copied();
        for frame in data.chunks_exact_mut(self.channels.max(1)).take(take / 2) {
            let (Some(l), Some(r)) = (samples.next(), samples.next()) else {
                break;
            };
            let (mut l, mut r) = self.eq.process(l, r);
            if tap_on {
                shared.tap.push(l, r);
            }
            if self.declick > 0 {
                let ramp = 1.0 - self.declick as f32 / DECLICK_FRAMES as f32;
                l *= ramp;
                r *= ramp;
                self.declick -= 1;
            }
            // One-pole smoothing so slider moves do not zipper.
            self.gain_l += (target_l - self.gain_l) * 0.002;
            self.gain_r += (target_r - self.gain_r) * 0.002;
            let (out_l, out_r) = (
                (l * self.gain_l).clamp(-1.0, 1.0),
                (r * self.gain_r).clamp(-1.0, 1.0),
            );
            if let [only] = frame {
                *only = (out_l + out_r) * 0.5;
            } else {
                frame[0] = out_l;
                frame[1] = out_r;
            }
        }
        drop(samples);
        chunk.commit_all();
        self.read_total += take as u64;
        shared.read_total.store(self.read_total, Ordering::Release);
    }
}
