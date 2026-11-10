//! ReplayGain: evens out loudness using values already in a file's tags. The
//! audio is never analysed, so a file without the tags plays as it is.

use symphonia::core::formats::FormatReader;
use symphonia::core::meta::{MetadataContainer, RawValue, StandardTag, Tag};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplayGain {
    Off,
    /// Every track at the same loudness.
    #[default]
    Track,
    /// Keeps the differences between the tracks of an album.
    Album,
}

/// What a file's tags say about its loudness.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Loudness {
    pub track_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain: Option<f32>,
    pub album_peak: Option<f32>,
}

impl Loudness {
    pub fn read(format: &mut dyn FormatReader) -> Self {
        let mut out = Self::default();
        if let Some(revision) = format.metadata().skip_to_latest() {
            out.apply(&revision.media);
            for track in &revision.per_track {
                out.apply(&track.metadata);
            }
        }
        out
    }

    fn apply(&mut self, container: &MetadataContainer) {
        for tag in &container.tags {
            self.apply_tag(tag);
        }
    }

    fn apply_tag(&mut self, tag: &Tag) {
        let (slot, value) = match &tag.std {
            Some(StandardTag::ReplayGainTrackGain(v)) => (&mut self.track_gain, v.to_string()),
            Some(StandardTag::ReplayGainTrackPeak(v)) => (&mut self.track_peak, v.to_string()),
            Some(StandardTag::ReplayGainAlbumGain(v)) => (&mut self.album_gain, v.to_string()),
            Some(StandardTag::ReplayGainAlbumPeak(v)) => (&mut self.album_peak, v.to_string()),
            // Not every tag format has these mapped to the standard names.
            _ => {
                let key = tag.raw.key.to_ascii_lowercase();
                let slot = if key.ends_with("replaygain_track_gain") {
                    &mut self.track_gain
                } else if key.ends_with("replaygain_track_peak") {
                    &mut self.track_peak
                } else if key.ends_with("replaygain_album_gain") {
                    &mut self.album_gain
                } else if key.ends_with("replaygain_album_peak") {
                    &mut self.album_peak
                } else {
                    return;
                };
                let RawValue::String(value) = &tag.raw.value else {
                    return;
                };
                (slot, value.to_string())
            }
        };
        if slot.is_none() {
            *slot = parse_number(&value);
        }
    }

    /// The factor to multiply samples by. A mode falls back to the other kind
    /// of value when its own is missing, and never pushes a peak past full scale.
    pub fn factor(&self, mode: ReplayGain) -> f32 {
        let track = (self.track_gain, self.track_peak);
        let album = (self.album_gain, self.album_peak);
        let (first, second) = match mode {
            ReplayGain::Off => return 1.0,
            ReplayGain::Track => (track, album),
            ReplayGain::Album => (album, track),
        };
        let (Some(db), peak) = (if first.0.is_some() { first } else { second }) else {
            return 1.0;
        };
        let factor = 10f32.powf(db / 20.0);
        match peak {
            Some(peak) if peak > 0.0 && factor * peak > 1.0 => 1.0 / peak,
            _ => factor,
        }
    }
}

/// `-6.54 dB`, `+1.2dB` or `0.98`.
fn parse_number(text: &str) -> Option<f32> {
    let text = text.trim();
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '+' | '-' | '.')))
        .unwrap_or(text.len());
    text[..end].parse().ok().filter(|v: &f32| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-3, "{a} is not {b}");
    }

    #[test]
    fn numbers_come_in_several_spellings() {
        assert_eq!(parse_number("-6.54 dB"), Some(-6.54));
        assert_eq!(parse_number("+1.5dB"), Some(1.5));
        assert_eq!(parse_number(" 0.988 "), Some(0.988));
        assert_eq!(parse_number("loud"), None);
    }

    #[test]
    fn modes_pick_their_own_value_and_fall_back() {
        let both = Loudness {
            track_gain: Some(-6.0),
            album_gain: Some(-12.0),
            ..Loudness::default()
        };
        close(both.factor(ReplayGain::Off), 1.0);
        close(both.factor(ReplayGain::Track), 0.5012);
        close(both.factor(ReplayGain::Album), 0.2512);
        let only_track = Loudness {
            track_gain: Some(-6.0),
            ..Loudness::default()
        };
        close(only_track.factor(ReplayGain::Album), 0.5012);
        close(Loudness::default().factor(ReplayGain::Track), 1.0);
    }

    #[test]
    fn a_boost_never_clips_the_peak() {
        let quiet = Loudness {
            track_gain: Some(6.0),
            track_peak: Some(0.8),
            ..Loudness::default()
        };
        close(quiet.factor(ReplayGain::Track), 1.25);
    }
}
