//! Web Push wire format: VAPID (RFC 8292), message encryption (RFC 8291) and the post-quantum inner layer.
//!
//! Pure functions, no I/O. The outer envelope is what browsers and push services require (ECDH P-256, AES-128-GCM,
//! ECDSA P-256). The inner layer is ours: ML-KEM-768 + AES-256-GCM, so a recording of today's push traffic stays
//! unreadable even after P-256 falls. Layout of the inner layer (version 1):
//!
//! ```text
//! 0x01 || kem_ciphertext (1088) || nonce (12) || AES-256-GCM(key, nonce, aad = first 1089 bytes, padded JSON || tag)
//! key    = HKDF-SHA256(salt = "", ikm = ML-KEM shared secret, info = "esc-push-v1")
//! padded = json || 0x80 || zeros, up to a multiple of 512 bytes
//! ```

use crate::aead::{aes_gcm_seal, hkdf_sha256};
use crate::crypto::{b64url_encode, hmac_sha256, hmac_sha256_parts, random_bytes};
use crate::json::Js;
use crate::{mlkem, p256};

/// RFC 8291: a push service must accept 4096 bytes in all, so with the 86-byte header, a 16-byte tag and the
/// 1-byte delimiter at most 3993 bytes of message fit.
pub const MAX_PLAINTEXT: usize = 3993;
/// The longest JSON the inner layer takes so that inner + outer stay within `MAX_PLAINTEXT` (1 + 1088 + 12 + 2560 + 16).
pub const MAX_INNER_JSON: usize = 2559;
const RECORD_SIZE: u32 = 4096;
const INNER_VERSION: u8 = 1;
const INNER_BLOCK: usize = 512;

/// `scheme://host[:port]` of an endpoint URL: the `aud` claim of a VAPID token.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let host = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if host.is_empty() || host.contains(['@', ' ']) {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

/// The `Authorization` header value for a push request: `vapid t=<JWT>, k=<public key>` (RFC 8292). Deterministic
/// (RFC 6979 signature). None only if `private` is not a valid P-256 key.
pub fn vapid_header(private: &[u8; 32], aud: &str, sub: &str, exp: u64) -> Option<String> {
    let public = p256::public_key(private)?;
    let head = b64url_encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let mut c = Js::new();
    c.obj().ks("aud", aud).ku("exp", exp).ks("sub", sub).end_obj();
    let signing = format!("{head}.{}", b64url_encode(c.finish().as_bytes()));
    let sig = p256::ecdsa_sign(private, signing.as_bytes())?;
    Some(format!("vapid t={signing}.{}, k={}", b64url_encode(&sig), b64url_encode(&public)))
}

/// RFC 8291 section 3.4: the content-encryption key and nonce from the ECDH secret, the auth secret and both keys.
pub(crate) fn derive_keys(
    ecdh: &[u8; 32],
    auth: &[u8; 16],
    ua_pub: &[u8; 65],
    as_pub: &[u8; 65],
    salt: &[u8; 16],
) -> ([u8; 16], [u8; 12]) {
    let prk_key = hmac_sha256(auth, ecdh);
    let mut info = Vec::with_capacity(14 + 130);
    info.extend_from_slice(b"WebPush: info\0");
    info.extend_from_slice(ua_pub);
    info.extend_from_slice(as_pub);
    let ikm = hmac_sha256_parts(&prk_key, &[&info, &[1]]); // HKDF-Expand to one block
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    hkdf_sha256(salt, &ikm, b"Content-Encoding: aes128gcm\0", &mut cek);
    hkdf_sha256(salt, &ikm, b"Content-Encoding: nonce\0", &mut nonce);
    (cek, nonce)
}

