# Background Push (with a post-quantum inner layer) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans (native, in this session: the project allows at most one Sonnet subagent, only for self-checking a module). Steps use checkbox (`- [ ]`) syntax.

**Goal:** A closed ESC on a phone shows "sender: preview" within seconds of a message, with the content hidden from the push service even against harvest-now-decrypt-later.

**Architecture:** Hand-written std-only primitives (SHA-3, AES-GCM, HKDF, P-256, ML-KEM-768 encapsulation) feed a pure `webpush` module (VAPID JWT, RFC 8291 envelope, inner ML-KEM layer). A Star-local `push` module stores subscriptions and runs a desk thread that the Bridge feeds when a message is applied (local send or Ring). The browser side is a lazy `mlkem.js` (keygen + decapsulation) used only by the service worker.

**Tech Stack:** Rust std only (no crates, not even dev-deps), plain JS, WebCrypto AES-GCM in the browser, `curl` through the Courier.

**Spec:** `docs/superpowers/specs/2026-11-23-background-push-design.md` (decisions D14, D15). Read it first; this plan only fixes names, signatures and tests.

## Global Constraints

- Zero third-party code: no crate, no npm, no CDN, no build step. Test vectors are *data* fetched once from the RFC/NIST text and committed under `server/tests/vectors/`.
- Standard algorithms only (FIPS 202/203, RFC 5869/6979/8291/8292, NIST SP 800-38D); no invented constructions.
- Nothing secret-dependent in branches or indexes in `p256.rs` scalar paths and `mlkem.rs` (`ct_eq` from `crypto.rs` for comparisons).
- Never log a payload, an endpoint, a key or a message; log only a status code and a short subscription id.
- VAPID `sub` claim = the relay's public `https://` URL; never an e-mail or personal detail.
- Hot paths allocate nothing; the push desk runs on its own thread so message commit never waits on a push service.
- Everything on `master`, in place; commit after every task; `cargo test --release` and `cargo clippy --all-targets -- -D warnings` must pass before each commit. Update `CLAUDE.md` "Current state" and the spec when done.
- Port 7878 is production: dev runs use 7979 and `--web ..\web`.
- Names: the push desk lives in the **Courier**'s family (`push.rs`, "the Courier's push desk").

## Review Focus (each line has a test in the owning task)

1. A subscription endpoint pointing at the relay itself, a private IP, `localhost` or an IP literal (SSRF) is refused. (T6)
2. A push-service `404`/`410` deletes the subscription; `429`/`5xx` retry with capped backoff, never forever. (T7)
3. The author never gets a push for their own message; a blocked sender or a disabled account never triggers one; a session with a live WebSocket is skipped but the user's other devices are not. (T7)
4. Preview text is cut on a character boundary (emoji, accents), and a file-only or voice-only message gets a sensible preview. (T7)
5. A garbled or undecryptable payload still shows a generic "New message" in the service worker (a silent push revokes the subscription). (T8)

## File structure

| File | Responsibility |
|---|---|
| `server/src/keccak.rs` (new) | SHA3-256/512, SHAKE128/256 |
| `server/src/aead.rs` (new) | AES-128/256, GCM seal/open, HKDF-SHA256 |
| `server/src/p256.rs` (new) | P-256 field/scalar math, key generation, ECDSA sign, ECDH |
| `server/src/mlkem.rs` (new) | ML-KEM-768 encapsulation (relay never decapsulates) |
| `server/src/webpush.rs` (new) | VAPID JWT, RFC 8291 envelope, inner PQ layer: pure functions |
| `server/src/push.rs` (new) | Subscription store (`push.log`), VAPID key file, desk thread, preview builder |
| `server/src/bridge.rs`, `gate.rs`, `lib.rs` (modify) | hooks, routes, module list |
| `web/mlkem.js` (new), `web/push.js` (new), `web/sw.js`, `web/app.js`, `web/i18n.js` (modify) | browser side |
| `server/tests/vectors/*` (new), `server/tests/push_e2e.rs` (new) | data and the end-to-end test |

---

### Task 1: SHA-3 and SHAKE (`keccak.rs`)

**Files:** Create `server/src/keccak.rs`; Modify `server/src/lib.rs` (add `pub mod keccak;`, list it in the header comment); Create `server/tests/vectors/` (FIPS 202 examples as text).

