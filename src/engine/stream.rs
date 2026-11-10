//! Internet radio. A background thread downloads into a bounded buffer and the
//! decoder reads from that buffer. The engine only decodes once enough is
//! buffered, so the engine thread never waits on the network and the
//! transport buttons stay instant even when a station stalls.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::http::{self, Icy, Response, Url};

/// Bytes to have in hand before the format is probed: a couple of seconds of a
/// typical 128 kbps station, and Icecast servers send that much at once.
pub const PREFILL: usize = 32 * 1024;
/// Bytes to have in hand before each packet is decoded. Larger than any one
/// MP3, AAC or Vorbis packet.
pub const LOW_WATER: usize = 8 * 1024;
/// Stop downloading ahead past this. Half a minute at 128 kbps.
const BUFFER_CAP: usize = 512 * 1024;
const READ_CHUNK: usize = 16 * 1024;
/// Consecutive failed reconnects before the station is given up on.
const RECONNECTS: u32 = 5;
/// Longest the decoder will wait for bytes before calling the stream ended.
const READER_WAIT: Duration = Duration::from_secs(20);

pub fn is_stream(path: &Path) -> bool {
    let s = path.as_os_str().as_encoded_bytes();
    let starts = |p: &[u8]| s.len() > p.len() && s[..p.len()].eq_ignore_ascii_case(p);
    starts(b"http://") || starts(b"https://")
}

/// What a station says about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamInfo {
    pub station: Option<String>,
    /// The current song, usually `Artist - Title`. Empty while the station
    /// sends none.
    pub title: Option<String>,
    pub kbps: Option<u32>,
    pub rate: Option<u32>,
    pub channels: Option<u8>,
}

pub type InfoSink = Arc<dyn Fn(&StreamInfo) + Send + Sync>;

struct State {
    data: VecDeque<u8>,
    /// Set once the download thread has given up for good.
    ended: Option<Result<(), String>>,
    /// Set when the player no longer wants this stream.
    closed: bool,
    hint: Option<&'static str>,
    info: StreamInfo,
}

struct Inner {
    state: Mutex<State>,
    cond: Condvar,
    sink: Option<InfoSink>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, f: impl FnOnce(&mut StreamInfo)) {
        let info = {
            let mut st = self.lock();
            let before = st.info.clone();
            f(&mut st.info);
            if st.info == before {
                return;
            }
            st.info.clone()
        };
        if let Some(sink) = &self.sink {
            sink(&info);
        }
    }
}

/// An open station. Dropping it stops the download.
pub struct Connection {
    inner: Arc<Inner>,
}

impl Connection {
    pub fn open(url: &str, sink: Option<InfoSink>) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                data: VecDeque::new(),
                ended: None,
                closed: false,
                hint: None,
                info: StreamInfo::default(),
            }),
            cond: Condvar::new(),
            sink,
        });
        let url = url.trim().to_string();
        let thread_inner = inner.clone();
        let spawned = std::thread::Builder::new()
            .name("ferroamp-net".into())
            .spawn(move || download(&url, &thread_inner));
        if let Err(e) = spawned {
            inner.lock().ended = Some(Err(format!("cannot start download: {e}")));
        }
        Self { inner }
    }

    /// Whether `need` bytes are buffered, or no more will ever come.
    pub fn ready(&self, need: usize) -> bool {
        let st = self.inner.lock();
        st.data.len() >= need || st.ended.is_some()
    }

    /// The download has stopped for good.
    pub fn finished(&self) -> bool {
        self.inner.lock().ended.is_some()
    }

    /// Why the station failed, once everything it sent has been played.
    pub fn failure(&self) -> Option<String> {
        let st = self.inner.lock();
        match &st.ended {
            Some(Err(e)) if st.data.is_empty() => Some(e.clone()),
            _ => None,
        }
    }

    /// A file extension matching the server's content type, to guide probing.
    pub fn hint(&self) -> Option<&'static str> {
        self.inner.lock().hint
    }

    pub fn reader(&self) -> Reader {
        Reader {
            inner: self.inner.clone(),
            ogg: OggCursor::default(),
        }
    }

    /// Records what the decoder found, for display.
    pub fn set_format(&self, rate: u32, channels: u8) {
        self.inner.publish(|i| {
            i.rate = Some(rate);
            i.channels = Some(channels);
        });
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.inner.lock().closed = true;
        self.inner.cond.notify_all();
    }
}

