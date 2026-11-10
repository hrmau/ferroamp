//! The decode side of playback, independent of any audio device: turns a
//! sequence of files into one continuous stream of device-rate stereo samples.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use super::gain::ReplayGain;
use super::resample::Resampler;
pub use super::source::{Clip, Span};
use super::source::{Block, Source};
use super::stream::{InfoSink, StreamInfo};

/// Receives what a station says about itself, tagged with its track.
pub type LiveSink = Arc<dyn Fn(u64, &StreamInfo) + Send + Sync>;

/// A point in the output stream where something new starts being heard.
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    /// Output sample index (counting both channels) where this applies.
    pub seq: u64,
    pub track: u64,
    /// Position within the track at `seq`, in seconds.
    pub start_secs: f64,
    pub duration: Option<f64>,
    pub source_rate: u32,
}

/// Tells the decoder what follows a track, so it can start early for gapless.
pub trait Queue: Send {
    fn next_after(&mut self, track: u64) -> Option<(u64, Clip)>;
    /// Where a track lives, for reopening it after an output device change.
    fn clip_of(&mut self, track: u64) -> Option<Clip>;
    /// Called when a track could not be opened. It is skipped.
    fn mark_failed(&mut self, _track: u64, _error: &str) {}
    /// Whether two tracks belong together, so must never be crossfaded.
    fn keeps_gapless(&mut self, _from: u64, _to: u64) -> bool {
        false
    }
}

/// Settings the decode side reads as it goes, shared with the UI.
#[derive(Debug, Default)]
pub struct Mixing {
    crossfade_ms: AtomicU32,
    replay_gain: AtomicU8,
}

impl Mixing {
    pub fn set_crossfade_ms(&self, ms: u32) {
        self.crossfade_ms.store(ms, Ordering::Relaxed);
    }

    pub fn set_replay_gain(&self, mode: ReplayGain) {
        let value = match mode {
            ReplayGain::Off => 0,
            ReplayGain::Track => 1,
            ReplayGain::Album => 2,
        };
        self.replay_gain.store(value, Ordering::Relaxed);
    }

    fn replay_gain(&self) -> ReplayGain {
        match self.replay_gain.load(Ordering::Relaxed) {
            1 => ReplayGain::Track,
            2 => ReplayGain::Album,
            _ => ReplayGain::Off,
        }
    }

    fn crossfade_frames(&self, rate: u32) -> u64 {
        self.crossfade_ms.load(Ordering::Relaxed) as u64 * rate as u64 / 1000
    }
}

/// The end of the previous track, being mixed with the start of the new one.
struct Fade {
    tail: Vec<f32>,
    pos: usize,
}

pub struct Pipeline {
    device_rate: u32,
    source: Option<Source>,
    track: u64,
    resampler: Option<Resampler>,
    /// The source rate `resampler` was built for.
    resampler_from: u32,
    /// Decoded, resampled samples waiting to enter the ring buffer.
    pending: Vec<f32>,
    pending_pos: usize,
    raw: Vec<f32>,
    /// One step's output, on its way to `pending`.
    block: Vec<f32>,
    /// The newest audio, held back while crossfade is on. When a track ends
    /// this is exactly its last few seconds, whatever the file claimed its
    /// length to be, so only one track is ever being decoded at a time.
    held: VecDeque<f32>,
    fade: Option<Fade>,
    /// Frames the current source has produced since it opened or sought.
    track_frames: u64,
    mixing: Arc<Mixing>,
    /// Every sample ever handed to the ring plus those still pending.
    produced: u64,
    markers: Vec<Marker>,
    live_sink: Option<LiveSink>,
    /// Problems to report, such as a station that went off the air.
    errors: Vec<String>,
}

impl Pipeline {
    pub fn new(device_rate: u32) -> Self {
        Self {
            device_rate,
            source: None,
            track: 0,
            resampler: None,
            resampler_from: 0,
            pending: Vec::new(),
            pending_pos: 0,
            raw: Vec::new(),
            block: Vec::new(),
            held: VecDeque::new(),
            fade: None,
            track_frames: 0,
            mixing: Arc::default(),
            produced: 0,
            markers: Vec::new(),
            live_sink: None,
            errors: Vec::new(),
        }
    }

    pub fn with_live_sink(mut self, sink: LiveSink) -> Self {
        self.live_sink = Some(sink);
        self
    }

    pub fn with_mixing(mut self, mixing: Arc<Mixing>) -> Self {
        self.mixing = mixing;
        self
    }

    pub fn mixing(&self) -> &Mixing {
        &self.mixing
    }

    fn open(&self, track: u64, clip: &Clip) -> Result<Source, String> {
        let sink = self.live_sink.clone().map(|s| {
            Arc::new(move |info: &StreamInfo| s(track, info)) as InfoSink
        });
        Source::open(clip, sink)
    }

