# ESC programme — Antigravity (`GEMINI.md`) session rules & reference

ESC = **Extremely Simple Communication**.
The design spec is the source of truth: `docs/superpowers/specs/2026-11-06-esc-design.md` (decisions D1–D12, the names glossary, storage rules R1–R9, and privacy rules P1–P7). Update it whenever a decision changes, and note who decided.

## Ground rules (from the user)

- **Zero third-party code.**
  - **Relay (`server/`)**: Rust `std` only (Edition 2024). Never add a crate in `Cargo.toml`, not even a `dev-dependency`. HTTP/1.1, WebSocket, JSON, SHA-256/PBKDF2/HMAC, and the append-only storage engine are all hand-written.
  - **Web app (`web/`)**: Plain HTML/CSS/JS. No npm, no frameworks, no CDN, no build step.
  - **OS-native tools**: Allowed (`curl` for outbound HTTPS/SMTP, `cloudflared` for the tunnel, WebCrypto/MediaRecorder/OffscreenCanvas in the browser).
- **Extreme performance, smallest footprint.** Measure, don't guess. No allocation in hot paths, positional reads from the Logbook (`8-byte` pointer per message in memory), SHA-NI hardware acceleration, and group-committed log flushes.
- **Everything on `master`, edited in place.** No feature branches.
- **Names glossary (use consistently in code, UI, and docs)**:
  - **User-facing**: **Callsign** (4-char account ID, e.g. `K7Q2`, drawn as signal flags), **Knock** (conversation request; accept = **Open the door**, decline = **Ignore**, block = **Bolt**), **Door** (accept-requests switch), **Line** (direct 1:1 chat), **Crew** (group ≤ 128), **Stash** (personal drive), **Tank** (storage quota, default 1 GiB), **Spillway** (flex % overflow ceiling, default 10%), **Keepers** (pinned / always-saved files), **Dustin** (auto-recycler robot; scheduled 24h cleanup = "Dustin's round"), **Bounced** / **Catch it** (refused file when Tank is full & auto-recycle is off), **Rescue** (restore recycled/deleted file while any peer/account still holds the blob), **Red Alert** (≥ 95% Tank warning), **Cleaning Room** (bulk file manager/filter), **Ticket** (invite code), **Lifeline** (account recovery: email / code / none), **Control Tower** (admin screen), **Tray** (pre-send attachment tray), **Peek** (in-app image viewer), **The Ring** (full-mirror relay network), **Star** (a single relay; this machine is **Polaris**), **Spark** (one-time relay join token).
  - **Relay internals (`server/src/`)**:
    - `bridge.rs` — **The Bridge**: holds locked state, enforces all rules (R1–R9, P1–P7), **Switchboard** (live WebSocket pushes), **Bouncer** (rate limits), **Dustin** (auto-recycling), and profile/crew pictures (`MAX_PIC` = 256 KiB).
    - `model.rs` — Deterministic event-sourced state (`State::apply`), rebuilt from the Logbook.
    - `logbook.rs` — **The Logbook**: append-only per-origin event logs (`[length][CRC-32][time][type][JSON]`), torn-tail recovery, group commit, positional reads.
    - `hold.rs` — **The Hold**: content-addressed SHA-256 blob store (`blobs/ab/cdef...`) and chunked resumable uploads (`tmp/`).
    - `gate.rs` — **The Gate**: HTTP/1.1 routes (`/api/*`, `/f/*`, `/t/*`, `/p/*`, `/pic/*`, `/ring/*`) and `/ws` WebSocket entrance.
    - `lobby.rs` — **The Lobby**: embeds `web/` assets at compile time (or reads `--web <dir>` in dev), rewrites `./x.js` module imports (1 level deep) and `index.html` links with content-hash `?v=<tag>` tags.
    - `ring.rs` — **The Ring**: pull-only byte-exact log replication (`/ring/pull`) and on-demand/background blob replication between Stars.
    - `courier.rs` — **The Courier**: outbound HTTP (plain socket for `http://`, `curl` for `https://`) and SMTP via `curl`, plus `cloudflared` quick-tunnel URL watcher.
    - `http.rs`, `ws.rs`, `json.rs`, `crypto.rs`, `ids.rs` — Zero-dependency plumbing.
- **Communication & workflow**:
  - Ask the user about product, privacy, and accounts/infrastructure (put the recommended option first), and continue working on anything that doesn't block on the answer.
  - Keep `GEMINI.md` and `docs/superpowers/specs/2026-11-06-esc-design.md` up to date and narrate changes clearly.
  - The user is Brazilian (`pt-BR`) and writes English as a second language; read for intent.

## Commands & Environment

