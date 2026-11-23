//! NIST P-256: key generation, ECDSA signing (RFC 6979) and ECDH. Std only, constant time on secrets.
//!
//! Layout: `Mont` is one Montgomery-form field (used for the curve prime and for the group order), points are
//! Jacobian, and every operation that touches a secret (scalar multiplication, nonce, modular inverse) is
//! branch-free and index-free. Only public data (the curve constants, public exponents, point validity,
//! whether a signature verifies) is allowed to steer control flow.
//!
//! This is hand-written and has had no external audit; see the design doc's risk section.

use crate::crypto::{hmac_sha256_parts, random_bytes, sha256};

type L = [u64; 4]; // 256-bit integer, little-endian limbs

// ---------------------------------------------------------------------------------------------
// Plain 256-bit helpers
// ---------------------------------------------------------------------------------------------

const fn adc(a: u64, b: u64, c: u64) -> (u64, u64) {
    let t = a as u128 + b as u128 + c as u128;
    (t as u64, (t >> 64) as u64)
}

const fn sbb(a: u64, b: u64, borrow: u64) -> (u64, u64) {
    let t = (a as u128).wrapping_sub(b as u128 + borrow as u128);
    (t as u64, ((t >> 64) as u64) & 1)
}

const fn add_l(a: &L, b: &L) -> (L, u64) {
    let (r0, c) = adc(a[0], b[0], 0);
    let (r1, c) = adc(a[1], b[1], c);
    let (r2, c) = adc(a[2], b[2], c);
    let (r3, c) = adc(a[3], b[3], c);
    ([r0, r1, r2, r3], c)
}

const fn sub_l(a: &L, b: &L) -> (L, u64) {
    let (r0, c) = sbb(a[0], b[0], 0);
    let (r1, c) = sbb(a[1], b[1], c);
    let (r2, c) = sbb(a[2], b[2], c);
    let (r3, c) = sbb(a[3], b[3], c);
    ([r0, r1, r2, r3], c)
}

/// 1 if `flag` is 1, else the other: `select(1, a, b) == a`. `flag` must be 0 or 1.
fn select(flag: u64, a: &L, b: &L) -> L {
    let m = 0u64.wrapping_sub(flag);
    [(a[0] & m) | (b[0] & !m), (a[1] & m) | (b[1] & !m), (a[2] & m) | (b[2] & !m), (a[3] & m) | (b[3] & !m)]
}

/// 1 if `a` is zero, else 0, without branching.
fn is_zero(a: &L) -> u64 {
    let z = a[0] | a[1] | a[2] | a[3];
    ((z | z.wrapping_neg()) >> 63) ^ 1
}

fn from_be(b: &[u8; 32]) -> L {
    let w = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
    [w(24), w(16), w(8), w(0)]
}

fn to_be(l: &L) -> [u8; 32] {
    let mut o = [0u8; 32];
    for i in 0..4 {
        o[8 * i..8 * i + 8].copy_from_slice(&l[3 - i].to_be_bytes());
    }
    o
}

// ---------------------------------------------------------------------------------------------
// Montgomery field
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Mont {
    m: L,
    np: u64,  // -m^-1 mod 2^64
    one: L,   // 2^256 mod m  (the number 1 in Montgomery form)
    rr: L,    // 2^512 mod m  (to enter Montgomery form)
}

const fn dbl_mod(x: &L, m: &L) -> L {
    let (s, c) = add_l(x, x);
    let (d, br) = sub_l(&s, m);
    if c == 1 || br == 0 {
        d
    } else {
        s
    }
}

