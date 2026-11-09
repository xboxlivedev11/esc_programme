//! The Gate: every HTTP route and the WebSocket entrance.
//!
//! - `/` and `/a/*`: the Lobby (the web app)
//! - `/api/*`: sign-in, sign-up, uploads (JSON / binary over plain HTTP)
//! - `/f/<file>/<o|t|p>/<name>`: your files (original, thumbnail, preview), Range-capable
//! - `/ws`: the live channel. Every other action is a JSON RPC over it.
//! - `/ring/*`: the Ring (relay replication), signed with the ring key

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

use crate::bridge::{Bridge, Client, Fail, MAX_PIC, Out, R};
use crate::crypto::{b64url_decode, hex};
use crate::http::{self, Conn, Request, pct_encode};
use crate::ids::{Aid, now_ms};
use crate::json::{self, Js, Json};
use crate::lobby::Lobby;
use crate::model::Hash;
use crate::ws::{self, Msg};

pub fn handler(bridge: Arc<Bridge>, lobby: Arc<Lobby>) -> http::Handler {
    Arc::new(move |req, conn| route(&bridge, &lobby, req, conn))
}

/// Browsers always send `Origin` on cross-site writes and WebSocket handshakes: it must be us.
fn same_origin(req: &Request) -> bool {
    let Some(origin) = req.header("origin") else { return true };
    let host = req.header("host").unwrap_or("");
    let o = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    o.eq_ignore_ascii_case(host)
}

fn route(b: &Arc<Bridge>, lobby: &Lobby, req: &Request, conn: &mut Conn) -> io::Result<()> {
    let p = req.path.as_str();
    let m = req.method.as_str();
    if (!matches!(m, "GET" | "HEAD") || p == "/ws") && !same_origin(req) {
        conn.close = true;
        return conn.send_text(403, "cross-site request refused");
    }
    match (m, p) {
        ("GET", "/ws") => websocket(b, req, conn),
        ("POST", "/api/pre") => {
            let r = body_json(conn).map(|j| format!("{{\"s\":\"{}\"}}", b.pre(j.s("id").unwrap_or(""))));
            reply(conn, r)
        }
        ("GET", "/api/me") => match session(b, req) {
            Some((a, _)) => conn.send_json(200, &format!("{{\"id\":\"{a}\"}}")),
            None => unauthorized(conn),
        },
        ("GET", "/api/info") => {
            let r = b.info();
            reply(conn, r)
        }
        ("POST", "/api/signup") => signup(b, req, conn),
        ("POST", "/api/login") => login(b, req, conn),
        ("POST", "/api/lifeline/code") => {
            let label = ua_label(req.header("user-agent").unwrap_or(""));
            let ip = req.client_ip();
            let r = body_json(conn).and_then(|j| {
                b.recover_code(j.s("id").unwrap_or(""), j.s("code").unwrap_or(""), &b64(j.s("s")), &b64(j.s("k")), &label, ip)
            });
            signed_in(req, conn, r)
        }
        ("POST", "/api/lifeline/email") => {
            let base = format!("{}://{}", if req.https() { "https" } else { "http" }, req.header("host").unwrap_or("localhost"));
            let ip = req.client_ip();
            let r = body_json(conn).and_then(|j| b.recover_email(j.s("id").unwrap_or(""), &base, ip));
            reply(conn, r)
        }
        ("POST", "/api/lifeline/token") => {
            let label = ua_label(req.header("user-agent").unwrap_or(""));
            let r = body_json(conn).and_then(|j| b.recover_token(j.s("t").unwrap_or(""), &b64(j.s("s")), &b64(j.s("k")), &label));
            signed_in(req, conn, r)
        }
        ("POST", "/api/logout") => {
            if let Some((_, h)) = session(b, req) {
                let _ = b.logout(&h);
            }
            conn.send(200, "application/json", &[("Set-Cookie", &clear_cookie()), ("Cache-Control", "no-store")], b"{}")
        }
        ("POST", "/api/up") => {
            let Some((a, _)) = session(b, req) else { return unauthorized(conn) };
            let r = body_json(conn).and_then(|j| b.upload_begin(a, j.u("z").ok_or_else(|| Fail(400, "size?".into()))?));
            reply(conn, r)
        }
        ("PUT", "/api/pic") => {
            let Some((a, _)) = session(b, req) else { return unauthorized(conn) };
            if req.body_len > MAX_PIC as u64 {
                conn.close = true;
                return conn.send_json(413, &err_json(413, "Pictures can be at most 256 KB."));
            }
            let r = conn
                .read_body(MAX_PIC)
                .map_err(|_| Fail(400, "The picture upload was cut short.".into()))
                .and_then(|body| b.set_pic(a, req.param("c").as_deref().filter(|c| !c.is_empty()), &body));
            reply(conn, r)
        }
        ("PUT", _) if p.starts_with("/api/up/") => {
            let Some((a, _)) = session(b, req) else { return unauthorized(conn) };
            if req.body_len > 16 << 20 {
                conn.close = true;
                return conn.send_json(413, "{\"e\":\"chunk too large\"}");
            }
            let off: u64 = req.param("o").and_then(|v| v.parse().ok()).unwrap_or(0);
            let len = req.body_len;
            let r = b.upload_put(a, &p[8..], off, len, &mut conn.body());
            if conn.body_left > 0 {
                conn.close = true;
            }
            reply(conn, r)
        }
        ("GET", _) if p.starts_with("/api/up/") => {
            let Some((a, _)) = session(b, req) else { return unauthorized(conn) };
            let r = b.upload_status(a, &p[8..]);
            reply(conn, r)
        }
        ("GET" | "HEAD", _) if p.starts_with("/f/") => file(b, req, conn),
        ("GET" | "HEAD", _) if p.starts_with("/pic/") => picture(b, req, conn),
        ("GET", _) if p.starts_with("/ring/") => ring(b, req, conn),
        ("POST", "/ring/join") => {
            let r = body_json(conn).and_then(|j| b.ring_join(&j));
            reply(conn, r)
        }
        ("GET" | "HEAD", _) => lobby.serve(req, conn),
        _ => conn.send_text(405, "method not allowed"),
    }
}

