//! The Ring: placeholder with the agreed API. The real replication module replaces this file.

use std::io;
use std::sync::Arc;
use std::time::Duration;

pub type Hash = [u8; 32];

pub trait Host: Send + Sync + 'static {
    fn me(&self) -> u64;
    fn ring_key(&self) -> Hash;
    fn have(&self) -> Vec<(u64, u64)>;
    fn read(&self, origin: u64, from: u64, max: usize) -> io::Result<Vec<u8>>;
    fn append(&self, origin: u64, at: u64, bytes: &[u8]) -> io::Result<u64>;
    fn wait(&self, have: &[(u64, u64)], timeout: Duration) -> bool;
    fn peers(&self) -> Vec<(u64, String)>;
    fn missing_blobs(&self) -> Vec<(Hash, u64)>;
    fn import_blob(&self, h: &Hash, len: u64, r: &mut dyn io::Read) -> io::Result<()>;
    fn open_blob(&self, h: &Hash) -> io::Result<(std::fs::File, u64)>;
}

pub struct PeerStatus {
    pub id: u64,
    pub url: String,
    pub connected: bool,
    pub last_ok: u64,
    pub pulled: u64,
    pub error: String,
}

pub fn sign(_key: &Hash, _me: u64, _method: &str, _target: &str, _now_ms: u64) -> String {
    String::new()
}

pub fn verify(_key: &Hash, _header: &str, _method: &str, _target: &str, _now_ms: u64) -> Option<u64> {
    None
}

pub fn serve_pull(_host: &dyn Host, _query: &str, _w: &mut dyn io::Write, _max_life: Duration) -> io::Result<()> {
    Ok(())
}

pub fn serve_blob(_host: &dyn Host, _hex: &str, _w: &mut dyn io::Write) -> io::Result<()> {
    Ok(())
}

pub fn start(_host: Arc<dyn Host>) {}

pub fn status() -> Vec<PeerStatus> {
    Vec::new()
}
