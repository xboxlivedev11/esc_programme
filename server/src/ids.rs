//! Identifiers.
//!
//! Account IDs (D5): 4 symbols from a 27-letter alphabet with no vowels (never spells a word) and no
//! look-alikes (no 0/O/1/I/L). 531,441 four-symbol IDs; when that space passes half-full, new IDs
//! get five symbols. An ID is stored as a `u32`: 4-symbol IDs are `0..27^4`, 5-symbol IDs follow.
//!
//! Other IDs (conversations, requests, relays) are random `u64`s written as 13-char base32.

use crate::crypto::random_u64;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ALPHABET: &[u8; 27] = b"23456789BCDFGHJKMNPQRSTVWXZ";
pub const SPACE4: u32 = 531_441; // 27^4
pub const SPACE5: u32 = 14_348_907; // 27^5

/// An account ID such as `K7Q2`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Aid(pub u32);

fn digit(c: u8) -> Option<u32> {
    let c = c.to_ascii_uppercase();
    ALPHABET.iter().position(|&a| a == c).map(|p| p as u32)
}

impl Aid {
    /// Parse user input: case-insensitive, surrounding spaces and `-` ignored.
    pub fn parse(s: &str) -> Option<Aid> {
        let b: Vec<u8> = s.bytes().filter(|c| !matches!(c, b' ' | b'-' | b'#')).collect();
        let mut v = 0u32;
        for &c in &b {
            v = v * 27 + digit(c)?;
        }
        match b.len() {
            4 => Some(Aid(v)),
            5 => Some(Aid(SPACE4 + v)),
            _ => None,
        }
    }

    /// Random ID: 4 symbols while that space is less than half used, else 5.
    pub fn random(used4: usize) -> Aid {
        if (used4 as u32) < SPACE4 / 2 {
            Aid((random_u64() % SPACE4 as u64) as u32)
        } else {
            Aid(SPACE4 + (random_u64() % SPACE5 as u64) as u32)
        }
    }

    pub fn is_short(self) -> bool {
        self.0 < SPACE4
    }
}

impl fmt::Display for Aid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (mut v, n) = if self.0 < SPACE4 { (self.0, 4) } else { (self.0 - SPACE4, 5) };
        let mut out = [0u8; 5];
        for i in (0..n).rev() {
            out[i] = ALPHABET[(v % 27) as usize];
            v /= 27;
        }
        f.write_str(std::str::from_utf8(&out[..n]).unwrap_or("????"))
    }
}

const B32: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// A `u64` as 13 base32 characters (fixed width, sortable).
pub fn id64(v: u64) -> String {
    let mut out = [0u8; 13];
    let mut x = v;
    for i in (0..13).rev() {
        out[i] = B32[(x & 31) as usize];
        x >>= 5;
    }
    String::from_utf8(out.to_vec()).unwrap_or_default()
}

/// Inverse of [`id64`].
pub fn parse_id64(s: &str) -> Option<u64> {
    if s.len() != 13 {
        return None;
    }
    let mut v: u64 = 0;
    for (i, c) in s.bytes().enumerate() {
        let d = B32.iter().position(|&x| x == c)? as u64;
        if i == 0 && d > 15 {
            return None; // would overflow 64 bits
        }
        v = (v << 5) | d;
    }
    Some(v)
}

/// A fresh random non-zero u64 id.
pub fn new_id64() -> u64 {
    loop {
        let v = random_u64();
        if v != 0 {
            return v;
        }
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ids_round_trip() {
        for v in [0, 1, 26, 27, SPACE4 - 1, SPACE4, SPACE4 + 1, SPACE4 + SPACE5 - 1] {
            let s = Aid(v).to_string();
            assert_eq!(Aid::parse(&s), Some(Aid(v)), "{v} -> {s}");
        }
        assert_eq!(Aid(0).to_string(), "2222");
        assert_eq!(Aid(SPACE4 - 1).to_string(), "ZZZZ");
        assert_eq!(Aid(SPACE4).to_string(), "22222");
        assert_eq!(Aid::parse("k7q2"), Aid::parse("K7Q2"));
        assert_eq!(Aid::parse(" #K7-Q2 "), Aid::parse("K7Q2"));
        for bad in ["", "K7Q", "K7Q2X2", "A7Q2", "K0Q2", "K1Q2", "KOQ2", "KIQ2", "KLQ2"] {
            assert_eq!(Aid::parse(bad), None, "{bad}");
        }
        let r = Aid::random(0);
        assert!(r.is_short());
        assert!(!Aid::random(SPACE4 as usize).is_short());
    }

    #[test]
    fn id64_round_trip() {
        for v in [0u64, 1, 31, 32, u64::MAX, 0x0123_4567_89ab_cdef] {
            assert_eq!(parse_id64(&id64(v)), Some(v));
        }
        assert_eq!(id64(0), "0000000000000");
        assert_eq!(parse_id64("zzzzzzzzzzzzz"), None);
        assert_eq!(parse_id64("short"), None);
        assert_eq!(parse_id64("000000000000u"), None);
    }
}
