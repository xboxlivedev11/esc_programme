# ESC programme — rules for every session

ESC = Extremely Simple Communication. The spec is the source of truth:
`docs/superpowers/specs/2026-11-06-esc-design.md` (decisions D1–D12, the names glossary, rules
R1–R9 and P1–P7). Update it whenever a decision changes, and say who decided.

## Ground rules (from the user)

- **Zero third-party code.** The relay is Rust std only: never add a crate, not even a
  dev-dependency. The web app is plain HTML/CSS/JS: no npm, no frameworks, no CDN, no build step.
  OS-native things are fine: `curl` for outbound TLS/SMTP, `cloudflared` for the tunnel, WebCrypto
  and native codecs in the browser.
- **Extreme performance, smallest footprint.** Measure, don't guess. No allocation in hot paths,
  positional reads, SHA-NI, group commit.
- **Everything on `master`, edited in place.** The background-job worktree guard is off for this
  repo (`.claude/settings.json`, approved by the user on 2026-11-06). No feature branches.
- **At most ONE subagent, a Sonnet model**, used only for self-checking modules.
- **Names**: the user asked for a name for everything (glossary in the spec §1): Callsign, Knock,
  Line, Crew, Stash, Tank, Spillway, Keepers, Dustin, Bounced/Catch it, Rescue, Red Alert, Ticket,
  Lifeline, Control Tower, Tray, Peek, the Ring of Stars (Polaris = this box), Spark. Inside:
  Bridge, Logbook, Hold, Gate, Lobby, Switchboard, Bouncer, Courier.
- **Ask the user** about product, privacy and their accounts/infrastructure (recommended option
  first), and keep working on everything that doesn't depend on the answer. The user likes to read
  what is being done: keep this file and the spec current, and narrate.
- The user writes English as a second language (Brazilian, pt-BR); read for intent.

## Layout

- `server/`: crate `esc`. `src/lib.rs` lists who is who:
  - `bridge` holds state and every rule;
  - `model` is the deterministic `apply()`;
  - `logbook`, `hold`, `gate`, `lobby`, `ring` and `courier` are the named parts above;
  - `http`, `ws`, `json`, `crypto` and `ids` are the plumbing.

  `src/main.rs` is the CLI (`serve`, `join`).
- `web/`: the app (`index.html`, `app.js`, `app.css`, `sw.js`, manifest, icon). It is embedded into
  the binary at compile time; `--web <dir>` serves it from disk in dev (edit + reload, no rebuild).
- `deploy/`: `install.ps1` (D:\esc + boot tasks + quick tunnel), `status.ps1`, `uninstall.ps1`.
- `data/` (gitignored): local scratch, e.g. `data/testfiles` for browser upload tests.

## Commands

- Tests: `cd server; cargo test --release`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Release build: `cargo build --release` → `server/target/release/esc.exe`
- Dev run: copy the binary first (a running `target\release\esc.exe` blocks `cargo build`), then
  `esc serve --data <scratch dir> --listen 127.0.0.1:7979 --web ..\web` and open the printed
  `#ticket=` link. **Port 7878 is production** (Task Scheduler, SYSTEM): never run dev there.
- Production data folder: `D:\esc_data` (D: has the free space; C: is tight).

## Gotchas learned

- PowerShell `-replace` is case-insensitive; use `-creplace` for code renames.
- The app's `rpc(m, args)` puts `i`/`m` last so arguments can't overwrite them. Never use `m` or
  `i` as an RPC argument name.
- Attach order matters in `model.rs`: create the file refs first, then release the upload
  ("pending") holds, or a blob passes through zero holders and is deleted.
- `upload_file` in the browser tool only accepts paths inside the project, so use `data/testfiles`.

## Current state (newest first)

### 2026-11-09: Pictures, voice messages, pt-BR localization, themes & TUI manager live on Polaris

- See `GEMINI.md` for full handover notes and current state.
- Deployed to `D:\esc\esc.exe` (keeping `cloudflared` tunnel alive across restarts/deploys):
  - Profile & Crew pictures (`PUT /api/pic`, `GET /pic/{u,c}/*`, `pic_clear`, replicated across Stars);
  - Voice messages (`MediaRecorder`, duration `d` in `FileMeta`, inline player);
  - Brazilian Portuguese (`pt-BR`) & English dictionary (`web/i18n.js`), flash-free theme loader (`web/theme.js`), in-app toast pings & `sw.js` notification routing;
  - Interactive animated TUI Star Manager (`esc-manager.cmd`).

### 2026-11-06, night: LIVE

- **Polaris is live** on this box:
  - `D:\esc\esc.exe` with data in `D:\esc_data`;
  - SYSTEM boot tasks "ESC Relay" and "ESC Tunnel" (auto-restart loops; logs in `D:\esc\*.log`);
  - a Cloudflare quick tunnel whose URL changes on every restart (`D:\esc\status.ps1` prints the
    current URL and, until an admin exists, the first-admin Ticket).
- **Verified through the real tunnel** on a throwaway Star:
  - the Secure cookie;
  - Cloudflare gzip (app.js is 29.6 KB on the wire);
  - security headers;
  - an idle WebSocket lasting 166 s, past Cloudflare's 100 s cut;
  - automatic URL announce.
- **Footprint**: the live relay idles at a 6.1 MB working set.
- **Admin**: no account was created on production. The first admin Ticket is for the user.
- **Redeploying** after code changes: `cargo build --release`, then `deploy\install.ps1`. It stops
  the tasks, copies the binary and restarts. The quick-tunnel URL changes when the tunnel restarts.

### 2026-11-06, evening

- **Built and verified in Chrome**, two isolated users, desktop and phone, dark and light:
  - sign-up with a Ticket, Knock and open the door, Lines, Crews (void invites masked);
  - text with links, files with browser-made thumbnails, Peek, Stash, Cleaning Room;
  - Bounced, then Catch it; Red Alert (N over C); Dustin recycling the oldest file; Control Tower;
  - restart replay.
- **Sizes**: relay binary 1.3 MB including the embedded app; 118 unit tests; clippy clean on
  Windows, Linux gnu/musl/aarch64 and macOS.
- **The Ring**: `ring.rs` (Sonnet agent) is tested with three in-process Stars. The real two-binary
  path (`esc join` + replication) is being covered by e2e tests in `server/tests/e2e.rs`.
- **Known gaps** (in the spec §16):
  - Callsign collisions across Stars;
  - no snapshots (replay grows with the log);
  - no background push notifications;
  - account settings are not last-writer-wins across Stars;
  - email Lifelines are untested until an SMTP account is configured;
  - HEIC/JXL previews depend on the sender's browser.
- **Next**: finish the two-Star e2e, then go live on D: with the boot tasks and the quick tunnel.

### 2026-11-06, morning

- **M0 done**:
  - The user decided: Rust std-only, full-mirror ring, Cloudflare quick tunnel (no domain yet),
    server-trusted storage, 4-char IDs, master-only.
  - Spec written.
