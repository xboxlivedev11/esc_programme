//! SHA-3 and SHAKE (FIPS 202) on Keccak-f[1600]. Std only, no allocation.
//!
//! Used by `mlkem` (SHAKE128/256, SHA3-256/512). Bytes go in and out through the 25-lane state directly, so
//! nothing is buffered or allocated.

const RC: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];
const ROT: [u32; 24] = [1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44];
const PI: [usize; 24] = [10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1];

fn keccak_f(st: &mut [u64; 25]) {
    for rc in RC {
        // theta
        let mut bc = [0u64; 5];
        for (i, b) in bc.iter_mut().enumerate() {
            *b = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
        }
        for i in 0..5 {
            let t = bc[(i + 4) % 5] ^ bc[(i + 1) % 5].rotate_left(1);
            for j in (0..25).step_by(5) {
                st[j + i] ^= t;
            }
        }
        // rho and pi
        let mut t = st[1];
        for i in 0..24 {
            let j = PI[i];
            let b = st[j];
            st[j] = t.rotate_left(ROT[i]);
            t = b;
        }
        // chi
        for j in (0..25).step_by(5) {
            let b = [st[j], st[j + 1], st[j + 2], st[j + 3], st[j + 4]];
            for i in 0..5 {
                st[j + i] = b[i] ^ (!b[(i + 1) % 5] & b[(i + 2) % 5]);
            }
        }
        // iota
        st[0] ^= rc;
    }
}

/// The sponge: absorb bytes, then squeeze. `rate` is in bytes, `ds` the domain-separation byte (0x06 SHA-3, 0x1f SHAKE).
#[derive(Clone)]
struct Sponge {
    st: [u64; 25],
    pos: usize,
    rate: usize,
    ds: u8,
    squeezing: bool,
}

impl Sponge {
    fn new(rate: usize, ds: u8) -> Self {
        Sponge { st: [0; 25], pos: 0, rate, ds, squeezing: false }
    }

    fn xor_byte(&mut self, at: usize, b: u8) {
        self.st[at / 8] ^= u64::from(b) << (8 * (at % 8));
    }

    fn absorb(&mut self, data: &[u8]) {
        debug_assert!(!self.squeezing, "absorb after squeeze");
        for &b in data {
            self.xor_byte(self.pos, b);
            self.pos += 1;
            if self.pos == self.rate {
                keccak_f(&mut self.st);
                self.pos = 0;
            }
        }
    }

    fn squeeze(&mut self, out: &mut [u8]) {
        if !self.squeezing {
            self.xor_byte(self.pos, self.ds);
            self.xor_byte(self.rate - 1, 0x80);
            keccak_f(&mut self.st);
            self.pos = 0;
            self.squeezing = true;
        }
        for o in out.iter_mut() {
            if self.pos == self.rate {
                keccak_f(&mut self.st);
                self.pos = 0;
            }
            *o = (self.st[self.pos / 8] >> (8 * (self.pos % 8))) as u8;
            self.pos += 1;
        }
    }
}

/// SHAKE128 / SHAKE256: an extendable-output function.
#[derive(Clone)]
pub struct Shake(Sponge);

impl Shake {
    pub fn shake128() -> Self {
        Shake(Sponge::new(168, 0x1f))
    }
    pub fn shake256() -> Self {
        Shake(Sponge::new(136, 0x1f))
    }
    pub fn absorb(&mut self, data: &[u8]) {
        self.0.absorb(data);
    }
    pub fn squeeze(&mut self, out: &mut [u8]) {
        self.0.squeeze(out);
    }
}

/// SHA3-256 / SHA3-512, incremental.
#[derive(Clone)]
pub struct Sha3(Sponge);

impl Sha3 {
    pub fn sha3_256() -> Self {
        Sha3(Sponge::new(136, 0x06))
    }
    pub fn sha3_512() -> Self {
        Sha3(Sponge::new(72, 0x06))
    }
    pub fn absorb(&mut self, data: &[u8]) {
        self.0.absorb(data);
    }
    pub fn finish_256(mut self) -> [u8; 32] {
        let mut o = [0u8; 32];
        self.0.squeeze(&mut o);
        o
    }
    pub fn finish_512(mut self) -> [u8; 64] {
        let mut o = [0u8; 64];
        self.0.squeeze(&mut o);
        o
    }
}

pub fn sha3_256(data: &[u8]) -> [u8; 32] {
    let mut s = Sha3::sha3_256();
    s.absorb(data);
    s.finish_256()
}

