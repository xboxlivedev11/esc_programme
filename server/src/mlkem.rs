//! ML-KEM-768 encapsulation (FIPS 203). The relay only encapsulates; the browser holds the decapsulation key,
//! so key generation and decapsulation are deliberately not in this binary.
//!
//! Parameters for ML-KEM-768: k = 3, eta1 = eta2 = 2, d_u = 10, d_v = 4, q = 3329.
//! Secret-dependent work (noise sampling, NTT, compression) is branch-free and indexes only with public values;
//! the only data-dependent loop is rejection sampling of the public matrix from the public seed rho.
//! Hand-written, no external audit; see the design doc's risk section.

use crate::crypto::random_bytes;
use crate::keccak::{Shake, sha3_256, sha3_512};

pub const EK_LEN: usize = 1184;
pub const CT_LEN: usize = 1088;
const K: usize = 3;
const Q: u32 = 3329;
type Poly = [u16; 256];

const fn bitrev7(x: usize) -> usize {
    let (mut r, mut b) = (0, 0);
    while b < 7 {
        if (x >> b) & 1 == 1 {
            r |= 1 << (6 - b);
        }
        b += 1;
    }
    r
}

const fn pow_mod(mut base: u32, mut e: u32) -> u32 {
    let mut r = 1;
    base %= Q;
    while e > 0 {
        if e & 1 == 1 {
            r = r * base % Q;
        }
        base = base * base % Q;
        e >>= 1;
    }
    r
}

/// zeta^BitRev7(i) with zeta = 17, the 256th root of unity mod q.
const ZETAS: [u16; 128] = {
    let mut z = [0u16; 128];
    let mut i = 0;
    while i < 128 {
        z[i] = pow_mod(17, bitrev7(i) as u32) as u16;
        i += 1;
    }
    z
};

/// zeta^(2*BitRev7(i)+1): the factor for the i-th pair in NTT-domain multiplication.
const GAMMAS: [u16; 128] = {
    let mut g = [0u16; 128];
    let mut i = 0;
    while i < 128 {
        g[i] = pow_mod(17, (2 * bitrev7(i) + 1) as u32) as u16;
        i += 1;
    }
    g
};

/// Forward number-theoretic transform (FIPS 203 Algorithm 9).
fn ntt(f: &mut Poly) {
    let mut k = 1;
    let mut len = 128;
    while len >= 2 {
        let mut start = 0;
        while start < 256 {
            let z = u32::from(ZETAS[k]);
            k += 1;
            for j in start..start + len {
                let t = z * u32::from(f[j + len]) % Q;
                f[j + len] = ((u32::from(f[j]) + Q - t) % Q) as u16;
                f[j] = ((u32::from(f[j]) + t) % Q) as u16;
            }
            start += 2 * len;
        }
        len >>= 1;
    }
}

/// Inverse transform (Algorithm 10), including the final multiplication by 128^-1 = 3303.
fn inv_ntt(f: &mut Poly) {
    let mut k = 127usize;
    let mut len = 2;
    while len <= 128 {
        let mut start = 0;
        while start < 256 {
            let z = u32::from(ZETAS[k]);
            k = k.wrapping_sub(1);
            for j in start..start + len {
                let t = u32::from(f[j]);
                let u = u32::from(f[j + len]);
                f[j] = ((t + u) % Q) as u16;
                f[j + len] = (z * ((u + Q - t) % Q) % Q) as u16;
            }
            start += 2 * len;
        }
        len <<= 1;
    }
    for x in f.iter_mut() {
        *x = (u32::from(*x) * 3303 % Q) as u16;
    }
}

