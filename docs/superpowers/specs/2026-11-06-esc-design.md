# ESC — Extremely Simple Communication — design

Date: 2026-11-06 · Status: living document, updated as the build moves · Owner: the user · Builder: Claude

ESC is a tiny, very fast web app that lets people talk to each other: text forever, files within a
storage quota. It runs on a ring of **relays** (this dev box is relay #1). Every relay is one small
native binary with zero third-party code, and every relay holds a full copy of everything.

---

## 1. Decisions

| #   | Decision | Choice | By |
| --- | --- | --- | --- |
| D1  | Server runtime | **Rust, std only, zero crates.** One binary. HTTP/1.1, WebSocket, JSON, SHA-256/PBKDF2 and the storage engine are hand-written. | user |
| D2  | Relay ring | **Full mirror.** Every relay holds all accounts, messages and files. A new relay joins with one command, syncs everything and becomes an equal peer. | user |
| D3  | Public access | **Cloudflare Tunnel**, a quick tunnel (`*.trycloudflare.com`) for now; its URL changes on restart. TLS ends at Cloudflare. | user |
| D4  | Trust model | **Server-trusted.** TLS in transit; relays (your own machines) store plaintext. No end-to-end encryption in v1. | user |
| D5  | Account IDs | **4-character short code** (user). Alphabet `23456789BCDFGHJKMNPQRSTVWXZ`: no vowels, so an ID never spells a word in any language, and no 0/O/1/I/L look-alikes; 531,441 IDs (Claude). | user + Claude |
| D6  | Git | Everything on `master`, edited in place (background-job guard off for this repo only, `.claude/settings.json`). | user |
| D7  | Sign-up | **Invite-only by default**, admin can open it. A public URL + 1 GB per account + ~51 GB free disk means open sign-up could fill the disk. | Claude |
| D8  | Recovery | Per account, changeable: **email link**, **recovery code**, or **none** (unrecoverable). Email needs an SMTP account set in Admin. | Claude |
| D9  | Web app | Plain HTML + CSS + JS, zero libraries, no build step, embedded in the binary. | Claude |
| D10 | Passwords | Browser runs PBKDF2-SHA256 (600,000 rounds) with native WebCrypto; relay stores only HMAC(ring pepper, result). | Claude |
| D11 | Privacy | No presence, no "last seen", no typing indicators, no read or delivery receipts, no way to test whether an ID exists. | Claude (from the brief) |
| D12 | Data folder | `D:\esc_data` on the dev box (D: has ~51 GB free, C: ~21 GB). | Claude |

Anything marked "Claude" is a default you can overrule. Just say so.

---

## 2. Architecture

```
   browser (installable web app)              browser
        │  HTTPS + WSS                            │
   Cloudflare edge (TLS ends here)          Cloudflare edge
        │  cloudflared (quick tunnel)             │
 ┌──────▼─────────── relay #1 ──────┐     ┌───────▼──────── relay #2 ──┐
 │ esc (one binary, std only)       │     │ esc                        │
 │  net:  HTTP/1.1 + WebSocket      │ ring│                            │
 │  api:  endpoints + live RPC      │◄───►│  same binary, same data    │
 │  core: state + every rule        │ pull│                            │
 │  log:  append-only event logs    │     │                            │
 │  blobs: content-addressed files  │     │                            │
 │  web:  embedded app (~60 KB)     │     │                            │
 └──────────────────────────────────┘     └────────────────────────────┘
```

- **Threads, not async.** One OS thread per connection (small stacks), blocking I/O. Simple and fast
  up to a few thousand live connections. Upgrade path: a single `poll()`/`WSAPoll` reactor for idle
  WebSockets.
- **Event-sourced core.** Every change is an event appended to the relay's own log. The in-memory
  state is rebuilt from the logs at start (plus a snapshot for speed). Replication copies log bytes.
- **The rules live in one place** (`core`): quotas, recycling, privacy and group limits. The API
  only translates.
- **Behind Cloudflare:** the relay listens on `127.0.0.1` over plain HTTP; `cloudflared` provides
  HTTPS. The relay trusts `CF-Connecting-IP` only on loopback connections.

---

## 3. Data model

| Entity | Key fields |
| --- | --- |
| **Account** | `id` (e.g. `K7Q2`), display name (optional, ≤ 40 chars), password salt + verifier, recovery (email / code / none), `accept_requests` (default on), `auto_recycle` (default on), quota override, admin flag, disabled flag, scheduled cleanup time |
| **Session** | SHA-256(token) → account, created, device label. Listed and revocable in Settings |
| **Invite** | code → created by, uses left, expiry |
| **Request** | from, to, short note (≤ 280 chars), time, state. **Void** requests (unknown ID, requests off, blocked) are stored but shown to nobody except as "sent" to the sender |
| **Conversation** | random id, kind (direct / group), name (group), members with state (invited / joined / left) and role (owner / admin / member) |
| **Message** | conversation, sender, time, text (≤ 16,000 chars), attachments. ID = (origin relay, sequence) |
| **Blob** | SHA-256 of the bytes, size. Stored once on disk, however many people hold it |
| **Attachment** | per message: blob, file name, MIME type, width/height, thumbnail blob, preview blob |
| **File ref** | per account and blob: state (active / recycled / deleted / refused), pinned ("always saved"), where it came from (drive upload, sent, received + conversation), first added time |

A blob is deleted from disk when no account holds an active ref to it.

---

## 4. Storage, quotas and auto-recycling

These are the core rules, all enforced in `core` and covered by table tests.

**Definitions** (per account `a`)

- `quota(a)` = the account's override, or the admin default (**1 GiB**).
- `usage(a)` = sum of the **charged size** of `a`'s active file refs. Each blob counts once per
  account, even if it appears in several conversations.
- **Charged size** = original + its preview images (thumbnail and large preview). The app always
  shows the charged size, so the numbers add up.
- `pinned(a)` = sum of charged sizes of active refs marked "always saved".
- **Flex** = admin setting, default **10 %**. Ceiling = `quota × 1.10`.
- **Text never counts.** Message text is stored forever and is never refused.

**R1 – Sending a file** (attachment or drive upload; the sender's own copy counts for the sender)

- Auto-recycle **on**: allowed if `pinned + size ≤ quota`. After the upload, the oldest unpinned
  files are recycled until `usage ≤ quota`.
- Auto-recycle **off**: allowed if `usage + size ≤ quota`. Otherwise: "Not enough space: free some
  in the Cleaning Room, or turn on auto-recycling."

**R2 – Receiving a file** (each recipient is judged separately)

- Auto-recycle **on**: accepted if `pinned + size ≤ ceiling`. Then the oldest unpinned files (never
  the new one) are recycled until `usage ≤ quota`. If only pinned files are left, the account may
  sit above quota, but never above the ceiling. **This is the "always received" guarantee.**
- Auto-recycle **off**: accepted if `usage + size ≤ quota`. The ceiling does not apply.
- **Refused** means the message text still arrives, and the attachment shows "Not received, your
  storage was full", with a **Receive now** button. That button works later, after you free
  space, as long as anyone else still holds the file. **The sender is never told** (privacy
  rule P6).

**R3 – Warnings** (auto-recycle off)

- ≥ 80 %: yellow notice on the storage meter.
- ≥ 95 %: **extreme warning**: a red banner on every screen that cannot be dismissed, plus a
  full-screen notice once per session: *"Your storage is 96 % full. New messages with files will
  probably NOT be received."* It offers the Cleaning Room and the auto-recycle switch.

**R4 – Pinned files ("always saved")** are never auto-recycled. Pinning is per account.

**R5 – Turning auto-recycle back on** while over quota schedules a cleanup:

- The cleanup runs **24 h** later (admin setting). It covers every file, including files received
  under the old rule.
- Until then the app shows a countdown and the exact list of files that will go (oldest unpinned
  first, until `usage ≤ quota`), with **Pin** buttons and **Recycle now**.
- New files during the wait are accepted under R2 without immediate recycling. The scheduled
  cleanup brings the total back under quota.

**R6 – Turning auto-recycle off** cancels a scheduled cleanup. R3 applies from then on.

**R7 – Admin changes a quota.** If the account is now over quota, it gets an R5 cleanup when
auto-recycle is on, or R3 warnings when it is off.

**R8 – Recycling and deleting** remove only that account's ref. Your copy shows "Recycled
automatically" or "Deleted by you"; other people keep theirs. **Get again** restores your copy
while anyone else still holds the file.

**R9 – Relay disk guard.** Each relay keeps a free-space reserve (default 5 % of the disk or 2 GB,
whichever is larger). Uploads that would cross it are refused for everyone, with "The relay is out
of disk space; contact the admin." Admin shows overcommit: the sum of quotas against free disk.
*"Always received" cannot hold if the relay's disk itself is full.*

---

## 5. Privacy: nothing is sent without a literal user action

- **P1** No presence, no last seen, no typing indicator, no read or delivery receipts.
- **P2 Requests are fire-and-forget.** The sender always sees "Request sent", whether the ID
  exists, has requests off, has blocked them, or declines. The only visible outcome is an
  **accept**: the conversation then appears for both.
- **P3 Decline is silent.** The recipient can also **Block**, which silently voids future
  requests from that ID. The sender can cancel a pending request.
- **P4** Your display name is visible only to people you share an accepted conversation with. You
  can give contacts private nicknames.
- **P5 Groups** (≤ 128 accounts, invited included) follow the same rules:
  - You join only by accepting the invite. Until then you see nothing of the group.
  - Members see you as "invited".
  - New members see messages from their join time on.
  - If you have requests turned off, only people you already share a conversation with can invite you.
- **P6** Nothing about a recipient's storage (refused files, warnings) is ever reported to the
  sender.
- **P7** Admin sees accounts (ID, dates, usage, quota, flags), never message content.
- **Abuse limits:**
  - at most 20 pending outgoing requests per account, and 50 new requests per day;
  - login and sign-up attempts are rate-limited per IP and per ID.

---

## 6. Accounts, sign-in, recovery

- **Sign-up**:
  1. Enter an invite code (when sign-up is invite-only) and a password.
  2. The relay assigns a random free ID.
  3. The ID is shown large, with copy and share.
  4. Optional: display name and a recovery method.
- **Password flow** (D10):
  1. The app asks `/api/pre` for the ID's salt. Unknown IDs get a fake but stable salt, so the
     reply never reveals whether an ID exists.
  2. The browser computes `K = PBKDF2-SHA256(password, salt, 600000)` with WebCrypto.
  3. The relay compares `HMAC(pepper, K)` in constant time.
  4. A stolen database alone is useless without the pepper. With the pepper, every guess still
     costs 600k rounds.
- **Sessions**: a 256-bit token in an `HttpOnly; Secure; SameSite=Strict` cookie; the relay keeps
  only its SHA-256.
- **Recovery**:
  - **Email**: a single-use link valid for 30 minutes, sent through the admin's SMTP account
    (the relay calls the OS's `curl`).
  - **Recovery code**: shown once (`XXXXX-XXXXX-XXXXX-XXXXX`); the relay stores only its HMAC.
  - **None**: a lost password means a lost account; the app says so clearly when you choose it.
- **First admin**: `esc init` prints a one-time admin sign-up link in the console. No
  "first visitor becomes admin" race.

## 7. IDs

- **Format**: 4 symbols from `23456789BCDFGHJKMNPQRSTVWXZ`, random, case-insensitive, always shown
  in uppercase monospace. Share link: `https://<relay>/#K7Q2`.
- **Growth**: if the 4-symbol space ever passes 50 % use, new IDs get 5 symbols (14 million more).
- **Cross-relay collision**: two relays could hand out the same ID inside the same second of
  replication, a roughly one-in-half-a-million coincidence. The earlier one keeps it; the other
  account gets a new ID and a notice. Its password keeps working, because the salt is stored per
  account.

---

## 8. Files and media

- **Upload**:
  - Chunked: 8 MiB pieces, safely under Cloudflare's 100 MB request cap.
  - Resumable after a dropped connection.
  - SHA-256 is computed while the bytes stream in, using the CPU's SHA instructions when present.
  - Identical content is stored once.
- **Thumbnails and previews, made by the sender's browser with its native codecs**:
  - `createImageBitmap` + `OffscreenCanvas` produce a WebP thumbnail (≤ 320 px) and, when needed, a
    WebP preview (≤ 2048 px).
  - A preview is needed when the original is not a web-safe format, or is over 4 MB or 4096 px.
  - This works for every format the sender's browser can decode: PNG, JPEG, GIF, WebP, AVIF, BMP and
    ICO everywhere; HEIC in Safari; JPEG XL in browsers that support it.
- **Before sending**: picked, pasted or dropped files appear as a preview tray (thumbnails, names,
  sizes, remove buttons) with the space they will use.
- **Viewer**:
  - Images open in an in-app viewer with zoom, pan, swipe/arrow keys through the conversation's
    images, download and pin.
  - The original is shown when your browser can decode it; otherwise the preview. GIFs animate.
- **Video and audio** open in a new tab with the browser's own player. Downloads support HTTP Range,
  so seeking works.
- **Everything else** downloads with its original name.
- **Safety**: user files are served with:
  - `Content-Security-Policy: sandbox`, `X-Content-Type-Options: nosniff` and `Cache-Control: private`;
  - HTML, SVG, XML and script types are always forced to download.

  A malicious file can never run inside ESC.
- **Known limit (your call, later):** if the *sender's* browser cannot decode a format (for
  example HEIC sent from Chrome on Windows), there is no thumbnail. The file shows a format badge
  and still downloads. Recipients on Safari still get a live preview. Fixing it everywhere needs a
  decoder library (WASM) or server-side codecs, both against the zero-library rule.

---

## 9. Screens

| Screen | What it does |
| --- | --- |
| Welcome | Sign in · Create account (invite code) · Recover |
| Main | Conversation list + open conversation (one pane at a time on phones). Header: your ID (tap = copy/share), storage meter, settings |
| New conversation | Type an ID (+ optional note) → "Request sent" |
| Requests | Incoming requests and group invites: Accept · Decline · Block |
| Conversation | Messages, image thumbnails, file cards, composer with attach / paste / drop, the pre-send tray |
| Group | Create (name + IDs), members, invite more (≤ 128), leave, rename (owner/admin) |
| Drive | All your files: your own uploads plus conversation files (sent and received). Grid or list, filters, upload, pin, attach from Drive into a conversation (no re-upload) |
| Cleaning Room | Filter by conversation / type / sent-received-drive / pinned / date. Sort by size, date, name. Multi-select with a running "will free X MB" total. Delete and pin in bulk. Shows the auto-recycle queue |
| Settings | Display name, accept requests, auto-recycle (+ cleanup countdown), recovery method, password, sessions, blocked IDs, sign out |
| Admin | Default quota, per-account quota, flex %, cleanup delay, disk reserve, sign-up mode, invites, accounts table, SMTP, relays (status, lag, URL, join token) |

Design direction: calm, dense, fast. System font stack, one accent colour, light and dark themes,
no web fonts, no images in the shell (inline SVG icons only).

## 10. Wire protocol

- **HTTP**:
  - `GET /` serves the app shell. `GET /a/<hash>.<ext>` serves immutable assets.
  - `POST /api/{pre,signup,login,logout,recover}` handle sign-in and recovery.
  - `POST /api/up` creates an upload; `PUT /api/up/<id>?o=<offset>` sends a chunk.
  - `GET /f/<blob>/<name>` serves the original (with Range). `GET /t/<blob>` and `/p/<blob>` serve
    the thumbnail and the preview.
- **WebSocket `/ws`** (JSON with short keys):
  - Requests: `{"i":7,"m":"send",…}`.
  - Replies: `{"i":7,"r":…}` or `{"i":7,"e":"…"}`.
  - Pushes: `{"p":"msg",…}`.
  - The server pings every 30 s; Cloudflare closes idle sockets at 100 s.
- **On connect** the app sends its last cursor and receives what changed: conversations, requests,
  file refs, settings and recent messages. History pages are fetched on scroll.

## 11. On disk

```
D:\esc_data\
  relay.ini            this relay: id, name, listen address, public URL
  secret               ring secret: pepper + ring HMAC key (read by owner only)
  log\<relay>.log      one append-only event log per origin relay (byte-identical on every relay)
  blobs\ab\cdef…       content-addressed files (SHA-256)
  tmp\                 uploads in progress
  snap\state.snap      periodic snapshot for fast start
```

Each log record is `[length][CRC-32][time][type][JSON payload]`. A torn record at the tail (power
loss mid-write) is detected by its length or CRC and cut off at start. Writes are group-committed:
one flush per batch of a few milliseconds. A message is only acknowledged once it is on disk.
Message history is read straight from the logs with positional reads; memory holds only an 8-byte
pointer per message.

## 12. The relay ring (full mirror)

- **Identity**: each relay has a random id, a name, its URL (Cloudflare or LAN) and the shared
  ring secret.
- **Genesis**: `esc init` on relay #1 creates the ring secret and the first admin link.
- **Join**:
  1. Admin → Relays → **Add relay** makes a one-time join token.
  2. On the new machine, run `esc join https://<any relay> <token>`.
  3. It fetches the ring secret and peer list, pulls every log and blob, then starts serving.
- **Replication is pull-only and byte-exact**:
  - Each relay remembers how many bytes it has of every origin's log.
  - For each peer it holds one long-lived stream, `GET /ring/pull?have=…`, signed with the ring key.
  - It appends what arrives (CRC-checked) to its copy, then applies it to its state.
  - A relay forwards every origin it has, so A–B–C works even when A cannot reach C directly.
- **Blobs** are fetched from any peer on first need, then in the background, and verified by
  SHA-256.
- **Transport**: `http://` peers (LAN) use a plain socket. `https://` peers (Cloudflare) go
  through a `curl` child process, which ships with Windows 10+, Linux and macOS.
- **Ordering and conflicts**:
  - Messages are ordered by (time, origin, sequence).
  - Settings are last-writer-wins.
  - Recycle and delete are idempotent.
  - ID collisions are resolved as in §7.
- **Quick tunnel URLs change on every restart**: the relay reads its new URL from `cloudflared`
  and announces it to the ring as an event. The app lists all relay URLs, so users can switch if
  one is down. Sessions are replicated, but cookies are per site, so you sign in once per relay URL.

---

## 13. Targets

| Target | Goal |
| --- | --- |
| Relay binary | < 2 MB, zero crates |
| Relay idle memory | < 10 MB |
| Text message, relay time | < 1 ms (excluding the disk flush) |
| Web app | ~60 KB total uncompressed, no build step, no libraries; Cloudflare compresses it |
| Add a relay | copy one file, run one command |

## 14. Testing

- **Unit** (`cargo test`):
  - crypto against RFC vectors, JSON, HTTP parsing, WebSocket frames;
  - log framing and torn-tail recovery;
  - table tests for every quota and recycle rule (R1–R9) and every privacy rule (P1–P6).
- **Integration**: the relay on a random port, driven over HTTP and WebSocket; a two-relay ring on
  one machine (replication, blob fetch, collision handling).
- **Browser**: the real UI driven through Chrome DevTools:
  - sign-up, request and accept, text, files and previews, video in a new tab;
  - groups, Drive, Cleaning Room, auto-recycle schedule, the 95 % warning, admin.

## 15. Milestones

1. **M1 Relay core**: crypto, JSON, HTTP, WebSocket, event log, state, accounts, sessions, invites.
2. **M2 Talking**: requests, direct conversations, groups, messages, history, live push. The web
   app's welcome, main, conversation, requests and settings screens.
3. **M3 Files**: chunked upload, blob store, refs, quota rules R1–R9, pins, previews, viewer,
   Drive, Cleaning Room.
4. **M4 Admin and recovery**: admin screen, recovery codes, email via curl.
5. **M5 Ring**: `esc join`, pull replication, blob fetch, relay list.
6. **M6 Live on this box**: data on `D:`, autostart (Task Scheduler), quick tunnel, outside test.

## 16. Open for you, later

- HEIC / JPEG XL fallback when the sender's browser cannot decode (§8).
- An SMTP account for email recovery (Gmail app password or any provider). Until then, recovery
  codes work.
- A domain on Cloudflare, for a stable URL instead of a quick tunnel that changes on restart.
- Out of scope for v1, possible later:
  - Background push notifications. The app does notify while it is open.
  - End-to-end encryption, message edit/delete, reactions.
