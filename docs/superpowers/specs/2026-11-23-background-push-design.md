# Background push notifications, with a post-quantum inner layer

Status: **approved by the user (2026-11-23)**, including the OpenSSH 10.x upgrade on haswell and collapsing a burst into one notification per conversation (`Topic`). Not built yet.
Decisions marked *user* were made by the user; everything marked *call* is Claude's proposal and can be reversed.

## 1. Intent

The user wants the phone to buzz when a message arrives **while ESC is closed**. Today the app only notifies while
it is open (spec §16 lists "background push" as a known gap). The user also asked that the **content** of the
notification be encrypted, and that **post-quantum (PQ) protection be considered project-wide**.

Why it matters: ESC's whole point is a small, self-hosted, private chat. A phone that only learns about messages when
the app is opened is not a chat.

## 2. Goals / non-goals / anti-goals

**Goals**
- A closed ESC on Android Chrome/Firefox (and iOS 16.4+ when installed to the Home Screen) shows
  "Alice: see you at 8" within seconds of the message reaching the Star the phone is subscribed to.
- The push service (Google, Mozilla or Apple) never sees sender, text, conversation or file names, **even against an
  attacker who records traffic today and breaks elliptic curves later** ("harvest now, decrypt later").
- Zero third-party code: every primitive is written in Rust std (relay) or plain JS (service worker), same rule as
  the rest of ESC. Footprint stays small; the main page does not load the PQ code.
- A short audit of every other place ESC uses public-key crypto, with what is and is not fixable.

**Non-goals**
- Making the *outer* Web Push envelope PQ. Browsers fix it at ECDH P-256 + ECDSA P-256 (RFC 8291/8292); we cannot
  change it. We protect the content with an inner PQ layer instead.
- Hiding metadata from the push service (that *a* push for *this* device happened, when, and its approximate size).
  We pad to blunt size leakage, nothing more.
- Notifications on devices that block push services (no Google/Apple/Mozilla push = no background wake on phones).
- Replacing Cloudflare/Tailscale/OpenSSH's own crypto. We only report and recommend.

**Anti-goals** (a failure even if it "works")
- Message text, sender or conversation ever appearing in a log, a URL, a header or a push payload in clear.
- The user's e-mail or any personal detail inside the VAPID claim (RFC 8292 `sub`).
- A new crate, npm package, CDN script or build step.
- A push for a message the user already has open on a live WebSocket (double alert).

## 3. Constraints

- Relay is Rust std only; outbound HTTPS is `curl` through the Courier (existing rule). Web app is plain JS.
- Subscriptions and keys are **per Star, never replicated**: an endpoint URL is a bearer capability and a push
  subscription is bound to one origin's VAPID key. Each Star notifies *its own* subscribers when it applies a message,
  whether the message was written locally or arrived through the Ring.
- Windows `curl` (Schannel) negotiates classical X25519 on both boxes; the Star→push-service hop cannot be PQ.
- Hot paths allocate nothing; push work happens on its own thread so message commit never waits on a push service.

## 4. Approaches considered

| | Approach | Verdict |
|---|---|---|
| A | Wake-up only: push has no data; the phone calls the Star to fetch sender and preview | Smallest, but fails when the phone cannot reach the Star at that moment, and the user chose differently |
| B | Encrypted content, standard RFC 8291 only | Content is safe today, but a recorded push is readable once P-256 falls |
| **C** | **Encrypted content with a PQ inner layer: ML-KEM-768 + AES-256-GCM inside, RFC 8291 outside** | **Chosen (user: encrypted content; call: PQ inner layer)** |
| D | Generic "You have a new message" | Most private, least useful |

Why C: layered encryption is a hybrid. An attacker with a quantum computer breaks the outer P-256 layer and still faces
ML-KEM-768; an attacker who finds a flaw in the young ML-KEM still faces the outer P-256 layer. Confidentiality holds
if *either* layer holds. Because the PQ layer is inside our own payload, it needs no browser support: the service
worker decrypts it with ~500 lines of JS. (ML-KEM is not in WebCrypto yet; AES-GCM is.)

## 5. Design

### 5.1 Primitives (new `server/src/` modules, std only, each with known-answer tests)
- `p256.rs`: field and scalar arithmetic on 4×u64 limbs, point multiplication, **ECDSA sign** (deterministic nonce,
  RFC 6979), **ECDH**, key generation. Test: RFC 6979 §A.2.5 vector, RFC 8292 VAPID example.
- `aead.rs`: **AES-GCM** (128 and 256; AES-NI path with a portable fallback, like SHA-NI today) and **HKDF-SHA256**
  on the existing HMAC. Test: NIST GCM vectors, RFC 5869 vectors.
- `keccak.rs`: SHA3-256/512, SHAKE128/256 (FIPS 202). `mlkem.rs`: **ML-KEM-768 encapsulation** (FIPS 203): the relay
  only encapsulates, never decapsulates, so keygen/decaps stay out of the binary. Test: ACVP known-answer vectors
  (test data only, committed as small files).
