//! Reads tags and stream facts without decoding any audio.

use std::path::Path;

use symphonia::core::formats::TrackType;
use symphonia::core::meta::{MetadataContainer, RawValue, StandardTag};

use crate::engine::gain::Loudness;
use crate::engine::pipeline::Span;
use crate::engine::source::{open_format, track_seconds};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub kbps: Option<u32>,
    /// For internet radio: the station's name. `title` is then the song.
    pub station: Option<String>,
}

pub fn read(path: &Path) -> Result<TrackInfo, String> {
    let mut format = open_format(path)?;
    let mut info = TrackInfo::default();

    if let Some(track) = format.default_track(TrackType::Audio) {
        let params = track.codec_params.as_ref().and_then(|p| p.audio());
        info.sample_rate = params.and_then(|p| p.sample_rate);
        info.channels = params
            .and_then(|p| p.channels.as_ref())
            .map(|c| c.count() as u8);
        info.duration = track_seconds(track, info.sample_rate.unwrap_or(44_100));
    }
    if let (Some(duration), Ok(file)) = (info.duration, std::fs::metadata(path))
        && duration > 0.0
    {
        info.kbps = Some((file.len() as f64 * 8.0 / duration / 1000.0).round() as u32);
    }

    if let Some(revision) = format.metadata().skip_to_latest() {
        apply_tags(&mut info, &revision.media);
        for track in &revision.per_track {
            apply_tags(&mut info, &track.metadata);
        }
    }
    Ok(info)
}

fn apply_tags(info: &mut TrackInfo, container: &MetadataContainer) {
    for tag in &container.tags {
        let clean = |s: &str| {
            let s = s.trim().trim_matches('\0').trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        match &tag.std {
            Some(StandardTag::TrackTitle(v)) if info.title.is_none() => info.title = clean(v),
            Some(StandardTag::Artist(v)) if info.artist.is_none() => info.artist = clean(v),
            Some(StandardTag::Album(v)) if info.album.is_none() => info.album = clean(v),
            _ => {}
        }
    }
}

fn clock(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    if total >= 3600 {
        format!("{}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
    } else {
        format!("{}:{:02}", total / 60, total % 60)
    }
}

/// Everything known about a file, as text for the file info panel. Reads the
/// file, so call it away from the UI thread.
pub fn report(path: &Path, span: Option<Span>) -> Result<String, String> {
    const LONGEST_VALUE: usize = 400;
    let mut format = open_format(path)?;
    let mut out = format!("File: {}\n", path.display());
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut duration = None;
    let mut facts = vec![format.format_info().short_name.to_uppercase()];
    if let Some(track) = format.default_track(TrackType::Audio) {
        let params = track.codec_params.as_ref().and_then(|p| p.audio());
        if let Some(p) = params {
            let codec = symphonia::default::get_codecs()
                .get_audio_decoder(p.codec)
                .map(|d| d.codec.info.short_name);
            if let Some(name) = codec.filter(|n| !n.eq_ignore_ascii_case(&facts[0])) {
                facts.push(name.to_string());
            }
            if let Some(rate) = p.sample_rate {
                facts.push(format!("{rate} Hz"));
            }
            match p.channels.as_ref().map(|c| c.count()) {
                Some(1) => facts.push("mono".into()),
                Some(2) => facts.push("stereo".into()),
                Some(n) => facts.push(format!("{n} channels")),
                None => {}
            }
            if let Some(bits) = p.bits_per_sample {
                facts.push(format!("{bits}-bit"));
            }
        }
        duration = track_seconds(track, params.and_then(|p| p.sample_rate).unwrap_or(44_100));
    }
    out.push_str(&format!("Format: {}\n", facts.join(", ")));
    if let Some(d) = duration {
        out.push_str(&format!("Length of file: {}\n", clock(d)));
    }
    out.push_str(&format!("Size: {:.1} MB", size as f64 / 1_048_576.0));
    if let Some(d) = duration.filter(|d| *d > 0.0) {
        out.push_str(&format!(", {} kbps", (size as f64 * 8.0 / d / 1000.0).round()));
    }
    out.push('\n');
    if let Some(span) = span {
        let end = span.end.or(duration).map_or_else(|| "end".to_string(), clock);
        out.push_str(&format!("Part of the file: {} to {end}\n", clock(span.start)));
    }
    let loudness = Loudness::read(format.as_mut());
    for (name, gain, peak) in [
        ("track", loudness.track_gain, loudness.track_peak),
        ("album", loudness.album_gain, loudness.album_peak),
    ] {
        if let Some(gain) = gain {
            let peak = peak.map_or(String::new(), |p| format!(", peak {p:.3}"));
            out.push_str(&format!("ReplayGain {name}: {gain:+.2} dB{peak}\n"));
        }
    }

    // A fresh reader, because reading the loudness used up the metadata.
    let mut format = open_format(path)?;
    let mut tags = String::new();
    if let Some(revision) = format.metadata().skip_to_latest() {
        let containers = std::iter::once(&revision.media).chain(revision.per_track.iter().map(|t| &t.metadata));
        for tag in containers.flat_map(|c| &c.tags) {
            let value = match &tag.raw.value {
                RawValue::String(v) => v.to_string(),
                RawValue::StringList(list) => list.join("; "),
                RawValue::SignedInt(v) => v.to_string(),
                RawValue::UnsignedInt(v) => v.to_string(),
                RawValue::Float(v) => v.to_string(),
                RawValue::Boolean(v) => v.to_string(),
                _ => continue,
            };
            let value = value.trim().trim_matches('\0');
            let mut shown: String = value.chars().take(LONGEST_VALUE).collect();
            if value.chars().count() > LONGEST_VALUE {
                shown.push_str("...");
            }
            tags.push_str(&format!("{}: {shown}\n", tag.raw.key));
        }
    }
    if !tags.is_empty() {
        out.push_str("\nTags\n");
        out.push_str(&tags);
    }
    Ok(out)
}

/// UTF-8 when the bytes are valid UTF-8, otherwise Windows-1252. Playlists
/// written by older Windows players use the system code page, and 1252 is
/// what nearly all of them used in western Europe and the Americas.
pub fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    }
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{81}', '\u{201A}', '\u{192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
        '\u{2C6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8D}', '\u{17D}', '\u{8F}',
        '\u{90}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
        '\u{2DC}', '\u{2122}', '\u{161}', '\u{203A}', '\u{153}', '\u{9D}', '\u{17E}', '\u{178}',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}
