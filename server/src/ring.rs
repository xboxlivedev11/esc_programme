//! The Ring: full-mirror replication between Stars (relays). Every Star keeps a byte-identical copy of
//! every origin's Logbook; replication is pull-only. A puller thread per peer keeps one long-lived
//! `GET /ring/pull` open and appends the frames it receives; a blob fetcher fills in missing files.
//! All requests are signed with the shared ring key. Std only.

use crate::courier;
use crate::crypto::{b64url_decode, b64url_encode, ct_eq, hex, hmac_sha256, unhex};
use crate::http::{pct_decode, reason};
use crate::ids::{id64, now_ms, parse_id64};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub type Hash = [u8; 32];

/// Implemented by the Bridge.
pub trait Host: Send + Sync + 'static {
    fn me(&self) -> u64;
    fn ring_key(&self) -> Hash;
    /// (origin star id, bytes we hold of its log), for every origin we hold, ours included.
    fn have(&self) -> Vec<(u64, u64)>;
    /// Up to max raw bytes of origin's log from `from`, cut at a record boundary (Logbook::read_raw semantics).
    fn read(&self, origin: u64, from: u64, max: usize) -> io::Result<Vec<u8>>;
    /// Append bytes for origin at offset `at` (must equal what we hold; creates the local copy if origin is new),
    /// apply the events, and return the new length. Err(InvalidData) on a wrong offset or bad records.
    fn append(&self, origin: u64, at: u64, bytes: &[u8]) -> io::Result<u64>;
    /// Block until some origin grows past `have`, or until timeout. True = something grew.
    fn wait(&self, have: &[(u64, u64)], timeout: Duration) -> bool;
    /// Peers to pull from: (star id, url). Never includes me. Changes over time (re-read it every loop).
    fn peers(&self) -> Vec<(u64, String)>;
    /// Blobs referenced in state but missing on disk, with sizes.
    fn missing_blobs(&self) -> Vec<(Hash, u64)>;
    /// Store a fetched blob (verifies SHA-256; Hold::import semantics).
    fn import_blob(&self, h: &Hash, len: u64, r: &mut dyn Read) -> io::Result<()>;
    /// Open a stored blob for serving: (file, len).
    fn open_blob(&self, h: &Hash) -> io::Result<(std::fs::File, u64)>;
}

/// One peer as shown on the admin screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    pub id: u64,
    pub url: String,
    pub connected: bool,
    /// Unix ms of the last frame (or heartbeat) received.
    pub last_ok: u64,
    /// Log bytes appended from this peer since start.
    pub pulled: u64,
    /// Last error; empty when healthy.
    pub error: String,
}

/// Longest tolerated age difference between a signature and the verifier's clock.
const MAX_SKEW_MS: u64 = 5 * 60 * 1000;
/// Largest chunk of log sent in one frame (a single bigger record is still sent whole).
const CHUNK: usize = 1 << 20;
/// Silence on a pull stream longer than this gets a heartbeat frame.
const HEARTBEAT: Duration = Duration::from_secs(25);
/// Response head of a pull stream: no Content-Length, the body is the open-ended frame stream.
const PULL_HEAD: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
/// Frame header: origin u64, at u64, len u32 (all little-endian).
const FRAME_HDR: usize = 20;
/// Largest accepted frame payload: one maximal Logbook record (16 MiB payload + 8 header) plus slack.
const FRAME_MAX: usize = (16 << 20) + 64;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------------------------

fn mac(key: &Hash, me: u64, ts: u64, method: &str, target: &str) -> Hash {
    hmac_sha256(key, format!("{}.{ts}.{method} {target}", id64(me)).as_bytes())
}

/// `X-Ring` header value for a request (method + path?query), signed with the ring key.
pub fn sign(key: &Hash, me: u64, method: &str, target: &str, now_ms: u64) -> String {
    format!("{}.{now_ms}.{}", id64(me), b64url_encode(&mac(key, me, now_ms, method, target)))
}

/// Verify an X-Ring header: correct HMAC, |now - ts| <= 5 min. Returns the caller's star id.
pub fn verify(key: &Hash, header: &str, method: &str, target: &str, now_ms: u64) -> Option<u64> {
    let mut parts = header.trim().splitn(3, '.');
    let me = parse_id64(parts.next()?)?;
    let ts_text = parts.next()?;
    if ts_text.is_empty() || !ts_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ts: u64 = ts_text.parse().ok()?;
    let given = b64url_decode(parts.next()?)?;
    let ok = ct_eq(&given, &mac(key, me, ts, method, target));
    (ok && now_ms.abs_diff(ts) <= MAX_SKEW_MS).then_some(me)
}

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/// One piece of an origin's log: `data` sits at byte offset `at`. Origin 0 with no data is a heartbeat.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    origin: u64,
    at: u64,
    data: Vec<u8>,
}

fn encode_frame(origin: u64, at: u64, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(FRAME_HDR + data.len());
    f.extend_from_slice(&origin.to_le_bytes());
    f.extend_from_slice(&at.to_le_bytes());
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(data);
    f
}

/// Reads one frame; `None` at a clean end of stream, `UnexpectedEof` if cut mid-frame.
fn read_frame(r: &mut dyn Read) -> io::Result<Option<Frame>> {
    let mut hdr = [0u8; FRAME_HDR];
    let mut got = 0;
    while got < FRAME_HDR {
        match r.read(&mut hdr[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let origin = u64::from_le_bytes(hdr[0..8].try_into().expect("8 bytes"));
    let at = u64::from_le_bytes(hdr[8..16].try_into().expect("8 bytes"));
    let len = u32::from_le_bytes(hdr[16..20].try_into().expect("4 bytes")) as usize;
    if len > FRAME_MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "ring frame too large"));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)?;
    Ok(Some(Frame { origin, at, data }))
}

