//! AES-GCM (128 and 256) and HKDF-SHA256, std only.
//!
//! AES uses AES-NI when the CPU has it and a portable implementation otherwise; GHASH is a constant-time
//! bitwise multiply (no tables), which is plenty fast for push-sized payloads (a few KB).

use crate::crypto::{ct_eq, hmac_sha256, hmac_sha256_parts};

// ---------------------------------------------------------------------------------------------
// AES block cipher (encrypt only: GCM never needs the inverse cipher)
// ---------------------------------------------------------------------------------------------

const fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// The AES S-box, derived from its definition: the multiplicative inverse in GF(2^8) (x^254) followed by the
/// affine transform. `sbox_known_entries` pins it to published values.
const SBOX: [u8; 256] = {
    let mut s = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let x = i as u8;
        let mut inv = 1u8;
        let mut sq = x;
        let mut k = 0;
        while k < 7 {
            sq = gmul(sq, sq); // x^2, x^4, ... x^128
            inv = gmul(inv, sq); // 2+4+...+128 = 254
            k += 1;
        }
        let v = if x == 0 { 0 } else { inv };
        s[i] = v ^ v.rotate_left(1) ^ v.rotate_left(2) ^ v.rotate_left(3) ^ v.rotate_left(4) ^ 0x63;
        i += 1;
    }
    s
};

struct Aes {
    rk: [[u8; 16]; 15],
    rounds: usize,
    ni: bool,
}

impl Aes {
    fn new(key: &[u8], force_soft: bool) -> Aes {
        assert!(key.len() == 16 || key.len() == 32, "AES key must be 16 or 32 bytes");
        let nk = key.len() / 4;
        let rounds = nk + 6;
        let mut w = [[0u8; 4]; 60];
        for i in 0..nk {
            w[i].copy_from_slice(&key[4 * i..4 * i + 4]);
        }
        let mut rcon = 1u8;
        for i in nk..4 * (rounds + 1) {
            let mut t = w[i - 1];
            if i % nk == 0 {
                t = [SBOX[t[1] as usize] ^ rcon, SBOX[t[2] as usize], SBOX[t[3] as usize], SBOX[t[0] as usize]];
                rcon = gmul(rcon, 2);
            } else if nk > 6 && i % nk == 4 {
                t = [SBOX[t[0] as usize], SBOX[t[1] as usize], SBOX[t[2] as usize], SBOX[t[3] as usize]];
            }
            for j in 0..4 {
                w[i][j] = w[i - nk][j] ^ t[j];
            }
        }
        let mut rk = [[0u8; 16]; 15];
        for r in 0..=rounds {
            for c in 0..4 {
                rk[r][4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
            }
        }
        Aes { rk, rounds, ni: !force_soft && has_aes_ni() }
    }

    fn encrypt_block(&self, b: &mut [u8; 16]) {
        #[cfg(target_arch = "x86_64")]
        if self.ni {
            // SAFETY: `ni` is only true when the CPU reported AES support at runtime.
            unsafe { encrypt_block_ni(&self.rk, self.rounds, b) };
            return;
        }
        let _ = self.ni;
        self.encrypt_block_soft(b);
    }

    fn encrypt_block_soft(&self, s: &mut [u8; 16]) {
        for (x, k) in s.iter_mut().zip(&self.rk[0]) {
            *x ^= k;
        }
        for r in 1..=self.rounds {
            let mut t = [0u8; 16];
            for c in 0..4 {
                for row in 0..4 {
                    // SubBytes + ShiftRows: row `row` of column `c` comes from column `c + row`
                    t[4 * c + row] = SBOX[s[4 * ((c + row) % 4) + row] as usize];
                }
            }
            if r != self.rounds {
                for c in 0..4 {
                    let a = [t[4 * c], t[4 * c + 1], t[4 * c + 2], t[4 * c + 3]];
                    t[4 * c] = gmul(a[0], 2) ^ gmul(a[1], 3) ^ a[2] ^ a[3];
                    t[4 * c + 1] = a[0] ^ gmul(a[1], 2) ^ gmul(a[2], 3) ^ a[3];
                    t[4 * c + 2] = a[0] ^ a[1] ^ gmul(a[2], 2) ^ gmul(a[3], 3);
                    t[4 * c + 3] = gmul(a[0], 3) ^ a[1] ^ a[2] ^ gmul(a[3], 2);
                }
            }
            for i in 0..16 {
                s[i] = t[i] ^ self.rk[r][i];
            }
        }
    }
}

fn has_aes_ni() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("aes")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "aes")]
unsafe fn encrypt_block_ni(rk: &[[u8; 16]; 15], rounds: usize, b: &mut [u8; 16]) {
    use std::arch::x86_64::{__m128i, _mm_aesenc_si128, _mm_aesenclast_si128, _mm_loadu_si128, _mm_storeu_si128, _mm_xor_si128};
    let load = |k: &[u8; 16]| unsafe { _mm_loadu_si128(k.as_ptr() as *const __m128i) };
    let mut s = _mm_xor_si128(load(b), load(&rk[0]));
    for k in &rk[1..rounds] {
        s = _mm_aesenc_si128(s, load(k));
    }
    s = _mm_aesenclast_si128(s, load(&rk[rounds]));
    unsafe { _mm_storeu_si128(b.as_mut_ptr() as *mut __m128i, s) };
}