impl Mont {
    const fn new(m: L) -> Mont {
        let m0 = m[0];
        let mut inv = m0; // correct to 3 bits for odd m0; each step doubles the precision
        let mut i = 0;
        while i < 5 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(m0.wrapping_mul(inv)));
            i += 1;
        }
        let mut x: L = [1, 0, 0, 0];
        let mut one: L = [0; 4];
        let mut k = 0;
        while k < 512 {
            x = dbl_mod(&x, &m);
            k += 1;
            if k == 256 {
                one = x;
            }
        }
        Mont { m, np: inv.wrapping_neg(), one, rr: x }
    }

    fn mul(&self, a: &L, b: &L) -> L {
        let mut t = [0u64; 6];
        for &ai in a {
            let mut c = 0u64;
            for j in 0..4 {
                let v = t[j] as u128 + ai as u128 * b[j] as u128 + c as u128;
                t[j] = v as u64;
                c = (v >> 64) as u64;
            }
            let v = t[4] as u128 + c as u128;
            t[4] = v as u64;
            t[5] = (v >> 64) as u64;
            let mm = t[0].wrapping_mul(self.np);
            let v = t[0] as u128 + mm as u128 * self.m[0] as u128;
            let mut c = (v >> 64) as u64;
            for j in 1..4 {
                let v = t[j] as u128 + mm as u128 * self.m[j] as u128 + c as u128;
                t[j - 1] = v as u64;
                c = (v >> 64) as u64;
            }
            let v = t[4] as u128 + c as u128;
            t[3] = v as u64;
            t[4] = t[5] + (v >> 64) as u64;
            t[5] = 0;
        }
        let r = [t[0], t[1], t[2], t[3]];
        let (d, br) = sub_l(&r, &self.m);
        select(t[4] | (br ^ 1), &d, &r)
    }

    fn sqr(&self, a: &L) -> L {
        self.mul(a, a)
    }

    fn add(&self, a: &L, b: &L) -> L {
        let (s, c) = add_l(a, b);
        let (d, br) = sub_l(&s, &self.m);
        select(c | (br ^ 1), &d, &s)
    }

    fn sub(&self, a: &L, b: &L) -> L {
        let (d, br) = sub_l(a, b);
        let (e, _) = add_l(&d, &self.m);
        select(br, &e, &d)
    }

    fn enter(&self, a: &L) -> L {
        self.mul(a, &self.rr)
    }

    fn leave(&self, a: &L) -> L {
        self.mul(a, &[1, 0, 0, 0])
    }

    /// `a^e` for a PUBLIC exponent `e` (the exponent's bits steer the loop; `a` never does).
    fn pow(&self, a: &L, e: &L) -> L {
        let mut r = self.one;
        for i in (0..256).rev() {
            r = self.sqr(&r);
            if (e[i / 64] >> (i % 64)) & 1 == 1 {
                r = self.mul(&r, a);
            }
        }
        r
    }

    /// Inverse by Fermat (m is prime); the result for 0 is 0.
    fn inv(&self, a: &L) -> L {
        let (e, _) = sub_l(&self.m, &[2, 0, 0, 0]);
        self.pow(a, &e)
    }
}

const P: L = [0xffff_ffff_ffff_ffff, 0x0000_0000_ffff_ffff, 0x0000_0000_0000_0000, 0xffff_ffff_0000_0001];
const N: L = [0xf3b9_cac2_fc63_2551, 0xbce6_faad_a717_9e84, 0xffff_ffff_ffff_ffff, 0xffff_ffff_0000_0000];
const B: L = [0x3bce_3c3e_27d2_604b, 0x651d_06b0_cc53_b0f6, 0xb3eb_bd55_7698_86bc, 0x5ac6_35d8_aa3a_93e7];
const GX: L = [0xf4a1_3945_d898_c296, 0x7703_7d81_2deb_33a0, 0xf8bc_e6e5_63a4_40f2, 0x6b17_d1f2_e12c_4247];
const GY: L = [0xcbb6_4068_37bf_51f5, 0x2bce_3357_6b31_5ece, 0x8ee7_eb4a_7c0f_9e16, 0x4fe3_42e2_fe1a_7f9b];

static FP: Mont = Mont::new(P);
static FN: Mont = Mont::new(N);

// ---------------------------------------------------------------------------------------------
// Points (Jacobian, coordinates in Montgomery form; Z = 0 is the point at infinity)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Jac {
    x: L,
    y: L,
    z: L,
}

fn select_pt(flag: u64, a: &Jac, b: &Jac) -> Jac {
    Jac { x: select(flag, &a.x, &b.x), y: select(flag, &a.y, &b.y), z: select(flag, &a.z, &b.z) }
}

fn infinity() -> Jac {
    Jac { x: FP.one, y: FP.one, z: [0; 4] }
}

fn generator() -> Jac {
    Jac { x: FP.enter(&GX), y: FP.enter(&GY), z: FP.one }
}

