//! CUE sheets: one audio file described as an album of separate tracks.

use std::path::{Path, PathBuf};

use crate::engine::pipeline::Span;
use crate::playlist::{AUDIO_EXTENSIONS, Item, resolve_entry};

struct Track {
    number: u32,
    path: PathBuf,
    title: Option<String>,
    artist: Option<String>,
    start: Option<f64>,
}

/// The tracks of a CUE sheet whose audio can be found. `base` is the folder
/// the sheet is in.
pub fn parse(text: &str, base: &Path) -> Vec<Item> {
    let mut album = None;
    let mut album_artist = None;
    let mut file: Option<PathBuf> = None;
    let mut tracks: Vec<Track> = Vec::new();
    // False after a data track, whose lines must not land on the track before.
    let mut in_audio_track = false;

    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        let (keyword, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match keyword.to_ascii_uppercase().as_str() {
            "FILE" => {
                file = locate(file_name(rest), base);
                in_audio_track = false;
            }
            "TRACK" => {
                let mut words = rest.split_whitespace();
                let number = words.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                in_audio_track = words.next().is_some_and(|t| t.eq_ignore_ascii_case("AUDIO"))
                    && file.is_some();
                if let (true, Some(path)) = (in_audio_track, &file) {
                    tracks.push(Track {
                        number,
                        path: path.clone(),
                        title: None,
                        artist: None,
                        start: None,
                    });
                }
            }
            "TITLE" => {
                if let (true, Some(track)) = (in_audio_track, tracks.last_mut()) {
                    track.title = text_value(rest);
                } else if tracks.is_empty() {
                    album = text_value(rest);
                }
            }
            "PERFORMER" => {
                if let (true, Some(track)) = (in_audio_track, tracks.last_mut()) {
                    track.artist = text_value(rest);
                } else if tracks.is_empty() {
                    album_artist = text_value(rest);
                }
            }
            // Index 00 is the pregap, which stays with the track before it.
            "INDEX" if in_audio_track => {
                let mut words = rest.split_whitespace();
                let number: Option<u32> = words.next().and_then(|n| n.parse().ok());
                let time = words.next().and_then(parse_time);
                if let (Some(track), Some(number), Some(time)) = (tracks.last_mut(), number, time)
                    && (number == 1 || (number == 0 && track.start.is_none()))
                {
                    track.start = Some(time);
                }
            }
            _ => {}
        }
    }

    tracks.retain(|t| t.start.is_some());
    let mut items = Vec::with_capacity(tracks.len());
    for (i, track) in tracks.iter().enumerate() {
        let end = tracks
            .get(i + 1)
            .filter(|next| next.path == track.path)
            .and_then(|next| next.start);
        items.push(Item {
            path: track.path.clone(),
            title: Some(
                track
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("Track {:02}", track.number)),
            ),
            artist: track.artist.clone().or_else(|| album_artist.clone()),
            album: album.clone(),
            span: Some(Span {
                start: track.start.unwrap_or(0.0),
                end,
            }),
        });
    }
    items
}

/// `mm:ss:ff`, with 75 frames to the second.
fn parse_time(text: &str) -> Option<f64> {
    let mut parts = text.split(':').map(|p| p.parse::<u32>().ok());
    let (minutes, seconds, frames) = (parts.next()??, parts.next()??, parts.next()??);
    Some(minutes as f64 * 60.0 + seconds as f64 + frames as f64 / 75.0)
}

/// `"Some Name.flac" WAVE`, or the same without the quotes.
fn file_name(rest: &str) -> &str {
    if let Some(quoted) = rest.strip_prefix('"') {
        return quoted.split('"').next().unwrap_or_default();
    }
    rest.rsplit_once(char::is_whitespace).map_or(rest, |(name, _)| name.trim())
}

fn text_value(rest: &str) -> Option<String> {
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.rsplit_once('"').map_or(quoted, |(v, _)| v),
        None => rest,
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Finds the audio a sheet refers to. Rips are often compressed after the
/// sheet was written, so `album.wav` may by now be `album.flac`.
fn locate(name: &str, base: &Path) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let as_written = resolve_entry(name, base).filter(|p| p.is_file());
    as_written.or_else(|| {
        let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let stem = Path::new(leaf).file_stem()?;
        let beside = base.join(leaf);
        std::iter::once(beside.clone())
            .chain(AUDIO_EXTENSIONS.iter().map(|ext| base.join(stem).with_extension(ext)))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHEET: &str = r#"
REM GENRE Rock
PERFORMER "The Band"
TITLE "The Album"
FILE "album.wav" WAVE
  TRACK 01 AUDIO
    TITLE "First"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Second"
    PERFORMER "Guest"
    INDEX 00 03:00:00
    INDEX 01 03:02:30
  TRACK 03 AUDIO
    INDEX 01 05:00:00
"#;

    fn dir_with(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferroamp-cue-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        dir
    }

    #[test]
    fn tracks_run_up_to_the_next_track() {
        let dir = dir_with("basic", &["album.wav"]);
        let items = parse(SHEET, &dir);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].title.as_deref(), Some("First"));
        assert_eq!(items[0].artist.as_deref(), Some("The Band"));
        assert_eq!(items[0].album.as_deref(), Some("The Album"));
        assert_eq!(items[0].span, Some(Span { start: 0.0, end: Some(182.4) }));
        assert_eq!(items[1].artist.as_deref(), Some("Guest"));
        assert_eq!(items[1].span, Some(Span { start: 182.4, end: Some(300.0) }));
        assert_eq!(items[2].title.as_deref(), Some("Track 03"));
        assert_eq!(items[2].span, Some(Span { start: 300.0, end: None }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rip_compressed_later_is_still_found() {
        let dir = dir_with("moved", &["album.flac"]);
        assert_eq!(parse(SHEET, &dir)[0].path, dir.join("album.flac"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_audio_gives_no_tracks() {
        let dir = dir_with("missing", &[]);
        assert!(parse(SHEET, &dir).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_file_ends_its_own_last_track() {
        let dir = dir_with("multi", &["a.flac", "b.flac"]);
        let text = "FILE a.flac WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 01:00:00\n\
                    FILE \"b.flac\" WAVE\nTRACK 03 AUDIO\nINDEX 01 00:00:00\nTRACK 04 MODE1/2352\nINDEX 01 00:30:00\n";
        let items = parse(text, &dir);
        assert_eq!(items.len(), 3, "the data track is left out");
        assert_eq!(items[1].span, Some(Span { start: 60.0, end: None }));
        assert_eq!(items[2].path, dir.join("b.flac"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
