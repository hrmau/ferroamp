//! The playlist is just a list of files. There is no library or database, and
//! nothing is imported or moved: files stay where they are.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::engine::pipeline::{Clip, Queue, Span};
pub use crate::engine::stream::is_stream;
use crate::meta::{TrackInfo, decode_text};
use crate::ui::font::fold;

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "mp2", "mp1", "ogg", "oga", "flac", "wav", "wave", "aif", "aiff", "aifc", "m4a", "m4b",
    "mp4", "aac", "caf", "mka",
];
/// Playlists that can be opened. Saving always writes `.m3u8`.
pub const PLAYLIST_EXTENSIONS: &[&str] = &["m3u", "m3u8", "pls", "cue"];

/// What happens when a track ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    fn wraps(self) -> bool {
        self != Repeat::Off
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: u64,
    pub path: PathBuf,
    /// The part of the file this entry plays, for a track of a CUE sheet.
    pub span: Option<Span>,
    pub info: TrackInfo,
    pub scanned: bool,
    pub failed: bool,
}

impl Entry {
    pub fn is_stream(&self) -> bool {
        is_stream(&self.path)
    }

    pub fn clip(&self) -> Clip {
        Clip {
            path: self.path.clone(),
            span: self.span,
        }
    }

    /// Takes in what a scan of the file found. A track of a CUE sheet keeps
    /// the sheet's names, since the file's own tags describe the whole album.
    fn apply_scan(&mut self, info: TrackInfo) {
        let Some(span) = self.span else {
            self.info = info;
            return;
        };
        let title = self.info.title.take();
        let artist = self.info.artist.take().or(info.artist.clone());
        let album = self.info.album.take().or(info.album.clone());
        let duration = span.length(info.duration);
        self.info = TrackInfo {
            title,
            artist,
            album,
            duration,
            ..info
        };
    }

    pub fn display_title(&self) -> String {
        if self.is_stream() {
            let song = self.info.title.as_deref().filter(|t| !t.is_empty());
            return match (song, &self.info.station) {
                (Some(song), Some(station)) => format!("{song} ({station})"),
                (Some(song), None) => song.to_string(),
                (None, Some(station)) => station.clone(),
                (None, None) => short_url(&self.path.to_string_lossy()),
            };
        }
        match (&self.info.artist, &self.info.title) {
            (Some(a), Some(t)) => format!("{a} - {t}"),
            (None, Some(t)) => t.clone(),
            _ => self
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.display().to_string()),
        }
    }

    /// The title written to a saved playlist. For a station that is its name,
    /// not whatever song happened to be on.
    fn saved_title(&self) -> Option<String> {
        if self.is_stream() {
            self.info.station.clone()
        } else {
            (self.scanned || self.span.is_some()).then(|| self.display_title())
        }
    }
}

/// `https://ice2.example.com/groove-128` → `ice2.example.com/groove-128`.
fn short_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.trim_end_matches('/').to_string()
}

#[derive(Default)]
pub struct Playlist {
    pub entries: Vec<Entry>,
    pub current: Option<u64>,
    pub selected: Vec<u64>,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Tracks asked to play next, before the normal order resumes. An entry
    /// stays until it is heard: the decoder asks for what follows seconds
    /// early, and pressing Next in that moment should still find it here.
    queue: Vec<u64>,
    /// Play order while shuffling. Rebuilt whenever shuffle is switched on.
    order: Vec<u64>,
    next_id: u64,
    rng: u64,
    /// Where the last removed entry was, so playback that has moved past a
    /// deleted track still knows what came after it.
    tombstone: Option<(u64, usize)>,
    /// Bumped on every change, so the UI knows when to redraw.
    pub revision: u64,
}

