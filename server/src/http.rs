//! HTTP/1.1 server: one OS thread per connection, keep-alive, `Content-Length` request bodies,
//! single-range `Range` responses (so video seeking works), `100-continue`.
//!
//! The relay sits behind `cloudflared` on loopback, so `CF-Connecting-IP` / `X-Forwarded-Proto`
//! are trusted only when the TCP peer is a loopback address.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// Largest request head (request line + headers) accepted.
pub const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const IDLE: Duration = Duration::from_secs(75);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// A request handler. It must either consume the body or leave `body_left` for the server to
/// drain. Returning an error closes the connection.
pub type Handler = Arc<dyn Fn(&Request, &mut Conn) -> io::Result<()> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Percent-decoded path, without the query.
    pub path: String,
    /// Raw query string, without `?`.
    pub query: String,
    /// Header names are lowercase.
    pub headers: Vec<(String, String)>,
    pub peer: SocketAddr,
    pub body_len: u64,
    pub http10: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Percent-decoded query parameter (`+` means space).
    pub fn param(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (pct_decode(k, true)? == key).then(|| pct_decode(v, true)).flatten()
        })
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k == "cookie")
            .flat_map(|(_, v)| v.split(';'))
            .find_map(|c| {
                let (k, v) = c.trim().split_once('=')?;
                (k == name).then_some(v)
            })
    }

    fn via_proxy(&self) -> bool {
        self.peer.ip().is_loopback()
    }

    /// Client address: Cloudflare's header when the peer is the local tunnel, else the peer.
    pub fn client_ip(&self) -> IpAddr {
        if self.via_proxy()
            && let Some(ip) = self.header("cf-connecting-ip").and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
        self.peer.ip()
    }

    /// True when the browser reached us over HTTPS (through the tunnel).
    pub fn https(&self) -> bool {
        self.via_proxy() && self.header("x-forwarded-proto").is_some_and(|v| v.eq_ignore_ascii_case("https"))
    }

    fn wants_close(&self) -> bool {
        let conn = self.header("connection").unwrap_or("");
        let has = |t: &str| conn.split(',').any(|p| p.trim().eq_ignore_ascii_case(t));
        if self.http10 { !has("keep-alive") } else { has("close") }
    }
}

/// One client connection. Handlers write their response through it.
pub struct Conn {
    pub r: BufReader<TcpStream>,
    pub w: TcpStream,
    /// Bytes of the current request body not yet read.
    pub body_left: u64,
    /// Close after this response.
    pub close: bool,
    /// A handler took the socket over (WebSocket); the HTTP loop ends.
    pub upgraded: bool,
    head_only: bool,
}

/// `Read` over the remaining request body.
pub struct Body<'a>(&'a mut Conn);

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.0.body_left == 0 {
            return Ok(0);
        }
        let max = buf.len().min(self.0.body_left.min(usize::MAX as u64) as usize);
        let n = self.0.r.read(&mut buf[..max])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "body cut short"));
        }
        self.0.body_left -= n as u64;
        Ok(n)
    }
}

impl Conn {
    /// Read the whole body into memory if it is at most `max` bytes.
    pub fn read_body(&mut self, max: usize) -> io::Result<Vec<u8>> {
        if self.body_left > max as u64 {
            self.close = true;
            return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
        }
        let mut v = vec![0u8; self.body_left as usize];
        self.r.read_exact(&mut v)?;
        self.body_left = 0;
        Ok(v)
    }