// ---------------------------------------------------------------------------------------------
// GCM
// ---------------------------------------------------------------------------------------------

/// Multiplication in GF(2^128) as GCM defines it (bit 0 is the most significant bit), constant time.
fn gf_mul(x: u128, y: u128) -> u128 {
    let mut z = 0u128;
    let mut v = y;
    for i in 0..128 {
        z ^= v & 0u128.wrapping_sub((x >> (127 - i)) & 1);
        v = (v >> 1) ^ ((0xe1u128 << 120) & 0u128.wrapping_sub(v & 1));
    }
    z
}

fn ghash_blocks(y: &mut u128, h: u128, data: &[u8]) {
    for chunk in data.chunks(16) {
        let mut b = [0u8; 16];
        b[..chunk.len()].copy_from_slice(chunk);
        *y = gf_mul(*y ^ u128::from_be_bytes(b), h);
    }
}

fn counter_block(nonce: &[u8; 12], ctr: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..12].copy_from_slice(nonce);
    b[12..].copy_from_slice(&ctr.to_be_bytes());
    b
}

fn ctr_xor(aes: &Aes, nonce: &[u8; 12], data: &mut [u8]) {
    let mut ctr = 2u32;
    for chunk in data.chunks_mut(16) {
        let mut ks = counter_block(nonce, ctr);
        aes.encrypt_block(&mut ks);
        for (d, k) in chunk.iter_mut().zip(ks) {
            *d ^= k;
        }
        ctr = ctr.wrapping_add(1);
    }
}

fn tag(aes: &Aes, h: u128, nonce: &[u8; 12], aad: &[u8], ct: &[u8]) -> [u8; 16] {
    let mut y = 0u128;
    ghash_blocks(&mut y, h, aad);
    ghash_blocks(&mut y, h, ct);
    let lens = (((aad.len() as u128) * 8) << 64) | ((ct.len() as u128) * 8);
    y = gf_mul(y ^ lens, h);
    let mut j0 = counter_block(nonce, 1);
    aes.encrypt_block(&mut j0);
    (y ^ u128::from_be_bytes(j0)).to_be_bytes()
}

fn hash_key(aes: &Aes) -> u128 {
    let mut z = [0u8; 16];
    aes.encrypt_block(&mut z);
    u128::from_be_bytes(z)
}

/// Seals `plaintext`: returns ciphertext followed by the 16-byte tag. `key` is 16 or 32 bytes (else panics).
/// Never reuse a nonce under the same key.
pub fn aes_gcm_seal(key: &[u8], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    seal_with(false, key, nonce, aad, plaintext)
}

/// Opens what `aes_gcm_seal` made; `None` if anything (key, nonce, aad, ciphertext, tag) is wrong.
pub fn aes_gcm_open(key: &[u8], nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    open_with(false, key, nonce, aad, sealed)
}

fn seal_with(force_soft: bool, key: &[u8], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let aes = Aes::new(key, force_soft);
    let h = hash_key(&aes);
    let mut out = Vec::with_capacity(plaintext.len() + 16);
    out.extend_from_slice(plaintext);
    ctr_xor(&aes, nonce, &mut out);
    let t = tag(&aes, h, nonce, aad, &out);
    out.extend_from_slice(&t);
    out
}

fn open_with(force_soft: bool, key: &[u8], nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 16 {
        return None;
    }
    let (ct, got) = sealed.split_at(sealed.len() - 16);
    let aes = Aes::new(key, force_soft);
    let h = hash_key(&aes);
    if !ct_eq(&tag(&aes, h, nonce, aad, ct), got) {
        return None;
    }
    let mut out = ct.to_vec();
    ctr_xor(&aes, nonce, &mut out);
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// HKDF (RFC 5869) with HMAC-SHA256
// ---------------------------------------------------------------------------------------------

/// Fills `out` (at most 255 * 32 bytes, else panics) with HKDF-SHA256 output. An empty `salt` means 32 zero bytes.
pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) {
    assert!(out.len() <= 255 * 32, "HKDF output too long");
    let zero = [0u8; 32];
    let prk = hmac_sha256(if salt.is_empty() { &zero } else { salt }, ikm);
    let mut t = [0u8; 32];
    let mut prev = 0;
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        t = hmac_sha256_parts(&prk, &[&t[..prev], info, &[(i + 1) as u8]]);
        chunk.copy_from_slice(&t[..chunk.len()]);
        prev = 32;
    }
}

#[cfg(test)]
mod sbox_tests {
    use super::SBOX;