- `push.rs` (the **Courier**'s push desk): VAPID JWT (ES256), RFC 8291 `aes128gcm` envelope, queue, retry, expiry.

### 5.2 Wire format of one notification (call)
```
outer (RFC 8291, aes128gcm, to the browser's p256dh/auth)
  inner  = version(1) || kem_ct(1088) || nonce(12) || AES-256-GCM(key=HKDF(ml_kem_ss, "esc-push-v1"), padded JSON)
  JSON   = {c: conversation id, f: sender callsign, t: text preview ≤120 chars, n: unread in that conversation, i: message id, ts}
```
Padding: inner plaintext is padded to a multiple of 512 bytes (call). Total stays under 2.5 KB, far below the 4096-byte
RFC 8291 limit. Headers: `TTL: 86400`, `Urgency: high`, `Topic: <hash of conversation id>` so a burst in one
conversation collapses to one notification showing the latest `n` (call). A version byte lets us change the inner
format without breaking installed phones.

### 5.3 Subscribing (web)
Settings → "Notifications in the background" (extends the existing `prefs.notify` toggle):
1. `Notification.requestPermission()`, then `pushManager.subscribe({userVisibleOnly:true, applicationServerKey})`.
2. Generate an **ML-KEM-768 keypair in the browser**; the private key is stored in IndexedDB (readable by the service
   worker, never sent anywhere). Honest limit: unlike WebCrypto keys it cannot be non-extractable, so a compromised
   device exposes it, which is also true of the session cookie (call).
3. `POST /api/push/sub {endpoint, p256dh, auth, kem_pk, label}` over the existing session. The relay stores it in a
   Star-local append-only file (`push.log` in the data folder, not in the Logbook, not replicated). Max 10 per account.
4. `GET /api/push/key` returns the Star's VAPID public key. The VAPID private key is generated on first start and kept in
   `data/vapid` (file permissions as for `secret`). `sub` claim = the relay's public `https://` URL, never an e-mail.

Endpoint safety (call): the relay POSTs to a URL the client supplies, so it accepts only `https://` hosts under the known push services (`.googleapis.com`, `.mozilla.com`, `.push.apple.com`, `.notify.windows.com`), never IP literals, `localhost`, other ports or userinfo (SSRF guard). A test-only env switch allows loopback.

Removal: logout, "devices" session end, account disable, or a 404/410 from the push service deletes the subscription.

### 5.4 Sending (relay)
`Bridge` already knows when a message is applied. For each recipient account (never the author): skip if the sender is blocked or the account disabled (ESC has no mute), and per subscription skip if its own session has a live WebSocket (an open desktop must not silence a phone):
the Switchboard already tracks sessions. Otherwise hand `(account, preview)` to the push desk
thread. The desk coalesces per account for ~2 s, builds the envelope per subscription, and POSTs through `curl` with the
payload on stdin (never on the command line, so it cannot appear in process lists). Bouncer rate limits apply. No payload
or endpoint is ever logged; failures log only a status code and a short subscription id.

### 5.5 Receiving (service worker)
`push` event: decrypt (outer is done by the browser), read inner, `showNotification(title, {tag: conversation, body,
data:{go}})`. **Always show something**: if decryption fails the worker shows a generic "New message" (Chrome requires a
visible notification per push, and a silent failure would get the subscription revoked). The existing
`notificationclick` handler opens the conversation. `mlkem.js` is loaded by the worker with `importScripts` only when a
push arrives, never by the main page.

### 5.6 Project-wide post-quantum audit

| Where | Public-key crypto | PQ today | What we can do |
|---|---|---|---|
| Push content | ECDH P-256 outer | **No → Yes** with this design | Inner ML-KEM-768 layer (this document) |
| Push envelope, VAPID | ECDSA/ECDH P-256 | No, fixed by browsers | Nothing; metadata only |
| Browser ↔ Cloudflare | TLS | Yes: current Chrome/Android offer X25519MLKEM768 and Cloudflare's edge accepts it | Nothing needed |
| Cloudflare ↔ `cloudflared` | tunnel TLS | **Unverified**; haswell forces `--protocol http2` (QUIC is flaky there) | Verify, and try QUIC + `--post-quantum` if the network allows |
| Star → push service, SMTP | `curl` (Schannel) | **No**: X25519 on both boxes, TLS 1.2 on haswell | Inner layer covers content; metadata stays |
| Ring (Star ↔ Star) | HMAC-SHA256 shared key over TLS | Symmetric, quantum-safe at 256 bits; the join hand-over relies on TLS | Nothing |
| Passwords, sessions | PBKDF2-SHA256, random tokens | Symmetric, fine | Nothing |
| SSH admin to haswell | OpenSSH 9.5, `curve25519-sha256` | **No** (server offers no PQ kex; this PC's client 10.3 offers ML-KEM) | Upgrade Windows OpenSSH on haswell to 10.x, then it negotiates `mlkem768x25519-sha256` |
| Tailscale (WireGuard) | Curve25519 | No | Outside our control; SSH adds its own layer on top |

## 6. Done means
- All known-answer tests pass; `cargo clippy -D warnings` clean; test count and binary size reported (budget: relay
  +80 KB, web +30 KB, `mlkem.js` lazy).
- Cross-check: ML-KEM encapsulation from Rust decapsulates correctly in JS (browser test), and a real push to a
  real Android Chrome subscription arrives encrypted, with the app closed, showing sender and preview.
- Verified on haswell after a deploy (service stopped for the upgrade, as the user allowed).
- Spec updated (decisions D14, D15, §16 gap closed), CLAUDE.md "Current state" updated, README mentions the privacy
  model in two sentences.

## 7. Left to the builder
Module boundaries inside `push.rs`, queue sizes, the exact retry schedule, the AES software fallback style, the Settings
wording and its pt-BR strings, and the test vector files' layout.

## 8. Risks (stated plainly)
- Hand-written crypto is a risk by nature. Mitigations: only standardised algorithms, known-answer tests from the RFCs
  and NIST, no custom constructions, constant-time comparisons through the existing `ct_eq`, and no secret-dependent
  branching in the scalar multiplication paths that touch private keys (call). It has **not** had an external audit.
- ML-KEM is young. The outer layer is the hedge.
- iOS needs the app installed to the Home Screen, and Apple's push service is the least predictable of the three.