**Interfaces:**
- Produces: `pub fn sha3_256(data: &[u8]) -> [u8; 32]`, `pub fn sha3_512(data: &[u8]) -> [u8; 64]`, `pub struct Shake { .. }` with `Shake::shake128() -> Shake`, `Shake::shake256() -> Shake`, `absorb(&mut self, &[u8])`, `squeeze(&mut self, out: &mut [u8])` (absorb after squeeze is a bug: debug-assert).

- [ ] **Step 1:** Write tests `sha3_256_empty_and_abc`, `sha3_512_abc`, `shake128_empty_32`, `shake256_empty_32`, `shake_incremental_equals_oneshot` (split input at every offset 0..=200, squeeze in odd chunk sizes) using the NIST FIPS 202 example values (fetch the expected hex from NIST's "SHA3-256_Msg0/abc" examples with `curl`, commit them as constants).
- [ ] **Step 2:** Run `cargo test --release keccak` → fails (module missing).
- [ ] **Step 3:** Implement Keccak-f[1600] (24 rounds, u64 lanes, no allocation) and the sponge with rates 168/136/72/104.
- [ ] **Step 4:** Run tests → pass. Run clippy → clean.
- [ ] **Step 5:** `git add -A; git commit -m "Add keccak.rs: SHA-3 and SHAKE (FIPS 202)"`

### Task 2: AES-GCM and HKDF (`aead.rs`)

**Files:** Create `server/src/aead.rs`; Modify `lib.rs`.

**Interfaces:**
- Consumes: `crypto::hmac_sha256`.
- Produces: `pub fn aes_gcm_seal(key: &[u8], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8>` (ciphertext ‖ 16-byte tag; key is 16 or 32 bytes, else panic), `pub fn aes_gcm_open(key: &[u8], nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>>` (constant-time tag check), `pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8])` (out ≤ 255·32).

- [ ] **Step 1:** Tests: `gcm_nist_vectors_128_and_256` (SP 800-38D / NIST CAVP gcmEncryptExtIV cases incl. AAD, empty plaintext and non-multiple-of-16 lengths), `gcm_open_rejects_flipped_bit_in_ct_tag_and_aad`, `hkdf_rfc5869_cases_1_to_3`.
- [ ] **Step 2:** Run → fail.
- [ ] **Step 3:** Implement AES with a table-free portable core (bitsliced or S-box with no secret-indexed table is *not* required for the relay's key use, but keep the portable path simple and correct) plus an AES-NI path chosen at runtime like `has_sha_ni()`; GHASH portable (4-bit tables acceptable) with PCLMULQDQ optional. A test must force the portable path and compare it with the fast path on random inputs.
- [ ] **Step 4:** Run → pass; clippy clean.
- [ ] **Step 5:** Commit `Add aead.rs: AES-GCM and HKDF`.

### Task 3: P-256 (`p256.rs`)

**Files:** Create `server/src/p256.rs`; Modify `lib.rs`; vectors in `server/tests/vectors/`.

**Interfaces:**
- Consumes: `crypto::{hmac_sha256, sha256, random_bytes}`.
- Produces: `pub fn generate() -> [u8; 32]` (private scalar in [1, n-1], rejection sampling), `pub fn public_key(d: &[u8; 32]) -> Option<[u8; 65]>` (uncompressed 0x04‖X‖Y; None if d invalid), `pub fn ecdsa_sign(d: &[u8; 32], msg: &[u8]) -> [u8; 64]` (SHA-256 of msg, RFC 6979 deterministic k, low-S **not** required, output r‖s), `pub fn ecdh(d: &[u8; 32], peer: &[u8]) -> Option<[u8; 32]>` (peer uncompressed 65 bytes, validates on-curve, rejects infinity; returns X), `pub fn ecdsa_verify(public: &[u8; 65], msg: &[u8], sig: &[u8; 64]) -> bool` (needed only to test signing without a second implementation; may be `#[cfg(test)]`).

- [ ] **Step 1:** Tests: `rfc6979_a25_sha256_sample_and_test` (fetch RFC 6979 §A.2.5 with `curl`; assert r‖s equals the RFC's values for messages "sample" and "test"), `public_key_matches_rfc6979_vector`, `ecdh_rfc8291_example_shared_secret` (RFC 8291 §5 keys), `ecdh_rejects_off_curve_and_infinity`, `sign_then_verify_random_100`, `scalar_zero_and_n_are_rejected`.
- [ ] **Step 2:** Run → fail.
- [ ] **Step 3:** Implement 256-bit field arithmetic on `[u64; 4]` (add/sub/mul with the NIST P-256 fast reduction, Montgomery or Solinas), Jacobian point ops, and a **constant-time** scalar multiplication (fixed-window or Montgomery ladder with masked selects); modular inverse via Fermat. Any secret-dependent branch or table index is a bug.
- [ ] **Step 4:** Run → pass; clippy clean.
- [ ] **Step 5:** Commit `Add p256.rs: ECDSA sign and ECDH on P-256`.

### Task 4: ML-KEM-768 encapsulation (`mlkem.rs`)

**Files:** Create `server/src/mlkem.rs`; Modify `lib.rs`; vectors in `server/tests/vectors/mlkem768_encaps.txt`.

**Interfaces:**
- Consumes: `keccak::{sha3_256, sha3_512, Shake}`, `crypto::random_bytes`.
- Produces: `pub const EK_LEN: usize = 1184; pub const CT_LEN: usize = 1088;` `pub fn check_ek(ek: &[u8]) -> bool` (length and the FIPS 203 §7.2 modulus check: every 12-bit coefficient < q), `pub fn encaps_with(ek: &[u8; 1184], m: &[u8; 32]) -> ([u8; 1088], [u8; 32])`, `pub fn encapsulate(ek: &[u8]) -> Option<([u8; 1088], [u8; 32])>` (random m, None if `!check_ek`).

- [ ] **Step 1:** Tests: `encaps_matches_acvp_vectors` (NIST ACVP-Server ML-KEM encapsulation AFT, parameter set ML-KEM-768, a handful of cases; assert ciphertext and shared secret), `check_ek_rejects_wrong_length_and_coefficient_ge_q`, `encapsulate_two_calls_differ`.
- [ ] **Step 2:** Run → fail.
- [ ] **Step 3:** Implement FIPS 203 Algorithms 14 (K-PKE.Encrypt) and 17 (ML-KEM.Encaps): ByteDecode/Encode, NTT/inverse NTT with the standard zetas, SamplePolyCBD(η=2), compress/decompress (d_u=10, d_v=4), matrix expansion with SHAKE128 (XOF) and PRF with SHAKE256. Fixed-size arrays, no allocation.
- [ ] **Step 4:** Run → pass; clippy clean. Also write the Rust-side vector the JS test will check in Task 8: `#[test] #[ignore] fn print_crosscheck_vector` prints a hex `(dk, ek, ct, ss)` only after Task 8 provides `dk` (kept out of the repo until then).
- [ ] **Step 5:** Commit `Add mlkem.rs: ML-KEM-768 encapsulation (FIPS 203)`.

### Task 5: Web Push wire format (`webpush.rs`)

**Files:** Create `server/src/webpush.rs`; Modify `lib.rs`.

**Interfaces:**
- Consumes: Tasks 2-4.
- Produces:
  - `pub fn vapid_header(private: &[u8; 32], endpoint_origin: &str, sub: &str, exp_secs: u64) -> String` returning `vapid t=<jwt>, k=<b64url public>` (JWT = `{"typ":"JWT","alg":"ES256"}.{"aud":origin,"exp":..,"sub":..}`, signature r‖s, base64url no padding).
  - `pub fn encrypt_aes128gcm(p256dh: &[u8], auth: &[u8; 16], plaintext: &[u8], pad_to: usize) -> Option<Vec<u8>>` and its deterministic twin `encrypt_aes128gcm_with(eph_private: &[u8; 32], salt: &[u8; 16], p256dh: &[u8], auth: &[u8; 16], plaintext: &[u8], pad_to: usize) -> Option<Vec<u8>>`: RFC 8291 §3.3-3.4, single record, header = salt‖rs(4096 BE)‖idlen(65)‖ephemeral public key.
  - `pub fn seal_inner(kem_ek: &[u8], json: &[u8]) -> Option<Vec<u8>>` = `0x01 ‖ kem_ct(1088) ‖ nonce(12) ‖ AES-256-GCM(key = HKDF(salt = "", ikm = ml_kem_ss, info = "esc-push-v1"), padded)`; padding: `json ‖ 0x80 ‖ zeros` to a multiple of 512 bytes (decrypt side strips back to the last 0x80).
  - `pub fn open_inner_for_test(...)` is **not** provided: decapsulation lives only in JS; the cross-check is Task 8.

- [ ] **Step 1:** Tests: `rfc8291_section5_example_exact_bytes` (fixed ephemeral key, salt, plaintext "When I grow up, I want to be a watermelon", output equals the RFC's base64url body), `rfc8292_vapid_example_verifies` (build a JWT with a fixed key, verify with `p256::ecdsa_verify`, check header form and `k=`), `encrypt_rejects_bad_p256dh_length_and_off_curve`, `inner_layout_lengths` (output length = 1+1088+12+padded+16, padded multiple of 512), `inner_two_seals_differ`.
- [ ] **Step 2:** Run → fail.
- [ ] **Step 3:** Implement per RFC; the record's last-record delimiter is `0x02` after the optional padding; reject plaintext that would exceed 4096-86.
- [ ] **Step 4:** Run → pass; clippy clean.
- [ ] **Step 5:** Commit `Add webpush.rs: VAPID, RFC 8291 and the PQ inner layer`.

### Task 6: Subscriptions, keys and routes (`push.rs` store, `gate.rs`)

**Files:** Create `server/src/push.rs`; Modify `server/src/bridge.rs` (own a `push::Store`), `server/src/gate.rs`, `server/src/lib.rs`, `server/src/main.rs` if startup needs the data path.

**Interfaces:**
- Produces: `pub struct Sub { pub id: u64, pub aid: Aid, pub sess: Hash, pub endpoint: String, pub p256dh: Vec<u8>, pub auth: [u8; 16], pub kem_ek: Vec<u8>, pub label: String }`; `pub struct Store` with `open(data: &Path) -> io::Result<Store>` (loads/creates `data/vapid`, 32 random bytes as a P-256 key, written with owner-only permissions where the OS supports it; replays `data/push.log`), `vapid_public(&self) -> [u8; 65]`, `vapid_private(&self) -> [u8; 32]`, `add(&self, Sub) -> Result<u64, PushErr>` (max 10 per account: the oldest is evicted), `remove(&self, id: u64)`, `remove_session(&self, sess: &Hash)`, `remove_account(&self, a: Aid)`, `for_account(&self, a: Aid) -> Vec<Sub>`; `pub fn endpoint_ok(url: &str) -> bool`.
- Routes (gate.rs): `GET /api/push/key` → `{"k":"<b64url 65 bytes>"}` (session required); rpc methods `push_sub` (`{endpoint,p256dh,auth,kem,label}` → `{"id":..}`) and `push_unsub` (`{id}`), added in the rpc match next to `devices`/`device_end`; `Bridge::logout`, `session_end`, account disable call `remove_session`/`remove_account`.
- `endpoint_ok`: `https://`, DNS hostname (no IP literal, no `localhost`, no userinfo, no non-443 port), host ends with one of `.googleapis.com`, `.mozilla.com`, `.push.apple.com`, `.notify.windows.com`, plus loopback `http://127.0.0.1:<port>` **only** when env `ESC_PUSH_ALLOW_LOOPBACK=1` (used by the e2e test).

- [ ] **Step 1:** Tests: `store_roundtrip_after_reopen`, `eleventh_subscription_evicts_oldest`, `remove_session_and_account`, `endpoint_ok_accepts_fcm_mozilla_apple` and `endpoint_ok_refuses_ip_literals_localhost_userinfo_ports_other_hosts` (Review Focus 1), `vapid_key_persists_and_file_is_not_world_readable` (skip the permission assert on Windows).
- [ ] **Step 2:** Run → fail.
- [ ] **Step 3:** Implement the store as an append-only line log (`+ <json>` / `- <id>`), compacted at startup when more than half is dead; `push_sub` also validates `p256dh` is on-curve (`p256::ecdh` dry run), `auth` is 16 bytes, and `mlkem::check_ek`.
- [ ] **Step 4:** Run → pass; `cargo test --release` whole suite, clippy clean.
- [ ] **Step 5:** Commit `Add push store, VAPID key and subscription routes`.

### Task 7: The push desk and the Bridge hooks (`push.rs`, `bridge.rs`)

**Files:** Modify `server/src/push.rs`, `server/src/bridge.rs`; Create `server/tests/push_e2e.rs`.

**Interfaces:**
- Consumes: `courier::request`, `webpush`, `Store`.
- Produces: `pub struct Desk` with `Desk::start(store: Arc<Store>, relay_url: Arc<Mutex<String>>) -> Desk`, `Desk::notify(&self, a: Aid, skip_sess: &dyn Fn(&Hash) -> bool, n: Note)`; `pub struct Note { pub conv: u64, pub from_name: String, pub text: String, pub unread: u32, pub msg_id: String, pub ts: u64 }`; `pub fn preview(text: &str, has_files: bool, has_voice: bool) -> String` (≤120 chars, cut on a char boundary with `…`; files → "📎 file"/"📎 N files", voice → "🎤 voice message"; pt-BR is the phone's job, send English keys the service worker translates: `"[file]"`, `"[files:N]"`, `"[voice]"`); `Switchboard::session_online(&self, sess: &Hash) -> bool`.
- Bridge hooks: after the local `kind::MSG` commit (the `tx.push` loop near `bridge.rs:1497-1514`) and in `remote_pushes` (`bridge.rs:2213`), call a private `fn push_wake(&self, st: &State, c: u64, from: Aid, j: &Json, recipients: impl Iterator<Item = Aid>)`: skip the author, skip if the recipient `blocked` the sender or is disabled (Review Focus 3), compute `unread` from the existing read-pointer logic, then `desk.notify`.
- Desk behaviour: per-account coalescing window 2 s (latest note wins per conversation); one envelope per subscription; JSON `{"c","f","t","n","i","ts"}`; headers `TTL: 86400`, `Urgency: high`, `Topic: <first 22 chars of b64url(sha256(conv id))>`, `Content-Encoding: aes128gcm`, `Authorization: vapid …` (exp = now+12 h); statuses: 2xx done, `404`/`410` → `store.remove(id)`, `429`/`5xx`/network → retry at 5 s, 30 s, 5 min then drop (Review Focus 2), `4xx` otherwise drop and log the code only.

- [ ] **Step 1:** Unit tests: `preview_cuts_on_char_boundary_emoji_and_accents`, `preview_file_only_and_voice_only`, `coalesce_keeps_latest_per_conversation` (inject a fake clock/sender through a small `trait Sender`), `retry_schedule_is_capped`, `gone_status_removes_subscription`, `session_online_true_only_for_that_session`.
- [ ] **Step 2:** `server/tests/push_e2e.rs`: start a Star on a temp dir with `ESC_PUSH_ALLOW_LOOPBACK=1`, start a fake push service (`std::net::TcpListener` on 127.0.0.1) that records requests, sign up two users, subscribe user B's session with a valid-looking subscription (generate P-256 + an ML-KEM `ek` fixture from Task 8's vector file once available; until then use `mlkem` encaps-only fixture bytes that pass `check_ek`), have A message B, assert the fake service got exactly one POST with `Authorization: vapid`, `TTL`, `Topic`, body length within 4096, **and the body does not contain the message text or sender name as bytes**; message again within 2 s → still one POST; B with a live WebSocket on that session → zero POSTs; A messages themself/blocked → zero; the fake returns 410 → subscription gone.
- [ ] **Step 3:** Run → fail. **Step 4:** Implement `Desk`, `Switchboard::session_online`, the two hook calls. **Step 5:** Run whole suite + clippy → clean. **Step 6:** Commit `Add the push desk and wire the Bridge to it`.

### Task 8: Browser side (`mlkem.js`, `push.js`, `sw.js`, `app.js`, `i18n.js`)

**Files:** Create `web/mlkem.js`, `web/push.js`; Modify `web/sw.js`, `web/app.js`, `web/i18n.js`, the Lobby's embedded file list in `server/src/lobby.rs`, `web/manifest` only if needed.

**Interfaces:**
- Produces in `mlkem.js` (ES module, also loadable by `importScripts`-free worker via `import`): `export function keygen(): {ek: Uint8Array(1184), dk: Uint8Array(2400)}`, `export function decaps(dk: Uint8Array, ct: Uint8Array): Uint8Array(32)` (FIPS 203 Algorithms 16 and 18, with the implicit-rejection re-encryption check), `export const _shake: ...` (SHA-3 for internal use).
- Produces in `push.js`: `export async function enable(rpc): Promise<'on'|'denied'|'unsupported'>`, `export async function disable(rpc)`, `export async function openPush(data: ArrayBuffer, dk: Uint8Array): Promise<{c,f,t,n,i,ts}|null>` (parse inner v1: AES-256-GCM via `crypto.subtle`, HKDF via `crypto.subtle.deriveBits`, strip padding at last 0x80).
- Service worker: `self.addEventListener('push', …)` imports `push.js` lazily, reads `dk` from IndexedDB (`esc-push`/`keys`), shows `{title: sender, body: preview, tag: conversation, renotify: true, data:{go: '#c=<c>'}}` (map `[file]`, `[files:N]`, `[voice]` through the page's language saved in IndexedDB); on **any** failure shows `New message` (Review Focus 5). `pushsubscriptionchange` re-subscribes and re-registers.
- Settings: under the existing notifications toggle (`web/app.js` near `notifyOn`, ~line 2663) add "Background notifications" with state text; strings in `web/i18n.js` for pt-BR.

- [ ] **Step 1:** Write the cross-check as a test page script `data/testfiles/mlkem-check.mjs` (not shipped) that: runs `keygen`, prints `ek` hex; a Rust `#[ignore]` test (Task 4 Step 4) reads that hex, encapsulates, prints `ct`/`ss`; the script decapsulates and asserts equality, then does the reverse mismatch case (flipped ct byte → different, non-throwing 32-byte secret). Also assert JS SHAKE/SHA3 against the FIPS 202 examples. Run it in the browser tool (`javascript_tool` in Chrome) first: expected `ALL OK`.
- [ ] **Step 2:** Implement `mlkem.js` (typed arrays, no allocations in the NTT loop), then `push.js`, `sw.js`, the settings UI and i18n strings.
- [ ] **Step 3:** Run the cross-check → `ALL OK`; run the Task 7 e2e again with a real `ek` from `keygen` and decrypt its captured body in the browser with `openPush` (the outer RFC 8291 layer is removed by a small JS test helper using the same subscription keys) → plaintext JSON equals what was sent.
- [ ] **Step 4:** In Chrome via the browser tool against a dev relay (`127.0.0.1:7979`, `--web ..\web`): enabling shows the permission prompt path and `GET /api/push/key` returns 65 bytes; main page network log shows `mlkem.js` is **not** fetched until a push or subscribe.
- [ ] **Step 5:** Commit `Add the browser side of background push`.

### Task 9: Docs, deploy and the real phone

**Files:** Modify `CLAUDE.md`, `README.md`, the spec (`§16`: close the background-push gap; D14/D15 stay), `deploy/install.ps1` only if the manager needs a "push" status line (skip otherwise).

- [ ] **Step 1:** Report sizes: relay binary before/after (was 1.3 MB; budget +80 KB), `web/` additions (budget +30 KB, `mlkem.js` lazy), test count (was 118 + later additions), clippy clean on Windows and the Linux/macOS targets used before.
- [ ] **Step 2:** `deploy\sync-haswell.ps1`; on haswell stop `ESC Relay`/`ESC Tunnel` tasks, `cargo build --release`, `deploy\install.ps1` (tasks restart), confirm `https://<haswell domain>` serves and `GET /api/push/key` answers.
- [ ] **Step 3:** With the user's phone: enable Background notifications, close ESC, send from a second account, confirm the notification shows sender and text and tapping opens the conversation. Record the result honestly (iOS only if installed to the Home Screen).
- [ ] **Step 4:** D15 follow-ups on haswell: install Win32-OpenSSH 10.x (Microsoft's signed release, **ask before restarting sshd**; keep the old binaries in a dated folder for rollback), then `ssh -v` must show `mlkem768x25519-sha256`; check `cloudflared` for `--post-quantum` availability on the named tunnel and report whether QUIC works on that network. Report, do not force `--protocol quic` if the tunnel gets flaky.
- [ ] **Step 5:** Update `CLAUDE.md` (what shipped, the new files, "Never log a payload" and the endpoint allowlist gotcha), commit, `sync-haswell.ps1`, `git push origin master`.

## Self-review notes

- Spec coverage: §5.1 → T1-T5; §5.2 wire format → T5; §5.3 subscribing → T6, T8; §5.4 sending → T7; §5.5 receiving → T8; §5.6 audit actions → T9 Step 4; §6 done-means → T9; risks → Global Constraints and the vectors in every crypto task.
- Interface consistency: `seal_inner(kem_ek, json)` (T5) is what `Desk` calls (T7) and what `openPush` reverses (T8); `Sub` fields match the `push_sub` rpc fields; `Note` is rendered to the JSON keys `c,f,t,n,i,ts` read by `openPush`.
- Design changes found while planning (applied to the design doc): no mute concept in ESC, so only blocked/disabled skip; the "live WebSocket" skip is per session, not per account; endpoint allowlist added against SSRF.