fn unauthorized(conn: &mut Conn) -> io::Result<()> {
    conn.send_json(401, "{\"e\":\"Sign in first.\"}")
}

fn body_json(conn: &mut Conn) -> R<Json> {
    let body = conn.read_body(64 * 1024).map_err(|_| Fail(413, "Request too large.".into()))?;
    json::parse_bytes(&body).map_err(|_| Fail(400, "Bad JSON.".into()))
}

fn err_json(code: u16, msg: &str) -> String {
    if msg.starts_with('{') {
        return msg.to_string(); // already structured (e.g. upload offsets)
    }
    let mut js = Js::new();
    js.obj().ks("e", msg).ku("c", code as u64).end_obj();
    js.finish()
}

fn reply(conn: &mut Conn, r: R<String>) -> io::Result<()> {
    match r {
        Ok(s) => conn.send_json(200, &s),
        Err(Fail(code, msg)) => conn.send_json(code, &err_json(code, &msg)),
    }
}

fn session(b: &Bridge, req: &Request) -> Option<(Aid, Hash)> {
    b.auth(req.cookie("esc")?)
}

fn set_cookie(req: &Request, token: &str) -> String {
    format!(
        "esc={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000{}",
        if req.https() { "; Secure" } else { "" }
    )
}

fn clear_cookie() -> String {
    "esc=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0".into()
}

fn b64(s: Option<&str>) -> Vec<u8> {
    s.and_then(b64url_decode).unwrap_or_default()
}

/// "Chrome on Windows"-style label for the device list.
pub fn ua_label(ua: &str) -> String {
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if ua.contains("curl/") {
        "curl"
    } else {
        "Browser"
    };
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "macOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "somewhere"
    };
    format!("{browser} on {os}")
}