/// `acc += a * b` in the NTT domain (Algorithms 11 and 12, pairwise "base case" products).
fn mul_acc(acc: &mut Poly, a: &Poly, b: &Poly) {
    for i in 0..128 {
        let g = u32::from(GAMMAS[i]);
        let (a0, a1) = (u32::from(a[2 * i]), u32::from(a[2 * i + 1]));
        let (b0, b1) = (u32::from(b[2 * i]), u32::from(b[2 * i + 1]));
        let c0 = (a0 * b0 + (a1 * b1 % Q) * g) % Q;
        let c1 = (a0 * b1 + a1 * b0) % Q;
        acc[2 * i] = ((u32::from(acc[2 * i]) + c0) % Q) as u16;
        acc[2 * i + 1] = ((u32::from(acc[2 * i + 1]) + c1) % Q) as u16;
    }
}

fn compress(x: u16, d: u32) -> u16 {
    // round(2^d * x / q) mod 2^d; q is odd, so a tie is impossible and this integer form is exact
    ((((u32::from(x) << d) + Q / 2) / Q) & ((1 << d) - 1)) as u16
}


/// ByteEncode_d: packs 256 d-bit values little-endian into `32 * d` bytes.
fn byte_encode(f: &Poly, d: usize, out: &mut [u8]) {
    debug_assert_eq!(out.len(), 32 * d);
    out.fill(0);
    for (i, &v) in f.iter().enumerate() {
        for b in 0..d {
            let at = i * d + b;
            out[at / 8] |= (((v >> b) & 1) as u8) << (at % 8);
        }
    }
}

/// ByteDecode_d without any reduction (values are below 2^d).
fn byte_decode(bytes: &[u8], d: usize) -> Poly {
    debug_assert_eq!(bytes.len(), 32 * d);
    let mut f = [0u16; 256];
    for (i, v) in f.iter_mut().enumerate() {
        let mut x = 0u16;
        for b in 0..d {
            let at = i * d + b;
            x |= u16::from((bytes[at / 8] >> (at % 8)) & 1) << b;
        }
        *v = x;
    }
    f
}

/// FIPS 203 section 7.2: the key is 1184 bytes and every 12-bit coefficient of t-hat is below q.
pub fn check_ek(ek: &[u8]) -> bool {
    if ek.len() != EK_LEN {
        return false;
    }
    (0..K).all(|i| byte_decode(&ek[384 * i..384 * (i + 1)], 12).iter().all(|&c| u32::from(c) < Q))
}

/// Algorithm 7: the public matrix entry from the public seed, by rejection sampling.
fn sample_ntt(rho: &[u8], j: u8, i: u8) -> Poly {
    let mut x = Shake::shake128();
    x.absorb(rho);
    x.absorb(&[j, i]);
    let mut p = [0u16; 256];
    let mut n = 0;
    let mut buf = [0u8; 168]; // one SHAKE128 block; 168 is a multiple of 3, so groups never straddle blocks
    while n < 256 {
        x.squeeze(&mut buf);
        for c in buf.as_chunks::<3>().0 {
            let d1 = u16::from(c[0]) + 256 * u16::from(c[1] & 15);
            let d2 = u16::from(c[1] >> 4) + 16 * u16::from(c[2]);
            if u32::from(d1) < Q && n < 256 {
                p[n] = d1;
                n += 1;
            }
            if u32::from(d2) < Q && n < 256 {
                p[n] = d2;
                n += 1;
            }
        }
    }
    p
}

/// PRF_2 followed by SamplePolyCBD_2 (Algorithms 8 and the PRF of section 4.1): centered binomial noise.
fn noise(r: &[u8; 32], n: u8) -> Poly {
    let mut x = Shake::shake256();
    x.absorb(r);
    x.absorb(&[n]);
    let mut bytes = [0u8; 128];
    x.squeeze(&mut bytes);
    let mut f = [0u16; 256];
    for i in 0..32 {
        let t = u32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]);
        let d = (t & 0x5555_5555) + ((t >> 1) & 0x5555_5555); // sums of adjacent bit pairs
        for j in 0..8 {
            let a = (d >> (4 * j)) & 3;
            let b = (d >> (4 * j + 2)) & 3;
            f[8 * i + j] = ((a + Q - b) % Q) as u16;
        }
    }
    f
}