    /// Whether the current source can be stepped without waiting on the
    /// network.
    pub fn ready(&self) -> bool {
        self.source.as_ref().is_none_or(Source::ready)
    }

    pub fn is_live(&self) -> bool {
        self.source.as_ref().is_some_and(Source::is_live)
    }

    pub fn take_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.errors)
    }

    pub fn produced(&self) -> u64 {
        self.produced
    }
    pub fn current_track(&self) -> Option<u64> {
        self.source.as_ref().map(|_| self.track)
    }
    pub fn has_source(&self) -> bool {
        self.source.is_some()
    }
    pub fn pending(&self) -> &[f32] {
        &self.pending[self.pending_pos..]
    }
    pub fn consume(&mut self, n: usize) {
        self.pending_pos += n;
        if self.pending_pos >= self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        }
    }
    pub fn take_markers(&mut self) -> Vec<Marker> {
        std::mem::take(&mut self.markers)
    }

    /// Drops all audio not yet in the ring. Returns the sequence number at
    /// which fresh audio will begin, so the callback can skip what is queued.
    fn discard(&mut self) -> u64 {
        self.produced -= self.pending().len() as u64;
        self.pending.clear();
        self.pending_pos = 0;
        self.held.clear();
        self.fade = None;
        self.track_frames = 0;
        if let Some(r) = &mut self.resampler {
            r.reset();
        }
        self.produced
    }

    /// Hard start of `track` at `start_secs`, cutting off whatever was queued.
    pub fn start(
        &mut self,
        track: u64,
        clip: impl Into<Clip>,
        start_secs: f64,
    ) -> Result<u64, String> {
        let mut source = self.open(track, &clip.into())?;
        let start_secs = if start_secs > 0.0 && !source.is_live() {
            source.seek(start_secs)?
        } else {
            0.0
        };
        let boundary = self.discard();
        self.push_marker(track, start_secs, &source, boundary);
        self.source = Some(source);
        self.track = track;
        Ok(boundary)
    }

    /// Seeks within the current source. `None` if there is no source to seek.
    pub fn seek(&mut self, secs: f64) -> Option<Result<u64, String>> {
        let source = self.source.as_mut()?;
        let landed = match source.seek(secs) {
            Ok(landed) => landed,
            Err(e) => return Some(Err(e)),
        };
        let boundary = self.discard();
        let source = self.source.take()?;
        self.push_marker(self.track, landed, &source, boundary);
        self.source = Some(source);
        Some(Ok(boundary))
    }

    pub fn stop(&mut self) {
        self.discard();
        self.source = None;
    }

    fn push_marker(&mut self, track: u64, start_secs: f64, source: &Source, seq: u64) {
        self.markers.push(Marker {
            seq,
            track,
            start_secs,
            duration: source.duration,
            source_rate: source.rate,
        });
    }

    /// Decodes one packet into `pending`. On end of track moves straight on to
    /// the next without flushing, which is what makes the transition gapless.
    pub fn step(&mut self, queue: &mut dyn Queue) {
        let Some(source) = self.source.as_mut() else {
            return;
        };
        self.raw.clear();
        match source.next_block(&mut self.raw) {
            Block::Audio => {
                let rate = source.rate;
                let gain = source.gain(self.mixing.replay_gain());
                if gain != 1.0 {
                    self.raw.iter_mut().for_each(|s| *s *= gain);
                }
                self.ensure_resampler(rate);
                self.emit();
            }
            Block::End => self.advance(queue),
            Block::Error(e) => {
                queue.mark_failed(self.track, &e);
                self.errors.push(e);
                self.advance(queue);
            }
        }
    }

    fn advance(&mut self, queue: &mut dyn Queue) {
        let mut after = self.track;
        // Bounded so a playlist of only broken files on repeat cannot spin.
        for _ in 0..256 {
            let Some((next, clip)) = queue.next_after(after) else {
                break;
            };
            if let Some(source) = self.source.as_mut().filter(|s| s.continues_into(&clip)) {
                source.continue_into(&clip);
                let (rate, duration) = (source.rate, source.duration);
                if self.fade.is_some() {
                    self.flush_resampler();
                    self.finish_fade();
                }
                let seq = self.next_seq();
                self.markers.push(Marker {
                    seq,
                    track: next,
                    start_secs: 0.0,
                    duration,
                    source_rate: rate,
                });
                self.track = next;
                self.track_frames = 0;
                return;
            }
            match self.open(next, &clip) {
                Ok(source) => {
                    let gapless = queue.keeps_gapless(self.track, next);
                    self.join(next, source, gapless);
                    return;
                }
                Err(e) => {
                    queue.mark_failed(next, &e);
                    after = next;
                }
            }
        }
        self.flush_resampler();
        self.finish_fade();
        self.release(self.held.len());
        self.source = None;
    }

    /// Where the next sample to be decoded will land in the output.
    fn next_seq(&self) -> u64 {
        let latency = self.resampler.as_ref().map_or(0, Resampler::latency_frames);
        self.produced + self.held.len() as u64 + latency as u64 * 2
    }

    /// Moves on to the next track, crossfading if that is wanted and possible.
    fn join(&mut self, next: u64, source: Source, gapless: bool) {
        let overlap = if gapless { 0 } else { self.overlap_with(&source) };
        if overlap > 0 || self.fade.is_some() {
            self.flush_resampler();
            self.finish_fade();
        }
        let seq = if overlap > 0 && !self.held.is_empty() {
            self.release(self.held.len().saturating_sub(overlap * 2));
            self.fade = Some(Fade {
                tail: self.held.drain(..).collect(),
                pos: 0,
            });
            self.produced
        } else {
            if self.resampler_from != source.rate {
                self.flush_resampler();
            }
            self.next_seq()
        };
        self.push_marker(next, 0.0, &source, seq);
        self.track = next;
        self.source = Some(source);
        self.track_frames = 0;
    }

    /// Frames to overlap. Never more than half of either track, so a short
    /// one is not faded from end to end. Stations have no end to fade.
    fn overlap_with(&self, next: &Source) -> usize {
        let want = self.mixing.crossfade_frames(self.device_rate);
        if want == 0 || next.is_live() || self.source.as_ref().is_some_and(Source::is_live) {
            return 0;
        }
        let of_next = next
            .duration
            .map_or(want, |d| (d * self.device_rate as f64 / 2.0) as u64);
        want.min(self.track_frames / 2).min(of_next) as usize
    }

    /// Plays out what is left of a fade when a track ended in the middle of it.
    fn finish_fade(&mut self) {
        if let Some(fade) = &self.fade {
            self.block.clear();
            self.block.resize(fade.tail.len() - fade.pos, 0.0);
            self.deliver();
        }
    }

    /// Lets `samples` of the oldest held-back audio through.
    fn release(&mut self, samples: usize) {
        let samples = samples.min(self.held.len()) & !1;
        self.pending.extend(self.held.drain(..samples));
        self.produced += samples as u64;
    }

    fn flush_resampler(&mut self) {
        if let Some(mut r) = self.resampler.take() {
            self.block.clear();
            r.flush(&mut self.block);
            self.deliver();
        }
        self.resampler_from = 0;
    }

    fn ensure_resampler(&mut self, rate: u32) {
        if rate == self.resampler_from {
            return;
        }
        self.flush_resampler();
        if rate != self.device_rate {
            match Resampler::new(rate, self.device_rate) {
                Ok(r) => self.resampler = Some(r),
                // Audible pitch error beats silence for a very odd rate.
                Err(e) => eprintln!("ferroamp: {e}"),
            }
        }
        self.resampler_from = rate;
    }

    fn emit(&mut self) {
        self.block.clear();
        match &mut self.resampler {
            Some(r) => r.push(&self.raw, &mut self.block),
            None => self.block.extend_from_slice(&self.raw),
        }
        self.deliver();
    }

    /// Sends `block` onward: through any fade in progress, then to be held
    /// back if crossfade is on, and otherwise straight out.
    fn deliver(&mut self) {
        let block = std::mem::take(&mut self.block);
        self.track_frames += (block.len() / 2) as u64;
        let mut rest = &block[..];
        if let Some(fade) = &mut self.fade {
            let n = rest.len().min(fade.tail.len() - fade.pos);
            let len = fade.tail.len() as f32;
            for (i, &incoming) in rest[..n].iter().enumerate() {
                let at = fade.pos + i;
                // Equal power, so the level doesn't dip in the middle.
                let angle = (at & !1) as f32 / len * std::f32::consts::FRAC_PI_2;
                self.pending.push(fade.tail[at] * angle.cos() + incoming * angle.sin());
            }
            self.produced += n as u64;
            fade.pos += n;
            if fade.pos >= fade.tail.len() {
                self.fade = None;
            }
            rest = &rest[n..];
        }
        let live = self.source.as_ref().is_some_and(Source::is_live);
        let keep = if live {
            0
        } else {
            self.mixing.crossfade_frames(self.device_rate) as usize * 2
        };
        if keep == 0 && self.held.is_empty() {
            self.pending.extend_from_slice(rest);
            self.produced += rest.len() as u64;
        } else {
            self.held.extend(rest.iter().copied());
            self.release(self.held.len().saturating_sub(keep));
        }
        self.block = block;
    }
}