/// Answer a successful sign-in-like call by setting the session cookie.
fn signed_in(req: &Request, conn: &mut Conn, r: R<(Aid, String)>) -> io::Result<()> {
    match r {
        Ok((id, token)) => {
            let body = format!("{{\"id\":\"{id}\"}}");
            conn.send(200, "application/json", &[("Set-Cookie", &set_cookie(req, &token)), ("Cache-Control", "no-store")], body.as_bytes())
        }
        Err(Fail(code, msg)) => conn.send_json(code, &err_json(code, &msg)),
    }
}

fn signup(b: &Bridge, req: &Request, conn: &mut Conn) -> io::Result<()> {
    let ip = req.client_ip().to_string();
    if !b.bouncer(5, &ip, 3.0, 10.0) {
        return conn.send_json(429, &err_json(429, "Too many sign-ups from here. Try later."));
    }
    let r = body_json(conn).and_then(|j| {
        let label = ua_label(req.header("user-agent").unwrap_or(""));
        b.signup(j.s("t").unwrap_or(""), &b64(j.s("s")), &b64(j.s("k")), j.s("n").unwrap_or(""), &label)
    });
    match r {
        Ok((id, token)) => {
            let body = format!("{{\"id\":\"{id}\"}}");
            conn.send(200, "application/json", &[("Set-Cookie", &set_cookie(req, &token)), ("Cache-Control", "no-store")], body.as_bytes())
        }
        Err(Fail(code, msg)) => conn.send_json(code, &err_json(code, &msg)),
    }
}

fn login(b: &Bridge, req: &Request, conn: &mut Conn) -> io::Result<()> {
    let r = body_json(conn).and_then(|j| {
        let label = ua_label(req.header("user-agent").unwrap_or(""));
        b.login(j.s("id").unwrap_or(""), &b64(j.s("k")), &label, req.client_ip())
    });
    match r {
        Ok((id, token)) => {
            let body = format!("{{\"id\":\"{id}\"}}");
            conn.send(200, "application/json", &[("Set-Cookie", &set_cookie(req, &token)), ("Cache-Control", "no-store")], body.as_bytes())
        }
        Err(Fail(code, msg)) => conn.send_json(code, &err_json(code, &msg)),
    }
}

// ------------------------------------------------------------------------------------------------
// Files
// ------------------------------------------------------------------------------------------------

/// Only plain MIME tokens survive; anything odd becomes a download.
fn safe_mime(m: &str) -> &str {
    let ok = !m.is_empty()
        && m.len() <= 100
        && m.contains('/')
        && m.bytes().all(|c| c.is_ascii_alphanumeric() || b"!#$&^_.+-/".contains(&c));
    if ok { m } else { "application/octet-stream" }
}

/// Types a browser shows without running anything of ours.
fn inline_ok(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    matches!(
        m.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/jxl"
            | "image/heic"
            | "image/heif"
            | "application/pdf"
            | "text/plain"
    ) || m.starts_with("video/")
        || m.starts_with("audio/")
}

/// The real image type from a file's first bytes (the file position is restored).
fn sniff_image(f: &mut File) -> &'static str {
    let mut head = [0u8; 12];
    let n = f.read(&mut head).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(0));
    match &head[..n] {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P'] => "image/webp",
        _ => "application/octet-stream",
    }
}