/// K-PKE.Encrypt (Algorithm 14) for ML-KEM-768.
fn pke_encrypt(ek: &[u8; EK_LEN], m: &[u8; 32], r: &[u8; 32]) -> [u8; CT_LEN] {
    let mut that = [[0u16; 256]; K];
    for (i, t) in that.iter_mut().enumerate() {
        *t = byte_decode(&ek[384 * i..384 * (i + 1)], 12);
        for c in t.iter_mut() {
            *c = (u32::from(*c) % Q) as u16;
        }
    }
    let rho = &ek[384 * K..];
    let mut n = 0u8;
    let mut yhat = [[0u16; 256]; K];
    for y in yhat.iter_mut() {
        *y = noise(r, n);
        n += 1;
        ntt(y);
    }
    let mut e1 = [[0u16; 256]; K];
    for e in e1.iter_mut() {
        *e = noise(r, n);
        n += 1;
    }
    let e2 = noise(r, n);

    let mut c = [0u8; CT_LEN];
    for i in 0..K {
        // u[i] = NTT^-1( sum_j A[j][i] * yhat[j] ) + e1[i], with A[j][i] = SampleNTT(rho || i || j)
        let mut acc = [0u16; 256];
        for (j, y) in yhat.iter().enumerate() {
            mul_acc(&mut acc, &sample_ntt(rho, i as u8, j as u8), y);
        }
        inv_ntt(&mut acc);
        let mut u = [0u16; 256];
        for x in 0..256 {
            u[x] = compress(((u32::from(acc[x]) + u32::from(e1[i][x])) % Q) as u16, 10);
        }
        byte_encode(&u, 10, &mut c[320 * i..320 * (i + 1)]);
    }
    let mut acc = [0u16; 256];
    for (t, y) in that.iter().zip(&yhat) {
        mul_acc(&mut acc, t, y);
    }
    inv_ntt(&mut acc);
    let mut v = [0u16; 256];
    for x in 0..256 {
        let mu = u32::from((m[x / 8] >> (x % 8)) & 1) * 1665; // Decompress_1(bit)
        v[x] = compress(((u32::from(acc[x]) + u32::from(e2[x]) + mu) % Q) as u16, 4);
    }
    byte_encode(&v, 4, &mut c[320 * K..]);
    c
}

/// ML-KEM.Encaps_internal (Algorithm 17) with the message `m` given: returns (ciphertext, shared secret).
/// The key must have passed `check_ek`. For tests and known answers; use `encapsulate` otherwise.
pub fn encaps_with(ek: &[u8; EK_LEN], m: &[u8; 32]) -> ([u8; CT_LEN], [u8; 32]) {
    let mut g_in = [0u8; 64];
    g_in[..32].copy_from_slice(m);
    g_in[32..].copy_from_slice(&sha3_256(ek));
    let g = sha3_512(&g_in);
    let (mut key, mut r) = ([0u8; 32], [0u8; 32]);
    key.copy_from_slice(&g[..32]);
    r.copy_from_slice(&g[32..]);
    (pke_encrypt(ek, m, &r), key)
}

