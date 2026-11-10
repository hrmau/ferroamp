//! One open audio file or radio stream: demux, decode, and convert to
//! interleaved stereo f32.

use std::fs::File;
use std::path::{Path, PathBuf};

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase, Timestamp};

use super::gain::{Loudness, ReplayGain};
use super::stream::{self, Connection, InfoSink};

/// A stretch of a file in seconds. No end means to the end of the file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub start: f64,
    pub end: Option<f64>,
}

impl Span {
    /// The length, given the length of the whole file if that is known.
    pub fn length(&self, file: Option<f64>) -> Option<f64> {
        Some((self.end.or(file)? - self.start).max(0.0))
    }
}

/// What to play: a whole file or station, or part of a file, such as one
/// track of a CUE sheet.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub path: PathBuf,
    pub span: Option<Span>,
}

impl From<PathBuf> for Clip {
    fn from(path: PathBuf) -> Self {
        Self { path, span: None }
    }
}

impl From<&PathBuf> for Clip {
    fn from(path: &PathBuf) -> Self {
        path.clone().into()
    }
}

impl From<&Path> for Clip {
    fn from(path: &Path) -> Self {
        path.to_path_buf().into()
    }
}

impl From<&Clip> for Clip {
    fn from(clip: &Clip) -> Self {
        clip.clone()
    }
}

pub struct Source {
    /// `None` for a stream that has not buffered enough to be probed yet.
    dec: Option<Decoding>,
    live: Option<Connection>,
    pub rate: u32,
    /// Of the clip being played, which may be less than the whole file.
    pub duration: Option<f64>,
    file_duration: Option<f64>,
    path: PathBuf,
    span: Option<Span>,
    /// Frames until the clip ends.
    left: Option<u64>,
    /// Audio decoded past the end of a clip, which the next clip begins with
    /// when it carries straight on in the same file.
    overhang: Vec<f32>,
    /// After a seek, audio before this timestamp is decoded but thrown away.
    discard_before: Option<Timestamp>,
    scratch: Vec<f32>,
    loudness: Loudness,
}

struct Decoding {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
}

pub enum Block {
    /// Samples were appended (possibly none, if a packet was undecodable).
    Audio,
    End,
    /// The item cannot be played, such as a station that is off the air.
    Error(String),
}

pub(crate) fn open_format(path: &Path) -> Result<Box<dyn FormatReader>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported file: {e}"))
}

impl Source {
    /// Opens a file, or starts connecting to a station. Never waits on the
    /// network: a station is probed later, once `ready` says so.
    pub fn open(clip: &Clip, sink: Option<InfoSink>) -> Result<Self, String> {
        let Clip { path, span } = clip;
        if stream::is_stream(path) {
            return Ok(Self {
                dec: None,
                live: Some(Connection::open(&path.to_string_lossy(), sink)),
                rate: 44_100,
                duration: None,
                file_duration: None,
                path: path.clone(),
                span: None,
                left: None,
                overhang: Vec::new(),
                discard_before: None,
                scratch: Vec::new(),
                loudness: Loudness::default(),
            });
        }
        let mut format = open_format(path)?;
        let loudness = Loudness::read(format.as_mut());
        let (dec, rate, file_duration) = Decoding::new(format)?;
        let mut source = Self {
            dec: Some(dec),
            live: None,
            rate,
            duration: span.map_or(file_duration, |s| s.length(file_duration)),
            file_duration,
            path: path.clone(),
            span: *span,
            left: None,
            overhang: Vec::new(),
            discard_before: None,
            scratch: Vec::new(),
            loudness,
        };
        if span.is_some_and(|s| s.start > 0.0) {
            source.seek(0.0)?;
        } else {
            source.left = source.frames_left_from(0.0);
        }
        Ok(source)
    }

    fn frames_left_from(&self, file_secs: f64) -> Option<u64> {
        let frame = |secs: f64| (secs * self.rate as f64).round() as u64;
        let end = self.span?.end?;
        Some(frame(end).saturating_sub(frame(file_secs)))
    }