/// `GET /pic/u/<callsign>` and `GET /pic/c/<conversation id>`: profile and crew pictures.
fn picture(b: &Bridge, req: &Request, conn: &mut Conn) -> io::Result<()> {
    let Some((a, _)) = session(b, req) else { return conn.send_text(401, "Sign in first.") };
    let Some((blob, _)) = req.path[5..].split_once('/').and_then(|(what, id)| b.pic_for(a, what, id)) else {
        return conn.send_text(404, "No such picture.");
    };
    let Ok((mut f, len)) = b.store.open_blob(&blob) else { return conn.send_text(404, "No such picture.") };
    let etag = format!("\"{}\"", &hex(&blob)[..20]);
    let cache = "private, max-age=31536000, immutable";
    if req.header("if-none-match") == Some(etag.as_str()) {
        return conn.send(304, "", &[("ETag", &etag), ("Cache-Control", cache)], b"");
    }
    let ctype = sniff_image(&mut f);
    let hdrs = [
        ("X-Content-Type-Options", "nosniff"),
        ("Cache-Control", cache),
        ("Content-Security-Policy", "sandbox; default-src 'none'"),
        ("Cross-Origin-Resource-Policy", "same-origin"),
    ];
    conn.send_file(req, &mut f, len, ctype, Some(&etag), &hdrs)
}

fn file(b: &Bridge, req: &Request, conn: &mut Conn) -> io::Result<()> {
    let Some((a, _)) = session(b, req) else { return conn.send_text(401, "Sign in first.") };
    let mut parts = req.path[3..].splitn(3, '/');
    let (Some(orig), which) = (parts.next(), parts.next().unwrap_or("o")) else {
        return conn.send_text(404, "not found");
    };
    let Some((blob, mime, name)) = b.blob_for(a, orig, which) else {
        return conn.send_text(404, "That file is not yours, or it was recycled.");
    };
    let (mut f, len) = match b.store.open_blob(&blob) {
        Ok(x) => x,
        Err(_) => return conn.send_text(503, "That file is still on its way to this relay. Try again in a moment."),
    };
    let etag = format!("\"{}\"", &hex(&blob)[..20]);
    if req.header("if-none-match") == Some(etag.as_str()) {
        return conn.send(304, "", &[("ETag", &etag), ("Cache-Control", "private, max-age=31536000, immutable")], b"");
    }
    // Thumbnails and previews are whatever the sender's browser could encode (WebP, or JPEG/PNG on
    // browsers without a WebP encoder): read their real type from the first bytes.
    let sniffed;
    let mime = if which == "o" {
        safe_mime(&mime)
    } else {
        sniffed = sniff_image(&mut f);
        sniffed
    };
    let inline = which != "o" || (inline_ok(mime) && req.param("dl").is_none());
    let disp = format!("{}; filename*=UTF-8''{}", if inline { "inline" } else { "attachment" }, pct_encode(&name));
    let ctype = if mime.starts_with("text/") { format!("{mime}; charset=utf-8") } else { mime.to_string() };
    let mut hdrs: Vec<(&str, &str)> = vec![
        ("Content-Disposition", &disp),
        ("X-Content-Type-Options", "nosniff"),
        ("Cache-Control", "private, max-age=31536000, immutable"),
        ("Cross-Origin-Resource-Policy", "same-origin"),
        ("Referrer-Policy", "no-referrer"),
    ];
    // PDF viewers refuse to run in a sandboxed document; everything else gets the sandbox,
    // so even a hostile file opened in its own tab can never act as ESC.
    if mime != "application/pdf" {
        hdrs.push((
            "Content-Security-Policy",
            "sandbox; default-src 'none'; img-src 'self' data: blob:; media-src 'self' blob:; style-src 'unsafe-inline'",
        ));
    }
    conn.send_file(req, &mut f, len, &ctype, Some(&etag), &hdrs)
}

// ------------------------------------------------------------------------------------------------
// The Ring
// ------------------------------------------------------------------------------------------------

