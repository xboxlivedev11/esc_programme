//! End-to-end tests that run the REAL `esc` binary: HTTP + WebSocket over loopback, one or two Stars.
//! Std only; the tiny HTTP/WebSocket client below is test-only.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esc::crypto::{b64url_encode, random_bytes};
use esc::json::{self, Json};

// ------------------------------------------------------------------------------------------------
// Process and directory guards
// ------------------------------------------------------------------------------------------------

/// A scratch directory removed on drop (declare it BEFORE the Stars that use it, so they die first).
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!("esc-e2e-{}-{}-{tag}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Dir(p)
    }

    fn sub(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        for _ in 0..20 {
            if fs::remove_dir_all(&self.0).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn esc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_esc"))
}

/// A running `esc serve`, killed on drop (also when a test panics). Its output is shown if the test fails.
struct Star {
    child: Child,
    port: u16,
    /// The one-time admin sign-up code, when the Star printed one.
    ticket: Option<String>,
    log: Arc<Mutex<String>>,
}

impl Star {
    fn start(data: &Path, port: u16) -> Star {
        let mut child = esc()
            .args(["serve", "--data"])
            .arg(data)
            .args(["--listen", &format!("127.0.0.1:{port}")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn esc serve");
        let log = Arc::new(Mutex::new(String::new()));
        let (tx, rx) = mpsc::channel::<String>();
        let out = child.stdout.take().unwrap();
        let (log2, tx2) = (log.clone(), tx.clone());
        thread::spawn(move || drain(out, &log2, Some(tx2)));
        let err = child.stderr.take().unwrap();
        let log3 = log.clone();
        thread::spawn(move || drain(err, &log3, None));
        drop(tx);

        let mut star = Star { child, port, ticket: None, log };
        // Wait for the listener, then pick up the ticket line (printed together with the banner).
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "esc serve did not start listening\n{}", star.output());
            if let Ok(Some(status)) = star.child.try_wait() {
                panic!("esc serve exited early ({status})\n{}", star.output());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let until = Instant::now() + Duration::from_millis(600);
        while let Ok(line) = rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            if let Some((_, t)) = line.split_once("#ticket=") {
                star.ticket = Some(t.trim().to_string());
            }
        }
        star
    }

    fn output(&self) -> String {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn stop(mut self) {
        self.kill();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Star {
    fn drop(&mut self) {
        self.kill();
        if thread::panicking() {
            eprintln!("---- esc serve (port {}) output ----\n{}", self.port, self.output());
        }
    }
}

fn drain(mut r: impl Read, log: &Mutex<String>, lines: Option<mpsc::Sender<String>>) {
    let mut pending = String::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = r.read(&mut buf) {
        if n == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        log.lock().unwrap_or_else(|e| e.into_inner()).push_str(&text);
        if let Some(tx) = &lines {
            pending.push_str(&text);
            while let Some(i) = pending.find('\n') {
                let line: String = pending.drain(..=i).collect();
                let _ = tx.send(line);
            }
        }
    }
}

fn eventually(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < end, "timed out after {secs}s waiting for: {what}");
        thread::sleep(Duration::from_millis(150));
    }
}

// ------------------------------------------------------------------------------------------------
// A minimal HTTP/1.1 client
// ------------------------------------------------------------------------------------------------

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Json {
        json::parse_bytes(&self.body).unwrap_or_else(|e| panic!("reply is not JSON ({e}): {:?}", String::from_utf8_lossy(&self.body)))
    }

    /// The `esc` session token from `Set-Cookie`.
    fn session(&self) -> Option<String> {
        self.headers
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .find_map(|(_, v)| v.strip_prefix("esc=").map(|r| r.split(';').next().unwrap_or("").to_string()))
    }
}

fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if !raw.is_empty() => {
                // The server closes right after the response; a late reset must not lose what arrived.
                let _ = e;
                break;
            }
            Err(e) => panic!("{method} {path}: {e}"),
        }
    }
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or_else(|| panic!("{method} {path}: no response head")) + 4;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Reply { status, headers, body: raw[split..].to_vec() }
}

// ------------------------------------------------------------------------------------------------
// A minimal WebSocket client (masked frames), speaking the app's JSON RPC
// ------------------------------------------------------------------------------------------------

