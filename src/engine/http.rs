//! Just enough HTTP/1.1 to play internet radio: GET, redirects, chunked
//! bodies, and the ICY dialect of old Shoutcast servers, which answer
//! `ICY 200 OK` and mix song titles into the audio. TLS is the system's own.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const USER_AGENT: &str = concat!("ferroamp/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// A station that sends nothing for this long is treated as gone.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REDIRECTS: usize = 6;
const MAX_HEADER_LINE: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (tls, rest) = if let Some(r) = strip_prefix_ci(s, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(s, "http://") {
            (false, r)
        } else {
            return Err(format!("not a web address: {s}"));
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let path = path.split('#').next().unwrap_or_default();
        let path = match path.chars().next() {
            Some('/') => path.to_string(),
            _ => format!("/{path}"),
        };
        // Drop any user:password@ part; radio never needs it.
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let default_port = if tls { 443 } else { 80 };
        let bad_port = || format!("bad port in {s}");
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let end = v6.find(']').ok_or_else(|| format!("bad address: {s}"))?;
            let port = match v6[end + 1..].strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| bad_port())?,
                None => default_port,
            };
            (v6[..end].to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, "")) => (h.to_string(), default_port),
                Some((h, p)) => (h.to_string(), p.parse().map_err(|_| bad_port())?),
                None => (authority.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return Err(format!("no host in {s}"));
        }
        Ok(Self {
            tls,
            host,
            port,
            path,
        })
    }

    /// Resolves a redirect `Location` or a playlist entry against this URL.
    pub fn join(&self, reference: &str) -> Result<Self, String> {
        let reference = reference.trim();
        if reference.contains("://") {
            return Self::parse(reference);
        }
        if let Some(rest) = reference.strip_prefix("//") {
            let scheme = if self.tls { "https" } else { "http" };
            return Self::parse(&format!("{scheme}://{rest}"));
        }
        let path = if reference.starts_with('/') {
            reference.to_string()
        } else {
            let without_query = self.path.split('?').next().unwrap_or("/");
            let dir_end = without_query.rfind('/').map_or(0, |i| i + 1);
            format!("{}{reference}", &without_query[..dir_end])
        };
        Ok(Self {
            path,
            ..self.clone()
        })
    }

    fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default_port = if self.tls { 443 } else { 80 };
        if self.port == default_port {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.host_header(), self.path)
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

pub struct Response {
    pub status: u16,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    /// Where the response actually came from, after redirects.
    pub url: Url,
    pub body: Box<dyn Read + Send>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// GET with redirects followed. Only a 2xx response is returned.
/// `icy` asks the server to mix song titles into the body.
pub fn get(url: &Url, icy: bool) -> Result<Response, String> {
    let mut url = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let response = request(&url, icy)?;
        match response.status {
            200..=299 => return Ok(response),
            301 | 302 | 303 | 307 | 308 => {
                let location = response
                    .header("location")
                    .ok_or("the server redirected to nowhere")?;
                url = url.join(location)?;
            }
            404 => return Err("the station's address was not found (404)".into()),
            status => return Err(format!("the server refused (HTTP {status})")),
        }
    }
    Err("too many redirects".into())
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

fn connect(url: &Url) -> Result<Box<dyn Stream>, String> {
    let addrs = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot find {}: {e}", url.host))?;
    let mut last = format!("cannot find {}", url.host);
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(tcp) => {
                let _ = tcp.set_read_timeout(Some(READ_TIMEOUT));
                let _ = tcp.set_write_timeout(Some(READ_TIMEOUT));
                let _ = tcp.set_nodelay(true);
                if !url.tls {
                    return Ok(Box::new(tcp));
                }
                let tls = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
                return tls
                    .connect(&url.host, tcp)
                    .map(|s| Box::new(s) as Box<dyn Stream>)
                    .map_err(|e| format!("secure connection to {} failed: {e}", url.host));
            }
            Err(e) => last = format!("cannot connect to {}: {e}", url.host),
        }
    }
    Err(last)
}