fn dbl(p: &Jac) -> Jac {
    let f = &FP;
    let delta = f.sqr(&p.z);
    let gamma = f.sqr(&p.y);
    let beta = f.mul(&p.x, &gamma);
    let alpha = {
        let t = f.mul(&f.sub(&p.x, &delta), &f.add(&p.x, &delta));
        f.add(&f.add(&t, &t), &t)
    };
    let b2 = f.add(&beta, &beta);
    let b4 = f.add(&b2, &b2);
    let b8 = f.add(&b4, &b4);
    let x3 = f.sub(&f.sqr(&alpha), &b8);
    let z3 = f.sub(&f.sub(&f.sqr(&f.add(&p.y, &p.z)), &gamma), &delta);
    let g2 = f.sqr(&gamma);
    let g4 = f.add(&g2, &g2);
    let g8 = f.add(&g4, &g4);
    let g16 = f.add(&g8, &g8);
    let y3 = f.sub(&f.mul(&alpha, &f.sub(&b4, &x3)), &g16);
    Jac { x: x3, y: y3, z: z3 }
}

/// Complete addition: handles infinity on either side, equal points and opposite points without branching.
fn add(p: &Jac, q: &Jac) -> Jac {
    let f = &FP;
    let z1z1 = f.sqr(&p.z);
    let z2z2 = f.sqr(&q.z);
    let u1 = f.mul(&p.x, &z2z2);
    let u2 = f.mul(&q.x, &z1z1);
    let s1 = f.mul(&f.mul(&p.y, &q.z), &z2z2);
    let s2 = f.mul(&f.mul(&q.y, &p.z), &z1z1);
    let h = f.sub(&u2, &u1);
    let i = f.sqr(&f.add(&h, &h));
    let j = f.mul(&h, &i);
    let d = f.sub(&s2, &s1);
    let r = f.add(&d, &d);
    let v = f.mul(&u1, &i);
    let x3 = f.sub(&f.sub(&f.sqr(&r), &j), &f.add(&v, &v));
    let y3 = f.sub(&f.mul(&r, &f.sub(&v, &x3)), &f.mul(&f.add(&s1, &s1), &j));
    let z3 = f.mul(&f.sub(&f.sub(&f.sqr(&f.add(&p.z, &q.z)), &z1z1), &z2z2), &h);
    let generic = Jac { x: x3, y: y3, z: z3 };
    let p_inf = is_zero(&p.z);
    let q_inf = is_zero(&q.z);
    let same = is_zero(&h) & is_zero(&r) & (p_inf ^ 1) & (q_inf ^ 1);
    let mut out = select_pt(same, &dbl(p), &generic);
    out = select_pt(p_inf, q, &out);
    select_pt(q_inf, p, &out)
}

/// `k * p` for a big-endian scalar, always 256 doublings and 256 additions, selecting instead of branching.
fn scalar_mul(k: &[u8; 32], p: &Jac) -> Jac {
    let mut acc = infinity();
    for byte in k {
        for bit in (0..8).rev() {
            acc = dbl(&acc);
            let sum = add(&acc, p);
            acc = select_pt(u64::from((byte >> bit) & 1), &sum, &acc);
        }
    }
    acc
}

/// Affine x and y in plain (non-Montgomery) form, or None for infinity.
fn to_affine(p: &Jac) -> Option<(L, L)> {
    if is_zero(&p.z) == 1 {
        return None;
    }
    let zi = FP.inv(&p.z);
    let zi2 = FP.sqr(&zi);
    let zi3 = FP.mul(&zi2, &zi);
    Some((FP.leave(&FP.mul(&p.x, &zi2)), FP.leave(&FP.mul(&p.y, &zi3))))
}

/// Parses an uncompressed point (0x04 || X || Y) and checks it is on the curve with coordinates below p.
fn decode_point(b: &[u8]) -> Option<Jac> {
    if b.len() != 65 || b[0] != 4 {
        return None;
    }
    let x = from_be(b[1..33].try_into().unwrap());
    let y = from_be(b[33..65].try_into().unwrap());
    if sub_l(&x, &P).1 == 0 || sub_l(&y, &P).1 == 0 {
        return None; // coordinate not below p
    }
    let (xm, ym) = (FP.enter(&x), FP.enter(&y));
    // y^2 == x^3 - 3x + b
    let x3 = FP.mul(&FP.sqr(&xm), &xm);
    let three_x = FP.add(&FP.add(&xm, &xm), &xm);
    let rhs = FP.add(&FP.sub(&x3, &three_x), &FP.enter(&B));
    if FP.sqr(&ym) != rhs {
        return None;
    }
    Some(Jac { x: xm, y: ym, z: FP.one })
}