/// RFC 8291 `aes128gcm` with the ephemeral key and salt supplied (the RFC example needs fixed ones). One record.
/// `pad_to` is the minimum record size before the tag (message + delimiter + zero padding); 0 means no padding.
pub fn encrypt_aes128gcm_with(
    eph_private: &[u8; 32],
    salt: &[u8; 16],
    p256dh: &[u8],
    auth: &[u8; 16],
    plaintext: &[u8],
    pad_to: usize,
) -> Option<Vec<u8>> {
    if plaintext.len() > MAX_PLAINTEXT || pad_to > MAX_PLAINTEXT + 1 {
        return None;
    }
    let ua_pub: &[u8; 65] = p256dh.try_into().ok()?;
    let as_pub = p256::public_key(eph_private)?;
    let secret = p256::ecdh(eph_private, p256dh)?; // also validates the receiver's point
    let (cek, nonce) = derive_keys(&secret, auth, ua_pub, &as_pub, salt);
    let mut record = Vec::with_capacity(pad_to.max(plaintext.len() + 1));
    record.extend_from_slice(plaintext);
    record.push(2); // delimiter of the last (here only) record
    record.resize(pad_to.max(record.len()), 0);
    let sealed = aes_gcm_seal(&cek, &nonce, b"", &record);
    let mut out = Vec::with_capacity(86 + sealed.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(65);
    out.extend_from_slice(&as_pub);
    out.extend_from_slice(&sealed);
    Some(out)
}

/// RFC 8291 encryption to a subscription's `p256dh` key and `auth` secret with a fresh ephemeral key and salt.
pub fn encrypt_aes128gcm(p256dh: &[u8], auth: &[u8; 16], plaintext: &[u8], pad_to: usize) -> Option<Vec<u8>> {
    let eph = p256::generate();
    let mut salt = [0u8; 16];
    random_bytes(&mut salt);
    encrypt_aes128gcm_with(&eph, &salt, p256dh, auth, plaintext, pad_to)
}

/// The inner post-quantum layer with the KEM message and nonce supplied (for tests). See the module docs.
pub fn seal_inner_with(kem_ek: &[u8], m: &[u8; 32], nonce: &[u8; 12], json: &[u8]) -> Option<Vec<u8>> {
    if json.len() > MAX_INNER_JSON || !mlkem::check_ek(kem_ek) {
        return None;
    }
    let (kem_ct, shared) = mlkem::encaps_with(kem_ek.try_into().ok()?, m);
    let mut key = [0u8; 32];
    hkdf_sha256(b"", &shared, b"esc-push-v1", &mut key);
    let padded = (json.len() + 1).div_ceil(INNER_BLOCK) * INNER_BLOCK;
    let mut plain = Vec::with_capacity(padded);
    plain.extend_from_slice(json);
    plain.push(0x80);
    plain.resize(padded, 0);
    let mut out = Vec::with_capacity(1 + mlkem::CT_LEN + 12 + padded + 16);
    out.push(INNER_VERSION);
    out.extend_from_slice(&kem_ct);
    out.extend_from_slice(nonce);
    let sealed = aes_gcm_seal(&key, nonce, &out[..1 + mlkem::CT_LEN], &plain);
    out.extend_from_slice(&sealed);
    Some(out)
}

/// The inner post-quantum layer for a device's ML-KEM-768 encapsulation key, with fresh randomness.
pub fn seal_inner(kem_ek: &[u8], json: &[u8]) -> Option<Vec<u8>> {
    let (mut m, mut nonce) = ([0u8; 32], [0u8; 12]);
    random_bytes(&mut m);
    random_bytes(&mut nonce);
    seal_inner_with(kem_ek, &m, &nonce, json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aead::{aes_gcm_open, hkdf_sha256};
    use crate::crypto::{b64url_decode as dec, b64url_encode as enc};
    use crate::{mlkem, p256};

    const KAT: &str = include_str!("../tests/vectors/mlkem768_encaps.txt");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }
    fn b(s: &str) -> Vec<u8> {
        dec(s).unwrap()
    }
    fn a32(s: &str) -> [u8; 32] {
        b(s).try_into().unwrap()
    }
    fn a16(s: &str) -> [u8; 16] {
        b(s).try_into().unwrap()
    }
    fn first_ek() -> Vec<u8> {
        unhex(KAT.lines().find(|l| !l.starts_with('#')).unwrap().split(' ').next().unwrap())
    }

    // RFC 8291 section 5 and appendix A
    const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const AS_PUBLIC: &str = "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const TEXT: &[u8] = b"When I grow up, I want to be a watermelon";
    const BODY: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
                        mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT\
                        pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    /// What a browser does: undo RFC 8291 with the receiver's private key. Test-only.
    fn decrypt(ua_private: &[u8; 32], auth: &[u8; 16], body: &[u8]) -> Option<Vec<u8>> {
        let (salt, rs, idlen) = (&body[..16], u32::from_be_bytes(body[16..20].try_into().ok()?), body[20] as usize);
        assert_eq!((rs, idlen), (4096, 65));
        let as_pub = &body[21..21 + idlen];
        let ua_pub = p256::public_key(ua_private)?;
        let secret = p256::ecdh(ua_private, as_pub)?;
        let (cek, nonce) = derive_keys(&secret, auth, &ua_pub, as_pub.try_into().ok()?, salt.try_into().ok()?);
        let mut plain = aes_gcm_open(&cek, &nonce, b"", &body[21 + idlen..])?;
        while plain.last() == Some(&0) {
            plain.pop(); // padding zeros after the delimiter
        }
        (plain.pop() == Some(2)).then_some(plain) // the last-record delimiter
    }

    #[test]
    fn rfc8291_section5_example_exact_bytes() {
        let got = encrypt_aes128gcm_with(&a32(AS_PRIVATE), &a16(SALT), &b(UA_PUBLIC), &a16(AUTH), TEXT, 0).unwrap();
        assert_eq!(got.len(), 144);
        assert_eq!(enc(&got), BODY);
    }

    #[test]
    fn rfc8291_appendix_a_intermediate_keys() {
        let secret = p256::ecdh(&a32(AS_PRIVATE), &b(UA_PUBLIC)).unwrap();
        assert_eq!(enc(&secret), "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");
        let (cek, nonce) =
            derive_keys(&secret, &a16(AUTH), &b(UA_PUBLIC).try_into().unwrap(), &b(AS_PUBLIC).try_into().unwrap(), &a16(SALT));
        assert_eq!(enc(&cek), "oIhVW04MRdy2XN9CiKLxTg");
        assert_eq!(enc(&nonce), "4h_95klXJ5E_qnoN");
    }

    #[test]
    fn what_the_receiver_decrypts_is_what_was_sent_with_and_without_padding() {
        let ua_priv = p256::generate();
        let ua_pub = p256::public_key(&ua_priv).unwrap();
        let auth: [u8; 16] = [7; 16];
        for (len, pad_to) in [(0usize, 0usize), (1, 0), (40, 0), (40, 200), (199, 200), (200, 200), (1000, 0), (3000, 3500)] {
            let msg: Vec<u8> = (0..len).map(|i| (i * 7 + 1) as u8 | 1).collect(); // never a zero byte
            let body = encrypt_aes128gcm(&ua_pub, &auth, &msg, pad_to).unwrap();
            assert_eq!(decrypt(&ua_priv, &auth, &body).unwrap(), msg, "len {len} pad_to {pad_to}");
            if pad_to > len {
                assert_eq!(body.len(), 86 + pad_to + 16, "padded to a fixed size");
            }
        }
        // the receiver's own example key also opens the RFC body
        assert_eq!(decrypt(&a32(UA_PRIVATE), &a16(AUTH), &b(BODY)).unwrap(), TEXT);
    }

    #[test]
    fn encrypt_refuses_bad_inputs() {
        let good = p256::public_key(&p256::generate()).unwrap();
        let auth = [1u8; 16];
        assert!(encrypt_aes128gcm(&good[..64], &auth, b"x", 0).is_none(), "short key");
        let mut off = good;
        off[64] ^= 1;
        assert!(encrypt_aes128gcm(&off, &auth, b"x", 0).is_none(), "off-curve key");
        assert!(encrypt_aes128gcm(&good, &auth, &vec![1u8; MAX_PLAINTEXT + 1], 0).is_none(), "plaintext too long");
        assert!(encrypt_aes128gcm(&good, &auth, &vec![1u8; MAX_PLAINTEXT], 0).is_some(), "exactly the limit");
        assert!(encrypt_aes128gcm(&good, &auth, b"x", MAX_PLAINTEXT + 2).is_none(), "padding beyond the limit");
        let two = (encrypt_aes128gcm(&good, &auth, b"x", 0).unwrap(), encrypt_aes128gcm(&good, &auth, b"x", 0).unwrap());
        assert_ne!(two.0, two.1, "fresh ephemeral key and salt each time");
    }

    // RFC 8292 section 2 example token and key
    const RFC8292_T: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9.eyJhdWQiOiJodHRwczovL3B1c2guZXhhbXBsZS5uZXQiLCJleHAiOjE0NTM1MjM3NjgsInN1YiI6Im1haWx0bzpwdXNoQGV4YW1wbGUuY29tIn0.i3CYb7t4xfxCDquptFOepC9GAu_HLGkMlMuCGSK2rpiUfnK9ojFwDXb1JrErtmysazNjjvW2L9OkSSHzvoD1oA";
    const RFC8292_K: &str = "BA1Hxzyi1RUM1b5wjxsn7nGxAszw2u61m164i3MrAIxHF6YK5h4SDYic-dRuU_RCPCfA5aq9ojSwk5Y2EmClBPs";

    #[test]
    fn the_rfc8292_example_token_verifies_with_our_verifier() {
        let (signing, sig) = RFC8292_T.rsplit_once('.').unwrap();
        assert!(p256::ecdsa_verify(&b(RFC8292_K).try_into().unwrap(), signing.as_bytes(), &b(sig).try_into().unwrap()));
    }

    #[test]
    fn our_vapid_header_has_the_rfc8292_shape_and_verifies() {
        let d = p256::generate();
        let h = vapid_header(&d, "https://push.example.net", "mailto:push@example.com", 1453523768).unwrap();
        let rest = h.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, enc(&p256::public_key(&d).unwrap()));
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(String::from_utf8(b(parts[0])).unwrap(), r#"{"typ":"JWT","alg":"ES256"}"#);
        assert_eq!(
            String::from_utf8(b(parts[1])).unwrap(),
            r#"{"aud":"https://push.example.net","exp":1453523768,"sub":"mailto:push@example.com"}"#
        );
        let signing = format!("{}.{}", parts[0], parts[1]);
        assert!(p256::ecdsa_verify(&p256::public_key(&d).unwrap(), signing.as_bytes(), &b(parts[2]).try_into().unwrap()));
        assert_eq!(h, vapid_header(&d, "https://push.example.net", "mailto:push@example.com", 1453523768).unwrap(), "deterministic");
        assert!(vapid_header(&[0u8; 32], "https://x", "https://y", 1).is_none(), "invalid key gives None, never a panic");
    }

    #[test]
    fn vapid_claims_are_escaped() {
        let d = p256::generate();
        let h = vapid_header(&d, "https://a.example", "https://r.example/\"x\\y", 5).unwrap();
        let jwt = h.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap().0;
        let claims = String::from_utf8(b(jwt.split('.').nth(1).unwrap())).unwrap();
        assert!(crate::json::parse(&claims).is_ok(), "{claims}");
    }

    #[test]
    fn origin_of_endpoint_is_scheme_and_host() {
        assert_eq!(origin_of("https://fcm.googleapis.com/fcm/send/abc:def?x=1").as_deref(), Some("https://fcm.googleapis.com"));
        assert_eq!(origin_of("https://updates.push.services.mozilla.com:443/wpush/v2/xyz").as_deref(), Some("https://updates.push.services.mozilla.com:443"));
        assert_eq!(origin_of("http://127.0.0.1:9000/p").as_deref(), Some("http://127.0.0.1:9000"));
        assert_eq!(origin_of("not a url"), None);
        assert_eq!(origin_of("https://"), None);
    }

    #[test]
    fn inner_layout_padding_and_aad() {
        let ek = first_ek();
        for (len, want_padded) in [(0usize, 512usize), (1, 512), (511, 512), (512, 1024), (700, 1024), (1023, 1024), (1024, 1536)] {
            let json = vec![b'a'; len];
            let m = [9u8; 32];
            let nonce = [3u8; 12];
            let out = seal_inner_with(&ek, &m, &nonce, &json).unwrap();
            assert_eq!(out.len(), 1 + 1088 + 12 + want_padded + 16, "json len {len}");
            assert_eq!(out[0], 1, "version byte");
            // open it the way the browser will: key from the ML-KEM shared secret, AAD = version || kem ciphertext
            let (ct, ss) = mlkem::encaps_with(ek.as_slice().try_into().unwrap(), &m);
            assert_eq!(&out[1..1089], &ct[..], "same message gives the same KEM ciphertext");
            assert_eq!(&out[1089..1101], &nonce);
            let mut key = [0u8; 32];
            hkdf_sha256(b"", &ss, b"esc-push-v1", &mut key);
            let mut plain = aes_gcm_open(&key, &nonce, &out[..1089], &out[1101..]).unwrap();
            assert_eq!(plain.len(), want_padded);
            while plain.last() == Some(&0) {
                plain.pop();
            }
            assert_eq!(plain.pop(), Some(0x80), "padding marker");
            assert_eq!(plain, json);
            // the version and the KEM ciphertext are authenticated
            let mut bad = out.clone();
            bad[0] = 2;
            assert!(aes_gcm_open(&key, &nonce, &bad[..1089], &out[1101..]).is_none());
            let mut bad = out.clone();
            bad[500] ^= 1;
            assert!(aes_gcm_open(&key, &nonce, &bad[..1089], &out[1101..]).is_none());
        }
    }

    #[test]
    fn seal_inner_refuses_a_bad_key_and_is_fresh_each_time() {
        let ek = first_ek();
        assert!(seal_inner(&ek[..1000], b"{}").is_none());
        let mut bad = ek.clone();
        bad[0] = 0xff;
        bad[1] |= 0x0f;
        assert!(seal_inner(&bad, b"{}").is_none());
        let (a, b) = (seal_inner(&ek, b"{}").unwrap(), seal_inner(&ek, b"{}").unwrap());
        assert_ne!(a, b);
        assert!(seal_inner(&ek, &vec![b'x'; MAX_INNER_JSON + 1]).is_none(), "keeps the whole push under the 4096-byte limit");
        assert!(seal_inner(&ek, &vec![b'x'; MAX_INNER_JSON]).is_some());
    }

    #[test]
    fn a_full_notification_fits_one_push_message() {
        // inner (longest allowed) wrapped by the outer layer must stay within what push services accept (4096 bytes)
        let ek = first_ek();
        let inner = seal_inner(&ek, &vec![b'x'; MAX_INNER_JSON]).unwrap();
        let outer = encrypt_aes128gcm(&p256::public_key(&p256::generate()).unwrap(), &[1; 16], &inner, 0).unwrap();
        assert!(outer.len() <= 4096, "{}", outer.len());
    }
}