// ---------------------------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------------------------

/// Parses `h=<id64>:<len>,...` out of a raw query string; `None` if malformed, empty map if absent.
fn parse_have(query: &str) -> Option<HashMap<u64, u64>> {
    let mut map = HashMap::new();
    let Some(raw) = query.split('&').find_map(|kv| kv.strip_prefix("h=")) else { return Some(map) };
    for part in pct_decode(raw, false)?.split(',').filter(|p| !p.is_empty()) {
        let (origin, len) = part.split_once(':')?;
        map.insert(parse_id64(origin)?, len.parse().ok()?);
    }
    Some(map)
}

fn plain(w: &mut dyn Write, status: u16, text: &str) -> io::Result<()> {
    let msg = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{text}",
        reason(status),
        text.len()
    );
    w.write_all(msg.as_bytes())?;
    w.flush()
}

/// Serve GET /ring/pull?h=<id64>:<len>,... (already verified by the caller). Writes the head itself, then the
/// frame stream `[u64 origin][u64 at][u32 len][bytes]`: everything the puller lacks for every origin we hold
/// (an origin missing from `h` means from 0), then live growth, with a heartbeat frame (origin 0, len 0) after
/// 25 s of silence. Stops after `max_life` or on any write error.
pub fn serve_pull(host: &dyn Host, query: &str, w: &mut dyn Write, max_life: Duration) -> io::Result<()> {
    serve_pull_with(host, query, w, max_life, HEARTBEAT)
}

fn serve_pull_with(host: &dyn Host, query: &str, w: &mut dyn Write, max_life: Duration, heartbeat: Duration) -> io::Result<()> {
    let Some(mut sent) = parse_have(query) else { return plain(w, 400, "bad h parameter") };
    w.write_all(PULL_HEAD)?;
    w.flush()?;
    let start = Instant::now();
    loop {
        let local = host.have();
        let mut progressed = false;
        for &(origin, len) in &local {
            let pos = sent.entry(origin).or_insert(0);
            while *pos < len {
                let chunk = host.read(origin, *pos, CHUNK)?;
                if chunk.is_empty() {
                    break;
                }
                w.write_all(&encode_frame(origin, *pos, &chunk))?;
                *pos += chunk.len() as u64;
                progressed = true;
            }
        }
        if progressed {
            w.flush()?;
        }
        let left = max_life.saturating_sub(start.elapsed());
        if left.is_zero() {
            return Ok(());
        }
        if progressed {
            continue; // look again before sleeping: more may have arrived while sending
        }
        let marks: Vec<(u64, u64)> = local.iter().map(|&(o, l)| (o, sent.get(&o).copied().unwrap_or(0).max(l))).collect();
        if !host.wait(&marks, heartbeat.min(left)) && start.elapsed() < max_life {
            w.write_all(&encode_frame(0, 0, &[]))?;
            w.flush()?;
        }
    }
}

/// Serve GET /ring/blob/<64 hex> (already verified): 200 + Content-Length + bytes, or 404.
pub fn serve_blob(host: &dyn Host, hex_hash: &str, w: &mut dyn Write) -> io::Result<()> {
    let Some(h) = unhex(hex_hash).and_then(|v| Hash::try_from(v).ok()) else { return plain(w, 400, "bad blob hash") };
    let Ok((file, len)) = host.open_blob(&h) else { return plain(w, 404, "no such blob") };
    w.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )?;
    if io::copy(&mut file.take(len), w)? != len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "blob file shorter than expected"));
    }
    w.flush()
}

// ---------------------------------------------------------------------------------------------
// Pulling
// ---------------------------------------------------------------------------------------------

/// Timing knobs (shortened in tests).
#[derive(Clone, Copy)]
struct Tuning {
    /// How often the peer list is re-read.
    refresh: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// Blob fetcher period.
    blob_every: Duration,
    /// Connect timeout and read timeout of plain sockets; must exceed the heartbeat period.
    net_timeout: Duration,
}

impl Tuning {
    const DEFAULT: Tuning = Tuning {
        refresh: Duration::from_secs(10),
        backoff_min: Duration::from_secs(1),
        backoff_max: Duration::from_secs(30),
        blob_every: Duration::from_secs(15),
        net_timeout: Duration::from_secs(70),
    };
}

/// Live numbers for one peer, shared between its puller thread and `status()`.
struct PeerStat {
    star: u64,
    /// 0 while the peer is only a seed URL whose Star id is not known yet.
    id: AtomicU64,
    url: String,
    connected: AtomicBool,
    last_ok: AtomicU64,
    pulled: AtomicU64,
    error: Mutex<String>,
}

static REGISTRY: OnceLock<Mutex<Vec<Arc<PeerStat>>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<Arc<PeerStat>>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Status for the admin screen: per peer (id, url, connected, last_ok_ms, bytes_pulled, last_error).
pub fn status() -> Vec<PeerStatus> {
    let mut out: Vec<PeerStatus> = lock(registry())
        .iter()
        .map(|s| PeerStatus {
            id: s.id.load(Ordering::Relaxed),
            url: s.url.clone(),
            connected: s.connected.load(Ordering::Relaxed),
            last_ok: s.last_ok.load(Ordering::Relaxed),
            pulled: s.pulled.load(Ordering::Relaxed),
            error: lock(&s.error).clone(),
        })
        .collect();
    out.sort_by(|a, b| (a.id, &a.url).cmp(&(b.id, &b.url)));
    out
}