pub fn sha3_512(data: &[u8]) -> [u8; 64] {
    let mut s = Sha3::sha3_512();
    s.absorb(data);
    s.finish_512()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn a_times(n: usize) -> Vec<u8> {
        vec![b'a'; n]
    }

    #[test]
    fn sha3_256_known_answers() {
        assert_eq!(hex(&sha3_256(b"")), "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a");
        assert_eq!(hex(&sha3_256(b"abc")), "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532");
    }

    #[test]
    fn sha3_256_around_the_rate_boundary() {
        // rate is 136 bytes: 135 pads in one block, 136 needs an extra block, 137 and 200 span two
        assert_eq!(hex(&sha3_256(&a_times(135))), "8094bb53c44cfb1e67b7c30447f9a1c33696d2463ecc1d9c92538913392843c9");
        assert_eq!(hex(&sha3_256(&a_times(136))), "3fc5559f14db8e453a0a3091edbd2bc25e11528d81c66fa570a4efdcc2695ee1");
        assert_eq!(hex(&sha3_256(&a_times(137))), "f8d6846cedd2ccfadf15c5879ef95af724d799eed7391fb1c91f95344e738614");
        assert_eq!(hex(&sha3_256(&a_times(200))), "cce34485baf2bf2aca99b94833892a4f52896d3d153f7b840cc4f9fe695f1387");
    }

    #[test]
    fn sha3_512_known_answers() {
        assert_eq!(
            hex(&sha3_512(b"abc")),
            "b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0"
        );
        assert_eq!(
            hex(&sha3_512(&a_times(200))),
            "eae6c85c6904f11075de9f9d5e1064371d000510fa3d2d79d40cf9be34892fb01859d0a0234e138bcb0ad5c84f6c0dca226a414b0c9a2897cb695f5185fe36ec"
        );
    }

    #[test]
    fn shake_empty_input_known_answers() {
        let mut o = [0u8; 32];
        let mut s = Shake::shake128();
        s.squeeze(&mut o);
        assert_eq!(hex(&o), "7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26");
        let mut s = Shake::shake256();
        s.squeeze(&mut o);
        assert_eq!(hex(&o), "46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f");
    }

    const SHAKE128_ABC_300: &str = "5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc844c50af32acd3f2cdd066568706f509bc1bdde58295dae3f891a9a0fca5783789a41f8611214ce612394df286a62d1a2252aa94db9c538956c717dc2bed4f232a0294c857c730aa16067ac1062f1201fb0d377cfb9cde4c63599b27f3462bba4a0ed296c801f9ff7f57302bb3076ee145f97a32ae68e76ab66c48d51675bd49acc29082f5647584e6aa01b3f5af057805f973ff8ecb8b226ac32ada6f01c1fcd4818cb006aa5b4cdb3611eb1e533c8964cacfdf31012cd3fb744d02225b988b475375faad996eb1b9176ecb0f8b2871723d6dbb804e23357e50732f5cfc904b1319795000d7361d9e5e1b77b4b8f5774aa1482cfa58f83096bdb2e06a3eed543a38919b57ecbec737f4086be";

    #[test]
    fn shake128_squeezes_across_blocks_in_any_chunking() {
        // 300 bytes crosses the 168-byte rate; every chunk size must give the same stream
        for chunk in [1usize, 7, 64, 167, 168, 169, 300] {
            let mut s = Shake::shake128();
            s.absorb(b"abc");
            let mut out = Vec::new();
            while out.len() < 300 {
                let n = chunk.min(300 - out.len());
                let mut buf = vec![0u8; n];
                s.squeeze(&mut buf);
                out.extend_from_slice(&buf);
            }
            assert_eq!(hex(&out), SHAKE128_ABC_300, "chunk {chunk}");
        }
    }

    #[test]
    fn shake256_long_input_known_answer() {
        let mut s = Shake::shake256();
        s.absorb(&a_times(300));
        let mut o = [0u8; 100];
        s.squeeze(&mut o);
        assert_eq!(
            hex(&o),
            "542e4338ca18e43776834307a2b6158f70c14f22400081b2dc345cc54a6298cd125383fc7cc5e05937618e0474a71308d82cbd6bacd48a6d69f540bd72f2b19e41f006067f6f782d5f8bf1fe6660fe84a6b2f402aa545efb76ff39cd85030ad8a31bc3ae"
        );
    }

    #[test]
    fn incremental_absorb_equals_oneshot_at_every_split() {
        let data = a_times(200);
        let want = sha3_256(&data);
        for cut in 0..=200 {
            let mut s = Sha3::sha3_256();
            s.absorb(&data[..cut]);
            s.absorb(&data[cut..]);
            assert_eq!(s.finish_256(), want, "split at {cut}");
        }
    }
}