/// The decoder's side: plain bytes, blocking only briefly if the buffer runs
/// dry, which the engine avoids by checking `Connection::ready` first.
///
/// An Ogg station starts a whole new Ogg stream at every song, and a reconnect
/// does the same. The decoder can't cross that seam itself, so the reader stops
/// exactly there and reports the end. The engine then opens a fresh reader,
/// which starts cleanly at the new stream's first page.
pub struct Reader {
    inner: Arc<Inner>,
    ogg: OggCursor,
}

#[derive(Default)]
struct OggCursor {
    decided: bool,
    on: bool,
    /// Bytes left in the current page.
    page_left: usize,
    /// A page other than a stream's first has gone by.
    past_first: bool,
    /// Stopped at the start of the next stream.
    seam: bool,
}

impl OggCursor {
    /// Bytes needed in hand before the next read can be cut correctly.
    fn need(&self, data: &VecDeque<u8>) -> usize {
        if !self.decided {
            return 4;
        }
        if !self.on || self.page_left > 0 {
            return 1;
        }
        match data.get(26) {
            Some(&segments) => 27 + segments as usize,
            None => 27,
        }
    }

    /// At a page boundary, reads the page header. False at a new stream.
    fn enter_page(&mut self, data: &VecDeque<u8>) -> bool {
        let sync = data.len() >= 4 && data.range(..4).eq(b"OggS");
        if !self.decided {
            self.decided = true;
            self.on = sync;
        }
        if !self.on || self.page_left > 0 {
            return true;
        }
        let Some(&segments) = data.get(26).filter(|_| sync) else {
            // Lost sync or a torn page at the very end: pass bytes through.
            self.on = false;
            return true;
        };
        let header = 27 + segments as usize;
        if data.len() < header {
            self.on = false;
            return true;
        }
        let first_page = data[5] & 0x02 != 0;
        if first_page && self.past_first {
            self.seam = true;
            return false;
        }
        self.past_first |= !first_page;
        self.page_left = header + data.range(27..header).map(|&b| b as usize).sum::<usize>();
        true
    }
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.ogg.seam || buf.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + READER_WAIT;
        let mut st = self.inner.lock();
        while st.data.len() < self.ogg.need(&st.data) {
            let left = deadline.saturating_duration_since(Instant::now());
            if st.ended.is_some() || st.closed || left.is_zero() {
                if st.data.is_empty() || left.is_zero() {
                    return Ok(0);
                }
                // The station is gone: hand over whatever is left as it is.
                break;
            }
            st = self
                .inner
                .cond
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if !self.ogg.enter_page(&st.data) {
            return Ok(0);
        }
        let limit = if self.ogg.on { self.ogg.page_left } else { usize::MAX };
        let want = buf.len().min(limit);
        let n = st.data.read(&mut buf[..want])?;
        if self.ogg.on {
            self.ogg.page_left -= n;
        }
        drop(st);
        // There is room again for the download thread.
        self.inner.cond.notify_all();
        Ok(n)
    }
}

fn download(url: &str, inner: &Inner) {
    let mut failures = 0;
    let mut ever_received = false;
    loop {
        let mut received = false;
        let result = session(url, inner, &mut received);
        if inner.lock().closed {
            return;
        }
        ever_received |= received;
        failures = if received { 1 } else { failures + 1 };
        let error = match result {
            Ok(()) => "the station stopped sending".to_string(),
            Err(e) => e,
        };
        // A station that never worked fails at once, so the next one can play.
        if !ever_received || failures > RECONNECTS {
            inner.lock().ended = Some(Err(error));
            inner.cond.notify_all();
            return;
        }
        // Back off a little more each time, waking early if closed.
        let wait = Duration::from_secs(failures as u64);
        let st = inner.lock();
        let (st, _) = inner
            .cond
            .wait_timeout_while(st, wait, |s| !s.closed)
            .unwrap_or_else(|e| e.into_inner());
        if st.closed {
            return;
        }
    }
}

