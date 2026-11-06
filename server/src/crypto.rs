//! Cryptographic primitives, std only: OS CSPRNG, SHA-1, SHA-256 (scalar + x86_64 SHA-NI),
//! HMAC-SHA256, PBKDF2-HMAC-SHA256, CRC-32, base64/base64url and hex.

use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------------
// OS CSPRNG
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod os_rng {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(alg: *mut core::ffi::c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
    }

    pub fn fill(buf: &mut [u8]) {
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
        for chunk in buf.chunks_mut(u32::MAX as usize) {
            // SAFETY: `chunk` is a valid writable region of exactly `len` bytes; no algorithm handle is used.
            let status = unsafe {
                BCryptGenRandom(
                    core::ptr::null_mut(),
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                )
            };
            assert!(status == 0, "BCryptGenRandom failed: NTSTATUS {status:#010x}");
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod os_rng {
    unsafe extern "C" {
        fn getrandom(buf: *mut core::ffi::c_void, len: usize, flags: u32) -> isize;
    }

    pub fn fill(mut buf: &mut [u8]) {
        while !buf.is_empty() {
            // SAFETY: `buf` is a valid writable region of exactly `buf.len()` bytes.
            let n = unsafe { getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n > 0 {
                buf = &mut buf[n as usize..];
            } else {
                let err = std::io::Error::last_os_error();
                if n < 0 && err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                panic!("getrandom failed: {err}");
            }
        }
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
mod os_rng {
    unsafe extern "C" {
        fn getentropy(buf: *mut core::ffi::c_void, len: usize) -> core::ffi::c_int;
    }

    pub fn fill(buf: &mut [u8]) {
        for chunk in buf.chunks_mut(256) {
            // SAFETY: `chunk` is a valid writable region of at most 256 bytes (the getentropy limit).
            let rc = unsafe { getentropy(chunk.as_mut_ptr().cast(), chunk.len()) };
            assert!(rc == 0, "getentropy failed: {}", std::io::Error::last_os_error());
        }
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
compile_error!("crypto: no OS CSPRNG binding for this target");

/// Fills `buf` from the OS CSPRNG; panics if the OS fails.
pub fn random_bytes(buf: &mut [u8]) {
    os_rng::fill(buf);
}

/// Returns a uniformly random `u64` from the OS CSPRNG.
pub fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    random_bytes(&mut b);
    u64::from_ne_bytes(b)
}

// ---------------------------------------------------------------------------------------------
// SHA-1 (scalar)
// ---------------------------------------------------------------------------------------------

fn sha1_compress(state: &mut [u32; 5], blocks: &[u8]) {
    for block in blocks.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (wi, c) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *wi = u32::from_be_bytes(*c);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = *state;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d, e]) {
            *s = s.wrapping_add(v);
        }
    }
}

/// SHA-1 digest of `data` (scalar; only for the WebSocket handshake).
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut st = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let (full, rest) = data.split_at(data.len() & !63);
    sha1_compress(&mut st, full);
    let (tail, n) = pad_tail(rest, data.len() as u64);
    sha1_compress(&mut st, &tail[..n]);
    let mut out = [0u8; 20];
    for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(st) {
        *o = s.to_be_bytes();
    }
    out
}

/// MD-style padding of the final partial block (< 64 bytes): returns the tail buffer and its used length (64 or 128).
fn pad_tail(rest: &[u8], total_len: u64) -> ([u8; 128], usize) {
    let mut tail = [0u8; 128];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let n = if rest.len() < 56 { 64 } else { 128 };
    tail[n - 8..n].copy_from_slice(&total_len.wrapping_mul(8).to_be_bytes());
    (tail, n)
}

// ---------------------------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------------------------

static K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H256: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

fn sha256_compress_scalar(state: &mut [u32; 8], blocks: &[u8]) {
    for block in blocks.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (wi, c) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *wi = u32::from_be_bytes(*c);
        }
        for i in 16..64 {
            let (x, y) = (w[i - 15], w[i - 2]);
            let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
            let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for (k, wi) in K256.iter().zip(&w) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(*k).wrapping_add(*wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (h, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }
}

/// SHA-256 block function using the x86 SHA extensions (Intel reference structure).
///
/// # Safety
/// The CPU must support `sha`, `sse2`, `ssse3` and `sse4.1`; `blocks.len()` must be a multiple of 64.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
#[allow(unused_assignments)] // the schedule updates in the last rounds of a block are dead by design
unsafe fn sha256_compress_ni(state: &mut [u32; 8], blocks: &[u8]) {
    use std::arch::x86_64::*;
    // SAFETY: all pointer accesses stay inside `state` (8 words), `K256` (64 words) and the 64-byte `block`.
    unsafe {
        let mask = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b, 0x0405_0607_0001_0203);
        let tmp = _mm_loadu_si128(state.as_ptr().cast());
        let mut st1 = _mm_loadu_si128(state.as_ptr().add(4).cast());
        let tmp = _mm_shuffle_epi32::<0xB1>(tmp); // CDAB
        st1 = _mm_shuffle_epi32::<0x1B>(st1); // EFGH
        let mut st0 = _mm_alignr_epi8::<8>(tmp, st1); // ABEF
        st1 = _mm_blend_epi16::<0xF0>(st1, tmp); // CDGH

        for block in blocks.as_chunks::<64>().0 {
            let (abef, cdgh) = (st0, st1);
            let p = block.as_ptr();
            let mut w = [
                _mm_shuffle_epi8(_mm_loadu_si128(p.cast()), mask),
                _mm_shuffle_epi8(_mm_loadu_si128(p.add(16).cast()), mask),
                _mm_shuffle_epi8(_mm_loadu_si128(p.add(32).cast()), mask),
                _mm_shuffle_epi8(_mm_loadu_si128(p.add(48).cast()), mask),
            ];
            // Four rounds per group `$i`; the message schedule rotates through `w`.
            macro_rules! group {
                ($i:literal) => {{
                    let k = _mm_loadu_si128(K256.as_ptr().add(4 * $i).cast());
                    let mut msg = _mm_add_epi32(w[$i % 4], k);
                    st1 = _mm_sha256rnds2_epu32(st1, st0, msg);
                    if (3..15).contains(&$i) {
                        let t = _mm_alignr_epi8::<4>(w[$i % 4], w[($i + 3) % 4]);
                        w[($i + 1) % 4] = _mm_sha256msg2_epu32(_mm_add_epi32(w[($i + 1) % 4], t), w[$i % 4]);
                    }
                    msg = _mm_shuffle_epi32::<0x0E>(msg);
                    st0 = _mm_sha256rnds2_epu32(st0, st1, msg);
                    if (1..13).contains(&$i) {
                        w[($i + 3) % 4] = _mm_sha256msg1_epu32(w[($i + 3) % 4], w[$i % 4]);
                    }
                }};
            }
            group!(0);
            group!(1);
            group!(2);
            group!(3);
            group!(4);
            group!(5);
            group!(6);
            group!(7);
            group!(8);
            group!(9);
            group!(10);
            group!(11);
            group!(12);
            group!(13);
            group!(14);
            group!(15);
            st0 = _mm_add_epi32(st0, abef);
            st1 = _mm_add_epi32(st1, cdgh);
        }

        let tmp = _mm_shuffle_epi32::<0x1B>(st0); // FEBA
        st1 = _mm_shuffle_epi32::<0xB1>(st1); // DCHG
        st0 = _mm_blend_epi16::<0xF0>(tmp, st1); // DCBA
        st1 = _mm_alignr_epi8::<8>(st1, tmp); // HGFE
        _mm_storeu_si128(state.as_mut_ptr().cast(), st0);
        _mm_storeu_si128(state.as_mut_ptr().add(4).cast(), st1);
    }
}

/// Whether the SHA-NI path may be used on this CPU (detected once, cached).
#[cfg(target_arch = "x86_64")]
fn has_sha_ni() -> bool {
    static NI: OnceLock<bool> = OnceLock::new();
    *NI.get_or_init(|| {
        is_x86_feature_detected!("sha")
            && is_x86_feature_detected!("sse2")
            && is_x86_feature_detected!("ssse3")
            && is_x86_feature_detected!("sse4.1")
    })
}

/// Whether the SHA-NI path may be used on this CPU (never, off x86_64).
#[cfg(not(target_arch = "x86_64"))]
fn has_sha_ni() -> bool {
    false
}

/// Compresses whole 64-byte blocks with the selected implementation.
fn sha256_compress(ni: bool, state: &mut [u32; 8], blocks: &[u8]) {
    debug_assert!(blocks.len().is_multiple_of(64));
    #[cfg(target_arch = "x86_64")]
    if ni {
        // SAFETY: `ni` is only ever true after `has_sha_ni()` confirmed the required CPU features.
        unsafe { sha256_compress_ni(state, blocks) };
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = ni;
    sha256_compress_scalar(state, blocks);
}

/// Streaming SHA-256 hasher.
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
    ni: bool,
}

impl Sha256 {
    /// Creates a hasher using the fastest implementation available on this CPU.
    pub fn new() -> Self {
        Self::with_impl(has_sha_ni())
    }

    /// Creates a hasher with an explicit implementation (`true` = SHA-NI, which must be supported).
    fn with_impl(ni: bool) -> Self {
        assert!(!ni || has_sha_ni(), "SHA-NI requested but not supported by this CPU");
        Sha256 { state: H256, buf: [0; 64], buf_len: 0, total: 0, ni }
    }

    /// Absorbs `data`; full blocks are compressed straight from the input without copying.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 64 {
                return;
            }
            sha256_compress(self.ni, &mut self.state, &self.buf);
            self.buf_len = 0;
        }
        let (full, rest) = data.split_at(data.len() & !63);
        if !full.is_empty() {
            sha256_compress(self.ni, &mut self.state, full);
        }
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    /// Finishes the hash and returns the 32-byte digest.
    pub fn finalize(mut self) -> [u8; 32] {
        let (tail, n) = pad_tail(&self.buf[..self.buf_len], self.total);
        sha256_compress(self.ni, &mut self.state, &tail[..n]);
        let mut out = [0u8; 32];
        for (o, s) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *o = s.to_be_bytes();
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

// ---------------------------------------------------------------------------------------------
// HMAC-SHA256 / PBKDF2
// ---------------------------------------------------------------------------------------------

/// HMAC-SHA256 key with the inner/outer padded-key hash states precomputed.
struct HmacKey {
    inner: Sha256,
    outer: Sha256,
}

impl HmacKey {
    fn new(key: &[u8]) -> Self {
        let mut block = [0u8; 64];
        if key.len() > 64 {
            block[..32].copy_from_slice(&sha256(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let (mut inner, mut outer) = (Sha256::new(), Sha256::new());
        inner.update(&block.map(|b| b ^ 0x36));
        outer.update(&block.map(|b| b ^ 0x5c));
        HmacKey { inner, outer }
    }

    /// MAC of the concatenation of `parts`.
    fn mac(&self, parts: &[&[u8]]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        for p in parts {
            inner.update(p);
        }
        let mut outer = self.outer.clone();
        outer.update(&inner.finalize());
        outer.finalize()
    }
}

/// HMAC-SHA256 (RFC 2104) of `data` under `key`.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    hmac_sha256_parts(key, &[data])
}

/// HMAC-SHA256 over the concatenation of `parts`, without allocating.
pub fn hmac_sha256_parts(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    HmacKey::new(key).mac(parts)
}

/// PBKDF2-HMAC-SHA256 (RFC 8018) filling `out`; panics if `iterations` is 0 or `out` exceeds 2^32-1 blocks.
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    assert!(iterations >= 1, "pbkdf2: iterations must be at least 1");
    let key = HmacKey::new(password);
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let index = u32::try_from(i + 1).expect("pbkdf2: output too long");
        let mut u = key.mac(&[salt, &index.to_be_bytes()]);
        let mut t = u;
        for _ in 1..iterations {
            u = key.mac(&[&u]);
            for (a, b) in t.iter_mut().zip(u) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

// ---------------------------------------------------------------------------------------------
// CRC-32 (IEEE, slicing-by-8)
// ---------------------------------------------------------------------------------------------

static CRC_TABLE: [[u32; 256]; 8] = make_crc_table();

const fn make_crc_table() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 8 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

/// CRC-32 (IEEE 802.3) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0, data)
}

/// Continues a CRC-32 from a previous result: `crc32_update(crc32(a), b) == crc32(a ++ b)`.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let t = &CRC_TABLE;
    let mut c = !crc;
    let (chunks, remainder) = data.as_chunks::<8>();
    for ch in chunks {
        let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
        let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        c = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][((hi >> 8) & 0xFF) as usize]
            ^ t[1][((hi >> 16) & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in remainder {
        c = t[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

// ---------------------------------------------------------------------------------------------
// Constant-time compare, base64, hex
// ---------------------------------------------------------------------------------------------

/// Compares two byte strings in constant time (for equal lengths); unequal lengths give `false`.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode_with(data: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        let chars = ch.len() + 1; // 1 -> 2 chars, 2 -> 3 chars, 3 -> 4 chars
        for i in 0..4 {
            if i < chars {
                out.push(alphabet[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Base64 (RFC 4648 standard alphabet) with `=` padding.
pub fn b64_encode(data: &[u8]) -> String {
    b64_encode_with(data, B64_STD, true)
}

/// Base64url (RFC 4648 URL-safe alphabet) without padding.
pub fn b64url_encode(data: &[u8]) -> String {
    b64_encode_with(data, B64_URL, false)
}

/// Decodes unpadded base64url; `None` on invalid character, impossible length or non-zero trailing bits.
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 2);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

/// Lowercase hexadecimal encoding.
pub fn hex(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for &b in data {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// Decodes hexadecimal (either case); `None` on odd length or invalid character.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let s = s.as_bytes();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_chunks::<2>().0.iter().map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?)).collect()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const ABC448: &[u8] = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";

    /// Deterministic xorshift64* filler so failures are reproducible.
    fn fill_det(seed: &mut u64, buf: &mut [u8]) {
        for b in buf {
            *seed ^= *seed >> 12;
            *seed ^= *seed << 25;
            *seed ^= *seed >> 27;
            *b = (seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8;
        }
    }

    fn million_a_sha256(ni: bool) -> [u8; 32] {
        let mut h = Sha256::with_impl(ni);
        let chunk = [b'a'; 1000];
        for _ in 0..1000 {
            h.update(&chunk);
        }
        h.finalize()
    }

    #[test]
    fn sha1_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(ABC448)), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        assert_eq!(hex(&sha1(&vec![b'a'; 1_000_000])), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha256(ABC448)), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        let million = "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";
        assert_eq!(hex(&sha256(&vec![b'a'; 1_000_000])), million);
        assert_eq!(hex(&million_a_sha256(false)), million);
        if has_sha_ni() {
            assert_eq!(hex(&million_a_sha256(true)), million);
        }
    }

    #[test]
    fn sha256_ni_matches_scalar() {
        if !has_sha_ni() {
            println!("note: CPU lacks SHA-NI; skipping the SHA-NI half of sha256_ni_matches_scalar");
            return;
        }
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut lens: Vec<usize> = (0..=300).collect();
        lens.extend([1023, 1024, 1025, 4096, 65_535, 65_536, 1_000_003]);
        for len in lens {
            let mut data = vec![0u8; len];
            fill_det(&mut seed, &mut data);
            let (mut scalar, mut ni) = (Sha256::with_impl(false), Sha256::with_impl(true));
            scalar.update(&data);
            ni.update(&data);
            let want = scalar.finalize();
            assert_eq!(ni.finalize(), want, "one-shot mismatch at len {len}");
            // Random split points through the streaming API, on both paths.
            for _ in 0..4 {
                let (mut a, mut b) = (Sha256::with_impl(false), Sha256::with_impl(true));
                let mut rest = &data[..];
                while !rest.is_empty() {
                    let mut r = [0u8; 2];
                    fill_det(&mut seed, &mut r);
                    let cut = (u16::from_le_bytes(r) as usize % 200).min(rest.len());
                    a.update(&rest[..cut]);
                    b.update(&rest[..cut]);
                    rest = &rest[cut..];
                    if cut == 0 && rest.len() < 3 {
                        a.update(rest);
                        b.update(rest);
                        break;
                    }
                }
                assert_eq!(a.finalize(), want, "scalar split mismatch at len {len}");
                assert_eq!(b.finalize(), want, "ni split mismatch at len {len}");
            }
        }
    }

    #[test]
    fn hmac_rfc4231() {
        let h = |key: &[u8], data: &[u8]| hex(&hmac_sha256(key, data));
        assert_eq!(
            h(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            h(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            h(&[0xaa; 20], &[0xdd; 50]),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        let key4: Vec<u8> = (1..=25).collect();
        assert_eq!(
            h(&key4, &[0xcd; 50]),
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b"
        );
        // Case 5: truncated to the first 128 bits.
        assert_eq!(hex(&hmac_sha256(&[0x0c; 20], b"Test With Truncation")[..16]), "a3b6167473100ee06e0c796c2955552b");
        assert_eq!(
            h(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First"),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            h(
                &[0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than block-size data. \
                  The key needs to be hashed before being used by the HMAC algorithm."
            ),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    #[test]
    fn hmac_parts_equals_concatenation() {
        let (key, a, b, c) = (b"key", &b"hello "[..], &b"wor"[..], &b"ld"[..]);
        assert_eq!(hmac_sha256_parts(key, &[a, b, c]), hmac_sha256(key, b"hello world"));
        assert_eq!(hmac_sha256_parts(key, &[]), hmac_sha256(key, b""));
    }

    #[test]
    fn pbkdf2_vectors() {
        let mut out = [0u8; 64];
        pbkdf2_hmac_sha256(b"passwd", b"salt", 1, &mut out);
        assert_eq!(
            hex(&out),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
        pbkdf2_hmac_sha256(b"Password", b"NaCl", 80_000, &mut out);
        assert_eq!(
            hex(&out),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56\
             a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"
        );
        let mut out = [0u8; 32];
        for (c, want) in [
            (1, "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"),
            (2, "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"),
            (4096, "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"),
        ] {
            pbkdf2_hmac_sha256(b"password", b"salt", c, &mut out);
            assert_eq!(hex(&out), want, "c = {c}");
        }
        // Odd output lengths are prefixes of the longer derivation.
        let (mut long, mut short) = ([0u8; 64], [0u8; 45]);
        pbkdf2_hmac_sha256(b"password", b"salt", 3, &mut long);
        pbkdf2_hmac_sha256(b"password", b"salt", 3, &mut short);
        assert_eq!(&long[..45], &short[..]);
    }

    #[test]
    fn crc32_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let mut data = vec![0u8; 5000];
        fill_det(&mut 12345, &mut data);
        let whole = crc32(&data);
        for split in [0, 1, 7, 8, 9, 100, 4999, 5000] {
            let (a, b) = data.split_at(split);
            assert_eq!(crc32_update(crc32(a), b), whole, "split {split}");
            assert_eq!(crc32_update(crc32_update(0, a), b), whole);
        }
        assert_eq!(crc32_update(0, &data), whole);
    }

    #[test]
    fn base64_rfc4648() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, enc) in cases {
            assert_eq!(b64_encode(plain.as_bytes()), enc);
            assert_eq!(b64url_encode(plain.as_bytes()), enc.trim_end_matches('='));
            assert_eq!(b64url_decode(enc.trim_end_matches('=')).unwrap(), plain.as_bytes());
        }
        assert_eq!(b64_encode(&[0xfb, 0xff, 0xfe]), "+//+");
        assert_eq!(b64url_encode(&[0xfb, 0xff, 0xfe]), "-__-");
    }

    #[test]
    fn base64url_round_trip_and_rejects() {
        let mut seed = 42u64;
        for len in 0..200 {
            let mut data = vec![0u8; len];
            fill_det(&mut seed, &mut data);
            let enc = b64url_encode(&data);
            assert!(!enc.contains(['=', '+', '/']));
            assert_eq!(b64url_decode(&enc).unwrap(), data);
        }
        assert_eq!(b64url_decode("Zg"), Some(b"f".to_vec()));
        assert_eq!(b64url_decode("Zh"), None, "non-zero trailing bits");
        assert_eq!(b64url_decode("Zm9"), None, "non-zero trailing bits");
        assert_eq!(b64url_decode("Zm9vY"), None, "length % 4 == 1");
        assert_eq!(b64url_decode("Zg=="), None, "padding is not accepted");
        assert_eq!(b64url_decode("Zm+v"), None, "standard alphabet char");
        assert_eq!(b64url_decode("Zm/v"), None, "standard alphabet char");
        assert_eq!(b64url_decode("Zm 9"), None);
        assert_eq!(b64url_decode("Zm9\u{e9}"), None);
        assert_eq!(b64url_decode("Zm9v\n"), None);
    }

    #[test]
    fn hex_round_trip_and_rejects() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(unhex("000fa5ff").unwrap(), [0x00, 0x0f, 0xa5, 0xff]);
        assert_eq!(unhex("000FA5Ff").unwrap(), [0x00, 0x0f, 0xa5, 0xff]);
        assert_eq!(unhex("").unwrap(), Vec::<u8>::new());
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("0g"), None);
        assert_eq!(unhex("0x"), None);
        assert_eq!(unhex("\u{e9}\u{e9}"), None);
        assert_eq!(unhex(" 0"), None);
    }

    #[test]
    fn ct_eq_behaviour() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"", b"a"));
    }

    #[test]
    fn websocket_accept_rfc6455() {
        let digest = sha1(b"dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        assert_eq!(b64_encode(&digest), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn random_fills_buffers() {
        for len in [0usize, 1, 1000, 100_000] {
            let mut buf = vec![0u8; len];
            random_bytes(&mut buf);
            if len >= 1000 {
                assert!(buf.iter().any(|&b| b != 0), "len {len} still all zero");
            }
        }
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        random_bytes(&mut a);
        random_bytes(&mut b);
        assert_ne!(a, b);
        assert_ne!(random_u64(), random_u64());
    }

    #[test]
    #[ignore = "benchmark; run with --ignored --nocapture"]
    fn sha256_throughput() {
        const MIB: usize = 1 << 20;
        const TOTAL_MIB: usize = 256;
        let mut data = vec![0u8; MIB];
        fill_det(&mut 7, &mut data);
        let mut paths = vec![("scalar", false)];
        if has_sha_ni() {
            paths.push(("SHA-NI", true));
        } else {
            println!("note: CPU lacks SHA-NI; benchmarking scalar only");
        }
        for (name, ni) in paths {
            let mut h = Sha256::with_impl(ni);
            let start = Instant::now();
            for _ in 0..TOTAL_MIB {
                h.update(std::hint::black_box(&data));
            }
            let digest = h.finalize();
            let secs = start.elapsed().as_secs_f64();
            println!("sha256 {name:>6}: {:8.1} MB/s ({TOTAL_MIB} MiB, digest {})", (TOTAL_MIB * MIB) as f64 / 1e6 / secs, &hex(&digest)[..8]);
        }
    }
}