struct Ws {
    s: TcpStream,
    next: u64,
}

impl Ws {
    fn connect(port: u16, token: &str) -> Ws {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("ws connect");
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nCookie: esc={token}\r\n\r\n"
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).expect("ws handshake");
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 101 "), "websocket upgrade refused: {head}");
        assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "bad Sec-WebSocket-Accept: {head}");
        Ws { s, next: 1 }
    }

    fn send_text(&mut self, text: &str) {
        let payload = text.as_bytes();
        let key = [0x37, 0xfa, 0x21, 0x3d];
        let mut f = vec![0x81];
        match payload.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n <= 65535 => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        f.extend_from_slice(&key);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        self.s.write_all(&f).unwrap();
    }

    /// Next text frame (pings and pongs are skipped).
    fn recv_text(&mut self) -> String {
        loop {
            let mut h = [0u8; 2];
            self.s.read_exact(&mut h).expect("ws frame header");
            let mut len = (h[1] & 0x7F) as u64;
            if len == 126 {
                let mut b = [0u8; 2];
                self.s.read_exact(&mut b).unwrap();
                len = u16::from_be_bytes(b) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.s.read_exact(&mut b).unwrap();
                len = u64::from_be_bytes(b);
            }
            assert_eq!(h[1] & 0x80, 0, "server frames must not be masked");
            let mut payload = vec![0u8; len as usize];
            self.s.read_exact(&mut payload).unwrap();
            match h[0] & 0x0F {
                1 => return String::from_utf8(payload).unwrap(),
                8 => panic!("server closed the websocket: {:?}", String::from_utf8_lossy(&payload)),
                _ => {}
            }
        }
    }

    /// One RPC. `extra` is raw JSON members (`"c":"x","n":5`) or empty. Ok = the `r` value, Err = "code: message".
    fn rpc(&mut self, method: &str, extra: &str) -> Result<Json, String> {
        let i = self.next;
        self.next += 1;
        let sep = if extra.is_empty() { "" } else { "," };
        self.send_text(&format!("{{\"i\":{i},\"m\":\"{method}\"{sep}{extra}}}"));
        loop {
            let reply = json::parse(&self.recv_text()).expect("rpc reply is JSON");
            if reply.u("i") != Some(i) {
                continue; // a push
            }
            return match reply.get("r") {
                Some(r) => Ok(r.clone()),
                None => Err(format!("{}: {}", reply.u("c").unwrap_or(0), reply.s("e").unwrap_or("?"))),
            };
        }
    }

    fn ok(&mut self, method: &str, extra: &str) -> Json {
        self.rpc(method, extra).unwrap_or_else(|e| panic!("rpc {method} failed: {e}"))
    }
}

// ------------------------------------------------------------------------------------------------
// App-level helpers
// ------------------------------------------------------------------------------------------------

/// What a browser would hold after sign-up: callsign, salt and (fake) PBKDF2 output.
#[derive(Clone)]
struct Creds {
    id: String,
    salt: String,
    key: String,
}

struct User {
    port: u16,
    creds: Creds,
    token: String,
    ws: Ws,
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    random_bytes(&mut b);
    b64url_encode(&b)
}

impl User {
    fn open(port: u16, creds: Creds, token: String) -> User {
        let ws = Ws::connect(port, &token);
        User { port, creds, token, ws }
    }

    fn signup(port: u16, ticket: &str, name: &str) -> User {
        let (salt, key) = (random_b64(16), random_b64(32));
        let body = format!("{{\"t\":\"{ticket}\",\"s\":\"{salt}\",\"k\":\"{key}\",\"n\":\"{name}\"}}");
        let r = http(port, "POST", "/api/signup", &[("Content-Type", "application/json")], body.as_bytes());
        assert_eq!(r.status, 200, "signup: {}", String::from_utf8_lossy(&r.body));
        let id = r.json().s("id").unwrap().to_string();
        let token = r.session().expect("session cookie");
        User::open(port, Creds { id, salt, key }, token)
    }