    /// Streaming access to the body.
    pub fn body(&mut self) -> Body<'_> {
        Body(self)
    }

    fn drain_body(&mut self) -> io::Result<()> {
        io::copy(&mut self.body(), &mut io::sink()).map(|_| ())
    }

    fn head(&self, status: u16, ctype: &str, len: u64, extra: &[(&str, &str)]) -> String {
        let mut h = String::with_capacity(256);
        let _ = write!(h, "HTTP/1.1 {status} {}\r\nContent-Length: {len}\r\n", reason(status));
        if !ctype.is_empty() {
            let _ = write!(h, "Content-Type: {ctype}\r\n");
        }
        for (k, v) in extra {
            let _ = write!(h, "{k}: {v}\r\n");
        }
        if self.close {
            h.push_str("Connection: close\r\n");
        }
        h.push_str("\r\n");
        h
    }

    /// Send a complete response from memory (one write for small bodies).
    pub fn send(&mut self, status: u16, ctype: &str, extra: &[(&str, &str)], body: &[u8]) -> io::Result<()> {
        let head = self.head(status, ctype, body.len() as u64, extra);
        if self.head_only {
            return self.w.write_all(head.as_bytes());
        }
        if body.len() <= 64 * 1024 {
            let mut buf = Vec::with_capacity(head.len() + body.len());
            buf.extend_from_slice(head.as_bytes());
            buf.extend_from_slice(body);
            self.w.write_all(&buf)
        } else {
            self.w.write_all(head.as_bytes())?;
            self.w.write_all(body)
        }
    }

    pub fn send_json(&mut self, status: u16, json: &str) -> io::Result<()> {
        self.send(status, "application/json", &[("Cache-Control", "no-store")], json.as_bytes())
    }

    pub fn send_text(&mut self, status: u16, text: &str) -> io::Result<()> {
        self.send(status, "text/plain; charset=utf-8", &[("Cache-Control", "no-store")], text.as_bytes())
    }

    /// Send `len` bytes of `file`, honouring a single `Range` request (206 / 416).
    /// An `If-Range` that does not equal `etag` disables the range.
    pub fn send_file(
        &mut self,
        req: &Request,
        file: &mut File,
        len: u64,
        ctype: &str,
        etag: Option<&str>,
        extra: &[(&str, &str)],
    ) -> io::Result<()> {
        let mut range = req.header("range").and_then(|h| parse_range(h, len));
        if let Some(ir) = req.header("if-range")
            && etag != Some(ir)
        {
            range = None;
        }
        let mut hdrs: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 3);
        hdrs.push(("Accept-Ranges", "bytes"));
        if let Some(e) = etag {
            hdrs.push(("ETag", e));
        }
        hdrs.extend_from_slice(extra);
        let cr;
        let (status, start, n) = match range {
            Some(Err(())) => {
                cr = format!("bytes */{len}");
                hdrs.push(("Content-Range", &cr));
                return self.send(416, "", &hdrs, b"");
            }
            Some(Ok((a, b))) => {
                cr = format!("bytes {a}-{b}/{len}");
                hdrs.push(("Content-Range", &cr));
                (206, a, b - a + 1)
            }
            None => (200, 0, len),
        };
        let head = self.head(status, ctype, n, &hdrs);
        self.w.write_all(head.as_bytes())?;
        if self.head_only || n == 0 {
            return Ok(());
        }
        file.seek(SeekFrom::Start(start))?;
        copy_exact(file, &mut self.w, n)
    }
}

#[cfg(target_os = "linux")]
fn copy_exact(file: &mut File, w: &mut TcpStream, n: u64) -> io::Result<()> {
    // std specializes File -> TcpStream copies into sendfile/splice on Linux.
    let sent = io::copy(&mut file.take(n), w)?;
    if sent != n {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file shorter than expected"));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn copy_exact(file: &mut File, w: &mut TcpStream, mut n: u64) -> io::Result<()> {
    let mut buf = vec![0u8; (256 * 1024).min(n as usize)];
    while n > 0 {
        let want = buf.len().min(n as usize);
        let got = file.read(&mut buf[..want])?;
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file shorter than expected"));
        }
        w.write_all(&buf[..got])?;
        n -= got as u64;
    }
    Ok(())
}

/// Parse a `Range` header against a resource of `len` bytes.
/// `None` = serve the whole thing (absent, multi-range or malformed: RFC 9110 lets us ignore it);
/// `Some(Err)` = unsatisfiable (416); `Some(Ok((first, last)))` = inclusive byte range (206).
pub fn parse_range(h: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let suffix: u64 = b.parse().ok()?;
        if suffix == 0 || len == 0 {
            return Some(Err(()));
        }
        return Some(Ok((len - suffix.min(len), len - 1)));
    }
    let first: u64 = a.parse().ok()?;
    let last: u64 = if b.is_empty() { u64::MAX } else { b.parse().ok()? };
    if last < first {
        return None;
    }
    if first >= len {
        return Some(Err(()));
    }
    Some(Ok((first, last.min(len - 1))))
}

/// Percent-decode; `plus` turns `+` into space (query strings). None on bad escapes or non-UTF-8.
pub fn pct_decode(s: &str, plus: bool) -> Option<String> {
    let b = s.as_bytes();
    if !b.iter().any(|&c| c == b'%' || (plus && c == b'+')) {
        return Some(s.to_string());
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = b.get(i + 1..i + 3)?;
                let hex = |c: u8| (c as char).to_digit(16);
                out.push((hex(h[0])? * 16 + hex(h[1])?) as u8);
                i += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode everything except unreserved characters (for Content-Disposition filenames).
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
            out.push(c as char);
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
    out
}

pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Status",
    }
}

/// Read one request head (through the blank line) into `buf`.
/// Ok(false) = the client closed cleanly between requests.
fn read_head(r: &mut BufReader<TcpStream>, buf: &mut Vec<u8>) -> io::Result<bool> {
    loop {
        let before = buf.len();
        let room = (MAX_HEAD - before) as u64;
        let n = r.by_ref().take(room).read_until(b'\n', buf)?;
        if n == 0 {
            if before == 0 {
                return Ok(false);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "head cut short"));
        }
        if buf.last() != Some(&b'\n') {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
        let line = &buf[before..];
        if line == b"\r\n" || line == b"\n" {
            if before == 0 {
                // Tolerate stray blank lines between keep-alive requests (RFC 9112 §2.2).
                buf.clear();
                continue;
            }
            return Ok(true);
        }
    }
}