/// One connection, read until it fails. `received` is set once audio arrives.
fn session(url: &str, inner: &Inner, received: &mut bool) -> Result<(), String> {
    let response = open_audio(url)?;
    let metaint = response
        .header("icy-metaint")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let station = response
        .header("icy-name")
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from);
    // icy-br can be "128" or "128,128".
    let kbps = response
        .header("icy-br")
        .and_then(|v| v.split(',').next()?.trim().parse().ok());
    let hint = response.header("content-type").and_then(hint_for);
    inner.lock().hint = hint;
    inner.publish(|i| {
        if station.is_some() {
            i.station = station;
        }
        if kbps.is_some() {
            i.kbps = kbps;
        }
    });

    let mut body = Icy::new(response.body, metaint);
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        {
            let st = inner.lock();
            let st = inner
                .cond
                .wait_while(st, |s| s.data.len() >= BUFFER_CAP && !s.closed)
                .unwrap_or_else(|e| e.into_inner());
            if st.closed {
                return Ok(());
            }
        }
        let n = body.read(&mut chunk).map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "the station stopped sending".to_string()
            }
            _ => format!("connection lost: {e}"),
        })?;
        if n == 0 {
            return Ok(());
        }
        if let Some(title) = body.take_title() {
            inner.publish(|i| i.title = Some(title));
        }
        let mut st = inner.lock();
        st.data.extend(&chunk[..n]);
        *received = true;
        drop(st);
        inner.cond.notify_all();
    }
}

/// Connects, following any `.pls` or `.m3u` the address leads to.
fn open_audio(address: &str) -> Result<Response, String> {
    let mut url = Url::parse(address)?;
    for _ in 0..4 {
        let response = http::get(&url, true)?;
        let ctype = response
            .header("content-type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !is_playlist(&ctype, &response.url.path) {
            return Ok(response);
        }
        let base = response.url.clone();
        let mut bytes = Vec::new();
        response
            .body
            .take(256 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("reading the station's playlist failed: {e}"))?;
        let text = crate::meta::decode_text(&bytes);
        if text.contains("#EXT-X-") {
            return Err("this station uses HLS, which is not supported yet".into());
        }
        let first = crate::playlist::playlist_lines(&text)
            .into_iter()
            .find_map(|line| base.join(&line.target).ok())
            .ok_or("the station's playlist lists no streams")?;
        url = first;
    }
    Err("too many nested playlists".into())
}

fn is_playlist(ctype: &str, path: &str) -> bool {
    let path = path.split('?').next().unwrap_or_default().to_ascii_lowercase();
    ctype.contains("mpegurl")
        || ctype.contains("scpls")
        || ctype.starts_with("text/")
        || path.ends_with(".pls")
        || path.ends_with(".m3u")
        || path.ends_with(".m3u8")
}

fn hint_for(ctype: &str) -> Option<&'static str> {
    let ctype = ctype.split(';').next()?.trim().to_ascii_lowercase();
    Some(match ctype.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg" => "mp3",
        "audio/aac" | "audio/aacp" | "audio/x-aac" | "audio/x-hx-aac-adts" => "aac",
        "application/ogg" | "audio/ogg" | "audio/vorbis" | "audio/x-ogg" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_addresses_are_recognised() {
        assert!(is_stream(Path::new("http://radio.example/live")));
        assert!(is_stream(Path::new("HTTPS://radio.example/live")));
        assert!(!is_stream(Path::new("https://")));
        assert!(!is_stream(Path::new("/Users/me/http:/x.mp3")));
        assert!(!is_stream(Path::new("rtsp://radio.example/live")));
    }

    #[test]
    fn content_types_guide_the_decoder() {
        assert_eq!(hint_for("audio/mpeg"), Some("mp3"));
        assert_eq!(hint_for("audio/aacp; charset=x"), Some("aac"));
        assert_eq!(hint_for("application/ogg"), Some("ogg"));
        assert_eq!(hint_for("text/html"), None);
        assert!(is_playlist("audio/x-scpls", "/listen"));
        assert!(is_playlist("audio/x-mpegurl", "/listen"));
        assert!(is_playlist("application/octet-stream", "/groove.PLS?id=1"));
        assert!(!is_playlist("audio/mpeg", "/groove-128-mp3"));
    }
}