impl Playlist {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: seed | 1,
            next_id: 1,
            ..Self::default()
        }
    }

    fn random(&mut self) -> u64 {
        // xorshift64: plenty for shuffling and needs no dependency.
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Adds files, returning the new entries for metadata scanning.
    pub fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) -> Vec<(u64, PathBuf)> {
        self.add_items(paths.into_iter().map(Item::from))
    }

    /// Adds files and stations, returning the new entries for metadata scanning.
    pub fn add_items(&mut self, items: impl IntoIterator<Item = Item>) -> Vec<(u64, PathBuf)> {
        let mut added = Vec::new();
        for Item { path, title, artist, album, span } in items {
            let id = self.next_id;
            self.next_id += 1;
            let mut info = TrackInfo::default();
            // A file's own tags beat a playlist's idea of its title, but a
            // station has no tags until it is played, and the tags of a whole
            // album say nothing about one of its tracks.
            if is_stream(&path) {
                info.station = title;
            } else if let Some(span) = span {
                info.title = title;
                info.artist = artist;
                info.album = album;
                info.duration = span.length(None);
            }
            self.entries.push(Entry {
                id,
                path: path.clone(),
                span,
                info,
                scanned: false,
                failed: false,
            });
            if self.shuffle {
                let from = self
                    .current
                    .and_then(|c| self.order.iter().position(|&o| o == c))
                    .map_or(0, |p| p + 1);
                let span = (self.order.len() - from + 1) as u64;
                let at = from + (self.random() % span) as usize;
                self.order.insert(at, id);
            } else {
                self.order.push(id);
            }
            added.push((id, path));
        }
        self.revision += 1;
        added
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }
    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }
    pub fn get_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.revision += 1;
        self.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn set_current(&mut self, id: Option<u64>) {
        if self.current != id {
            self.current = id;
            self.revision += 1;
        }
    }

    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        self.revision += 1;
        self.order = self.entries.iter().map(|e| e.id).collect();
        if !on {
            return;
        }
        for i in (1..self.order.len()).rev() {
            let j = (self.random() % (i as u64 + 1)) as usize;
            self.order.swap(i, j);
        }
        // What is playing goes first so a whole pass still lies ahead of it.
        if let Some(c) = self.current
            && let Some(pos) = self.order.iter().position(|&o| o == c)
        {
            self.order.swap(0, pos);
        }
    }

    fn sequence(&self) -> Vec<u64> {
        if self.shuffle {
            self.order.clone()
        } else {
            self.entries.iter().map(|e| e.id).collect()
        }
    }

    fn step(&self, id: u64, forward: bool) -> Option<u64> {
        let seq = self.sequence();
        if seq.is_empty() {
            return None;
        }
        let Some(pos) = seq.iter().position(|&o| o == id) else {
            // The track was deleted. Whatever slid into its slot is next.
            let (gone, at) = self.tombstone?;
            if gone != id || !forward {
                return None;
            }
            return seq
                .get(at)
                .copied()
                .or_else(|| self.repeat.wraps().then(|| seq[0]));
        };
        if forward {
            match seq.get(pos + 1) {
                Some(&next) => Some(next),
                None => self.repeat.wraps().then(|| seq[0]),
            }
        } else if pos > 0 {
            Some(seq[pos - 1])
        } else {
            self.repeat.wraps().then(|| seq[seq.len() - 1])
        }
    }

    pub fn next_after(&self, id: u64) -> Option<u64> {
        self.step(id, true)
    }
    pub fn prev_before(&self, id: u64) -> Option<u64> {
        self.step(id, false)
    }

    /// The queued track to play after `id`, if there is one.
    fn queued_after(&self, id: u64) -> Option<u64> {
        // `id` itself may still be listed, if it was too short to be heard yet.
        self.queue.iter().copied().find(|&q| q != id)
    }

    /// What the Next button goes to.
    pub fn skip_from(&self, id: u64) -> Option<u64> {
        self.queued_after(id).or_else(|| self.next_after(id))
    }

    /// What plays when `id` reaches its end.
    pub fn following(&self, id: u64) -> Option<u64> {
        if let Some(queued) = self.queued_after(id) {
            return Some(queued);
        }
        if self.repeat == Repeat::One && self.get(id).is_some_and(|e| !e.failed) {
            return Some(id);
        }
        self.next_after(id)
    }

    /// Whether a change from `from` to `to` must be seamless: the same track
    /// again, the next track of one file, or the next track of an album.
    pub fn keeps_gapless(&self, from: u64, to: u64) -> bool {
        if from == to {
            return true;
        }
        let (Some(a), Some(b)) = (self.get(from), self.get(to)) else {
            return false;
        };
        if a.span.is_some() && b.span.is_some() && a.path == b.path {
            return true;
        }
        let adjacent = matches!(
            (self.index_of(from), self.index_of(to)),
            (Some(i), Some(j)) if i + 1 == j
        );
        adjacent && a.info.album.is_some() && a.info.album == b.info.album
    }

    /// Adds a track to the queue, or takes it out if it is already there.
    pub fn toggle_queued(&mut self, id: u64) {
        match self.queue.iter().position(|&q| q == id) {
            Some(at) => {
                self.queue.remove(at);
            }
            None => self.queue.push(id),
        }
        self.revision += 1;
    }

    /// A track has started being heard, so its place in the queue is used up.
    pub fn dequeue(&mut self, id: u64) {
        let before = self.queue.len();
        self.queue.retain(|&q| q != id);
        if self.queue.len() != before {
            self.revision += 1;
        }
    }

    /// 1 for the track that plays next, and so on.
    pub fn queue_position(&self, id: u64) -> Option<usize> {
        self.queue.iter().position(|&q| q == id).map(|at| at + 1)
    }

    /// Entries matching every word of `query`, in list order. Ignores case
    /// and accents, and looks at file names as well as titles.
    pub fn search(&self, query: &str) -> Vec<u64> {
        let folded = |text: &str| -> String { text.chars().map(fold).collect::<String>().to_lowercase() };
        let query = folded(query);
        let words: Vec<&str> = query.split_whitespace().collect();
        self.entries
            .iter()
            .filter(|e| {
                let text = folded(&format!("{} {}", e.display_title(), e.path.display()));
                words.iter().all(|w| text.contains(w))
            })
            .map(|e| e.id)
            .collect()
    }

    pub fn first(&self) -> Option<u64> {
        self.sequence().first().copied()
    }

    pub fn remove(&mut self, ids: &[u64]) {
        for &id in ids {
            let seq = self.sequence();
            if let Some(at) = seq.iter().position(|&o| o == id) {
                self.tombstone = Some((id, at));
            }
            self.entries.retain(|e| e.id != id);
            self.order.retain(|&o| o != id);
            self.selected.retain(|&s| s != id);
            self.queue.retain(|&q| q != id);
        }
        self.revision += 1;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.selected.clear();
        self.queue.clear();
        self.tombstone = None;
        self.revision += 1;
    }

    /// Sorts by artist and title, falling back to the filename.
    pub fn sort_by_title(&mut self) {
        self.entries
            .sort_by_cached_key(|e| e.display_title().to_lowercase());
        self.revision += 1;
    }

    /// Sorts by full path, which restores album order for a tidy folder tree.
    pub fn sort_by_path(&mut self) {
        self.entries
            .sort_by_cached_key(|e| e.path.to_string_lossy().to_lowercase());
        self.revision += 1;
    }

    /// Moves the selected entries by `delta` rows, keeping them together.
    /// Returns the distance actually moved.
    pub fn move_selected(&mut self, delta: isize) -> isize {
        if self.selected.is_empty() || delta == 0 {
            return 0;
        }
        let mut moved = 0;
        for _ in 0..delta.unsigned_abs() {
            let idx: Vec<usize> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| self.selected.contains(&e.id))
                .map(|(i, _)| i)
                .collect();
            if delta > 0 {
                if idx.last().is_none_or(|&l| l + 1 >= self.entries.len()) {
                    break;
                }
                for &i in idx.iter().rev() {
                    self.entries.swap(i, i + 1);
                }
                moved += 1;
            } else {
                if idx.first().is_none_or(|&f| f == 0) {
                    break;
                }
                for &i in &idx {
                    self.entries.swap(i, i - 1);
                }
                moved -= 1;
            }
        }
        if moved != 0 {
            self.revision += 1;
        }
        moved
    }

    pub fn total_seconds(&self) -> f64 {
        self.entries.iter().filter_map(|e| e.info.duration).sum()
    }

    pub fn to_m3u(&self) -> String {
        let mut out = String::from("#EXTM3U\n");
        for e in &self.entries {
            if let Some(title) = e.saved_title() {
                let secs = e.info.duration.map_or(-1, |d| d.round() as i64);
                out.push_str(&format!("#EXTINF:{secs},{}\n", title.replace('\n', " ")));
            }
            // A comment to other players, so the line below is still a plain
            // path. VLC reads these the same way.
            if let Some(span) = e.span {
                out.push_str(&format!("#EXTVLCOPT:start-time={:.5}\n", span.start));
                if let Some(end) = span.end {
                    out.push_str(&format!("#EXTVLCOPT:stop-time={end:.5}\n"));
                }
            }
            out.push_str(&e.path.to_string_lossy());
            out.push('\n');
        }
        out
    }
}