    /// Whether `clip` starts exactly where this one is finishing, in the same
    /// file. Then the decoder just keeps going, which is gapless even for
    /// lossy formats and needs no second seek.
    pub fn continues_into(&self, clip: &Clip) -> bool {
        self.live.is_none()
            && self.left == Some(0)
            && self.path == clip.path
            && match (self.span, clip.span) {
                (Some(a), Some(b)) => a.end == Some(b.start),
                _ => false,
            }
    }

    pub fn continue_into(&mut self, clip: &Clip) {
        self.span = clip.span;
        self.duration = clip.span.and_then(|s| s.length(self.file_duration));
        self.left = clip.span.and_then(|s| self.frames_left_from(s.start));
    }

    /// How much to scale this file's samples by.
    pub fn gain(&self, mode: ReplayGain) -> f32 {
        self.loudness.factor(mode)
    }

    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// Whether `next_block` can run without waiting on the network.
    pub fn ready(&self) -> bool {
        match &self.live {
            None => true,
            Some(c) if self.dec.is_none() => c.ready(stream::PREFILL),
            Some(c) => c.ready(stream::LOW_WATER),
        }
    }

    /// Seeks to `secs` into the clip and returns the position landed on.
    pub fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let secs = secs.max(0.0);
        let file_secs = secs + self.span.map_or(0.0, |s| s.start);
        let time = Time::try_from_secs_f64(file_secs).ok_or("bad seek time")?;
        let dec = match &mut self.dec {
            Some(dec) if self.live.is_none() => dec,
            _ => return Err("a live stream cannot seek".into()),
        };
        let to = SeekTo::Time {
            time,
            track_id: Some(dec.track_id),
        };
        let seeked = dec
            .format
            .seek(SeekMode::Accurate, to)
            .map_err(|e| format!("seek failed: {e}"))?;
        dec.decoder.reset();
        self.discard_before = Some(seeked.required_ts);
        self.overhang.clear();
        self.left = self.frames_left_from(file_secs);
        Ok(secs)
    }

    /// Decodes one packet, appending interleaved stereo to `out`.
    pub fn next_block(&mut self, out: &mut Vec<f32>) -> Block {
        if self.left == Some(0) {
            return Block::End;
        }
        if !self.overhang.is_empty() {
            let before = out.len();
            out.append(&mut self.overhang);
            self.keep_within_clip(out, before);
            return Block::Audio;
        }
        if self.dec.is_none() {
            let Some(conn) = &self.live else {
                return Block::End;
            };
            if let Some(e) = conn.failure() {
                return Block::Error(e);
            }
            match Decoding::open_stream(conn) {
                Ok((dec, rate, channels)) => {
                    conn.set_format(rate, channels);
                    self.rate = rate;
                    self.dec = Some(dec);
                }
                Err(e) => return Block::Error(conn.failure().unwrap_or(e)),
            }
        }
        let Some(dec) = self.dec.as_mut() else {
            return Block::End;
        };
        let packet = match dec.format.next_packet() {
            Ok(Some(p)) => p,
            // A station's Ogg stream restarts at every song, which looks like
            // the end. Open the next part, unless the station itself is gone.
            Ok(None) | Err(_) if self.live.as_ref().is_some_and(|c| !c.finished()) => {
                self.dec = None;
                return Block::Audio;
            }
            Ok(None) | Err(_) => {
                return match self.live.as_ref().and_then(Connection::failure) {
                    Some(e) => Block::Error(e),
                    // Anything else fatal also ends the track rather than
                    // stalling playback.
                    None => Block::End,
                };
            }
        };
        if packet.track_id != dec.track_id {
            return Block::Audio;
        }
        let mut skip_frames = 0usize;
        if let Some(target) = self.discard_before {
            let end = packet.pts.saturating_add(packet.dur);
            if end <= target {
                // Still decoded: decoders with overlap state need the history.
                let _ = dec.decoder.decode(&packet);
                return Block::Audio;
            }
            if packet.pts < target {
                skip_frames = ticks_to_frames(dec.time_base, self.rate, packet.pts, target);
            }
            self.discard_before = None;
        }
        let buf = match dec.decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(Error::DecodeError(_)) | Err(Error::IoError(_)) => return Block::Audio,
            Err(_) => return Block::End,
        };
        let channels = buf.spec().channels().count().max(1);
        self.rate = buf.spec().rate();
        self.scratch.resize(buf.samples_interleaved(), 0.0);
        buf.copy_to_slice_interleaved(&mut self.scratch);
        let frames = self.scratch.chunks_exact(channels).skip(skip_frames);
        out.reserve(frames.len() * 2);
        let before = out.len();
        for frame in frames {
            let (l, r) = to_stereo(frame);
            out.push(l);
            out.push(r);
        }
        self.keep_within_clip(out, before);
        Block::Audio
    }

    /// Stops at the clip's last frame. Anything after it is kept for `overhang`.
    fn keep_within_clip(&mut self, out: &mut Vec<f32>, before: usize) {
        let Some(left) = &mut self.left else {
            return;
        };
        let frames = ((out.len() - before) / 2) as u64;
        if frames > *left {
            let keep = before + *left as usize * 2;
            self.overhang.extend_from_slice(&out[keep..]);
            out.truncate(keep);
            *left = 0;
        } else {
            *left -= frames;
        }
    }
}