fn request(url: &Url, icy: bool) -> Result<Response, String> {
    let mut stream = connect(url)?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {USER_AGENT}\r\nAccept: */*\r\n{}Connection: close\r\n\r\n",
        url.path,
        url.host_header(),
        if icy { "Icy-MetaData: 1\r\n" } else { "" },
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| format!("sending to {} failed: {e}", url.host))?;
    let mut reader = BufReader::with_capacity(16 * 1024, stream);
    let (status, headers) =
        read_head(&mut reader).map_err(|e| format!("bad reply from {}: {e}", url.host))?;
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
    let length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<u64>().ok());
    let body: Box<dyn Read + Send> = if chunked {
        Box::new(Chunked::new(reader))
    } else if let Some(n) = length {
        Box::new(reader.take(n))
    } else {
        Box::new(reader)
    };
    Ok(Response {
        status,
        headers,
        url: url.clone(),
        body,
    })
}

/// Reads the status line and headers. Accepts `HTTP/1.x` and `ICY`.
pub fn read_head(reader: &mut impl BufRead) -> io::Result<(u16, Vec<(String, String)>)> {
    let invalid = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut line = String::new();
    let mut read_line = |line: &mut String| {
        line.clear();
        let mut bytes = Vec::new();
        reader.take(MAX_HEADER_LINE).read_until(b'\n', &mut bytes)?;
        *line = String::from_utf8_lossy(&bytes).trim_end().to_string();
        Ok::<usize, io::Error>(bytes.len())
    };
    if read_line(&mut line)? == 0 {
        return Err(invalid("the server closed the connection without replying"));
    }
    let mut parts = line.split_whitespace();
    let proto = parts.next().unwrap_or_default();
    if !(proto.starts_with("HTTP/") || proto == "ICY") {
        return Err(invalid("not an HTTP server"));
    }
    let status = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid("no status code"))?;
    let mut headers = Vec::new();
    for _ in 0..200 {
        if read_line(&mut line)? == 0 || line.is_empty() {
            return Ok((status, headers));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Err(invalid("too many headers"))
}

/// Decodes `Transfer-Encoding: chunked`.
pub struct Chunked<R> {
    inner: R,
    left: u64,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            left: 0,
            done: false,
        }
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            // The CRLF that ends the previous chunk shows up as an empty line.
            let mut line = String::new();
            loop {
                line.clear();
                if (&mut self.inner).take(1024).read_line(&mut line)? == 0 {
                    self.done = true;
                    return Ok(0);
                }
                if !line.trim().is_empty() {
                    break;
                }
            }
            let size = line.trim().split(';').next().unwrap_or_default().trim();
            self.left = u64::from_str_radix(size, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Strips ICY metadata blocks out of a body, keeping the latest song title.
/// Every `metaint` audio bytes the server inserts one length byte `n` and then
/// `n * 16` bytes of text such as `StreamTitle='Artist - Song';`.
pub struct Icy<R> {
    inner: R,
    metaint: usize,
    until_meta: usize,
    title: Option<String>,
    changed: bool,
}

impl<R: Read> Icy<R> {
    /// `metaint` 0 means the server sends no metadata.
    pub fn new(inner: R, metaint: usize) -> Self {
        Self {
            inner,
            metaint,
            until_meta: metaint,
            title: None,
            changed: false,
        }
    }

    /// The new song title, once per change. Empty means the station cleared it.
    pub fn take_title(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.changed) {
            return None;
        }
        self.title.clone()
    }

    fn read_meta(&mut self) -> io::Result<()> {
        let mut len = [0u8];
        self.inner.read_exact(&mut len)?;
        let len = len[0] as usize * 16;
        if len == 0 {
            return Ok(());
        }
        let mut block = vec![0; len];
        self.inner.read_exact(&mut block)?;
        if let Some(title) = parse_stream_title(&block)
            && self.title.as_ref() != Some(&title)
        {
            self.title = Some(title);
            self.changed = true;
        }
        Ok(())
    }
}

impl<R: Read> Read for Icy<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.metaint == 0 {
            return self.inner.read(buf);
        }
        if self.until_meta == 0 {
            self.read_meta()?;
            self.until_meta = self.metaint;
        }
        let want = buf.len().min(self.until_meta);
        let n = self.inner.read(&mut buf[..want])?;
        self.until_meta -= n;
        Ok(n)
    }
}

/// `StreamTitle='Artist - Song';StreamUrl='';` → `Artist - Song`.
pub fn parse_stream_title(block: &[u8]) -> Option<String> {
    let end = block.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let text = crate::meta::decode_text(&block[..end]);
    let start = text.find("StreamTitle='")? + "StreamTitle='".len();
    let rest = &text[start..];
    // Titles can themselves contain quotes, so look for the closing `';`.
    let value = match rest.find("';") {
        Some(i) => &rest[..i],
        None => rest.trim_end_matches('\''),
    };
    Some(value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_and_join() {
        let u = Url::parse("HTTPS://user:pw@radio.example:8443/live/mp3?x=1#frag").unwrap();
        assert_eq!(
            u,
            Url {
                tls: true,
                host: "radio.example".into(),
                port: 8443,
                path: "/live/mp3?x=1".into()
            }
        );
        assert_eq!(u.to_string(), "https://radio.example:8443/live/mp3?x=1");
        assert_eq!(Url::parse("http://h").unwrap().path, "/");
        assert_eq!(Url::parse("http://h?q").unwrap().path, "/?q");
        assert_eq!(Url::parse("http://[::1]:8000/s").unwrap().host, "::1");
        assert!(Url::parse("ftp://h/x").is_err());
        assert!(Url::parse("http://h:port/").is_err());

        assert_eq!(u.join("other.pls").unwrap().path, "/live/other.pls");
        assert_eq!(u.join("/root").unwrap().path, "/root");
        assert_eq!(u.join("//cdn.example/s").unwrap().to_string(), "https://cdn.example/s");
        assert_eq!(u.join("http://x:81/y").unwrap().to_string(), "http://x:81/y");
    }

    #[test]
    fn heads_accept_http_and_icy() {
        let mut r = &b"ICY 200 OK\r\nicy-name:Test FM\r\nicy-metaint: 8192\r\n\r\nAUDIO"[..];
        let (status, headers) = read_head(&mut r).unwrap();
        assert_eq!(status, 200);
        assert_eq!(headers[0], ("icy-name".into(), "Test FM".into()));
        assert_eq!(headers[1], ("icy-metaint".into(), "8192".into()));
        assert_eq!(r, b"AUDIO");

        let mut r = &b"HTTP/1.0 302 Found\nLocation: /x\n\n"[..];
        assert_eq!(read_head(&mut r).unwrap().0, 302);
        assert!(read_head(&mut &b"SSH-2.0-OpenSSH\r\n"[..]).is_err());
        assert!(read_head(&mut &b""[..]).is_err());
    }

    #[test]
    fn chunked_bodies_decode() {
        let raw = &b"4\r\nWiki\r\n5;ext=1\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\n\r\n"[..];
        let mut out = String::new();
        Chunked::new(raw).read_to_string(&mut out).unwrap();
        assert_eq!(out, "Wikipedia in\r\n\r\nchunks.");
        let mut out = Vec::new();
        assert!(Chunked::new(&b"A\r\nshort"[..]).read_to_end(&mut out).is_err());
    }

    #[test]
    fn icy_metadata_is_removed_and_titles_kept() {
        let meta = |s: &str| {
            let mut block = s.as_bytes().to_vec();
            block.resize(s.len().div_ceil(16) * 16, 0);
            let mut out = vec![(block.len() / 16) as u8];
            out.extend(block);
            out
        };
        let mut raw = b"abcd".to_vec();
        raw.extend(meta("StreamTitle='It's - Me';StreamUrl='';"));
        raw.extend(b"efgh");
        raw.push(0); // no metadata this time
        raw.extend(b"ij");
        let mut icy = Icy::new(&raw[..], 4);
        let mut audio = Vec::new();
        let mut titles = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let n = icy.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            audio.extend_from_slice(&buf[..n]);
            titles.extend(icy.take_title());
        }
        assert_eq!(audio, b"abcdefghij");
        assert_eq!(titles, ["It's - Me"]);
        assert_eq!(parse_stream_title(b"StreamTitle='';\0\0"), Some(String::new()));
        assert_eq!(parse_stream_title(b"StreamUrl='x';"), None);
        // Latin-1 titles from old servers.
        assert_eq!(
            parse_stream_title(b"StreamTitle='Bj\xF6rk';").as_deref(),
            Some("Bj\u{f6}rk")
        );
    }
}