/// Shared handle the decoder thread uses to ask what comes next.
pub struct SharedQueue(pub Arc<Mutex<Playlist>>);

impl Queue for SharedQueue {
    fn next_after(&mut self, track: u64) -> Option<(u64, Clip)> {
        let pl = self.0.lock().ok()?;
        let next = pl.following(track)?;
        Some((next, pl.get(next)?.clip()))
    }
    fn clip_of(&mut self, track: u64) -> Option<Clip> {
        Some(self.0.lock().ok()?.get(track)?.clip())
    }
    fn mark_failed(&mut self, track: u64, _error: &str) {
        if let Ok(mut pl) = self.0.lock()
            && let Some(e) = pl.get_mut(track)
        {
            e.failed = true;
        }
    }
    fn keeps_gapless(&mut self, from: u64, to: u64) -> bool {
        self.0.lock().is_ok_and(|pl| pl.keeps_gapless(from, to))
    }
}

pub fn is_audio(path: &Path) -> bool {
    has_extension(path, AUDIO_EXTENSIONS)
}

fn has_extension(path: &Path, list: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// One thing to add to the playlist: a file or a radio station URL, with the
/// title the playlist file gave it, if any.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub span: Option<Span>,
}

impl From<PathBuf> for Item {
    fn from(path: PathBuf) -> Self {
        Self {
            path,
            title: None,
            artist: None,
            album: None,
            span: None,
        }
    }
}

/// One entry of a playlist file, before its path is resolved.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub target: String,
    pub title: Option<String>,
    pub span: Option<Span>,
}