    #[test]
    fn sbox_known_entries() {
        // FIPS 197 figure 7
        assert_eq!([SBOX[0x00], SBOX[0x01], SBOX[0x53], SBOX[0xff], SBOX[0x10]], [0x63, 0x7c, 0xed, 0x16, 0xca]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::random_bytes;

    fn unhex(s: &str) -> Vec<u8> {
        if s == "-" {
            return Vec::new();
        }
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }
    fn nonce(s: &str) -> [u8; 12] {
        unhex(s).try_into().unwrap()
    }
    const VECTORS: &str = include_str!("../tests/vectors/aead.txt");

    #[test]
    fn gcm_matches_the_dotnet_oracle_on_both_paths() {
        let mut n = 0;
        for line in VECTORS.lines().filter(|l| l.starts_with("gcm ")) {
            let f: Vec<&str> = line.split(' ').collect();
            let (key, iv, aad, pt, want) = (unhex(f[1]), nonce(f[2]), unhex(f[3]), unhex(f[4]), unhex(f[5]));
            for soft in [false, true] {
                assert_eq!(seal_with(soft, &key, &iv, &aad, &pt), want, "seal soft={soft} line {n}");
                assert_eq!(open_with(soft, &key, &iv, &aad, &want).as_deref(), Some(&pt[..]), "open soft={soft} line {n}");
            }
            n += 1;
        }
        assert_eq!(n, 66);
    }

    #[test]
    fn gcm_published_nist_cases_2_4_and_16() {
        // McGrew-Viega GCM spec, test cases 2, 4 and 16
        let z = [0u8; 16];
        let s = aes_gcm_seal(&z, &[0; 12], b"", &z);
        assert_eq!(s, unhex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"));
        let iv = nonce("cafebabefacedbaddecaf888");
        let pt = unhex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39");
        let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let k128 = unhex("feffe9928665731c6d6a8f9467308308");
        let want4 = unhex("42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e0915bc94fbc3221a5db94fae95ae7121a47");
        assert_eq!(aes_gcm_seal(&k128, &iv, &aad, &pt), want4);
        let k256 = unhex("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308");
        let want16 = unhex("522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f66276fc6ece0f4e1768cddf8853bb2d551b");
        assert_eq!(aes_gcm_seal(&k256, &iv, &aad, &pt), want16);
    }

    #[test]
    fn gcm_open_rejects_any_flipped_bit() {
        let key = [7u8; 32];
        let iv = [9u8; 12];
        let sealed = aes_gcm_seal(&key, &iv, b"header", b"attack at dawn, bring snacks");
        assert!(aes_gcm_open(&key, &iv, b"header", &sealed).is_some());
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 0x01;
            assert!(aes_gcm_open(&key, &iv, b"header", &bad).is_none(), "flip byte {i}");
        }
        assert!(aes_gcm_open(&key, &iv, b"Header", &sealed).is_none(), "aad");
        let mut iv2 = iv;
        iv2[11] ^= 1;
        assert!(aes_gcm_open(&key, &iv2, b"header", &sealed).is_none(), "nonce");
        assert!(aes_gcm_open(&[8u8; 32], &iv, b"header", &sealed).is_none(), "key");
    }

    #[test]
    fn gcm_open_rejects_input_shorter_than_a_tag() {
        for n in 0..16 {
            assert!(aes_gcm_open(&[1u8; 16], &[0; 12], b"", &vec![0u8; n]).is_none());
        }
    }

    #[test]
    fn gcm_fast_and_portable_paths_agree_on_random_inputs() {
        for len in 0..300usize {
            let mut key = [0u8; 32];
            let mut iv = [0u8; 12];
            let mut pt = vec![0u8; len];
            let mut aad = vec![0u8; len % 41];
            random_bytes(&mut key);
            random_bytes(&mut iv);
            random_bytes(&mut pt);
            random_bytes(&mut aad);
            let klen = if len % 2 == 0 { 16 } else { 32 };
            assert_eq!(seal_with(false, &key[..klen], &iv, &aad, &pt), seal_with(true, &key[..klen], &iv, &aad, &pt), "len {len}");
        }
    }

    #[test]
    fn hkdf_matches_the_dotnet_oracle() {
        let mut n = 0;
        for line in VECTORS.lines().filter(|l| l.starts_with("hkdf ")) {
            let f: Vec<&str> = line.split(' ').collect();
            let (salt, ikm, info, l, want) = (unhex(f[1]), unhex(f[2]), unhex(f[3]), f[4].parse::<usize>().unwrap(), unhex(f[5]));
            let mut out = vec![0u8; l];
            hkdf_sha256(&salt, &ikm, &info, &mut out);
            assert_eq!(out, want, "hkdf line {n}");
            n += 1;
        }
        assert_eq!(n, 7);
    }

    #[test]
    fn hkdf_rfc5869_cases_1_and_3() {
        let mut out = [0u8; 42];
        hkdf_sha256(&unhex("000102030405060708090a0b0c"), &[0x0b; 22], &unhex("f0f1f2f3f4f5f6f7f8f9"), &mut out);
        assert_eq!(out.to_vec(), unhex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"));
        hkdf_sha256(b"", &[0x0b; 22], b"", &mut out);
        assert_eq!(out.to_vec(), unhex("8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"));
    }

    #[test]
    #[should_panic]
    fn hkdf_refuses_more_than_255_blocks() {
        let mut out = vec![0u8; 255 * 32 + 1];
        hkdf_sha256(b"", b"ikm", b"", &mut out);
    }
}