    /// Sign in with existing credentials (the browser's `/api/pre` then `/api/login`).
    fn login(port: u16, creds: &Creds) -> Result<User, String> {
        let pre = http(port, "POST", "/api/pre", &[], format!("{{\"id\":\"{}\"}}", creds.id).as_bytes());
        assert_eq!(pre.status, 200);
        if pre.json().s("s") != Some(creds.salt.as_str()) {
            return Err("salt differs (account not known here yet)".into());
        }
        let body = format!("{{\"id\":\"{}\",\"k\":\"{}\"}}", creds.id, creds.key);
        let r = http(port, "POST", "/api/login", &[], body.as_bytes());
        if r.status != 200 {
            return Err(format!("login {}: {}", r.status, String::from_utf8_lossy(&r.body)));
        }
        let token = r.session().expect("session cookie");
        Ok(User::open(port, creds.clone(), token))
    }

    fn cookie(&self) -> String {
        format!("esc={}", self.token)
    }

    fn get(&self, path: &str, extra: &[(&str, &str)]) -> Reply {
        let cookie = self.cookie();
        let mut h = vec![("Cookie", cookie.as_str())];
        h.extend_from_slice(extra);
        http(self.port, "GET", path, &h, b"")
    }

    /// Upload `data` in `chunk`-sized PUTs; returns the blob hash (hex).
    fn upload(&self, data: &[u8], chunk: usize) -> String {
        let cookie = self.cookie();
        let h = [("Cookie", cookie.as_str()), ("Content-Type", "application/json")];
        let begin = http(self.port, "POST", "/api/up", &h, format!("{{\"z\":{}}}", data.len()).as_bytes());
        assert_eq!(begin.status, 200, "{}", String::from_utf8_lossy(&begin.body));
        let up = begin.json().s("u").unwrap().to_string();
        let mut off = 0;
        loop {
            let end = (off + chunk).min(data.len());
            let r = http(self.port, "PUT", &format!("/api/up/{up}?o={off}"), &[("Cookie", cookie.as_str())], &data[off..end]);
            assert_eq!(r.status, 200, "chunk at {off}: {}", String::from_utf8_lossy(&r.body));
            let j = r.json();
            if end == data.len() {
                let b = j.s("b").unwrap_or_else(|| panic!("final chunk must return the blob: {j:?}")).to_string();
                assert_eq!(j.u("z"), Some(data.len() as u64));
                return b;
            }
            assert_eq!(j.u("o"), Some(end as u64), "written so far");
            off = end;
        }
    }

    /// Send a message with one attachment; returns the message reply.
    fn send_file(&mut self, conv: &str, text: &str, blob: &str, name: &str, size: usize) -> Json {
        self.ws.ok("send", &format!("\"c\":\"{conv}\",\"x\":\"{text}\",\"a\":[{{\"b\":\"{blob}\",\"n\":\"{name}\",\"m\":\"application/octet-stream\",\"z\":{size}}}]"))
    }

    fn say(&mut self, conv: &str, text: &str) {
        self.ws.ok("send", &format!("\"c\":\"{conv}\",\"x\":\"{text}\""));
    }

    /// Texts in the conversation (oldest first), or Err if it is not visible here (yet).
    fn texts(&mut self, conv: &str) -> Result<Vec<String>, String> {
        let r = self.ws.rpc("hist", &format!("\"c\":\"{conv}\",\"n\":200"))?;
        Ok(r.a("m").iter().map(|m| m.s("x").unwrap_or("").to_string()).collect())
    }

    fn has_text(&mut self, conv: &str, text: &str) -> bool {
        self.texts(conv).is_ok_and(|t| t.iter().any(|x| x == text))
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 131 % 251) as u8).collect()
}

/// `knock` + `answer accept`: returns the conversation id.
fn befriend(asker: &mut User, giver: &mut User) -> String {
    let r = asker.ws.ok("knock", &format!("\"to\":\"{}\",\"x\":\"hello\"", giver.creds.id));
    let req = r.s("r").unwrap_or_else(|| panic!("knock did not create a request: {r:?}")).to_string();
    let a = giver.ws.ok("answer", &format!("\"r\":\"{req}\",\"a\":\"accept\""));
    a.s("c").expect("answer returns the conversation").to_string()
}

// ------------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------------