/// The raw entries of a playlist, with titles, before any path is resolved.
/// PLS is recognised by its `[playlist]` header and anything else is read as
/// M3U, so a mislabelled file still works.
pub fn playlist_lines(text: &str) -> Vec<Line> {
    let lines = text.lines().map(|l| l.trim().trim_start_matches('\u{feff}').trim());
    let is_pls = lines
        .clone()
        .find(|l| !l.is_empty())
        .is_some_and(|l| l.eq_ignore_ascii_case("[playlist]"));
    if is_pls {
        return pls_lines(lines);
    }
    let mut out = Vec::new();
    let mut title = None;
    let (mut start, mut stop) = (None, None);
    for line in lines {
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            // #EXTINF:<seconds>,<title>
            title = info
                .split_once(',')
                .map(|(_, t)| t.trim().to_string())
                .filter(|t| !t.is_empty());
        } else if let Some(value) = line.strip_prefix("#EXTVLCOPT:start-time=") {
            start = value.parse::<f64>().ok();
        } else if let Some(value) = line.strip_prefix("#EXTVLCOPT:stop-time=") {
            stop = value.parse::<f64>().ok();
        } else if !line.is_empty() && !line.starts_with('#') {
            let span = (start.is_some() || stop.is_some()).then(|| Span {
                start: start.unwrap_or(0.0),
                end: stop,
            });
            (start, stop) = (None, None);
            out.push(Line {
                target: line.to_string(),
                title: title.take(),
                span,
            });
        }
    }
    out
}

/// `FileN=` lines in N order, with `TitleN=` attached. Lengths are ignored.
fn pls_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<Line> {
    let mut files: Vec<(u32, String)> = Vec::new();
    let mut titles: Vec<(u32, String)> = Vec::new();
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        if value.is_empty() {
            continue;
        }
        let numbered = |prefix: &str| key.strip_prefix(prefix)?.parse::<u32>().ok();
        if let Some(n) = numbered("file") {
            files.push((n, value.to_string()));
        } else if let Some(n) = numbered("title") {
            titles.push((n, value.to_string()));
        }
    }
    files.sort_by_key(|(n, _)| *n);
    files
        .into_iter()
        .map(|(n, target)| Line {
            target,
            title: titles.iter().find(|(t, _)| *t == n).map(|(_, t)| t.clone()),
            span: None,
        })
        .collect()
}

/// Parses M3U or PLS text. Relative paths resolve against `base`.
pub fn parse_playlist(text: &str, base: &Path) -> Vec<Item> {
    playlist_lines(text)
        .into_iter()
        .filter_map(|line| {
            Some(Item {
                path: resolve_entry(&line.target, base)?,
                title: line.title,
                span: line.span,
                ..Item::from(PathBuf::new())
            })
        })
        .collect()
}

/// Reads a playlist file of any supported kind.
pub fn read_playlist(path: &Path) -> Vec<Item> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let base = path.parent().unwrap_or(Path::new("/"));
    let text = decode_text(&bytes);
    if has_extension(path, &["cue"]) {
        crate::cue::parse(&text, base)
    } else {
        parse_playlist(&text, base)
    }
}

/// Turns one playlist line into a local path or a station URL, or `None` for
/// other schemes and for Windows drive paths that can't be found here.
pub(crate) fn resolve_entry(line: &str, base: &Path) -> Option<PathBuf> {
    let mut line = std::borrow::Cow::Borrowed(line);
    if let Some(rest) = line.strip_prefix("file://") {
        // file:///Users/x, file://localhost/Users/x, file:///C:/Music/x
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode(rest);
        line = match windows_drive_len(decoded.trim_start_matches('/')) {
            Some(_) => decoded.trim_start_matches('/').to_owned().into(),
            None => return Some(PathBuf::from(decoded)),
        };
    }
    if is_stream(Path::new(line.as_ref())) {
        return Some(PathBuf::from(line.trim()));
    }
    // Other schemes (rtsp, mms and so on) are not playable here.
    if line.contains("://") {
        return None;
    }
    let windows = windows_drive_len(&line).is_some()
        || line.starts_with("\\\\")
        || (line.contains('\\') && !line.starts_with('/'));
    if !windows {
        let p = Path::new(line.as_ref());
        return Some(if p.is_absolute() { p.to_path_buf() } else { base.join(p) });
    }
    let unified = line.replace('\\', "/");
    let absolute = windows_drive_len(&unified).is_some() || unified.starts_with('/');
    let parts: Vec<&str> = match windows_drive_len(&unified) {
        Some(n) => &unified[n..],
        None => unified.as_str(),
    }
    .split('/')
    .filter(|c| !c.is_empty())
    .collect();
    if !absolute {
        return Some(parts.iter().fold(base.to_path_buf(), |p, c| p.join(c)));
    }
    // The drive letter means nothing on a Mac. The music usually sits beside
    // the playlist with the same folder layout, so look for the longest tail
    // of the path under the playlist's folder. UNC paths also start with the
    // server and share names, which this skips the same way.
    (0..parts.len())
        .map(|skip| parts[skip..].iter().fold(base.to_path_buf(), |p, c| p.join(c)))
        .find(|candidate| candidate.is_file())
}

/// Length of a leading `C:` drive prefix, if there is one.
fn windows_drive_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then_some(2)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Expands whatever the user dropped or pasted: audio files and station URLs
/// stay, folders are walked in name order, and playlists are read. Anything
/// else is ignored.
pub fn expand_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<Item> {
    expand_items(paths.into_iter().map(Item::from))
}

