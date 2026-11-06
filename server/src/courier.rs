//! The Courier: outbound connections, std only: HTTP/1.1 client (plain `http://` over `TcpStream`, `https://` through the
//! OS `curl`) and SMTP mail through `curl` (credentials only ever in a private temp config file).

use crate::crypto::{b64_encode, hex, random_bytes, random_u64};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Longest accepted response line (status or header).
const MAX_LINE: usize = 16 * 1024;
/// Most header lines accepted in one response head.
const MAX_HEADERS: usize = 200;

/// An HTTP response: status, lowercase header names, and a lazily streamed body.
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl std::fmt::Debug for Resp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resp").field("status", &self.status).field("headers", &self.headers).finish_non_exhaustive()
    }
}

fn invalid_input(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why.into())
}

fn invalid_data(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn has_ctl(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7F)
}

/// Rejects anything that could split or smuggle a request line, header or URL.
fn validate_request(method: &str, url: &str, headers: &[(&str, &str)]) -> io::Result<()> {
    if !is_token(method) {
        return Err(invalid_input("invalid HTTP method"));
    }
    if has_ctl(url) || url.contains(' ') {
        return Err(invalid_input("invalid URL"));
    }
    for (k, v) in headers {
        if !is_token(k) || v.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(invalid_input(format!("invalid header {k:?}")));
        }
    }
    Ok(())
}

/// Performs an HTTP request. `http://` uses a plain socket (HTTP/1.1, `Connection: close`); `https://` uses the
/// OS `curl`. Redirects are never followed. `timeout` is the connect timeout (and the read/write timeout for plain
/// sockets); there is no overall limit, so streaming bodies can be long-lived.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> io::Result<Resp> {
    validate_request(method, url, headers)?;
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") {
        request_plain(method, url, headers, body, timeout)
    } else if lower.starts_with("https://") {
        request_curl(method, url, headers, body, timeout)
    } else {
        Err(invalid_input("URL must start with http:// or https://"))
    }
}

// ---------------------------------------------------------------------------------------------
// Response head / body parsing (shared by the plain and curl paths)
// ---------------------------------------------------------------------------------------------

/// Reads one CRLF/LF-terminated line (without the terminator), bounded by `MAX_LINE`.
fn read_line<R: BufRead>(r: &mut R) -> io::Result<String> {
    let mut line = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Err(if n >= MAX_LINE { invalid_data("response line too long") } else { io::ErrorKind::UnexpectedEof.into() });
    }
    while matches!(line.last(), Some(b'\r' | b'\n')) {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// Reads the status line and headers of the final response, skipping interim 1xx responses.
fn read_head<R: BufRead>(r: &mut R) -> io::Result<(u16, Vec<(String, String)>)> {
    loop {
        let line = read_line(r)?;
        let mut parts = line.split_whitespace();
        let status = match (parts.next(), parts.next().map(str::parse::<u16>)) {
            (Some(v), Some(Ok(s))) if v.starts_with("HTTP/1.") && (100..=999).contains(&s) => s,
            _ => return Err(invalid_data(format!("bad status line: {line:?}"))),
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let line = read_line(r)?;
            if line.is_empty() {
                break;
            }
            if headers.len() >= MAX_HEADERS {
                return Err(invalid_data("too many response headers"));
            }
            if line.starts_with([' ', '\t']) {
                if let Some(last) = headers.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(line.trim());
                }
            } else if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
            } else {
                return Err(invalid_data(format!("bad header line: {line:?}")));
            }
        }
        if status >= 200 {
            return Ok((status, headers));
        }
    }
}

enum Mode {
    Empty,
    Length(u64),
    Chunked(Chunk),
    Eof,
}

enum Chunk {
    Size,
    Data(u64),
    Done,
}

/// Response body reader for the plain-socket path.
struct HttpBody<R> {
    r: R,
    mode: Mode,
}