fn encode_point(x: &L, y: &L) -> [u8; 65] {
    let mut o = [0u8; 65];
    o[0] = 4;
    o[1..33].copy_from_slice(&to_be(x));
    o[33..].copy_from_slice(&to_be(y));
    o
}

fn valid_scalar(d: &[u8; 32]) -> bool {
    let l = from_be(d);
    is_zero(&l) == 0 && sub_l(&l, &N).1 == 1
}

/// `x mod n` for x < 2n (true for every 256-bit value and for any x < p), without branching.
fn reduce_n(x: &L) -> L {
    let (d, br) = sub_l(x, &N);
    select(br ^ 1, &d, x)
}

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// A fresh private scalar in [1, n-1] from the OS random generator (rejection sampling).
pub fn generate() -> [u8; 32] {
    loop {
        let mut d = [0u8; 32];
        random_bytes(&mut d);
        if valid_scalar(&d) {
            return d;
        }
    }
}

/// The uncompressed public key (0x04 || X || Y) of a private scalar; None if the scalar is not in [1, n-1].
pub fn public_key(d: &[u8; 32]) -> Option<[u8; 65]> {
    if !valid_scalar(d) {
        return None;
    }
    let (x, y) = to_affine(&scalar_mul(d, &generator()))?;
    Some(encode_point(&x, &y))
}

/// ECDH: the X coordinate of `d * peer`. None if `d` is invalid or `peer` is not a valid uncompressed point
/// on the curve (this is the full public-key validation RFC 8291 and SP 800-56A ask for).
pub fn ecdh(d: &[u8; 32], peer: &[u8]) -> Option<[u8; 32]> {
    if !valid_scalar(d) {
        return None;
    }
    let q = decode_point(peer)?;
    let (x, _) = to_affine(&scalar_mul(d, &q))?;
    Some(to_be(&x))
}

/// RFC 6979 deterministic nonce stream (HMAC-DRBG over SHA-256; qlen = hlen = 256, so bits2int is the identity).
struct Nonces {
    k: [u8; 32],
    v: [u8; 32],
}

impl Nonces {
    fn new(d: &[u8; 32], h1: &[u8; 32]) -> Nonces {
        let h1p = to_be(&reduce_n(&from_be(h1))); // bits2octets
        let mut s = Nonces { k: [0; 32], v: [1; 32] };
        s.k = hmac_sha256_parts(&s.k, &[&s.v, &[0], d, &h1p]);
        s.v = hmac_sha256_parts(&s.k, &[&s.v]);
        s.k = hmac_sha256_parts(&s.k, &[&s.v, &[1], d, &h1p]);
        s.v = hmac_sha256_parts(&s.k, &[&s.v]);
        s
    }

    /// The next candidate nonce in [1, n-1].
    fn next(&mut self) -> [u8; 32] {
        loop {
            self.v = hmac_sha256_parts(&self.k, &[&self.v]);
            if valid_scalar(&self.v) {
                return self.v;
            }
            self.retry();
        }
    }

    fn retry(&mut self) {
        self.k = hmac_sha256_parts(&self.k, &[&self.v, &[0]]);
        self.v = hmac_sha256_parts(&self.k, &[&self.v]);
    }
}

/// ECDSA over P-256 with SHA-256 and RFC 6979 nonces. Returns r || s (64 bytes); None if `d` is not in [1, n-1].
pub fn ecdsa_sign(d: &[u8; 32], msg: &[u8]) -> Option<[u8; 64]> {
    if !valid_scalar(d) {
        return None;
    }
    let h1 = sha256(msg);
    let e = FN.enter(&reduce_n(&from_be(&h1)));
    let dm = FN.enter(&from_be(d));
    let mut nonces = Nonces::new(d, &h1);
    loop {
        let k = nonces.next();
        let (x, _) = to_affine(&scalar_mul(&k, &generator()))?;
        let r = reduce_n(&x);
        if is_zero(&r) == 1 {
            nonces.retry();
            continue;
        }
        let rm = FN.enter(&r);
        let km = FN.enter(&from_be(&k));
        let s = FN.leave(&FN.mul(&FN.inv(&km), &FN.add(&e, &FN.mul(&rm, &dm))));
        if is_zero(&s) == 1 {
            nonces.retry();
            continue;
        }
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&to_be(&r));
        sig[32..].copy_from_slice(&to_be(&s));
        return Some(sig);
    }
}