fn ticks_to_frames(tb: Option<TimeBase>, rate: u32, from: Timestamp, to: Timestamp) -> usize {
    let Some(tb) = tb else {
        return 0;
    };
    let (a, b) = (tb.calc_time_saturating(from), tb.calc_time_saturating(to));
    ((b.as_secs_f64() - a.as_secs_f64()) * rate as f64).round() as usize
}

impl Decoding {
    /// Returns the decoder, its sample rate and the duration if known.
    fn new(format: Box<dyn FormatReader>) -> Result<(Self, u32, Option<f64>), String> {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or("no audio track")?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("no audio codec parameters")?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| format!("unsupported codec: {e}"))?;
        let rate = params.sample_rate.unwrap_or(44_100);
        let duration = track_seconds(track, rate);
        let dec = Self {
            track_id: track.id,
            time_base: track.time_base,
            format,
            decoder,
        };
        Ok((dec, rate, duration))
    }

    fn open_stream(conn: &Connection) -> Result<(Self, u32, u8), String> {
        let source = ReadOnlySource::new(conn.reader());
        let mss = MediaSourceStream::new(Box::new(source), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = conn.hint() {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| format!("the station sends audio this player can't read: {e}"))?;
        let channels = format
            .default_track(TrackType::Audio)
            .and_then(|t| t.codec_params.as_ref()?.audio()?.channels.as_ref())
            .map_or(2, |c| c.count() as u8);
        let (dec, rate, _) = Self::new(format)?;
        Ok((dec, rate, channels))
    }
}

pub(crate) fn track_seconds(
    track: &symphonia::core::formats::Track,
    rate: u32,
) -> Option<f64> {
    if let (Some(dur), Some(tb)) = (track.duration, track.time_base) {
        return Some(tb.calc_duration_saturating(dur).as_secs_f64());
    }
    track.num_frames.map(|n| n as f64 / rate.max(1) as f64)
}

/// Stereo passes through; mono is duplicated; surround layouts use the
/// conventional FL FR FC LFE BL BR order with centre and surrounds at -3 dB.
#[inline]
fn to_stereo(f: &[f32]) -> (f32, f32) {
    match f.len() {
        1 => (f[0], f[0]),
        2 => (f[0], f[1]),
        n => {
            const M: f32 = std::f32::consts::FRAC_1_SQRT_2;
            let c = if n > 2 { f[2] * M } else { 0.0 };
            let (sl, sr) = if n >= 6 { (f[4] * M, f[5] * M) } else { (0.0, 0.0) };
            let norm = if n >= 6 { 0.414 } else { 0.586 };
            ((f[0] + c + sl) * norm, (f[1] + c + sr) * norm)
        }
    }
}