impl<R: BufRead> Read for HttpBody<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match &mut self.mode {
            Mode::Empty => Ok(0),
            Mode::Eof => self.r.read(buf),
            Mode::Length(rem) => {
                if *rem == 0 {
                    return Ok(0);
                }
                let max = (*rem).min(buf.len() as u64) as usize;
                let n = self.r.read(&mut buf[..max])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                *rem -= n as u64;
                Ok(n)
            }
            Mode::Chunked(state) => loop {
                match *state {
                    Chunk::Done => return Ok(0),
                    Chunk::Size => {
                        let line = read_line(&mut self.r)?;
                        let size = line.split(';').next().unwrap_or("").trim();
                        if size.is_empty() || size.len() > 16 || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Err(invalid_data(format!("bad chunk size {size:?}")));
                        }
                        let n = u64::from_str_radix(size, 16).map_err(|_| invalid_data("bad chunk size"))?;
                        if n == 0 {
                            while !read_line(&mut self.r)?.is_empty() {} // trailers
                            *state = Chunk::Done;
                        } else {
                            *state = Chunk::Data(n);
                        }
                    }
                    Chunk::Data(rem) => {
                        let max = rem.min(buf.len() as u64) as usize;
                        let n = self.r.read(&mut buf[..max])?;
                        if n == 0 {
                            return Err(io::ErrorKind::UnexpectedEof.into());
                        }
                        let rem = rem - n as u64;
                        *state = if rem == 0 {
                            if !read_line(&mut self.r)?.is_empty() {
                                return Err(invalid_data("missing CRLF after chunk"));
                            }
                            Chunk::Size
                        } else {
                            Chunk::Data(rem)
                        };
                        return Ok(n);
                    }
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Plain http://
// ---------------------------------------------------------------------------------------------

/// Splits `http://host[:port]/path` into (host, port, path-and-query, Host header value).
fn parse_http_url(url: &str) -> io::Result<(&str, u16, String, String)> {
    let rest = &url["http://".len()..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err(invalid_input("userinfo in URL is not supported"));
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or_else(|| invalid_input("bad IPv6 literal"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return Err(invalid_input("bad URL authority")),
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(p) => p.parse::<u16>().map_err(|_| invalid_input("bad port"))?,
        None => 80,
    };
    if host.is_empty() {
        return Err(invalid_input("empty host"));
    }
    let tail = tail.split('#').next().unwrap_or("");
    let path = match tail {
        "" => "/".to_owned(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_owned(),
    };
    let shown = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
    let host_header = if port == 80 { shown } else { format!("{shown}:{port}") };
    Ok((host, port, path, host_header))
}

fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let timeout = timeout.max(Duration::from_millis(1));
    let mut last = io::Error::new(io::ErrorKind::NotFound, "host resolved to no addresses");
    for addr in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn request_plain(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> io::Result<Resp> {
    let (host, port, path, host_header) = parse_http_url(url)?;
    let mut stream = connect(host, port, timeout)?;
    let t = Some(timeout.max(Duration::from_millis(1)));
    stream.set_read_timeout(t)?;
    stream.set_write_timeout(t)?;
    stream.set_nodelay(true)?;

    let mut head = format!("{method} {path} HTTP/1.1\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        head.push_str(&format!("Host: {host_header}\r\n"));
    }
    head.push_str("Connection: close\r\n");
    for (k, v) in headers {
        if !["connection", "content-length", "transfer-encoding"].iter().any(|n| k.eq_ignore_ascii_case(n)) {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    match &body {
        Some(b) => head.push_str(&format!("Content-Length: {}\r\n", b.len())),
        None if matches!(method, "POST" | "PUT" | "PATCH") => head.push_str("Content-Length: 0\r\n"),
        None => {}
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(b) = &body {
        stream.write_all(b)?;
    }
    stream.flush()?;

    let mut r = BufReader::with_capacity(64 * 1024, stream);
    let (status, headers) = read_head(&mut r)?;
    let find = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let mode = if method == "HEAD" || status == 204 || status == 304 {
        Mode::Empty
    } else if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        Mode::Chunked(Chunk::Size)
    } else if let Some(v) = find("content-length") {
        Mode::Length(v.parse().map_err(|_| invalid_data("bad Content-Length"))?)
    } else {
        Mode::Eof
    };
    Ok(Resp { status, headers, body: Box::new(HttpBody { r, mode }) })
}

// ---------------------------------------------------------------------------------------------
// curl
// ---------------------------------------------------------------------------------------------

/// The curl executable: `%SystemRoot%\System32\curl.exe` on Windows if present, else `curl` on PATH.
fn curl_exe() -> OsString {
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        let p = PathBuf::from(root).join("System32").join("curl.exe");
        if p.is_file() {
            return p.into_os_string();
        }
    }
    OsString::from("curl")
}

fn curl_command() -> Command {
    let cmd = Command::new(curl_exe());
    #[cfg(windows)]
    let cmd = {
        let mut cmd = cmd;
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
        cmd
    };
    cmd
}

/// Kills and reaps the curl child when dropped.
struct CurlProc(Child);

impl CurlProc {
    fn stderr_text(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut e) = self.0.stderr.take() {
            let mut raw = Vec::new();
            let _ = e.by_ref().take(64 * 1024).read_to_end(&mut raw);
            text = String::from_utf8_lossy(&raw).trim().to_owned();
        }
        text
    }
}

impl Drop for CurlProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Body of a curl response: curl's remaining stdout. At EOF a failing curl exit becomes an error,
/// so a connection cut mid-body is not mistaken for a complete body.
struct CurlBody {
    r: BufReader<std::process::ChildStdout>,
    proc: CurlProc,
    finished: bool,
}

impl Read for CurlBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.r.read(buf)?;
        if n == 0 && !buf.is_empty() && !self.finished {
            self.finished = true;
            let status = self.proc.0.wait()?;
            if !status.success() {
                return Err(io::Error::other(format!("curl failed ({status}): {}", self.proc.stderr_text())));
            }
        }
        Ok(n)
    }
}

/// Runs the request through curl (used for `https://`; also callable for `http://` in tests).
///
/// Known limitation: curl's `-D -` also prints HTTP trailers (chunked responses that carry them) after the body,
/// so such trailers would appear as extra bytes at the end of the body stream. Trailers are very rare in practice.
fn request_curl(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> io::Result<Resp> {
    validate_request(method, url, headers)?;
    let mut cmd = curl_command();
    cmd.args(["-sS", "-N", "--http1.1", "-X", method, "-D", "-", "--connect-timeout"]);
    cmd.arg(timeout.as_secs_f64().ceil().max(1.0).to_string());
    for (k, v) in headers {
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    if body.is_some() {
        cmd.args(["--data-binary", "@-"]);
    }
    cmd.arg("--url").arg(url);
    cmd.stdin(if body.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut proc = CurlProc(cmd.spawn()?);
    if let (Some(data), Some(mut stdin)) = (body, proc.0.stdin.take()) {
        // A thread, so a large body cannot deadlock against curl's output; errors surface via curl's exit.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let stdout = proc.0.stdout.take().ok_or_else(|| io::Error::other("curl stdout missing"))?;
    let mut r = BufReader::with_capacity(64 * 1024, stdout);
    match read_head(&mut r) {
        Ok((status, headers)) => Ok(Resp { status, headers, body: Box::new(CurlBody { r, proc, finished: false }) }),
        Err(e) => {
            let _ = proc.0.wait();
            let text = proc.stderr_text();
            if text.is_empty() { Err(e) } else { Err(io::Error::other(format!("curl: {text}"))) }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Mail
// ---------------------------------------------------------------------------------------------

/// SMTP account and server settings.
pub struct Mail<'a> {
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
    pub pass: &'a str,
    pub from: &'a str,
    /// `"ssl"` (implicit TLS) or `"starttls"`.
    pub tls: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Security {
    Ssl,
    StartTls,
    /// Unencrypted; only the tests (against a local fake server) use it.
    #[cfg_attr(not(test), allow(dead_code))]
    Plain,
}

/// A curl config file with restricted permissions, deleted on drop.
struct TempConfig(PathBuf);

impl TempConfig {
    fn create(contents: &str) -> io::Result<TempConfig> {
        loop {
            let path = std::env::temp_dir().join(format!("esc-curl-{:016x}.cfg", random_u64()));
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            match opts.open(&path) {
                Ok(mut f) => {
                    let cfg = TempConfig(path); // from here on, drop deletes it
                    f.write_all(contents.as_bytes())?;
                    f.flush()?;
                    return Ok(cfg);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Quotes a value for a curl config file (double quotes with backslash escapes).
fn curl_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The bare address of `Name <addr>` or `addr`.
fn bare_address(s: &str) -> &str {
    match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => s[a + 1..b].trim(),
        _ => s.trim(),
    }
}

fn no_line_breaks(what: &str, s: &str) -> io::Result<()> {
    if s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        Err(invalid_input(format!("{what} must not contain line breaks")))
    } else {
        Ok(())
    }
}

/// Sends a UTF-8 plain-text email through curl SMTP (`ssl` = smtps://, `starttls` = smtp:// with `--ssl-reqd`).
/// Credentials go through a private temp config file, never the command line.
pub fn send_mail(m: &Mail, to: &str, subject: &str, body: &str) -> io::Result<()> {
    let sec = match m.tls {
        "ssl" => Security::Ssl,
        "starttls" => Security::StartTls,
        other => return Err(invalid_input(format!("unknown tls mode {other:?} (use \"ssl\" or \"starttls\")"))),
    };
    send_mail_with(m, to, subject, body, sec)
}

fn send_mail_with(m: &Mail, to: &str, subject: &str, body: &str, sec: Security) -> io::Result<()> {
    no_line_breaks("from", m.from)?;
    no_line_breaks("to", to)?;
    no_line_breaks("subject", subject)?;
    if m.host.is_empty() || !m.host.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.:_[]".contains(&b)) {
        return Err(invalid_input("invalid SMTP host"));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let message = build_message(m.from, to, subject, body, now);
    let cfg = if m.user.is_empty() {
        None
    } else {
        Some(TempConfig::create(&format!("user = {}\n", curl_quote(&format!("{}:{}", m.user, m.pass))))?)
    };
    let scheme = if sec == Security::Ssl { "smtps" } else { "smtp" };
    let mut cmd = curl_command();
    cmd.args(["-sS", "--connect-timeout", "30", "--max-time", "300"]);
    cmd.arg("--url").arg(format!("{scheme}://{}:{}", m.host, m.port));
    if sec == Security::StartTls {
        cmd.arg("--ssl-reqd");
    }
    cmd.arg("--mail-from").arg(bare_address(m.from));
    cmd.arg("--mail-rcpt").arg(bare_address(to));
    cmd.args(["--upload-file", "-"]);
    if let Some(c) = &cfg {
        cmd.arg("-K").arg(&c.0);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| io::Error::other("curl stdin missing"))?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(message.as_bytes()); // a failure shows up as curl's own error
    });
    let out = child.wait_with_output()?;
    let _ = writer.join();
    drop(cfg);
    if out.status.success() {
        Ok(())
    } else {
        let text = String::from_utf8_lossy(&out.stderr);
        Err(io::Error::other(format!("curl smtp failed ({}): {}", out.status, text.trim())))
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// RFC 5322 date, always UTC (`Tue, 14 Nov 2023 22:13:20 +0000`).
fn rfc5322_date(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{}, {d:02} {} {y:04} {:02}:{:02}:{:02} +0000",
        DAYS[((days + 4).rem_euclid(7)) as usize],
        MONTHS[m as usize - 1],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// RFC 2047 encoding of a header value when it is not plain ASCII (UTF-8 base64 encoded words of at most 75 chars).
fn encode_header_text(s: &str) -> String {
    if s.is_ascii() {
        return s.to_owned();
    }
    let mut words = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + 45).min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        words.push(format!("=?UTF-8?B?{}?=", b64_encode(&s.as_bytes()[start..end])));
        start = end;
    }
    words.join("\r\n ")
}

/// Replaces line breaks inside a header value so it can never start a new header.
fn single_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

/// Builds the RFC 5322 message (CRLF line endings) for `send_mail`; `now_secs` is the Unix time for the Date header.
pub fn build_message(from: &str, to: &str, subject: &str, body: &str, now_secs: u64) -> String {
    let mut id = [0u8; 16];
    random_bytes(&mut id);
    build_message_with_id(from, to, subject, body, now_secs, &hex(&id))
}

fn build_message_with_id(from: &str, to: &str, subject: &str, body: &str, now_secs: u64, id: &str) -> String {
    let from = single_line(from);
    let domain = bare_address(&from).rsplit_once('@').map_or("localhost", |(_, d)| d);
    let mut msg = format!(
        "From: {from}\r\nTo: {}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <{id}@{domain}>\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n",
        single_line(to),
        encode_header_text(&single_line(subject)),
        rfc5322_date(now_secs),
    );
    // Normalise line endings to CRLF; curl dot-stuffs lines starting with '.' itself.
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                msg.push_str("\r\n");
            }
            '\n' => msg.push_str("\r\n"),
            c => msg.push(c),
        }
    }
    if !msg.ends_with("\r\n") {
        msg.push_str("\r\n");
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    const T: Duration = Duration::from_secs(10);

    struct Req {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Req {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        }
    }

    fn read_req(s: &TcpStream) -> Option<Req> {
        let mut r = BufReader::new(s.try_clone().ok()?);
        let line = read_line(&mut r).ok()?;
        let mut it = line.split(' ');
        let (method, path) = (it.next()?.to_owned(), it.next()?.to_owned());
        let mut headers = Vec::new();
        loop {
            let l = read_line(&mut r).ok()?;
            if l.is_empty() {
                break;
            }
            let (k, v) = l.split_once(':')?;
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
        let mut req = Req { method, path, headers, body: Vec::new() };
        if req.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
            (&*s).write_all(b"HTTP/1.1 100 Continue\r\n\r\n").ok()?;
        }
        let len: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        req.body = vec![0; len];
        r.read_exact(&mut req.body).ok()?;
        Some(req)
    }

    /// Serves each connection on its own thread with `handler`; returns the port.
    fn spawn_server(handler: impl Fn(Req, TcpStream) + Send + Sync + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    if let Some(req) = read_req(&conn) {
                        let w = conn.try_clone().unwrap();
                        handler(req, w);
                        // Close politely: send FIN, then wait for the client to finish reading and hang up.
                        // Dropping the socket outright can make Windows answer with RST and destroy
                        // response bytes still in flight.
                        let _ = conn.shutdown(std::net::Shutdown::Write);
                        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut sink = [0u8; 1024];
                        while matches!((&conn).read(&mut sink), Ok(n) if n > 0) {}
                    }
                });
            }
        });
        port
    }

    fn http_app(go: Arc<Mutex<mpsc::Receiver<()>>>) -> u16 {
        spawn_server(move |req, mut s| {
            let out: Vec<u8> = match req.path.as_str() {
                "/len" => b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nX-Custom-Header: Some Value\r\nContent-Type: text/plain\r\n\r\nhello world".to_vec(),
                "/chunked" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n6;a=b;c\r\n world\r\nB\r\n,0123456789\r\n0\r\n\r\n".to_vec(),
                "/eof" => b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nuntil close".to_vec(),
                "/continue" => b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
                "/head" => b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_vec(),
                "/echo" => {
                    let mut o = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Method: {}\r\nX-Seen-Foo: {}\r\n\r\n",
                        req.body.len(),
                        req.method,
                        req.header("x-foo").unwrap_or("-")
                    )
                    .into_bytes();
                    o.extend_from_slice(&req.body);
                    o
                }
                "/auth" => {
                    let a = req.header("authorization").unwrap_or("none").to_owned();
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{a}", a.len()).into_bytes()
                }
                "/stream" => {
                    s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\npart1\r\n").unwrap();
                    s.flush().unwrap();
                    go.lock().unwrap().recv().unwrap();
                    b"5\r\npart2\r\n0\r\n\r\n".to_vec()
                }
                "/cut" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n".to_vec(),
                _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\nnope".to_vec(),
            };
            s.write_all(&out).unwrap();
        })
    }

    type Fetch = fn(&str, &str, &[(&str, &str)], Option<Vec<u8>>, Duration) -> io::Result<Resp>;

    fn read_all(mut r: Resp) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut body = Vec::new();
        r.body.read_to_end(&mut body).unwrap();
        (r.status, r.headers, body)
    }

    fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
        h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn curl_available() -> bool {
        curl_command().arg("--version").stdout(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// Runs the full scenario set through one implementation.
    fn exercise(fetch: Fetch) {
        let (go_tx, go_rx) = mpsc::channel();
        let port = http_app(Arc::new(Mutex::new(go_rx)));
        let url = |p: &str| format!("http://127.0.0.1:{port}{p}");

        let (st, h, body) = read_all(fetch("GET", &url("/len"), &[], None, T).unwrap());
        assert_eq!((st, &body[..]), (200, &b"hello world"[..]));
        assert_eq!(header(&h, "x-custom-header"), Some("Some Value"), "lowercase names, trimmed values");
        assert_eq!(header(&h, "content-type"), Some("text/plain"));
        assert!(h.iter().all(|(k, _)| *k == k.to_ascii_lowercase()));

        let (st, _, body) = read_all(fetch("GET", &url("/chunked"), &[], None, T).unwrap());
        assert_eq!((st, &body[..]), (200, &b"hello world,0123456789"[..]));

        let (st, _, body) = read_all(fetch("GET", &url("/eof"), &[], None, T).unwrap());
        assert_eq!((st, &body[..]), (200, &b"until close"[..]));

        let (st, h, body) = read_all(fetch("GET", &url("/continue"), &[], None, T).unwrap());
        assert_eq!((st, &body[..]), (200, &b"ok"[..]), "interim 100 skipped");
        assert_eq!(header(&h, "content-length"), Some("2"));

        let (st, h, body) = read_all(fetch("POST", &url("/echo"), &[("X-Foo", "bar baz")], Some(b"payload \x00\xff bytes".to_vec()), T).unwrap());
        assert_eq!((st, &body[..]), (200, &b"payload \x00\xff bytes"[..]));
        assert_eq!(header(&h, "x-method"), Some("POST"));
        assert_eq!(header(&h, "x-seen-foo"), Some("bar baz"));

        let (st, _, body) = read_all(fetch("PUT", &url("/echo"), &[], Some(Vec::new()), T).unwrap());
        assert_eq!((st, body.len()), (200, 0));

        let (st, _, body) = read_all(fetch("GET", &url("/nothing/here?x=1"), &[], None, T).unwrap());
        assert_eq!((st, &body[..]), (404, &b"nope"[..]));

        // Large request and response bodies (exercises the stdin writer thread and Expect: 100-continue).
        let big: Vec<u8> = (0..3 << 20).map(|i: usize| (i * 7 % 253) as u8).collect();
        let (st, _, body) = read_all(fetch("POST", &url("/echo"), &[], Some(big.clone()), T).unwrap());
        assert_eq!(st, 200);
        assert!(body == big, "3 MiB echo differs");

        // Lazy streaming: the first part is readable while the server still holds back the rest.
        let mut resp = fetch("GET", &url("/stream"), &[], None, T).unwrap();
        let mut first = [0u8; 5];
        resp.body.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"part1");
        go_tx.send(()).unwrap();
        let mut rest = Vec::new();
        resp.body.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"part2");

        // A body cut off mid-chunk is an error, not a clean EOF.
        let resp = fetch("GET", &url("/cut"), &[], None, T).unwrap();
        let mut r = resp.body;
        let mut sink = Vec::new();
        assert!(r.read_to_end(&mut sink).is_err(), "truncated chunked body must error");
    }

    #[test]
    fn http_plain() {
        exercise(request);
    }

    #[test]
    fn http_plain_head_and_connect_errors() {
        let port = http_app(Arc::new(Mutex::new(mpsc::channel().1)));
        let r = request("HEAD", &format!("http://127.0.0.1:{port}/head"), &[], None, T).unwrap();
        assert_eq!(header(&r.headers, "content-length"), Some("5"));
        let (_, _, body) = read_all(r);
        assert!(body.is_empty(), "HEAD has no body");
        // Closed port: connection refused quickly.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(request("GET", &format!("http://127.0.0.1:{dead}/"), &[], None, T).is_err());
        // Read timeout on a silent server.
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = silent.local_addr().unwrap().port();
        let started = std::time::Instant::now();
        let e = request("GET", &format!("http://127.0.0.1:{p}/"), &[], None, Duration::from_millis(300)).unwrap_err();
        assert!(matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{e:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn http_via_curl() {
        if !curl_available() {
            println!("note: curl not found; skipping http_via_curl");
            return;
        }
        exercise(request_curl);
    }

    #[test]
    fn curl_failure_carries_stderr() {
        if !curl_available() {
            return;
        }
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let e = request_curl("GET", &format!("http://127.0.0.1:{dead}/"), &[], None, T).unwrap_err();
        assert!(e.to_string().starts_with("curl: "), "{e}");
        assert!(e.to_string().to_lowercase().contains("fail") || e.to_string().to_lowercase().contains("connect"), "{e}");
    }

    #[test]
    fn request_rejects_bad_input() {
        let kind = |r: io::Result<Resp>| r.unwrap_err().kind();
        let bad = io::ErrorKind::InvalidInput;
        assert_eq!(kind(request("GET", "ftp://example.com/", &[], None, T)), bad);
        assert_eq!(kind(request("GET", "example.com", &[], None, T)), bad);
        assert_eq!(kind(request("GE T", "http://127.0.0.1:1/", &[], None, T)), bad);
        assert_eq!(kind(request("GET", "http://127.0.0.1:1/ x", &[], None, T)), bad);
        assert_eq!(kind(request("GET", "http://127.0.0.1:1/\r\nX: y", &[], None, T)), bad);
        assert_eq!(kind(request("GET", "http://127.0.0.1:1/", &[("X-A", "b\r\nEvil: 1")], None, T)), bad);
        assert_eq!(kind(request("GET", "http://127.0.0.1:1/", &[("X A", "b")], None, T)), bad);
        assert_eq!(kind(request("GET", "http://user@127.0.0.1:1/", &[], None, T)), bad);
        assert_eq!(kind(request("GET", "http://:80/", &[], None, T)), bad);
        assert_eq!(kind(request("GET", "http://h:99999/", &[], None, T)), bad);
    }

    #[test]
    fn url_parsing() {
        let p = |u: &str| parse_http_url(u).map(|(h, port, path, hh)| (h.to_owned(), port, path, hh)).unwrap();
        assert_eq!(p("http://example.com"), ("example.com".into(), 80, "/".into(), "example.com".into()));
        assert_eq!(p("http://example.com:8080/a/b?c=d#frag"), ("example.com".into(), 8080, "/a/b?c=d".into(), "example.com:8080".into()));
        assert_eq!(p("http://example.com?x=1"), ("example.com".into(), 80, "/?x=1".into(), "example.com".into()));
        assert_eq!(p("http://[::1]:9000/z"), ("::1".into(), 9000, "/z".into(), "[::1]:9000".into()));
        assert_eq!(p("http://[::1]/"), ("::1".into(), 80, "/".into(), "[::1]".into()));
    }

    #[test]
    fn chunked_decoder_edge_cases() {
        let read = |raw: &[u8]| -> io::Result<Vec<u8>> {
            let mut b = HttpBody { r: BufReader::new(raw), mode: Mode::Chunked(Chunk::Size) };
            let mut out = Vec::new();
            b.read_to_end(&mut out).map(|_| out)
        };
        assert_eq!(read(b"0\r\n\r\n").unwrap(), b"");
        assert_eq!(read(b"3\r\nabc\r\n0\r\n\r\n").unwrap(), b"abc");
        assert_eq!(read(b"1\r\na\r\n1\r\nb\r\n0\r\nT: v\r\nU: w\r\n\r\n").unwrap(), b"ab");
        assert_eq!(read(b"a\r\n0123456789\r\n0\r\n\r\n").unwrap(), b"0123456789");
        assert_eq!(read(b"zz\r\nabc\r\n").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read(b"\r\n").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read(b"+3\r\nabc\r\n0\r\n\r\n").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read(b"3\r\nabcXY0\r\n\r\n").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read(b"5\r\nab").unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read(b"3\r\nabc\r\n").unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn content_length_truncation_is_an_error() {
        let mut b = HttpBody { r: BufReader::new(&b"abc"[..]), mode: Mode::Length(10) };
        let mut out = Vec::new();
        assert_eq!(b.read_to_end(&mut out).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn head_parsing() {
        let mut r = BufReader::new(&b"HTTP/1.1 100 Continue\r\nX: 1\r\n\r\nHTTP/1.1 201 Created\r\nLocation: /x\r\nFOLDED: a\r\n  b\r\n\r\nbody"[..]);
        let (st, h) = read_head(&mut r).unwrap();
        assert_eq!(st, 201);
        assert_eq!(h, vec![("location".to_owned(), "/x".to_owned()), ("folded".to_owned(), "a b".to_owned())]);
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "body");
        for bad in ["", "garbage\r\n\r\n", "HTTP/1.1 abc OK\r\n\r\n", "HTTP/2 200\r\n\r\n", "HTTP/1.1 200 OK\r\nno colon here\r\n\r\n"] {
            assert!(read_head(&mut BufReader::new(bad.as_bytes())).is_err(), "{bad:?}");
        }
    }

    // ---- mail ----

    #[test]
    fn date_formatting() {
        assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(rfc5322_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 +0000");
        assert_eq!(rfc5322_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 +0000");
        assert_eq!(rfc5322_date(951_868_799), "Tue, 29 Feb 2000 23:59:59 +0000");
        assert_eq!(rfc5322_date(951_868_800), "Wed, 01 Mar 2000 00:00:00 +0000");
        assert_eq!(rfc5322_date(2_147_483_647), "Tue, 19 Jan 2038 03:14:07 +0000");
        assert_eq!(rfc5322_date(4_102_444_800), "Fri, 01 Jan 2100 00:00:00 +0000");
        assert_eq!(rfc5322_date(1_078_012_800), "Sun, 29 Feb 2004 00:00:00 +0000");
        assert_eq!(rfc5322_date(1_078_099_200), "Mon, 01 Mar 2004 00:00:00 +0000");
    }

    #[test]
    fn message_golden() {
        let m = build_message_with_id(
            "ESC Relay <relay@example.org>",
            "alice@example.com",
            "Your code",
            "Hello,\nyour code is 123456.\r\n\r\nBye",
            1_700_000_000,
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            m,
            "From: ESC Relay <relay@example.org>\r\n\
             To: alice@example.com\r\n\
             Subject: Your code\r\n\
             Date: Tue, 14 Nov 2023 22:13:20 +0000\r\n\
             Message-ID: <0123456789abcdef0123456789abcdef@example.org>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Transfer-Encoding: 8bit\r\n\
             \r\n\
             Hello,\r\nyour code is 123456.\r\n\r\nBye\r\n"
        );
        // Every line ends in CRLF, no bare LF or CR.
        assert!(!m.replace("\r\n", "").contains(['\r', '\n']));
    }

    #[test]
    fn message_non_ascii_subject_and_defaults() {
        let m = build_message_with_id("relay@host.test", "bob@example.com", "Grüße aus Köln ✓", "tschüss", 0, "ab");
        assert!(m.contains("Subject: =?UTF-8?B?R3LDvMOfZSBhdXMgS8O2bG4g4pyT?=\r\n"), "{m}");
        assert!(m.contains("From: relay@host.test\r\n"));
        assert!(m.contains("Message-ID: <ab@host.test>\r\n"));
        assert!(m.ends_with("\r\n\r\ntschüss\r\n"));
        // Long non-ASCII subjects fold into several encoded words, each at most 75 characters, never splitting a char.
        let long = "é".repeat(100) + &"日本語".repeat(30);
        let m = build_message_with_id("a@b.c", "d@e.f", &long, "x", 0, "id");
        let subject: String = m.split("\r\n\r\n").next().unwrap().split("Subject: ").nth(1).unwrap().split("\r\nDate:").next().unwrap().to_owned();
        let words: Vec<&str> = subject.split("\r\n ").collect();
        assert!(words.len() > 5);
        let mut decoded = Vec::new();
        for w in &words {
            assert!(w.len() <= 75, "{w}");
            let inner = w.strip_prefix("=?UTF-8?B?").unwrap().strip_suffix("?=").unwrap();
            let std_b64 = inner.replace('+', "-").replace('/', "_").trim_end_matches('=').to_owned();
            decoded.extend(crate::crypto::b64url_decode(&std_b64).unwrap());
        }
        assert_eq!(String::from_utf8(decoded).unwrap(), long);
        // build_message (random id) has a well-formed Message-ID.
        let m = build_message("Me <me@x.org>", "you@y.org", "s", "b", 5);
        let line = m.lines().find(|l| l.starts_with("Message-ID: <")).unwrap();
        let id = line.strip_prefix("Message-ID: <").unwrap().strip_suffix("@x.org>").unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(build_message("a@b", "c@d", "s", "b", 5), build_message("a@b", "c@d", "s", "b", 5));
    }

    #[test]
    fn message_never_has_header_injection() {
        let m = build_message_with_id("a@b.c\r\nBcc: evil@x", "d@e.f\nX: y", "s\r\nBcc: evil@x", "body", 0, "id");
        let head = m.split("\r\n\r\n").next().unwrap();
        assert!(head.lines().all(|l| !l.starts_with("Bcc:") && !l.starts_with("X:")), "{head}");
    }

    #[test]
    fn send_mail_refuses_line_breaks_and_bad_settings() {
        let mail = |from, tls| Mail { host: "smtp.example.org", port: 465, user: "u", pass: "p", from, tls };
        let bad = io::ErrorKind::InvalidInput;
        for from in ["a@b.c\r\nBcc: x@y", "a@b.c\nBcc: x@y", "a@b.c\rx"] {
            assert_eq!(send_mail(&mail(from, "ssl"), "to@x.org", "s", "b").unwrap_err().kind(), bad, "from {from:?}");
        }
        for to in ["x@y.z\r\nBcc: q@r", "x@y.z\n", "\rx@y.z"] {
            assert_eq!(send_mail(&mail("a@b.c", "ssl"), to, "s", "b").unwrap_err().kind(), bad, "to {to:?}");
        }
        for subject in ["s\r\nBcc: q@r", "s\n", "\r"] {
            assert_eq!(send_mail(&mail("a@b.c", "starttls"), "x@y.z", subject, "b").unwrap_err().kind(), bad, "subject {subject:?}");
        }
        assert_eq!(send_mail(&mail("a@b.c", "plain"), "x@y.z", "s", "b").unwrap_err().kind(), bad);
        let weird_host = Mail { host: "evil.org --upload-file x", ..mail("a@b.c", "ssl") };
        assert_eq!(send_mail(&weird_host, "x@y.z", "s", "b").unwrap_err().kind(), bad);
        // A body with line breaks is of course fine (it fails later, on the network, not here).
    }

    #[test]
    fn config_quoting_and_addresses() {
        assert_eq!(curl_quote("plain"), "\"plain\"");
        assert_eq!(curl_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(curl_quote("l1\nl2\r\t\x0b"), "\"l1\\nl2\\r\\t\\v\"");
        assert_eq!(bare_address("Name <a@b.c>"), "a@b.c");
        assert_eq!(bare_address("  a@b.c "), "a@b.c");
        assert_eq!(bare_address("\"Odd <x>\" <a@b.c>"), "a@b.c");
    }

    #[test]
    fn temp_config_is_private_and_deleted() {
        let path;
        {
            let c = TempConfig::create("user = \"x:y\"\n").unwrap();
            path = c.0.clone();
            assert_eq!(fs::read_to_string(&path).unwrap(), "user = \"x:y\"\n");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
        assert!(!path.exists());
        let _ = fs::File::open(&path).unwrap_err();
    }

    #[test]
    fn curl_reads_credentials_from_config_file() {
        if !curl_available() {
            println!("note: curl not found; skipping curl_reads_credentials_from_config_file");
            return;
        }
        let port = http_app(Arc::new(Mutex::new(mpsc::channel().1)));
        for (user, pass) in [("alice", "pw"), ("al:ice", "p\"w\\x y#z;'`$%"), ("üser", "pässwörd ✓"), ("u", "tab\there")] {
            let cfg = TempConfig::create(&format!("user = {}\n", curl_quote(&format!("{user}:{pass}")))).unwrap();
            let out = curl_command()
                .args(["-sS", "-K"])
                .arg(&cfg.0)
                .arg(format!("http://127.0.0.1:{port}/auth"))
                .output()
                .unwrap();
            let got = String::from_utf8(out.stdout).unwrap();
            assert_eq!(got, format!("Basic {}", b64_encode(format!("{user}:{pass}").as_bytes())), "{user}:{pass}");
        }
    }

    struct Captured {
        mail_from: String,
        rcpt: String,
        data: String,
    }

    /// One-connection fake SMTP server (no TLS, no AUTH) that reports what it received.
    fn fake_smtp() -> (u16, mpsc::Receiver<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((s, _)) = listener.accept() else { return };
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            w.write_all(b"220 fake ESMTP\r\n").unwrap();
            let mut cap = Captured { mail_from: String::new(), rcpt: String::new(), data: String::new() };
            while let Ok(line) = read_line(&mut r) {
                let up = line.to_ascii_uppercase();
                if up.starts_with("EHLO") || up.starts_with("HELO") {
                    w.write_all(b"250 fake\r\n").unwrap();
                } else if up.starts_with("MAIL FROM") {
                    cap.mail_from = line;
                    w.write_all(b"250 ok\r\n").unwrap();
                } else if up.starts_with("RCPT TO") {
                    cap.rcpt = line;
                    w.write_all(b"250 ok\r\n").unwrap();
                } else if up == "DATA" {
                    w.write_all(b"354 go\r\n").unwrap();
                    loop {
                        let l = read_line(&mut r).unwrap();
                        if l == "." {
                            break;
                        }
                        cap.data.push_str(&l);
                        cap.data.push_str("\r\n");
                    }
                    w.write_all(b"250 queued\r\n").unwrap();
                } else if up == "QUIT" {
                    w.write_all(b"221 bye\r\n").unwrap();
                    break;
                } else {
                    w.write_all(b"250 ok\r\n").unwrap();
                }
            }
            let _ = tx.send(cap);
        });
        (port, rx)
    }

    #[test]
    fn smtp_through_curl_dot_stuffs_exactly_once() {
        if !curl_available() {
            println!("note: curl not found; skipping smtp test");
            return;
        }
        let (port, rx) = fake_smtp();
        let m = Mail { host: "127.0.0.1", port, user: "", pass: "", from: "ESC <relay@example.org>", tls: "starttls" };
        let body = ".leading dot\nplain\n..two dots\n.\nlast é line";
        send_mail_with(&m, "Bob <bob@example.com>", "Test ✓", body, Security::Plain).unwrap();
        let cap = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(cap.mail_from, "MAIL FROM:<relay@example.org>");
        assert_eq!(cap.rcpt, "RCPT TO:<bob@example.com>");
        let (head, wire_body) = cap.data.split_once("\r\n\r\n").expect("header/body separator");
        assert!(head.starts_with("From: ESC <relay@example.org>\r\nTo: Bob <bob@example.com>\r\nSubject: =?UTF-8?B?"), "{head}");
        assert_eq!(wire_body, "..leading dot\r\nplain\r\n...two dots\r\n..\r\nlast é line\r\n");
    }

    #[test]
    fn send_mail_reports_curl_errors() {
        if !curl_available() {
            return;
        }
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // A password unique to this run, so another test process cannot be mistaken for a leftover file.
        let pass = format!("secret-{:016x}", random_u64());
        let m = Mail { host: "127.0.0.1", port: dead, user: "u", pass: &pass, from: "a@b.c", tls: "ssl" };
        let e = send_mail(&m, "x@y.z", "s", "b").unwrap_err();
        assert!(e.to_string().contains("curl smtp failed"), "{e}");
        assert!(!e.to_string().contains(&pass));
        // No config file is left behind.
        let leftovers = fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("esc-curl-") && fs::read_to_string(e.path()).is_ok_and(|c| c.contains(&pass)))
            .count();
        assert_eq!(leftovers, 0);
    }
}