/// Encapsulates a fresh shared secret to `ek` with OS randomness. None if the key is malformed.
pub fn encapsulate(ek: &[u8]) -> Option<([u8; CT_LEN], [u8; 32])> {
    if !check_ek(ek) {
        return None;
    }
    let mut m = [0u8; 32];
    random_bytes(&mut m);
    Some(encaps_with(ek.try_into().ok()?, &m))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KAT: &str = include_str!("../tests/vectors/mlkem768_encaps.txt");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }
    type Kat = (Vec<u8>, [u8; 32], Vec<u8>, [u8; 32]);
    fn cases() -> Vec<Kat> {
        KAT.lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(|l| {
                let f: Vec<&str> = l.split(' ').collect();
                (unhex(f[0]), unhex(f[1]).try_into().unwrap(), unhex(f[2]), unhex(f[3]).try_into().unwrap())
            })
            .collect()
    }

    #[test]
    fn sizes_are_the_fips_203_ones_for_ml_kem_768() {
        assert_eq!((EK_LEN, CT_LEN), (1184, 1088));
    }

    #[test]
    fn encaps_matches_the_nist_acvp_vectors() {
        let cs = cases();
        assert_eq!(cs.len(), 8);
        for (i, (ek, m, c, k)) in cs.iter().enumerate() {
            let (ct, ss) = encaps_with(ek.as_slice().try_into().unwrap(), m);
            assert_eq!(ct.to_vec(), *c, "ciphertext of case {i}");
            assert_eq!(ss, *k, "shared secret of case {i}");
        }
    }

    #[test]
    fn nist_public_keys_pass_the_modulus_check() {
        for (ek, ..) in cases() {
            assert!(check_ek(&ek));
        }
    }

    #[test]
    fn check_ek_rejects_wrong_lengths_and_coefficients_at_or_above_q() {
        let (ek, ..) = cases().remove(0);
        assert!(!check_ek(&ek[..1183]), "short");
        assert!(!check_ek(&[ek.as_slice(), &[0]].concat()), "long");
        assert!(!check_ek(&[]), "empty");
        // first coefficient = 3329 (= q) is out of range; 3328 is the largest legal value
        let mut bad = ek.clone();
        bad[0] = 0x01;
        bad[1] = (bad[1] & 0xf0) | 0x0d; // 0xd01 = 3329
        assert!(!check_ek(&bad), "q itself");
        let mut ok = ek.clone();
        ok[0] = 0x00;
        ok[1] = (ok[1] & 0xf0) | 0x0d; // 0xd00 = 3328
        assert!(check_ek(&ok), "q - 1");
        let mut max = ek.clone();
        max[0] = 0xff;
        max[1] |= 0x0f; // 4095
        assert!(!check_ek(&max), "4095");
        // a coefficient in the last polynomial, second half of the last byte pair
        let mut last = ek.clone();
        last[1151] = 0xff;
        assert!(!check_ek(&last), "last coefficient");
    }

    #[test]
    fn encapsulate_refuses_a_bad_key_and_is_random_for_a_good_one() {
        let (ek, ..) = cases().remove(0);
        assert!(encapsulate(&ek[..1000]).is_none());
        let mut bad = ek.clone();
        bad[0] = 0xff;
        bad[1] |= 0x0f;
        assert!(encapsulate(&bad).is_none());
        let a = encapsulate(&ek).unwrap();
        let b = encapsulate(&ek).unwrap();
        assert_ne!(a.0.to_vec(), b.0.to_vec(), "fresh randomness each time");
        assert_ne!(a.1, b.1);
        assert_eq!(a.0.len(), CT_LEN);
    }

    #[test]
    fn same_message_gives_the_same_result() {
        let (ek, m, ..) = cases().remove(3);
        let ek: [u8; 1184] = ek.try_into().unwrap();
        assert_eq!(encaps_with(&ek, &m), encaps_with(&ek, &m));
        let mut m2 = m;
        m2[0] ^= 1;
        assert_ne!(encaps_with(&ek, &m).1, encaps_with(&ek, &m2).1);
    }

    #[test]
    fn ntt_roundtrip_and_zetas() {
        // 17 has order 256 mod 3329; the table is 17^BitRev7(i)
        assert_eq!((ZETAS[0], ZETAS[1], ZETAS[2], ZETAS[127]), (1, 1729, 2580, 2154));
        let mut f = [0u16; 256];
        for (i, x) in f.iter_mut().enumerate() {
            *x = ((i * 37 + 11) % 3329) as u16;
        }
        let orig = f;
        ntt(&mut f);
        assert_ne!(f, orig);
        inv_ntt(&mut f);
        assert_eq!(f, orig);
    }
}
