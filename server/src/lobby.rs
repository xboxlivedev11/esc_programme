//! The Lobby: the web app, embedded in the binary at compile time (one file to copy per Star).
//! `--web <dir>` serves the files from disk instead, for development (no caching, edit + reload).
//!
//! Cache strategy: `index.html` is never cached; it references `/a/<file>?v=<content hash>`, and
//! those exact URLs are cached forever (immutable). A new build changes the hash, so browsers
//! always get the new app with zero stale-cache risk.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::crypto::{hex, sha256};
use crate::http::{Conn, Request};

const FILES: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../../web/index.html")),
    ("app.js", include_bytes!("../../web/app.js")),
    ("app.css", include_bytes!("../../web/app.css")),
    ("sw.js", include_bytes!("../../web/sw.js")),
    ("manifest.webmanifest", include_bytes!("../../web/manifest.webmanifest")),
    ("icon.svg", include_bytes!("../../web/icon.svg")),
];

/// The app shell's security policy: nothing but this origin, no inline script, no framing.
pub const CSP: &str = "default-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; style-src 'self'; \
script-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; frame-ancestors 'none'; \
base-uri 'none'; form-action 'none'; object-src 'none'";

struct Asset {
    body: Vec<u8>,
    tag: String,
}

pub struct Lobby {
    assets: HashMap<&'static str, Asset>,
    index: Vec<u8>,
    dir: Option<PathBuf>,
}

fn ctype(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "webmanifest" => "application/manifest+json",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

impl Lobby {
    pub fn new(dir: Option<PathBuf>) -> Lobby {
        let mut assets = HashMap::new();
        for (name, body) in FILES {
            let tag = hex(&sha256(body))[..12].to_string();
            assets.insert(*name, Asset { body: body.to_vec(), tag });
        }
        let mut index = String::from_utf8_lossy(&assets["index.html"].body).into_owned();
        for (name, a) in &assets {
            index = index.replace(&format!("/a/{name}\""), &format!("/a/{name}?v={}\"", a.tag));
        }
        Lobby { assets, index: index.into_bytes(), dir }
    }

    /// Size of the whole embedded app, for the startup banner.
    pub fn total_bytes(&self) -> usize {
        self.assets.values().map(|a| a.body.len()).sum()
    }

    pub fn serve(&self, req: &Request, conn: &mut Conn) -> io::Result<()> {
        let path = req.path.as_str();
        let name = match path {
            "/" | "/index.html" => "index.html",
            "/sw.js" => "sw.js",
            "/manifest.webmanifest" => "manifest.webmanifest",
            "/icon.svg" => "icon.svg",
            p => match p.strip_prefix("/a/") {
                Some(n) => n,
                None => return conn.send_text(404, "not found"),
            },
        };
        if let Some(dir) = &self.dir {
            // Development: straight from disk, never cached.
            if name.contains('/') || name.contains('\\') || name.starts_with('.') {
                return conn.send_text(404, "not found");
            }
            return match fs::read(dir.join(name)) {
                Ok(body) => conn.send(200, ctype(name), &headers(name, "no-store"), &body),
                Err(_) => conn.send_text(404, "not found"),
            };
        }
        let Some(asset) = self.assets.get(name) else { return conn.send_text(404, "not found") };
        let etag = format!("\"{}\"", asset.tag);
        if name == "index.html" {
            return conn.send(200, ctype(name), &headers(name, "no-cache"), &self.index);
        }
        let versioned = req.param("v").is_some_and(|v| v == asset.tag);
        let cache = if versioned { "public, max-age=31536000, immutable" } else { "no-cache" };
        if req.header("if-none-match") == Some(etag.as_str()) {
            return conn.send(304, "", &[("ETag", &etag), ("Cache-Control", cache)], b"");
        }
        let mut h = headers(name, cache);
        h.push(("ETag", &etag));
        conn.send(200, ctype(name), &h, &asset.body)
    }
}

fn headers<'a>(name: &str, cache: &'a str) -> Vec<(&'a str, &'a str)> {
    let mut h = vec![("Cache-Control", cache), ("X-Content-Type-Options", "nosniff")];
    if name == "index.html" {
        h.push(("Content-Security-Policy", CSP));
        h.push(("Referrer-Policy", "no-referrer"));
        h.push(("Cross-Origin-Opener-Policy", "same-origin"));
        h.push(("Permissions-Policy", "camera=(), microphone=(), geolocation=(), interest-cohort=()"));
    }
    h
}