/// Verifies an ECDSA/SHA-256 signature r || s against an uncompressed public key. Public inputs only.
pub fn ecdsa_verify(public: &[u8; 65], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Some(q) = decode_point(public) else { return false };
    let r = from_be(sig[..32].try_into().unwrap());
    let s = from_be(sig[32..].try_into().unwrap());
    let in_range = |v: &L| is_zero(v) == 0 && sub_l(v, &N).1 == 1;
    if !in_range(&r) || !in_range(&s) {
        return false;
    }
    let e = FN.enter(&reduce_n(&from_be(&sha256(msg))));
    let w = FN.inv(&FN.enter(&s));
    let u1 = to_be(&FN.leave(&FN.mul(&e, &w)));
    let u2 = to_be(&FN.leave(&FN.mul(&FN.enter(&r), &w)));
    let sum = add(&scalar_mul(&u1, &generator()), &scalar_mul(&u2, &q));
    match to_affine(&sum) {
        Some((x, _)) => reduce_n(&x) == r,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("../tests/vectors/p256.txt");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }
    fn a32(s: &str) -> [u8; 32] {
        unhex(s).try_into().unwrap()
    }
    fn a65(s: &str) -> [u8; 65] {
        unhex(s).try_into().unwrap()
    }
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn lines(kind: &str) -> Vec<Vec<&'static str>> {
        VECTORS.lines().filter(|l| l.starts_with(kind)).map(|l| l.split(' ').skip(1).collect()).collect()
    }

    const N: &str = "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551";
    const GX: &str = "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296";
    const GY: &str = "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5";

    #[test]
    fn private_key_one_gives_the_generator() {
        let mut d = [0u8; 32];
        d[31] = 1;
        let q = public_key(&d).unwrap();
        assert_eq!(hex(&q), format!("04{GX}{GY}"));
    }

    #[test]
    fn private_key_n_minus_one_is_the_generator_negated() {
        let mut d = a32(N);
        d[31] -= 1; // n - 1
        let q = public_key(&d).unwrap();
        assert_eq!(hex(&q[1..33]), GX, "same x");
        assert_ne!(hex(&q[33..]), GY, "negated y");
        assert!(ecdh(&a32(N), &q).is_none(), "scalar n is refused");
        let mut one = [0u8; 32];
        one[31] = 1;
        assert!(ecdh(&one, &q).is_some(), "the negated generator is a valid point");
    }

    #[test]
    fn invalid_private_scalars_are_refused() {
        assert!(public_key(&[0u8; 32]).is_none(), "zero");
        assert!(public_key(&a32(N)).is_none(), "n");
        assert!(public_key(&[0xff; 32]).is_none(), "above n");
        assert!(ecdsa_sign(&[0u8; 32], b"x").is_none());
        assert!(ecdh(&[0u8; 32], &a65(&format!("04{GX}{GY}"))).is_none());
    }

    #[test]
    fn public_keys_match_dotnet() {
        let keys = lines("key ");
        assert_eq!(keys.len(), 40);
        for k in keys {
            assert_eq!(hex(&public_key(&a32(k[0])).unwrap()), k[1]);
        }
    }

    #[test]
    fn ecdh_matches_dotnet() {
        let cases = lines("ecdh ");
        assert_eq!(cases.len(), 30);
        for c in cases {
            let got = ecdh(&a32(c[0]), &unhex(c[1])).unwrap();
            assert_eq!(hex(&got), c[2]);
        }
    }

    #[test]
    fn ecdh_is_symmetric() {
        let (a, b) = (generate(), generate());
        let (qa, qb) = (public_key(&a).unwrap(), public_key(&b).unwrap());
        assert_eq!(ecdh(&a, &qb), ecdh(&b, &qa));
        assert!(ecdh(&a, &qb).is_some());
    }

    #[test]
    fn ecdh_refuses_bad_peers() {
        let d = generate();
        let good = public_key(&generate()).unwrap();
        assert!(ecdh(&d, &good).is_some());
        let mut off = good;
        off[64] ^= 1; // y changed: no longer on the curve
        assert!(ecdh(&d, &off).is_none(), "off curve");
        let mut compressed = good;
        compressed[0] = 0x02;
        assert!(ecdh(&d, &compressed).is_none(), "wrong prefix");
        assert!(ecdh(&d, &good[..64]).is_none(), "too short");
        assert!(ecdh(&d, &[good.as_slice(), &[0]].concat()).is_none(), "too long");
        let mut big_x = good;
        big_x[1..33].copy_from_slice(&[0xff; 32]); // x >= p
        assert!(ecdh(&d, &big_x).is_none(), "x not below p");
        let mut zero = [0u8; 65];
        zero[0] = 4;
        assert!(ecdh(&d, &zero).is_none(), "(0,0) is not on the curve");
    }

    #[test]
    fn rfc6979_a25_public_key_and_signatures() {
        let x = a32("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721");
        let q = public_key(&x).unwrap();
        assert_eq!(
            hex(&q),
            "0460fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb67903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299"
        );
        let sample = ecdsa_sign(&x, b"sample").unwrap();
        assert_eq!(
            hex(&sample),
            "efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8"
        );
        let test = ecdsa_sign(&x, b"test").unwrap();
        assert_eq!(
            hex(&test),
            "f1abb023518351cd71d881567b1ea663ed3efcf6c5132b354f28d3b0b7d38367019f4113742a2b14bd25926b49c649155f267e60d3814b4c0cc84250e46f0083"
        );
        assert!(ecdsa_verify(&q, b"sample", &sample));
        assert!(ecdsa_verify(&q, b"test", &test));
        assert!(!ecdsa_verify(&q, b"test", &sample));
    }

    #[test]
    fn signatures_made_by_dotnet_verify() {
        let cases = lines("verify ");
        assert_eq!(cases.len(), 20);
        for c in cases {
            assert!(ecdsa_verify(&a65(c[0]), &unhex(c[1]), &unhex(c[2]).try_into().unwrap()), "msg {}", c[1]);
        }
    }

    #[test]
    fn verify_rejects_tampering() {
        let d = generate();
        let q = public_key(&d).unwrap();
        let sig = ecdsa_sign(&d, b"attack at dawn").unwrap();
        assert!(ecdsa_verify(&q, b"attack at dawn", &sig));
        assert!(!ecdsa_verify(&q, b"attack at dusk", &sig), "message");
        for i in [0usize, 31, 32, 63] {
            let mut bad = sig;
            bad[i] ^= 0x80;
            assert!(!ecdsa_verify(&q, b"attack at dawn", &bad), "sig byte {i}");
        }
        assert!(!ecdsa_verify(&public_key(&generate()).unwrap(), b"attack at dawn", &sig), "other key");
        let mut zero_r = sig;
        zero_r[..32].fill(0);
        assert!(!ecdsa_verify(&q, b"attack at dawn", &zero_r), "r = 0");
        let mut zero_s = sig;
        zero_s[32..].fill(0);
        assert!(!ecdsa_verify(&q, b"attack at dawn", &zero_s), "s = 0");
        let mut big_r = sig;
        big_r[..32].copy_from_slice(&a32(N));
        assert!(!ecdsa_verify(&q, b"attack at dawn", &big_r), "r = n");
    }

    #[test]
    fn sign_then_verify_for_100_random_keys_and_messages() {
        for i in 0..100u32 {
            let d = generate();
            let q = public_key(&d).unwrap();
            let msg = format!("message number {i} {}", "x".repeat(i as usize));
            let sig = ecdsa_sign(&d, msg.as_bytes()).unwrap();
            assert!(ecdsa_verify(&q, msg.as_bytes(), &sig), "i={i}");
            assert_eq!(sig, ecdsa_sign(&d, msg.as_bytes()).unwrap(), "RFC 6979 signatures are deterministic");
        }
    }

    #[test]
    fn generate_gives_distinct_valid_scalars() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let d = generate();
            assert!(public_key(&d).is_some());
            assert!(seen.insert(d));
        }
    }
}
