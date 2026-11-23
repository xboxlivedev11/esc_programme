//! SHA-3 and SHAKE (FIPS 202) on Keccak-f[1600]. Std only, no allocation.

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