/// Wakes the blob fetcher early (after new log data arrived).
#[derive(Default)]
struct Kick {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl Kick {
    fn notify(&self) {
        *lock(&self.flag) = true;
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) {
        let mut g = lock(&self.flag);
        if !*g {
            g = self.cv.wait_timeout(g, timeout).unwrap_or_else(|e| e.into_inner()).0;
        }
        *g = false;
    }
}

/// Sleeps up to `d`, returning early once `stop` is set.
fn nap(d: Duration, stop: &AtomicBool) {
    let end = Instant::now() + d;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// Start the background threads: one puller per peer (the peer list is re-read every 10 s, pullers are started
/// and stopped to match; reconnects back off from 1 s to 30 s) and one blob fetcher. Returns immediately.
pub fn start(host: Arc<dyn Host>) {
    start_with(host, Tuning::DEFAULT);
}

fn start_with(host: Arc<dyn Host>, t: Tuning) {
    let kick = Arc::new(Kick::default());
    {
        let (host, kick) = (host.clone(), kick.clone());
        let _ = thread::Builder::new().name("ring-blobs".into()).stack_size(256 * 1024).spawn(move || loop {
            fetch_missing(host.as_ref(), &t);
            kick.wait(t.blob_every);
        });
    }
    let _ = thread::Builder::new().name("ring-peers".into()).stack_size(256 * 1024).spawn(move || {
        // Pullers are keyed by URL: seed URLs have no Star id yet (0), and several seeds can coexist.
        let mut running: HashMap<String, (Arc<PeerStat>, Arc<AtomicBool>)> = HashMap::new();
        loop {
            let peers = host.peers();
            let me = host.me();
            running.retain(|url, (_, stop)| {
                let keep = peers.iter().any(|(_, purl)| purl == url);
                if !keep {
                    stop.store(true, Ordering::Relaxed);
                }
                keep
            });
            for (id, url) in peers.into_iter().filter(|(id, _)| *id != me) {
                if let Some((stat, _)) = running.get(&url) {
                    if id != 0 {
                        stat.id.store(id, Ordering::Relaxed); // a seed turned out to be a known Star
                    }
                    continue;
                }
                let stop = Arc::new(AtomicBool::new(false));
                let stat = Arc::new(PeerStat {
                    star: me,
                    id: AtomicU64::new(id),
                    url: url.clone(),
                    connected: AtomicBool::new(false),
                    last_ok: AtomicU64::new(0),
                    pulled: AtomicU64::new(0),
                    error: Mutex::new(String::new()),
                });
                {
                    let mut reg = lock(registry());
                    reg.retain(|s| !(s.star == me && s.url == url));
                    reg.push(stat.clone());
                }
                let (h, st, sp, k) = (host.clone(), stat.clone(), stop.clone(), kick.clone());
                let spawned = thread::Builder::new()
                    .name("ring-pull".into())
                    .stack_size(256 * 1024)
                    .spawn(move || puller(h.as_ref(), &st, &sp, &k, &t));
                if spawned.is_ok() {
                    running.insert(url, (stat, stop));
                }
            }
            thread::sleep(t.refresh);
        }
    });
}

fn puller(host: &dyn Host, stat: &Arc<PeerStat>, stop: &AtomicBool, kick: &Kick, t: &Tuning) {
    let mut backoff = t.backoff_min;
    while !stop.load(Ordering::Relaxed) {
        let result = pull_once(host, stat, stop, kick, t);
        stat.connected.store(false, Ordering::Relaxed);
        match result {
            Ok(()) => {
                backoff = t.backoff_min;
                nap(Duration::from_millis(100).min(t.backoff_min), stop);
            }
            Err(msg) => {
                *lock(&stat.error) = msg;
                nap(backoff, stop);
                backoff = (backoff * 2).min(t.backoff_max);
            }
        }
    }
    lock(registry()).retain(|s| !Arc::ptr_eq(s, stat));
}

/// One pull connection. `Ok` = ended cleanly or needs a fresh start; `Err` = failure to back off from.
fn pull_once(host: &dyn Host, stat: &PeerStat, stop: &AtomicBool, kick: &Kick, t: &Tuning) -> Result<(), String> {
    let have: Vec<String> = host.have().iter().map(|(o, l)| format!("{}:{l}", id64(*o))).collect();
    let target = format!("/ring/pull?h={}", have.join(","));
    let header = sign(&host.ring_key(), host.me(), "GET", &target, now_ms());
    let url = format!("{}{target}", stat.url.trim_end_matches('/'));
    let resp = courier::request("GET", &url, &[("X-Ring", &header)], None, t.net_timeout).map_err(|e| e.to_string())?;
    if resp.status != 200 {
        return Err(format!("HTTP {}", resp.status));
    }
    stat.connected.store(true, Ordering::Relaxed);
    lock(&stat.error).clear();
    stat.last_ok.store(now_ms(), Ordering::Relaxed);
    let started = Instant::now();
    let mut body = resp.body;
    while !stop.load(Ordering::Relaxed) {
        let Some(f) = read_frame(&mut body).map_err(|e| e.to_string())? else {
            // Clean end of stream: normal after the server's max_life, suspicious if it came right away.
            return if started.elapsed() > Duration::from_secs(2) { Ok(()) } else { Err("stream closed immediately".into()) };
        };
        stat.last_ok.store(now_ms(), Ordering::Relaxed);
        if f.origin == 0 {
            continue; // heartbeat
        }
        let held = host.have().iter().find(|(o, _)| *o == f.origin).map_or(0, |(_, l)| *l);
        if f.at > held {
            return Ok(()); // a gap: our view changed under us, start over with a fresh `have`
        }
        let skip = (held - f.at) as usize;
        if skip >= f.data.len() {
            continue; // already have all of it (another peer was faster)
        }
        let new = &f.data[skip..];
        host.append(f.origin, held, new).map_err(|e| format!("append from {}: {e}", id64(f.origin)))?;
        stat.pulled.fetch_add(new.len() as u64, Ordering::Relaxed);
        kick.notify();
    }
    Ok(())
}

/// Fetches every missing blob from the first peer that has it intact.
fn fetch_missing(host: &dyn Host, t: &Tuning) {
    let missing = host.missing_blobs();
    if missing.is_empty() {
        return;
    }
    let peers = host.peers();
    let me = host.me();
    for (h, len) in missing {
        let target = format!("/ring/blob/{}", hex(&h));
        for (_, url) in peers.iter().filter(|(id, _)| *id != me) {
            let header = sign(&host.ring_key(), me, "GET", &target, now_ms());
            let full = format!("{}{target}", url.trim_end_matches('/'));
            let Ok(mut resp) = courier::request("GET", &full, &[("X-Ring", &header)], None, t.net_timeout) else { continue };
            if resp.status != 200 {
                continue;
            }
            let advertised = resp.headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<u64>().ok());
            if advertised.is_some_and(|n| n != len) {
                continue;
            }
            if host.import_blob(&h, len, &mut resp.body).is_ok() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::sha256;
    use crate::http::{self, Handler};
    use crate::logbook::{self, Log};
    use std::collections::BTreeMap;
    use std::fs;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    const KEY: Hash = [7; 32];

    fn fast() -> Tuning {
        Tuning {
            refresh: Duration::from_millis(100),
            backoff_min: Duration::from_millis(50),
            backoff_max: Duration::from_millis(400),
            blob_every: Duration::from_millis(200),
            net_timeout: Duration::from_secs(5),
        }
    }

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> TmpDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!("esc-ring-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A Star for tests: real Logbooks in a temp dir, blobs as plain files.
    struct TestStar {
        me: u64,
        key: Hash,
        dir: TmpDir,
        logs: Mutex<BTreeMap<u64, Arc<Log>>>,
        grew: (Mutex<()>, Condvar),
        peers: Mutex<Vec<(u64, String)>>,
        wanted: Mutex<Vec<(Hash, u64)>>,
        port: AtomicU64,
        heartbeat: Duration,
    }

    impl TestStar {
        fn new(me: u64, key: Hash) -> Arc<TestStar> {
            let dir = TmpDir::new();
            fs::create_dir_all(dir.0.join("blobs")).unwrap();
            fs::create_dir_all(dir.0.join("log")).unwrap();
            Arc::new(TestStar {
                me,
                key,
                dir,
                logs: Mutex::new(BTreeMap::new()),
                grew: (Mutex::new(()), Condvar::new()),
                peers: Mutex::new(Vec::new()),
                wanted: Mutex::new(Vec::new()),
                port: AtomicU64::new(0),
                heartbeat: Duration::from_millis(300),
            })
        }

        fn log_for(&self, origin: u64) -> io::Result<Arc<Log>> {
            let mut logs = lock(&self.logs);
            if let Some(l) = logs.get(&origin) {
                return Ok(l.clone());
            }
            let (l, _) = Log::open(&self.dir.0.join("log").join(format!("{}.log", id64(origin))), |_, _, _, _| {})?;
            let l = Arc::new(l);
            logs.insert(origin, l.clone());
            Ok(l)
        }

        fn notify(&self) {
            let _g = lock(&self.grew.0);
            self.grew.1.notify_all();
        }

        /// Appends `n` events to our own Logbook.
        fn emit(&self, n: usize) {
            let log = self.log_for(self.me).unwrap();
            for i in 0..n {
                let json = format!("{{\"by\":\"{}\",\"n\":{},\"pad\":\"{}\"}}", id64(self.me), log.end(), "x".repeat(i * 7 % 300));
                log.append(&logbook::encode(now_ms(), 1, json.as_bytes())).unwrap();
            }
            self.notify();
        }

        fn bytes_of(&self, origin: u64) -> Vec<u8> {
            match lock(&self.logs).get(&origin) {
                Some(l) => l.read_raw(0, usize::MAX).unwrap(),
                None => Vec::new(),
            }
        }

        fn blob_path(&self, h: &Hash) -> PathBuf {
            self.dir.0.join("blobs").join(hex(h))
        }

        fn add_blob(&self, data: &[u8]) -> Hash {
            let h = sha256(data);
            fs::write(self.blob_path(&h), data).unwrap();
            h
        }

        /// Serves this Star's ring routes on a fresh port, as the Gate does.
        fn serve(self: &Arc<Self>) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            self.port.store(port as u64, Ordering::Relaxed);
            let host = self.clone();
            let handler: Handler = Arc::new(move |req, conn| {
                let target = if req.query.is_empty() { req.path.clone() } else { format!("{}?{}", req.path, req.query) };
                let ok = req.header("x-ring").and_then(|h| verify(&host.key, h, "GET", &target, now_ms()));
                conn.close = true;
                if ok.is_none() {
                    return conn.send_text(403, "not a star of this ring");
                }
                let mut w = conn.w.try_clone()?;
                if req.path == "/ring/pull" {
                    let _ = conn.r.get_ref().set_read_timeout(None);
                    return serve_pull_with(host.as_ref(), &req.query, &mut w, Duration::from_secs(60), host.heartbeat);
                }
                if let Some(h) = req.path.strip_prefix("/ring/blob/") {
                    return serve_blob(host.as_ref(), h, &mut w);
                }
                conn.send_text(404, "not found")
            });
            thread::spawn(move || http::serve(listener, 64, handler));
            format!("http://127.0.0.1:{port}")
        }

        fn set_peers(&self, peers: &[(&Arc<TestStar>, String)]) {
            *lock(&self.peers) = peers.iter().map(|(s, url)| (s.me, url.clone())).collect();
        }
    }

    impl Host for TestStar {
        fn me(&self) -> u64 {
            self.me
        }
        fn ring_key(&self) -> Hash {
            self.key
        }
        fn have(&self) -> Vec<(u64, u64)> {
            lock(&self.logs).iter().map(|(o, l)| (*o, l.end())).collect()
        }
        fn read(&self, origin: u64, from: u64, max: usize) -> io::Result<Vec<u8>> {
            self.log_for(origin)?.read_raw(from, max)
        }
        fn append(&self, origin: u64, at: u64, bytes: &[u8]) -> io::Result<u64> {
            if origin == self.me {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "own log"));
            }
            let end = self.log_for(origin)?.append_raw(at, bytes)?;
            self.notify();
            Ok(end)
        }
        fn wait(&self, have: &[(u64, u64)], timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut g = lock(&self.grew.0);
            loop {
                let grown = self.have().iter().any(|(o, l)| have.iter().find(|(h, _)| h == o).is_none_or(|(_, hl)| l > hl));
                if grown {
                    return true;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                g = self.grew.1.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
            }
        }
        fn peers(&self) -> Vec<(u64, String)> {
            lock(&self.peers).clone()
        }
        fn missing_blobs(&self) -> Vec<(Hash, u64)> {
            lock(&self.wanted).iter().filter(|(h, _)| !self.blob_path(h).exists()).copied().collect()
        }
        fn import_blob(&self, h: &Hash, len: u64, r: &mut dyn Read) -> io::Result<()> {
            let mut data = Vec::new();
            r.take(len).read_to_end(&mut data)?;
            if data.len() as u64 != len {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if sha256(&data) != *h {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "blob hash mismatch"));
            }
            fs::write(self.blob_path(h), data)
        }
        fn open_blob(&self, h: &Hash) -> io::Result<(fs::File, u64)> {
            let f = fs::File::open(self.blob_path(h))?;
            let len = f.metadata()?.len();
            Ok((f, len))
        }
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < end, "timed out waiting for: {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Collects written bytes; optionally fails after a byte budget.
    #[derive(Clone, Default)]
    struct Sink {
        buf: Arc<Mutex<Vec<u8>>>,
        budget: Option<usize>,
    }
    impl Write for Sink {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            let mut b = lock(&self.buf);
            if self.budget.is_some_and(|n| b.len() + data.len() > n) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
            }
            b.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Splits a served pull stream into (head, frames).
    fn parse_stream(raw: &[u8]) -> (String, Vec<Frame>) {
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("head") + 4;
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut rest = &raw[split..];
        let mut frames = Vec::new();
        while let Some(f) = read_frame(&mut rest).unwrap() {
            frames.push(f);
        }
        (head, frames)
    }

    /// The frames that carry log data (heartbeats dropped).
    fn data_frames(frames: Vec<Frame>) -> Vec<Frame> {
        frames.into_iter().filter(|f| f.origin != 0).collect()
    }

    fn joined(frames: &[Frame], origin: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for f in frames.iter().filter(|f| f.origin == origin) {
            assert_eq!(f.at, out.len() as u64, "frames for an origin must be contiguous from 0");
            out.extend_from_slice(&f.data);
        }
        out
    }

    #[test]
    fn sign_and_verify() {
        let now = 1_700_000_000_000;
        let h = sign(&KEY, 0xABCDEF, "GET", "/ring/pull?h=x:1", now);
        assert!(h.starts_with(&format!("{}.{now}.", id64(0xABCDEF))));
        assert_eq!(verify(&KEY, &h, "GET", "/ring/pull?h=x:1", now), Some(0xABCDEF));
        assert_eq!(verify(&KEY, &h, "GET", "/ring/pull?h=x:1", now + MAX_SKEW_MS), Some(0xABCDEF));
        assert_eq!(verify(&KEY, &h, "GET", "/ring/pull?h=x:1", now - MAX_SKEW_MS), Some(0xABCDEF));
        assert_eq!(verify(&KEY, &h, "GET", "/ring/pull?h=x:1", now + MAX_SKEW_MS + 1), None, "stale");
        assert_eq!(verify(&KEY, &h, "GET", "/ring/pull?h=x:1", now - MAX_SKEW_MS - 1), None, "from the future");
        assert_eq!(verify(&[8; 32], &h, "GET", "/ring/pull?h=x:1", now), None, "wrong key");
        assert_eq!(verify(&KEY, &h, "GET", "/ring/pull?h=x:2", now), None, "tampered target");
        assert_eq!(verify(&KEY, &h, "POST", "/ring/pull?h=x:1", now), None, "tampered method");
        // Tampered id or timestamp.
        let (_, rest) = h.split_once('.').unwrap();
        assert_eq!(verify(&KEY, &format!("{}.{rest}", id64(0xABCDF0)), "GET", "/ring/pull?h=x:1", now), None);
        let mac_part = h.rsplit('.').next().unwrap();
        assert_eq!(verify(&KEY, &format!("{}.{}.{mac_part}", id64(0xABCDEF), now + 1), "GET", "/ring/pull?h=x:1", now), None);
        for junk in ["", ".", "..", "a.b.c", "0000000000000.1.", &format!("{}..x", id64(1)), &format!("{}.-5.AA", id64(1)), "x"] {
            assert_eq!(verify(&KEY, junk, "GET", "/p", now), None, "{junk:?}");
        }
        assert_eq!(verify(&KEY, &format!(" {h} "), "GET", "/ring/pull?h=x:1", now), Some(0xABCDEF), "surrounding spaces tolerated");
    }

    #[test]
    fn frame_round_trip() {
        let f = encode_frame(0x1122_3344_5566_7788, 0xAABB, b"hello");
        assert_eq!(f.len(), FRAME_HDR + 5);
        assert_eq!(&f[..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&f[8..16], &0xAABBu64.to_le_bytes());
        assert_eq!(&f[16..20], &5u32.to_le_bytes());
        let mut two = f.clone();
        two.extend(encode_frame(0, 0, &[]));
        let mut r = &two[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some(Frame { origin: 0x1122_3344_5566_7788, at: 0xAABB, data: b"hello".to_vec() }));
        assert_eq!(read_frame(&mut r).unwrap(), Some(Frame { origin: 0, at: 0, data: vec![] }));
        assert_eq!(read_frame(&mut r).unwrap(), None, "clean end");
        for cut in 1..f.len() {
            assert_eq!(read_frame(&mut &f[..cut]).unwrap_err().kind(), io::ErrorKind::UnexpectedEof, "cut {cut}");
        }
        let mut huge = encode_frame(1, 0, &[]);
        huge[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(read_frame(&mut &huge[..]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn have_parsing() {
        let a = id64(5);
        let b = id64(u64::MAX);
        let q = format!("h={a}:10,{b}:0");
        assert_eq!(parse_have(&q), Some(HashMap::from([(5, 10), (u64::MAX, 0)])));
        assert_eq!(parse_have(&format!("x=1&h={a}%3A7%2C&y=2")), Some(HashMap::from([(5, 7)])), "percent-encoded and extra params");
        assert_eq!(parse_have(""), Some(HashMap::new()));
        assert_eq!(parse_have("h="), Some(HashMap::new()));
        for bad in ["h=zz:1", "h=0000000000005", &format!("h={a}:x"), &format!("h={a}:-1"), &format!("h={a}:1:2")] {
            assert_eq!(parse_have(bad), None, "{bad}");
        }
    }

    #[test]
    fn serve_pull_catch_up_resume_and_live() {
        let s = TestStar::new(10, KEY);
        s.emit(40);
        // A second origin, held as a copy.
        let other = TestStar::new(20, KEY);
        other.emit(25);
        s.append(20, 0, &other.bytes_of(20)).unwrap();
        let (mine, theirs) = (s.bytes_of(10), s.bytes_of(20));
        assert!(!mine.is_empty() && !theirs.is_empty());

        // Full catch-up: nothing known by the puller.
        let sink = Sink::default();
        serve_pull_with(s.as_ref(), "", &mut sink.clone(), Duration::from_millis(100), Duration::from_millis(30)).unwrap();
        let (head, frames) = parse_stream(&lock(&sink.buf));
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Type: application/octet-stream\r\n") && head.contains("Cache-Control: no-store\r\n"));
        assert!(head.contains("Connection: close\r\n") && !head.to_ascii_lowercase().contains("content-length"));
        assert_eq!(joined(&frames, 10), mine);
        assert_eq!(joined(&frames, 20), theirs);
        assert!(frames.iter().any(|f| f.origin == 0 && f.data.is_empty()), "heartbeat frames while idle");

        // Resume from a record boundary of origin 10, origin 20 already complete.
        let cut = s.read(10, 0, 1000).unwrap().len() as u64;
        let q = format!("h={}:{cut},{}:{}", id64(10), id64(20), theirs.len());
        let sink = Sink::default();
        serve_pull_with(s.as_ref(), &q, &mut sink.clone(), Duration::from_millis(50), Duration::from_secs(5)).unwrap();
        let frames = data_frames(parse_stream(&lock(&sink.buf)).1);
        assert!(frames.iter().all(|f| f.origin == 10), "only the missing origin is sent");
        assert_eq!(frames[0].at, cut);
        let mut rebuilt = mine[..cut as usize].to_vec();
        for f in &frames {
            assert_eq!(f.at, rebuilt.len() as u64);
            rebuilt.extend_from_slice(&f.data);
        }
        assert_eq!(rebuilt, mine);

        // A puller that is ahead of us (or equal) gets nothing.
        let q = format!("h={}:{},{}:{}", id64(10), mine.len() + 1000, id64(20), theirs.len());
        let sink = Sink::default();
        serve_pull_with(s.as_ref(), &q, &mut sink.clone(), Duration::from_millis(50), Duration::from_secs(5)).unwrap();
        assert!(data_frames(parse_stream(&lock(&sink.buf)).1).is_empty());

        // Live growth: the stream stays open and delivers events appended later.
        let sink = Sink::default();
        let q = format!("h={}:{},{}:{}", id64(10), mine.len(), id64(20), theirs.len());
        let t = {
            let (s, mut w) = (s.clone(), sink.clone());
            thread::spawn(move || serve_pull_with(s.as_ref(), &q, &mut w, Duration::from_millis(1500), Duration::from_secs(10)))
        };
        thread::sleep(Duration::from_millis(200));
        assert!(lock(&sink.buf).len() < 200, "idle stream sends only the head");
        s.emit(3);
        wait_until("live frames", || lock(&sink.buf).len() > 200);
        s.emit(2);
        let started = Instant::now();
        t.join().unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(4), "max_life ends the stream");
        let frames = data_frames(parse_stream(&lock(&sink.buf)).1);
        let full = s.bytes_of(10);
        assert!(full.len() > mine.len());
        let mut live = mine.clone();
        for f in &frames {
            assert_eq!(f.origin, 10);
            assert_eq!(f.at, live.len() as u64);
            live.extend_from_slice(&f.data);
        }
        assert_eq!(live, full);
    }

    #[test]
    fn serve_pull_stops_on_write_error_and_bad_query() {
        let s = TestStar::new(10, KEY);
        s.emit(30);
        let sink = Sink { budget: Some(PULL_HEAD.len() + 10), ..Sink::default() };
        let r = serve_pull_with(s.as_ref(), "", &mut sink.clone(), Duration::from_secs(30), Duration::from_secs(30));
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        // Heartbeat write failure also ends it.
        let t = TestStar::new(11, KEY);
        let sink = Sink { budget: Some(PULL_HEAD.len() + FRAME_HDR - 1), ..Sink::default() };
        let r = serve_pull_with(t.as_ref(), "", &mut sink.clone(), Duration::from_secs(30), Duration::from_millis(20));
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        let sink = Sink::default();
        serve_pull(s.as_ref(), "h=garbage", &mut sink.clone(), Duration::from_secs(1)).unwrap();
        assert!(String::from_utf8_lossy(&lock(&sink.buf)).starts_with("HTTP/1.1 400 "));
    }

    #[test]
    fn serve_blob_responses() {
        let s = TestStar::new(1, KEY);
        let data = vec![9u8; 100_000];
        let h = s.add_blob(&data);
        let sink = Sink::default();
        serve_blob(s.as_ref(), &hex(&h), &mut sink.clone()).unwrap();
        let raw = lock(&sink.buf).clone();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n") && head.contains("Content-Length: 100000\r\n"), "{head}");
        assert_eq!(&raw[split..], &data[..]);
        for (name, want) in [(hex(&[3; 32]), "404"), ("nothex".to_string(), "400"), (hex(&h)[..62].to_string(), "400")] {
            let sink = Sink::default();
            serve_blob(s.as_ref(), &name, &mut sink.clone()).unwrap();
            assert!(String::from_utf8_lossy(&lock(&sink.buf)).starts_with(&format!("HTTP/1.1 {want} ")), "{name}");
        }
    }

    #[test]
    fn three_stars_converge_over_http() {
        let (a, b, c) = (TestStar::new(101, KEY), TestStar::new(102, KEY), TestStar::new(103, KEY));
        let (ua, ub, uc) = (a.serve(), b.serve(), c.serve());
        // A <-> B, and C only knows B: A's and C's logs must reach each other through B.
        a.set_peers(&[(&b, ub.clone())]);
        b.set_peers(&[(&a, ua.clone()), (&c, uc.clone())]);
        c.set_peers(&[(&b, ub.clone())]);
        a.emit(60);
        b.emit(5);
        for s in [&a, &b, &c] {
            start_with(s.clone(), fast());
        }
        let converged = |want: &[(&Arc<TestStar>, u64)]| {
            want.iter().all(|(s, o)| [&a, &b, &c].iter().all(|x| x.bytes_of(*o) == s.bytes_of(*o) && !s.bytes_of(*o).is_empty()))
        };
        wait_until("initial convergence", || converged(&[(&a, 101), (&b, 102)]));
        // Live: more events at A, then at C, then at B.
        a.emit(7);
        c.emit(4);
        b.emit(3);
        wait_until("live convergence", || converged(&[(&a, 101), (&b, 102), (&c, 103)]));
        for s in [&a, &b, &c] {
            let have = s.have();
            assert_eq!(have.len(), 3, "every Star holds all three origins");
        }
        // Byte-identical, and every copy parses as whole valid records.
        let bytes = a.bytes_of(101);
        let mut rest = &bytes[..];
        let mut n = 0;
        while let Ok(Some((len, ..))) = logbook::decode(rest) {
            rest = &rest[len..];
            n += 1;
        }
        assert!(rest.is_empty());
        assert_eq!(n, 67);
        // Status shows the live peers.
        let st = status();
        let to_b: Vec<&PeerStatus> = st.iter().filter(|p| p.url == ub).collect();
        assert!(to_b.len() >= 2 && to_b.iter().all(|p| p.id == 102 && p.pulled > 0 && p.last_ok > 0), "{st:?}");
        assert!(to_b.iter().any(|p| p.connected && p.error.is_empty()));
    }

    #[test]
    fn redundant_peers_do_not_duplicate_or_diverge() {
        let (a, b, c) = (TestStar::new(201, KEY), TestStar::new(202, KEY), TestStar::new(203, KEY));
        let (ua, ub, uc) = (a.serve(), b.serve(), c.serve());
        a.set_peers(&[(&b, ub.clone()), (&c, uc.clone())]);
        b.set_peers(&[(&a, ua.clone()), (&c, uc.clone())]);
        c.set_peers(&[(&a, ua.clone()), (&b, ub.clone())]);
        a.emit(30);
        for s in [&a, &b, &c] {
            start_with(s.clone(), fast());
        }
        for round in 0..5 {
            let who = [&a, &b, &c][round % 3];
            who.emit(10);
            thread::sleep(Duration::from_millis(60));
        }
        wait_until("full mesh convergence", || {
            [201u64, 202, 203].iter().all(|o| {
                let want = [&a, &b, &c].iter().map(|s| s.bytes_of(*o)).max_by_key(Vec::len).unwrap();
                !want.is_empty() && [&a, &b, &c].iter().all(|s| s.bytes_of(*o) == want)
            })
        });
        let all_ok = |s: &Arc<TestStar>| {
            s.have().iter().all(|(o, len)| {
                let b = s.bytes_of(*o);
                let mut rest = &b[..];
                while let Ok(Some((n, ..))) = logbook::decode(rest) {
                    rest = &rest[n..];
                }
                rest.is_empty() && b.len() as u64 == *len
            })
        };
        assert!(all_ok(&a) && all_ok(&b) && all_ok(&c));
    }

    #[test]
    fn wrong_key_is_refused() {
        let (a, d) = (TestStar::new(301, KEY), TestStar::new(302, [9; 32]));
        let ua = a.serve();
        a.emit(10);
        d.set_peers(&[(&a, ua.clone())]);
        start_with(d.clone(), fast());
        wait_until("error recorded", || status().iter().any(|p| p.url == ua && p.error == "HTTP 403"));
        assert!(d.have().is_empty());
        assert!(!status().iter().find(|p| p.url == ua).unwrap().connected);
    }

    #[test]
    fn peer_changes_start_and_stop_pullers_and_backoff_recovers() {
        let (a, b) = (TestStar::new(401, KEY), TestStar::new(402, KEY));
        // B points at a port nobody listens on yet.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_url = format!("http://127.0.0.1:{}", dead.local_addr().unwrap().port());
        drop(dead);
        a.emit(12);
        b.set_peers(&[(&a, dead_url.clone())]);
        start_with(b.clone(), fast());
        wait_until("connection error", || status().iter().any(|p| p.url == dead_url && !p.error.is_empty()));
        // The peer list changes to the real address: a new puller takes over, the old one disappears.
        let ua = a.serve();
        b.set_peers(&[(&a, ua.clone())]);
        wait_until("recovered", || b.bytes_of(401) == a.bytes_of(401));
        wait_until("old puller gone", || !status().iter().any(|p| p.url == dead_url));
        // Removing the peer stops the puller.
        b.set_peers(&[]);
        wait_until("puller stopped", || !status().iter().any(|p| p.url == ua && p.id == 401));
    }

    #[test]
    fn several_seed_urls_without_ids_all_get_pullers() {
        let (a, c, x) = (TestStar::new(701, KEY), TestStar::new(702, KEY), TestStar::new(703, KEY));
        let (ua, uc) = (a.serve(), c.serve());
        a.emit(15);
        c.emit(9);
        // Seeds: URLs only, Star id still unknown (0), as a freshly joined Star has them.
        *lock(&x.peers) = vec![(0, ua.clone()), (0, uc.clone())];
        start_with(x.clone(), fast());
        wait_until("both seeds pulled", || x.bytes_of(701) == a.bytes_of(701) && x.bytes_of(702) == c.bytes_of(702) && !x.bytes_of(702).is_empty());
        let st = status();
        assert!(st.iter().any(|p| p.url == ua && p.pulled > 0) && st.iter().any(|p| p.url == uc && p.pulled > 0), "{st:?}");
        // A seed that turns out to be a known Star keeps its puller and learns its id.
        *lock(&x.peers) = vec![(701, ua.clone()), (0, uc.clone())];
        wait_until("id learned", || status().iter().any(|p| p.url == ua && p.id == 701));
        assert!(status().iter().any(|p| p.url == ua && p.connected));
    }

    #[test]
    fn blobs_are_fetched_and_verified() {
        let (a, b, c) = (TestStar::new(501, KEY), TestStar::new(502, KEY), TestStar::new(503, KEY));
        let (ua, ub) = (a.serve(), b.serve());
        let data: Vec<u8> = (0..1_500_000usize).map(|i| (i % 251) as u8).collect();
        let good = a.add_blob(&data);
        // B has a poisoned copy under another hash: right name, wrong content.
        let victim = sha256(b"the real content");
        fs::write(b.blob_path(&victim), b"tampered bytes").unwrap();
        // C wants both blobs and a third that nobody has.
        let nobody = sha256(b"nobody has this");
        *lock(&c.wanted) = vec![(good, data.len() as u64), (victim, 16), (nobody, 3)];
        c.set_peers(&[(&b, ub.clone()), (&a, ua.clone())]);
        start_with(c.clone(), fast());
        wait_until("good blob fetched", || c.blob_path(&good).exists());
        assert_eq!(fs::read(c.blob_path(&good)).unwrap(), data);
        thread::sleep(Duration::from_millis(500));
        assert!(!c.blob_path(&victim).exists(), "poisoned blob must be rejected");
        assert!(!c.blob_path(&nobody).exists());
        assert_eq!(c.missing_blobs().len(), 2);
        // Once a peer that really has the content shows up, it is fetched from there.
        let real = a.add_blob(b"the real content");
        assert_eq!(real, victim);
        wait_until("victim fetched from the honest peer", || c.blob_path(&victim).exists());
        assert_eq!(fs::read(c.blob_path(&victim)).unwrap(), b"the real content");
    }

    #[test]
    fn blob_length_mismatch_is_rejected() {
        let (a, c) = (TestStar::new(601, KEY), TestStar::new(602, KEY));
        let ua = a.serve();
        let h = a.add_blob(b"0123456789");
        *lock(&c.wanted) = vec![(h, 99)]; // wrong advertised size
        c.set_peers(&[(&a, ua)]);
        start_with(c.clone(), fast());
        thread::sleep(Duration::from_millis(700));
        assert!(!c.blob_path(&h).exists());
    }

    #[test]
    fn nap_and_kick() {
        let stop = AtomicBool::new(false);
        let t = Instant::now();
        nap(Duration::from_millis(120), &stop);
        assert!(t.elapsed() >= Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        let t = Instant::now();
        nap(Duration::from_secs(10), &stop);
        assert!(t.elapsed() < Duration::from_secs(1));
        let k = Arc::new(Kick::default());
        let k2 = k.clone();
        let t = Instant::now();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            k2.notify();
        });
        k.wait(Duration::from_secs(10));
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
