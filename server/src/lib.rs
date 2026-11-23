//! ESC relay: HTTP/1.1 + WebSocket server, append-only event log, blob store, relay ring.
//! Rust std only. No crates.
//!
//! Who is who:
//! - `bridge`: the Bridge, where every decision is made (state, rules, Switchboard, Bouncer, Dustin)
//! - `logbook`: the Logbook, append-only event logs
//! - `courier`: the Courier, outbound HTTP to other relays and email through curl
//! - `hold`: the Hold, content-addressed file store and uploads
//! - `gate`: the Gate, every HTTP route and the WebSocket entrance
//! - `lobby`: the Lobby, the embedded web app
//! - `ring`: the Ring, replication between Stars (relays)
//! - `model`: the state every relay rebuilds from the Logbook
//! - `keccak`, `aead`, `p256`, `mlkem`, `webpush`: hand-written crypto for background push (SHA-3, AES-GCM, P-256, ML-KEM)
//! - `http`, `ws`, `json`, `crypto`, `ids`: the plumbing

pub mod bridge;
pub mod aead;
pub mod courier;
pub mod crypto;
pub mod gate;
pub mod hold;
pub mod http;
pub mod ids;
pub mod json;
pub mod keccak;
pub mod lobby;
pub mod logbook;
pub mod p256;
pub mod model;
pub mod ring;
pub mod ws;