#[test]
fn single_star_everything_and_restart() {
    let dir = Dir::new("single");
    let data = dir.sub("star");
    let port = free_port();
    let star = Star::start(&data, port);
    let ticket = star.ticket.clone().unwrap_or_else(|| panic!("no #ticket= line in:\n{}", star.output()));

    // Admin sign-up with the printed ticket, then the session cookie opens the WebSocket.
    let mut admin = User::signup(port, &ticket, "Ada");
    assert_eq!(http(port, "POST", "/api/signup", &[], b"{}").status, 400, "bad sign-up is refused");
    let me = http(port, "GET", "/api/me", &[("Cookie", &admin.cookie())], b"");
    assert_eq!((me.status, me.json().s("id").map(String::from)), (200, Some(admin.creds.id.clone())));
    assert_eq!(http(port, "GET", "/api/me", &[], b"").status, 401);
    let hello = admin.ws.ok("hello", "");
    assert_eq!(hello.get("me").and_then(|m| m.b("adm")), Some(true));
    assert_eq!(admin.ws.rpc("nope", "").unwrap_err().split(':').next(), Some("400"));

    // The used-up ticket cannot create a second admin.
    let again = http(port, "POST", "/api/signup", &[], format!("{{\"t\":\"{ticket}\",\"s\":\"{}\",\"k\":\"{}\",\"n\":\"X\"}}", random_b64(16), random_b64(32)).as_bytes());
    assert_eq!(again.status, 403);

    // Tickets for two more people.
    let t = admin.ws.ok("ticket_new", "\"u\":2,\"d\":7");
    let code = t.s("c").expect("ticket code").to_string();
    let mut bob = User::signup(port, &code, "Bob");
    let eve = User::signup(port, &code, "Eve");
    assert_ne!(bob.creds.id, eve.creds.id);

    // Bob knocks, Ada accepts, they talk.
    let conv = befriend(&mut bob, &mut admin);
    bob.say(&conv, "hello Ada");
    admin.say(&conv, "hello Bob");
    assert_eq!(admin.texts(&conv).unwrap(), ["hello Ada", "hello Bob"]);
    assert_eq!(bob.texts(&conv).unwrap(), ["hello Ada", "hello Bob"]);
    let mut eve = eve;
    assert!(eve.texts(&conv).is_err(), "Eve is not in the Line");

    // Upload in three chunks and send the file.
    let data_bytes = pattern(100_000);
    let blob = admin.upload(&data_bytes, 40_000);
    assert_eq!(blob.len(), 64);
    admin.send_file(&conv, "here is a file", &blob, "data.bin", data_bytes.len());
    assert!(bob.has_text(&conv, "here is a file"));

    let path = format!("/f/{blob}/o/data.bin");
    let full = admin.get(&path, &[]);
    assert_eq!((full.status, full.body.len()), (200, 100_000));
    assert!(full.body == data_bytes);
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    let part = admin.get(&path, &[("Range", "bytes=10-19")]);
    assert_eq!(part.status, 206);
    assert_eq!(part.body, &data_bytes[10..20]);
    assert_eq!(part.header("content-range"), Some("bytes 10-19/100000"));
    let bobs = bob.get(&path, &[]);
    assert_eq!(bobs.status, 200);
    assert!(bobs.body == data_bytes, "Bob received the same bytes");
    assert_eq!(eve.get(&path, &[]).status, 404, "Eve is not in the Line");
    assert_eq!(http(port, "GET", &path, &[], b"").status, 401, "no session, no file");

    // Restart the process: everything is still there.
    let creds = admin.creds.clone();
    let bob_creds = bob.creds.clone();
    drop((admin, bob, eve));
    star.stop();
    let star = Star::start(&data, port);
    assert!(star.ticket.is_none(), "an admin exists now, so no ticket:\n{}", star.output());
    let mut admin = User::login(port, &creds).expect("admin logs in after the restart");
    let mut bob = User::login(port, &bob_creds).expect("bob logs in after the restart");
    let hello = admin.ws.ok("hello", "");
    assert_eq!(hello.a("convs").len(), 1);
    assert_eq!(admin.texts(&conv).unwrap(), ["hello Ada", "hello Bob", "here is a file"]);
    assert_eq!(bob.texts(&conv).unwrap().len(), 3);
    let again = admin.get(&path, &[]);
    assert_eq!(again.status, 200);
    assert!(again.body == data_bytes, "the file survived the restart");
    assert_eq!(bob.get(&path, &[("Range", "bytes=99990-")]).body, &data_bytes[99_990..]);
    admin.say(&conv, "after the restart");
    assert!(bob.has_text(&conv, "after the restart"));
    drop(star);
}