/// Parse a request head. Err = (status, message) to send before closing.
pub fn parse_head(head: &[u8], peer: SocketAddr) -> Result<Request, (u16, &'static str)> {
    let text = std::str::from_utf8(head).map_err(|_| (400, "head is not utf-8"))?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let first = lines.next().ok_or((400, "empty request"))?;
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err((400, "bad request line"));
    };
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return Err((505, "http version not supported")),
    };
    if method.is_empty() || !method.bytes().all(|c| c.is_ascii_uppercase()) {
        return Err((400, "bad method"));
    }
    if !target.starts_with('/') {
        return Err((400, "bad target"));
    }
    let (raw_path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = pct_decode(raw_path, false).ok_or((400, "bad path encoding"))?;
    if path.contains('\0') || path.split('/').any(|seg| seg == "..") {
        return Err((400, "bad path"));
    }
    let mut headers = Vec::with_capacity(16);
    let mut body_len: Option<u64> = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err((400, "obsolete header folding"));
        }
        let (k, v) = line.split_once(':').ok_or((400, "bad header"))?;
        if k.is_empty() || k.bytes().any(|c| c <= b' ' || c >= 0x7f) {
            return Err((400, "bad header name"));
        }
        let k = k.to_ascii_lowercase();
        let v = v.trim().to_string();
        if k == "transfer-encoding" {
            return Err((501, "chunked request bodies are not supported"));
        }
        if k == "content-length" {
            let n: u64 = v.parse().map_err(|_| (400, "bad content-length"))?;
            if body_len.is_some_and(|m| m != n) {
                return Err((400, "conflicting content-length"));
            }
            body_len = Some(n);
        }
        if headers.len() == MAX_HEADERS {
            return Err((431, "too many headers"));
        }
        headers.push((k, v));
    }
    Ok(Request {
        method: method.to_string(),
        path,
        query: query.to_string(),
        headers,
        peer,
        body_len: body_len.unwrap_or(0),
        http10,
    })
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Accept connections forever, one thread each, at most `max_conns` at a time.
pub fn serve(listener: TcpListener, max_conns: usize, handler: Handler) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => {
                // Usually descriptor exhaustion: back off instead of spinning.
                thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        if active.load(Ordering::Relaxed) >= max_conns {
            drop(stream);
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let guard = Active(active.clone());
        let h = handler.clone();
        let spawned = thread::Builder::new().name("conn".into()).stack_size(256 * 1024).spawn(move || {
            let _guard = guard;
            let _ = handle_conn(stream, &h);
        });
        if spawned.is_err() {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn handle_conn(stream: TcpStream, h: &Handler) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(IDLE))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let peer = stream.peer_addr()?;
    let w = stream.try_clone()?;
    let mut conn = Conn {
        r: BufReader::with_capacity(16 * 1024, stream),
        w,
        body_left: 0,
        close: false,
        upgraded: false,
        head_only: false,
    };
    let mut head = Vec::with_capacity(1024);
    loop {
        head.clear();
        match read_head(&mut conn.r, &mut head) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => {
                if e.kind() == io::ErrorKind::InvalidData {
                    conn.close = true;
                    let _ = conn.send_text(431, "request head too large");
                }
                return Ok(());
            }
        }
        let req = match parse_head(&head, peer) {
            Ok(r) => r,
            Err((code, msg)) => {
                conn.close = true;
                let _ = conn.send_text(code, msg);
                return Ok(());
            }
        };
        conn.body_left = req.body_len;
        conn.close = req.wants_close();
        conn.head_only = req.method == "HEAD";
        if req.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
            conn.w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        }
        h(&req, &mut conn)?;
        if conn.upgraded || conn.close {
            return Ok(());
        }
        if conn.body_left > 0 {
            if conn.body_left > 1024 * 1024 {
                return Ok(()); // not worth draining; just close
            }
            conn.drain_body()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn peer() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 7), 5555))
    }

    #[test]
    fn parses_heads() {
        let r = parse_head(
            b"GET /f/a%20b/c.jpg?n=x+y&z=%C3%A9 HTTP/1.1\r\nHost: h\r\nCookie: a=1; esc=tok\r\nContent-Length: 5\r\n\r\n",
            peer(),
        )
        .unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/f/a b/c.jpg");
        assert_eq!(r.param("n").as_deref(), Some("x y"));
        assert_eq!(r.param("z").as_deref(), Some("é"));
        assert_eq!(r.param("q"), None);
        assert_eq!(r.cookie("esc"), Some("tok"));
        assert_eq!(r.header("host"), Some("h"));
        assert_eq!(r.body_len, 5);
        assert!(!r.wants_close());
        assert_eq!(r.client_ip(), peer().ip());
        assert!(!r.https());
    }

    #[test]
    fn rejects_bad_heads() {
        let cases: [&[u8]; 9] = [
            b"GET / HTTP/2.0\r\n\r\n",
            b"get / HTTP/1.1\r\n\r\n",
            b"GET x HTTP/1.1\r\n\r\n",
            b"GET /a/../b HTTP/1.1\r\n\r\n",
            b"GET /%zz HTTP/1.1\r\n\r\n",
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"GET / HTTP/1.1\r\n folded\r\n\r\n",
            b"GET  / HTTP/1.1\r\n\r\n",
        ];
        for c in cases {
            assert!(parse_head(c, peer()).is_err(), "{:?}", String::from_utf8_lossy(c));
        }
    }

    #[test]
    fn trusts_proxy_headers_only_from_loopback() {
        let head = b"GET / HTTP/1.1\r\nCF-Connecting-IP: 203.0.113.9\r\nX-Forwarded-Proto: https\r\n\r\n";
        let r = parse_head(head, peer()).unwrap();
        assert_eq!(r.client_ip(), peer().ip());
        assert!(!r.https());
        let lo = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1));
        let r = parse_head(head, lo).unwrap();
        assert_eq!(r.client_ip().to_string(), "203.0.113.9");
        assert!(r.https());
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some(Ok((0, 99))));
        assert_eq!(parse_range("bytes=900-", 1000), Some(Ok((900, 999))));
        assert_eq!(parse_range("bytes=900-5000", 1000), Some(Ok((900, 999))));
        assert_eq!(parse_range("bytes=-100", 1000), Some(Ok((900, 999))));
        assert_eq!(parse_range("bytes=-5000", 1000), Some(Ok((0, 999))));
        assert_eq!(parse_range("bytes=1000-", 1000), Some(Err(())));
        assert_eq!(parse_range("bytes=-0", 1000), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=0-", 0), Some(Err(())));
    }

    #[test]
    fn percent_coding() {
        assert_eq!(pct_decode("a%2Fb%20c", false).as_deref(), Some("a/b c"));
        assert_eq!(pct_decode("a+b", false).as_deref(), Some("a+b"));
        assert_eq!(pct_decode("%ff", false), None);
        assert_eq!(pct_decode("%4", false), None);
        assert_eq!(pct_encode("ação 1.jpg"), "a%C3%A7%C3%A3o%201.jpg");
    }

    #[test]
    fn serves_keepalive_and_ranges_over_a_socket() {
        let dir = std::env::temp_dir().join(format!("esc-http-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fpath = dir.join("f.bin");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&fpath, &data).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let fp = fpath.clone();
        let handler: Handler = Arc::new(move |req, conn| {
            if req.path == "/echo" {
                let body = conn.read_body(1024)?;
                conn.send(200, "text/plain", &[], &body)
            } else {
                let mut f = File::open(&fp)?;
                let len = f.metadata()?.len();
                conn.send_file(req, &mut f, len, "application/octet-stream", Some("\"e1\""), &[])
            }
        });
        thread::spawn(move || serve(listener, 16, handler));

        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Two pipelined keep-alive requests on one connection.
        s.write_all(b"POST /echo HTTP/1.1\r\nContent-Length: 5\r\n\r\nhelloGET /f HTTP/1.1\r\nRange: bytes=9990-\r\n\r\n")
            .unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let resp1 = read_response(&mut r);
        assert!(resp1.0.starts_with("HTTP/1.1 200"));
        assert_eq!(resp1.1, b"hello");
        let resp2 = read_response(&mut r);
        assert!(resp2.0.starts_with("HTTP/1.1 206"), "{}", resp2.0);
        assert!(resp2.0.contains("Content-Range: bytes 9990-9999/10000"));
        assert_eq!(resp2.1, &data[9990..]);
        // Unsatisfiable range, then If-Range mismatch serves the whole file.
        s.write_all(b"GET /f HTTP/1.1\r\nRange: bytes=20000-\r\n\r\n").unwrap();
        let resp3 = read_response(&mut r);
        assert!(resp3.0.starts_with("HTTP/1.1 416"));
        s.write_all(b"GET /f HTTP/1.1\r\nRange: bytes=0-1\r\nIf-Range: \"other\"\r\nConnection: close\r\n\r\n").unwrap();
        let resp4 = read_response(&mut r);
        assert!(resp4.0.starts_with("HTTP/1.1 200"));
        assert_eq!(resp4.1.len(), 10_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_response(r: &mut BufReader<TcpStream>) -> (String, Vec<u8>) {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).unwrap();
        (head, body)
    }
}