fn ring(b: &Arc<Bridge>, req: &Request, conn: &mut Conn) -> io::Result<()> {
    let target = if req.query.is_empty() { req.path.clone() } else { format!("{}?{}", req.path, req.query) };
    let ok = req.header("x-ring").and_then(|h| crate::ring::verify(&b.ring_key, h, "GET", &target, now_ms()));
    if ok.is_none() {
        conn.close = true;
        return conn.send_text(403, "not a star of this ring");
    }
    conn.close = true;
    if req.path == "/ring/pull" {
        let host: Arc<dyn crate::ring::Host> = b.clone();
        let mut w = conn.w.try_clone()?;
        let _ = conn.r.get_ref().set_read_timeout(None);
        return crate::ring::serve_pull(host.as_ref(), &req.query, &mut w, Duration::from_secs(600));
    }
    if let Some(h) = req.path.strip_prefix("/ring/blob/") {
        let host: Arc<dyn crate::ring::Host> = b.clone();
        let mut w = conn.w.try_clone()?;
        return crate::ring::serve_blob(host.as_ref(), h, &mut w);
    }
    conn.send_text(404, "not found")
}

// ------------------------------------------------------------------------------------------------
// The live channel
// ------------------------------------------------------------------------------------------------

fn websocket(b: &Arc<Bridge>, req: &Request, conn: &mut Conn) -> io::Result<()> {
    let Some((aid, sess)) = session(b, req) else { return unauthorized(conn) };
    let upgrade = req.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let (Some(key), true, Some("13")) = (req.header("sec-websocket-key"), upgrade, req.header("sec-websocket-version")) else {
        conn.close = true;
        return conn.send_text(400, "websocket handshake expected");
    };
    let accept = ws::accept_key(key.trim());
    conn.w.write_all(
        format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")
            .as_bytes(),
    )?;
    conn.upgraded = true;
    // Browsers answer our pings, so silence for this long means the client is gone.
    conn.r.get_ref().set_read_timeout(Some(Duration::from_secs(70)))?;
    conn.w.set_write_timeout(Some(Duration::from_secs(20)))?;

    let (client, rx) = Client::new(aid, sess);
    b.board.add(client.clone());
    let mut w = conn.w.try_clone()?;
    let wc = client.clone();
    let _ = thread::Builder::new().name("ws-out".into()).stack_size(128 * 1024).spawn(move || {
        loop {
            let frame = match rx.recv_timeout(Duration::from_secs(25)) {
                Ok(Out::Text(t)) => ws::frame(ws::OP_TEXT, t.as_bytes()),
                Ok(Out::Pong(p)) => ws::frame(ws::OP_PONG, &p),
                Ok(Out::Close) | Err(RecvTimeoutError::Disconnected) => {
                    let _ = w.write_all(&ws::close_frame(1000, "bye"));
                    let _ = w.shutdown(Shutdown::Both);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => ws::frame(ws::OP_PING, b""),
            };
            if w.write_all(&frame).is_err() {
                wc.alive.store(false, Ordering::Relaxed);
                let _ = w.shutdown(Shutdown::Both);
                break;
            }
        }
    });

    let mut rd = ws::Reader::new(1 << 20);
    while client.alive.load(Ordering::Relaxed) {
        match rd.read(&mut conn.r) {
            Ok(Msg::Text(t)) => {
                let out = rpc(b, aid, sess, &t);
                if !client.queue(Out::Text(Arc::from(out))) {
                    break;
                }
            }
            Ok(Msg::Ping(p)) => {
                client.queue(Out::Pong(p));
            }
            Ok(Msg::Pong(_)) | Ok(Msg::Binary(_)) => {}
            Ok(Msg::Close(..)) | Err(_) => break,
        }
    }
    b.board.remove(&client);
    client.alive.store(false, Ordering::Relaxed);
    client.queue(Out::Close);
    let _ = conn.w.shutdown(Shutdown::Read);
    Ok(())
}

/// One RPC: `{"i":7,"m":"send",...}` -> `{"i":7,"r":...}` or `{"i":7,"e":"...","c":code}`.
pub fn rpc(b: &Bridge, a: Aid, sess: Hash, text: &str) -> String {
    let j = match json::parse(text) {
        Ok(j) => j,
        Err(_) => return "{\"e\":\"bad json\",\"c\":400}".into(),
    };
    let id = j.u("i").unwrap_or(0);
    match dispatch(b, a, sess, &j) {
        Ok(r) => format!("{{\"i\":{id},\"r\":{r}}}"),
        Err(Fail(code, msg)) => {
            let mut js = Js::new();
            js.obj().ku("i", id).ku("c", code as u64).ks("e", &msg).end_obj();
            js.finish()
        }
    }
}

fn dispatch(b: &Bridge, a: Aid, sess: Hash, j: &Json) -> R<String> {
    if !b.bouncer(4, &a.to_string(), 900.0, 300.0) {
        return Err(Fail(429, "Slow down a little.".into()));
    }
    let s = |k: &str| j.s(k).unwrap_or("");
    let admin = || if b.is_admin(a) { Ok(()) } else { Err(Fail(403, "Control Tower is for admins.".into())) };
    match s("m") {
        "hello" => b.hello(a),
        "hist" => b.history(a, s("c"), j.u("b"), j.u("n").unwrap_or(50) as usize),
        "send" => b.send(a, s("c"), s("x"), j.a("a"), j.u("k")),
        "read" => b.read(a, s("c"), j.u("t").unwrap_or(0)),
        "knock" => b.req_send(a, s("to"), s("x")),
        "knock_cancel" => b.req_cancel(a, s("r")),
        "answer" => b.req_answer(a, s("r"), s("a")),
        "crew_new" => b.group_new(a, s("n"), j.a("ids")),
        "crew_invite" => b.group_invite(a, s("c"), j.a("ids")),
        "crew_answer" => b.group_answer(a, s("c"), j.b("ok").unwrap_or(false)),
        "crew_leave" => b.group_leave(a, s("c")),
        "crew_remove" => b.group_remove(a, s("c"), s("a")),
        "crew_set" => b.group_set(a, s("c"), j),
        "me_set" => b.me_set(a, j),
        "block" => b.block(a, s("a"), j.b("on").unwrap_or(true)),
        "nick" => b.nick(a, s("a"), s("n")),
        "password" => b.password(a, sess, &b64(j.s("old")), &b64(j.s("s")), &b64(j.s("k"))),
        "lifeline" => b.lifeline_set(a, j),
        "devices" => b.sessions(a, sess),
        "device_end" => b.session_end(a, s("h")),
        "files" => b.files(a),
        "file_op" => b.file_op(a, s("o"), j.a("b")),
        "stash_add" => b.drive_add(a, j.get("f").unwrap_or(&Json::Null)),
        "tickets" => b.my_invites(a),
        "ticket_new" => b.invite_create(a, j.u("u").unwrap_or(1), j.u("d").unwrap_or(7), j.b("adm").unwrap_or(false)),
        "ticket_del" => {
            admin()?;
            b.invite_delete(s("c"))
        }
        "tower" => {
            admin()?;
            b.admin_overview()
        }
        "tower_set" => {
            admin()?;
            b.admin_set(a, j)
        }
        "tower_account" => {
            admin()?;
            b.admin_account(a, j)
        }
        "pic_clear" => b.clear_pic(a, j.s("c").filter(|c| !c.is_empty())),
        "tower_spark" => {
            admin()?;
            b.spark_create()
        }
        "tower_mail_test" => {
            admin()?;
            b.mail_test(a, s("to"))
        }
        _ => Err(Fail(400, "unknown method".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_mimes() {
        assert_eq!(
            ua_label("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36"),
            "Chrome on Windows"
        );
        assert_eq!(ua_label("Mozilla/5.0 (iPhone; CPU iPhone OS 19_0 like Mac OS X) AppleWebKit Version/19.0 Mobile Safari/604.1"), "Safari on iPhone");
        assert_eq!(safe_mime("image/png"), "image/png");
        assert_eq!(safe_mime("text/html\r\nX-Evil: 1"), "application/octet-stream");
        assert!(!inline_ok("image/svg+xml"));
        assert!(!inline_ok("text/html"));
        assert!(inline_ok("video/mp4"));
    }
}