/// Like `expand_paths`, keeping any titles the items already have.
pub fn expand_items(items: impl IntoIterator<Item = Item>) -> Vec<Item> {
    let mut out = Vec::new();
    for item in items {
        expand_one(item, &mut out, 0);
    }
    out
}

/// Text from the clipboard or the add-station box: station URLs, file paths
/// or `file://` URLs one per line, or a whole pasted playlist. A bare
/// `radio.example/live` is taken to be a web address.
pub fn parse_pasted(text: &str) -> Vec<Item> {
    let text = text.trim();
    let looks_like_host = !text.contains(char::is_whitespace)
        && !text.contains("://")
        && !text.starts_with(['/', '~', '.'])
        && text.split('/').next().is_some_and(|host| host.contains('.') && !host.ends_with('.'))
        && !has_extension(Path::new(text), AUDIO_EXTENSIONS);
    if looks_like_host {
        return vec![Item::from(PathBuf::from(format!("http://{text}")))];
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let expanded: String = text
        .lines()
        .map(|line| {
            let line = line.trim();
            match (line.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => format!("{}\n", home.join(rest).display()),
                _ => format!("{line}\n"),
            }
        })
        .collect();
    // Pasted text is often not a list of files at all, so a local path only
    // counts if it is really there. Relative lines mean nothing here.
    parse_playlist(&expanded, Path::new("/nonexistent"))
        .into_iter()
        .filter(|i| is_stream(&i.path) || i.path.exists())
        .collect()
}

fn is_hidden(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn expand_one(item: Item, out: &mut Vec<Item>, depth: u32) {
    let path = item.path.as_path();
    // The limit stops symlink loops from recursing forever.
    if depth > 32 {
        return;
    }
    if is_stream(path) {
        out.push(item);
    } else if path.is_dir() {
        let Ok(dir) = std::fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<PathBuf> = dir.flatten().map(|e| e.path()).collect();
        children.sort_by_cached_key(|p| p.to_string_lossy().to_lowercase());
        // A CUE sheet stands in for the single file it describes.
        let sheets: Vec<(PathBuf, Vec<Item>)> = children
            .iter()
            .filter(|c| has_extension(c, &["cue"]) && !is_hidden(c))
            .map(|c| (c.clone(), read_playlist(c)))
            .filter(|(_, tracks)| !tracks.is_empty())
            .collect();
        for child in children {
            if is_hidden(&child) || sheets.iter().any(|(_, t)| t.iter().any(|i| i.path == child)) {
                continue;
            }
            if let Some((_, tracks)) = sheets.iter().find(|(sheet, _)| *sheet == child) {
                out.extend(tracks.iter().cloned());
            } else if child.is_dir() || is_audio(&child) {
                // Other playlists in a folder would duplicate its own tracks.
                expand_one(Item::from(child), out, depth + 1);
            }
        }
    } else if has_extension(path, PLAYLIST_EXTENSIONS) {
        for entry in read_playlist(path) {
            if is_stream(&entry.path) || is_audio(&entry.path) {
                out.push(entry);
            }
        }
    } else if is_audio(path) {
        out.push(item);
    }
}

/// Background metadata reader. One thread is enough: only tags are read, and
/// playback must never compete with a large folder drop for disk.
pub fn spawn_scanner(
    playlist: Arc<Mutex<Playlist>>,
    notify: Arc<dyn Fn() + Send + Sync>,
) -> std::sync::mpsc::Sender<Vec<(u64, PathBuf)>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<(u64, PathBuf)>>();
    std::thread::Builder::new()
        .name("ferroamp-scan".into())
        .spawn(move || {
            while let Ok(batch) = rx.recv() {
                let mut since_notify = std::time::Instant::now();
                for (id, path) in batch {
                    // Stations are never contacted just to fill in the list.
                    if is_stream(&path) {
                        if let Ok(mut pl) = playlist.lock()
                            && let Some(entry) = pl.get_mut(id)
                        {
                            entry.scanned = true;
                        }
                        continue;
                    }
                    let result = crate::meta::read(&path);
                    if let Ok(mut pl) = playlist.lock()
                        && let Some(entry) = pl.get_mut(id)
                    {
                        entry.scanned = true;
                        match result {
                            Ok(info) => entry.apply_scan(info),
                            Err(_) => entry.failed = true,
                        }
                    }
                    if since_notify.elapsed().as_millis() > 200 {
                        notify();
                        since_notify = std::time::Instant::now();
                    }
                }
                notify();
            }
        })
        .expect("spawn scanner thread");
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: usize) -> (Playlist, Vec<u64>) {
        let mut pl = Playlist::new(42);
        let added = pl.add_paths((0..n).map(|i| PathBuf::from(format!("/m/{i}.mp3"))));
        (pl, added.into_iter().map(|(id, _)| id).collect())
    }

    #[test]
    fn sequence_stops_at_end_unless_repeating() {
        let (mut pl, ids) = list(3);
        assert_eq!(pl.next_after(ids[0]), Some(ids[1]));
        assert_eq!(pl.next_after(ids[2]), None);
        assert_eq!(pl.prev_before(ids[0]), None);
        pl.repeat = Repeat::All;
        assert_eq!(pl.next_after(ids[2]), Some(ids[0]));
        assert_eq!(pl.prev_before(ids[0]), Some(ids[2]));
    }

    #[test]
    fn repeat_one_only_holds_back_the_automatic_advance() {
        let (mut pl, ids) = list(3);
        pl.repeat = Repeat::One;
        assert_eq!(pl.following(ids[1]), Some(ids[1]));
        assert_eq!(pl.skip_from(ids[1]), Some(ids[2]));
        assert_eq!(pl.skip_from(ids[2]), Some(ids[0]), "wraps like repeat all");
        pl.get_mut(ids[1]).unwrap().failed = true;
        assert_eq!(pl.following(ids[1]), Some(ids[2]), "a broken track can't loop");
    }

    #[test]
    fn the_queue_plays_first_and_then_the_order_resumes() {
        let (mut pl, ids) = list(5);
        pl.toggle_queued(ids[3]);
        pl.toggle_queued(ids[1]);
        assert_eq!(pl.queue_position(ids[1]), Some(2));
        assert_eq!(pl.following(ids[0]), Some(ids[3]));
        // Still listed until heard, so Next finds it after the decoder did.
        assert_eq!(pl.skip_from(ids[0]), Some(ids[3]));
        pl.dequeue(ids[3]);
        assert_eq!(pl.following(ids[3]), Some(ids[1]));
        pl.dequeue(ids[1]);
        assert_eq!(pl.following(ids[1]), Some(ids[2]));
        pl.toggle_queued(ids[4]);
        pl.toggle_queued(ids[4]);
        assert_eq!(pl.queue_position(ids[4]), None);
        pl.toggle_queued(ids[4]);
        pl.remove(&[ids[4]]);
        assert_eq!(pl.following(ids[0]), Some(ids[1]));
    }

    #[test]
    fn a_queued_track_wins_over_repeat_one() {
        let (mut pl, ids) = list(3);
        pl.repeat = Repeat::One;
        pl.toggle_queued(ids[2]);
        assert_eq!(pl.following(ids[0]), Some(ids[2]));
    }

    #[test]
    fn search_ignores_case_accents_and_word_order() {
        let mut pl = Playlist::new(1);
        let ids: Vec<u64> = pl
            .add_paths([
                PathBuf::from("/m/Rock/Blue Monday.mp3"),
                PathBuf::from("/m/Polish/\u{17b}\u{f3}\u{142}w.flac"),
                PathBuf::from("/m/Rock/Other.ogg"),
            ])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(pl.search("monday BLUE"), [ids[0]]);
        assert_eq!(pl.search("zolw"), [ids[1]]);
        assert_eq!(pl.search("rock"), [ids[0], ids[2]], "folders count too");
        assert_eq!(pl.search(""), ids);
        assert!(pl.search("nothing like this").is_empty());
    }

    #[test]
    fn crossfade_leaves_albums_and_single_files_alone() {
        let mut pl = Playlist::new(1);
        let album = |n: &str| Item { album: Some("LP".into()), ..Item::from(PathBuf::from(n)) };
        let ids: Vec<u64> = pl
            .add_paths([PathBuf::from("/a.mp3"), PathBuf::from("/b.mp3"), PathBuf::from("/c.mp3"), PathBuf::from("/d.mp3")])
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for &id in &ids[..3] {
            pl.get_mut(id).unwrap().info.album = album("x").album;
        }
        assert!(pl.keeps_gapless(ids[0], ids[0]), "repeating one track");
        assert!(pl.keeps_gapless(ids[0], ids[1]), "same album, next in the list");
        assert!(!pl.keeps_gapless(ids[0], ids[2]), "same album but skipped ahead");
        assert!(!pl.keeps_gapless(ids[2], ids[3]), "different albums");
    }

    #[test]
    fn cue_tracks_survive_a_save_and_a_scan() {
        let span = Span { start: 182.4, end: Some(300.0) };
        let mut pl = Playlist::new(1);
        let id = pl.add_items([Item {
            title: Some("Second".into()),
            artist: Some("Guest".into()),
            span: Some(span),
            ..Item::from(PathBuf::from("/m/album.flac"))
        }])[0].0;
        let scanned = TrackInfo {
            title: Some("Whole Album".into()),
            artist: Some("The Band".into()),
            album: Some("The Album".into()),
            duration: Some(1000.0),
            ..TrackInfo::default()
        };
        pl.get_mut(id).unwrap().apply_scan(scanned);
        let e = pl.get(id).unwrap();
        assert_eq!(e.display_title(), "Guest - Second");
        assert_eq!(e.info.album.as_deref(), Some("The Album"));
        assert!((e.info.duration.unwrap() - 117.6).abs() < 1e-6);

        let saved = pl.to_m3u();
        assert!(saved.contains("#EXTVLCOPT:start-time=182.40000\n#EXTVLCOPT:stop-time=300.00000\n/m/album.flac\n"));
        let items = parse_playlist(&saved, Path::new("/"));
        assert_eq!(items[0].span, Some(span));
        let mut again = Playlist::new(1);
        let id = again.add_items(items)[0].0;
        assert_eq!(again.get(id).unwrap().display_title(), "Guest - Second");
    }

    #[test]
    fn a_folder_with_a_cue_sheet_shows_tracks_instead_of_the_big_file() {
        let dir = music_dir("cuefolder", &["album.flac", "bonus.mp3"]);
        std::fs::write(
            dir.join("album.cue"),
            "FILE \"album.flac\" WAVE\nTRACK 01 AUDIO\nTITLE \"A\"\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nTITLE \"B\"\nINDEX 01 01:00:00\n",
        )
        .unwrap();
        let items = expand_paths([dir.clone()]);
        let titles: Vec<_> = items.iter().map(|i| i.title.clone()).collect();
        assert_eq!(titles, [Some("A".to_string()), Some("B".to_string()), None]);
        assert_eq!(items[2].path, dir.join("bonus.mp3"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shuffle_visits_every_track_once_starting_from_current() {
        let (mut pl, ids) = list(50);
        pl.set_current(Some(ids[17]));
        pl.set_shuffle(true);
        let mut seen = vec![ids[17]];
        let mut at = ids[17];
        while let Some(next) = pl.next_after(at) {
            assert!(!seen.contains(&next), "repeated a track");
            seen.push(next);
            at = next;
        }
        assert_eq!(seen.len(), 50);
        assert_ne!(seen[1..4], ids[18..21], "order does not look shuffled");
    }

    #[test]
    fn tracks_added_while_shuffling_are_still_reachable() {
        let (mut pl, ids) = list(5);
        pl.set_current(Some(ids[0]));
        pl.set_shuffle(true);
        let new = pl.add_paths([PathBuf::from("/m/new.mp3")])[0].0;
        let mut at = ids[0];
        let mut found = false;
        while let Some(next) = pl.next_after(at) {
            found |= next == new;
            at = next;
        }
        assert!(found);
    }

    #[test]
    fn removing_the_playing_track_keeps_the_next_one_correct() {
        let (mut pl, ids) = list(4);
        pl.remove(&[ids[1]]);
        assert_eq!(pl.next_after(ids[1]), Some(ids[2]));
        pl.remove(&[ids[3]]);
        assert_eq!(pl.next_after(ids[3]), None);
    }

    #[test]
    fn move_selected_keeps_block_together_and_stops_at_edges() {
        let (mut pl, ids) = list(5);
        pl.selected = vec![ids[1], ids[2]];
        assert_eq!(pl.move_selected(2), 2);
        let order: Vec<u64> = pl.entries.iter().map(|e| e.id).collect();
        assert_eq!(order, [ids[0], ids[3], ids[4], ids[1], ids[2]]);
        assert_eq!(pl.move_selected(5), 0);
        assert_eq!(pl.move_selected(-9), -3);
        let order: Vec<u64> = pl.entries.iter().map(|e| e.id).collect();
        assert_eq!(order, [ids[1], ids[2], ids[0], ids[3], ids[4]]);
    }

    #[test]
    fn m3u_round_trip_and_path_forms() {
        let (pl, _) = list(2);
        assert_eq!(
            parse(&pl.to_m3u(), Path::new("/")),
            [PathBuf::from("/m/0.mp3"), PathBuf::from("/m/1.mp3")]
        );
        let text = "\u{feff}#EXTM3U\n#EXTINF:1,x\nsub/a b.mp3\n/abs/c.flac\nfile:///u/d%20e.ogg\nhttp://radio/stream\n";
        assert_eq!(
            parse(text, Path::new("/base")),
            [
                PathBuf::from("/base/sub/a b.mp3"),
                PathBuf::from("/abs/c.flac"),
                PathBuf::from("/u/d e.ogg"),
                PathBuf::from("http://radio/stream"),
            ]
        );
    }

    fn parse(text: &str, base: &Path) -> Vec<PathBuf> {
        paths(parse_playlist(text, base))
    }

    fn paths(items: Vec<Item>) -> Vec<PathBuf> {
        items.into_iter().map(|i| i.path).collect()
    }

    #[test]
    fn stations_keep_their_names_through_a_save() {
        let mut pl = Playlist::new(1);
        let text = "#EXTM3U\n#EXTINF:-1,Groove FM\nhttps://radio.example/groove\nhttp://radio.example/nameless\n";
        let ids: Vec<u64> = pl
            .add_items(parse_playlist(text, Path::new("/")))
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let e = pl.get(ids[0]).unwrap();
        assert!(e.is_stream());
        assert_eq!(e.display_title(), "Groove FM");
        assert_eq!(pl.get(ids[1]).unwrap().display_title(), "radio.example/nameless");

        // A song title shows while playing but is not saved as the name.
        pl.get_mut(ids[0]).unwrap().info.title = Some("Artist - Song".into());
        assert_eq!(pl.get(ids[0]).unwrap().display_title(), "Artist - Song (Groove FM)");
        let saved = pl.to_m3u();
        assert!(saved.contains("#EXTINF:-1,Groove FM\nhttps://radio.example/groove\n"));
        assert!(saved.contains("\nhttp://radio.example/nameless\n"));
        assert!(!saved.contains("Artist - Song"));
        let reloaded = parse_playlist(&saved, Path::new("/"));
        assert_eq!(reloaded[0].title.as_deref(), Some("Groove FM"));
    }

    /// A fresh folder of empty files laid out like a copied Windows music dir.
    fn music_dir(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferroamp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in files {
            let path = dir.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        dir
    }

    #[test]
    fn pls_reads_files_in_number_order_with_titles() {
        let text = "[playlist]\r\nNumberOfEntries=4\r\nFile2=b.mp3\r\nTitle2=B\r\n\
                    file1=/abs/a.flac\r\nFile3=http://radio/stream\r\nFILE10=c.ogg\r\nVersion=2\r\n";
        assert_eq!(
            parse(text, Path::new("/base")),
            [
                PathBuf::from("/abs/a.flac"),
                PathBuf::from("/base/b.mp3"),
                PathBuf::from("http://radio/stream"),
                PathBuf::from("/base/c.ogg"),
            ]
        );
        let titles: Vec<_> = parse_playlist(text, Path::new("/"))
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(titles, [None, Some("B".to_string()), None, None]);
        // Detected by content, whatever the extension says.
        assert_eq!(parse("[Playlist]\nFile1=x.mp3\n", Path::new("/b")), [PathBuf::from("/b/x.mp3")]);
    }

    #[test]
    fn windows_paths_resolve_beside_the_playlist() {
        let dir = music_dir("win", &["Artist/Album/01.mp3", "02.mp3", "Other/03.mp3"]);
        let text = "Artist\\Album\\01.mp3\r\n\
                    C:\\Users\\me\\Music\\02.mp3\r\n\
                    D:\\Music\\Other\\03.mp3\r\n\
                    \\\\server\\share\\Artist\\Album\\01.mp3\r\n\
                    file:///E:/Rips/Other/03.mp3\r\n\
                    C:\\Missing\\nowhere.mp3\r\n";
        assert_eq!(
            parse(text, &dir),
            [
                dir.join("Artist/Album/01.mp3"),
                dir.join("02.mp3"),
                dir.join("Other/03.mp3"),
                dir.join("Artist/Album/01.mp3"),
                dir.join("Other/03.mp3"),
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_playlists_decode_as_windows_1252() {
        let dir = music_dir("cp1252", &["Bj\u{f6}rk \u{2013} J\u{f3}ga.mp3"]);
        let playlist = dir.join("old.m3u");
        // "Björk – Jóga.mp3" as an old Windows player would have written it.
        std::fs::write(&playlist, b"#EXTM3U\r\nBj\xF6rk \x96 J\xF3ga.mp3\r\n").unwrap();
        assert_eq!(paths(read_playlist(&playlist)), [dir.join("Bj\u{f6}rk \u{2013} J\u{f3}ga.mp3")]);
        // Valid UTF-8 is left alone, with or without a byte-order mark.
        assert_eq!(decode_text("\u{feff}Bj\u{f6}rk".as_bytes()), "Bj\u{f6}rk");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pasted_text_becomes_stations_and_files() {
        let dir = music_dir("paste", &["a b.mp3"]);
        let file_url = format!("file://{}", dir.join("a%20b.mp3").display());
        let text = format!("https://radio.example/live\n{file_url}\nhello there\n");
        assert_eq!(
            paths(expand_items(parse_pasted(&text))),
            [PathBuf::from("https://radio.example/live"), dir.join("a b.mp3")]
        );
        assert_eq!(
            paths(parse_pasted("  radio.example:8000/stream \n")),
            [PathBuf::from("http://radio.example:8000/stream")]
        );
        assert!(paths(expand_items(parse_pasted("song.mp3"))).is_empty());
        assert!(paths(expand_items(parse_pasted("just some words"))).is_empty());
        let pls = "[playlist]\nFile1=http://radio.example/a\nTitle1=A FM\n";
        assert_eq!(parse_pasted(pls)[0].title.as_deref(), Some("A FM"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropped_pls_is_expanded() {
        let dir = music_dir("pls", &["a.mp3", "b.flac"]);
        let playlist = dir.join("mix.pls");
        std::fs::write(&playlist, "[playlist]\nFile1=a.mp3\nFile2=notes.txt\nFile3=b.flac\n").unwrap();
        assert_eq!(
            paths(expand_paths([playlist, PathBuf::from("https://radio.example/x")])),
            [dir.join("a.mp3"), dir.join("b.flac"), PathBuf::from("https://radio.example/x")]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