#[test]
fn two_stars_replicate_both_ways() {
    let dir = Dir::new("ring");
    let (da, db) = (dir.sub("a"), dir.sub("b"));
    let (pa, pb) = (free_port(), free_port());
    let a = Star::start(&da, pa);
    let ticket = a.ticket.clone().expect("A prints an admin ticket");

    // A: admin, a second person, a Line between them.
    let mut ada = User::signup(pa, &ticket, "Ada");
    let t = ada.ws.ok("ticket_new", "\"u\":1,\"d\":7");
    let mut bob = User::signup(pa, t.s("c").unwrap(), "Bob");
    let conv = befriend(&mut bob, &mut ada);
    ada.say(&conv, "written before B exists");

    // The Control Tower makes a Spark; B joins with it and is lit.
    let spark = ada.ws.ok("tower_spark", "");
    let spark = spark.s("spark").expect("spark code").to_string();
    let join = esc()
        .args(["join", &format!("http://127.0.0.1:{pa}"), &spark, "--data"])
        .arg(&db)
        .args(["--url", &format!("http://127.0.0.1:{pb}")])
        .output()
        .expect("run esc join");
    assert!(join.status.success(), "esc join failed: {}{}", String::from_utf8_lossy(&join.stdout), String::from_utf8_lossy(&join.stderr));
    let again = esc().args(["join", &format!("http://127.0.0.1:{pa}"), &spark, "--data"]).arg(dir.sub("c")).output().unwrap();
    assert!(!again.status.success(), "a Spark works only once");

    let b = Star::start(&db, pb);
    assert!(b.ticket.is_none(), "a joined Star must not mint its own admin ticket:\n{}", b.output());

    // B replicates A: the account is known there and the same password signs in.
    let (ada_creds, bob_creds) = (ada.creds.clone(), bob.creds.clone());
    eventually("B learns Ada's account", 25, || {
        http(pb, "POST", "/api/pre", &[], format!("{{\"id\":\"{}\"}}", ada_creds.id).as_bytes()).json().s("s") == Some(ada_creds.salt.as_str())
    });
    let mut ada_b = User::login(pb, &ada_creds).expect("Ada signs in on B");
    let mut bob_b = User::login(pb, &bob_creds).expect("Bob signs in on B");
    eventually("B has the old message", 25, || ada_b.has_text(&conv, "written before B exists"));

    // A -> B: a new message shows up on B.
    ada.say(&conv, "from A to B");
    eventually("A's message reaches B", 25, || bob_b.has_text(&conv, "from A to B"));

    // B -> A: needs A to pull from B (it learned B's URL from the RELAY event of the join).
    bob_b.say(&conv, "from B to A");
    eventually("B's message reaches A", 25, || ada.has_text(&conv, "from B to A"));
    assert_eq!(ada.texts(&conv).unwrap(), bob_b.texts(&conv).unwrap(), "same Line, same order, on both Stars");

    // Files: upload on A, send, fetch on B once the blob has been pulled.
    let bytes = pattern(300_000);
    let blob = ada.upload(&bytes, 128 * 1024);
    ada.send_file(&conv, "file from A", &blob, "a-to-b.bin", bytes.len());
    let path = format!("/f/{blob}/o/a-to-b.bin");
    let mut got = None;
    eventually("the blob reaches B", 30, || {
        let r = bob_b.get(&path, &[]);
        if r.status == 200 {
            got = Some(r.body);
            true
        } else {
            false
        }
    });
    assert!(got.unwrap() == bytes, "B serves the identical bytes");
    let ranged = ada_b.get(&path, &[("Range", "bytes=10-19")]);
    assert_eq!((ranged.status, ranged.body), (206, bytes[10..20].to_vec()));

    // ... and the other way: upload on B, fetch on A (A pulls blobs from B).
    let bytes2 = pattern(70_000).into_iter().rev().collect::<Vec<u8>>();
    let blob2 = bob_b.upload(&bytes2, 70_000);
    bob_b.send_file(&conv, "file from B", &blob2, "b-to-a.bin", bytes2.len());
    let path2 = format!("/f/{blob2}/o/b-to-a.bin");
    let mut got2 = None;
    eventually("B's blob reaches A", 30, || {
        let r = ada.get(&path2, &[]);
        if r.status == 200 {
            got2 = Some(r.body);
            true
        } else {
            false
        }
    });
    assert!(got2.unwrap() == bytes2);

    // Kill B; A keeps going; B catches up after a restart.
    drop((ada_b, bob_b));
    b.stop();
    ada.say(&conv, "sent while B was down");
    let bytes3 = pattern(5_000);
    let blob3 = ada.upload(&bytes3, 5_000);
    ada.send_file(&conv, "file while B was down", &blob3, "late.bin", bytes3.len());
    let b = Star::start(&db, pb);
    assert!(b.ticket.is_none());
    let mut bob_b = User::login(pb, &bob_creds).expect("Bob signs in on the restarted B");
    eventually("B catches up after the restart", 30, || bob_b.has_text(&conv, "sent while B was down"));
    assert!(bob_b.has_text(&conv, "from B to A"), "B kept its own history");
    let path3 = format!("/f/{blob3}/o/late.bin");
    eventually("B fetches the blob it missed", 30, || bob_b.get(&path3, &[]).status == 200);
    assert_eq!(bob_b.get(&path3, &[]).body, bytes3);
    bob_b.say(&conv, "B is back");
    eventually("A hears B again", 25, || ada.has_text(&conv, "B is back"));
    drop((a, b));
}