- **Unit & E2E tests**: `cd server; cargo test --release` (or `--lib` for fast unit tests).
- **Linter**: `cd server; cargo clippy --all-targets -- -D warnings`
- **Release build**: `cd server; cargo build --release` → `server/target/release/esc.exe`
- **Dev server**:
  - **Port `7878` is LIVE PRODUCTION** (run by Windows Task Scheduler as `SYSTEM` from `D:\esc\esc.exe` with data in `D:\esc_data`). **Never run dev on port 7878.**
  - For dev runs, copy `esc.exe` first (so a running dev process doesn't lock `target\release\esc.exe`) and run on port `7979` with `--web ..\web`.
- **TUI Star Manager**: `.\esc-manager.cmd` (or double-click `esc-manager.cmd` in Explorer; `.\esc-manager.cmd status` for non-interactive status). Single-file `.cmd` + PowerShell TUI to start/stop/restart Production (`Polaris` + Cloudflare Tunnel), build & deploy (`cargo build --release` → `D:\esc`), toggle the Dev server on port `7979`, copy/open public or local URLs, watch live logs, and run `clippy`/tests.
- **Production deployment (`D:\esc` & `D:\esc_data`)**:
  - Check status & current Cloudflare quick-tunnel URL: `.\esc-manager.cmd status` (or `powershell -ExecutionPolicy Bypass -File deploy\status.ps1`)
  - Deploy/update live relay: `cd server; cargo build --release`, then `powershell -ExecutionPolicy Bypass -File deploy\install.ps1` (stops tasks, copies `esc.exe` to `D:\esc\esc.exe`, restarts `ESC Relay` and `ESC Tunnel`).

## Gotchas & Invariants

1. **PowerShell case sensitivity**: `-replace` is case-insensitive; always use `-creplace` for code transformations.
2. **WebSocket RPC keys**: In `web/app.js`, `rpc(m, args)` places `i` and `m` last so arguments cannot overwrite them. Never use `m` or `i` as an RPC argument key.
3. **Blob ref-counting order (`model.rs`)**: Always create/attach file or picture refs *before* releasing upload ("pending") holds (`unpend`), otherwise a blob's holder count drops to zero and it gets deleted from disk immediately.
4. **Embedded ES module imports (`lobby.rs`)**: `Lobby` rewrites `'./x.js'` imports to `'./x.js?v=<tag>'` one level deep. Any file imported by `app.js` (such as `i18n.js`) must not itself import another local module (enforced by `lobby::tests::module_imports_are_versioned_and_one_level_deep`).
5. **Line endings**: `.gitattributes` enforces `* text=auto eol=lf`.

## Current State

### 2026-11-09: Deployed & Live on `Polaris` (Gemini / Antigravity)
- **Live Production (`Polaris`)**:
  - Running at `D:\esc\esc.exe` (data in `D:\esc_data`, 3 accounts: `84FZ`, `SPKN`, `B488`) on `127.0.0.1:7878` behind `cloudflared`.
  - `deploy\install.ps1` and `esc-manager.cmd` now preserve a running `cloudflared` process across relay restarts and code deployments so the active `*.trycloudflare.com` URL does not rotate unless the machine reboots or the user explicitly rotates it in `[S] Settings`.
  - `deploy\status.ps1` and `esc-manager.cmd` inspect only the latest boot section of `esc.log` so stale first-boot `#ticket=` lines are never shown after an admin exists.
- **Completed & Deployed Feature Batch (126 unit tests + 4 multi-Star E2E tests passing)**:
  - **Profile & Crew pictures**: `PUT /api/pic` (`?c=<conv>` for Crew), `GET /pic/u/<callsign>` & `GET /pic/c/<conv>`, `pic_clear` RPC, PNG/JPEG/WebP magic-byte sniffing, max 256 KiB (`MAX_PIC`), privacy-scoped visibility (only shared conversations/inviters/knocks), replicated across Stars, not counted against Tank usage.
  - **Voice messages**: `d` (duration in ms) on `FileMeta` (`du` in file list JSON so `d` stays direction), microphone permission policy (`microphone=(self)`), voice message recording & playback UI in `web/app.js`.
  - **Localization (`web/i18n.js`)**: Complete Brazilian Portuguese (`pt-BR`, 421/421 keys) dictionary + English switch (`lang="pt-BR"` default in `index.html`) + clear offline/expired quick-tunnel notice on Welcome.
  - **Theme persistence (`web/theme.js`)**: Pre-paint theme loader (`light` / `dark` / `auto` via `localStorage['esc-prefs']`).
  - **Service Worker (`web/sw.js`)**: `notificationclick` handler that focuses an existing client or opens `/#<go>` for the target conversation.
  - **Animated TUI Star Manager (`esc-manager.cmd`)**: Single-file `.cmd` + PowerShell TUI with 102-column boxed telemetry, animated `ESC` block logo & constellation, Logbook user/settings inspector, backup tool, configurable paths (`esc-paths.ini`), `Join-ExistingRing` (`[J]`) wizard, live HTTP health check on `/api/info`, and automatic detection/self-healing when Cloudflare garbage-collects an anonymous quick tunnel (`Unauthorized: Tunnel not found`).