#[test]
fn third_star_joins_through_the_second() {
    let dir = Dir::new("chain");
    let (da, db, dc) = (dir.sub("a"), dir.sub("b"), dir.sub("c"));
    let (pa, pb, pc) = (free_port(), free_port(), free_port());
    let a = Star::start(&da, pa);
    let mut ada = User::signup(pa, a.ticket.as_deref().expect("A ticket"), "Ada");
    let t = ada.ws.ok("ticket_new", "\"u\":1,\"d\":7");
    let mut bob = User::signup(pa, t.s("c").unwrap(), "Bob");
    let conv = befriend(&mut bob, &mut ada);

    let join = |peer_port: u16, spark: &str, data: &Path, my_port: u16| {
        let out = esc()
            .args(["join", &format!("http://127.0.0.1:{peer_port}"), spark, "--data"])
            .arg(data)
            .args(["--url", &format!("http://127.0.0.1:{my_port}")])
            .output()
            .unwrap();
        assert!(out.status.success(), "esc join failed: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    };
    let spark_a = ada.ws.ok("tower_spark", "").s("spark").unwrap().to_string();
    join(pa, &spark_a, &db, pb);
    let b = Star::start(&db, pb);
    let ada_creds = ada.creds.clone();
    eventually("B knows Ada", 25, || User::login(pb, &ada_creds).is_ok());
    let mut ada_b = User::login(pb, &ada_creds).unwrap();

    // C joins through B (not A); A must learn about C through the Ring itself.
    let sp = ada_b.ws.ok("tower_spark", "");
    assert_eq!(sp.s("url"), Some(format!("http://127.0.0.1:{pb}").as_str()), "a Spark carries the Star's URL");
    let spark_b = sp.s("spark").unwrap().to_string();
    join(pb, &spark_b, &dc, pc);
    let c = Star::start(&dc, pc);
    assert!(c.ticket.is_none());
    let bob_creds = bob.creds.clone();
    let mut bob_c = None;
    eventually("C knows Bob", 25, || match User::login(pc, &bob_creds) {
        Ok(u) => {
            bob_c = Some(u);
            true
        }
        Err(_) => false,
    });
    let mut bob_c = bob_c.unwrap();
    eventually("C has the Line", 25, || bob_c.texts(&conv).is_ok());

    bob_c.say(&conv, "hello from C");
    eventually("C's message reaches A", 30, || ada.has_text(&conv, "hello from C"));
    ada.say(&conv, "hello from A");
    eventually("A's message reaches C", 30, || bob_c.has_text(&conv, "hello from A"));
    assert_eq!(ada.texts(&conv).unwrap(), bob_c.texts(&conv).unwrap());
    drop((a, b, c));
}
