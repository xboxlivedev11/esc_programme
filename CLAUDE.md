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

### 2026-11-23: published on GitHub under GPL-3.0 (decision D13, by the user)

- Public repo, `LICENSE` = GPL-3.0 text, README has a license section. History was scanned first (no tokens, keys or
  credential files ever committed; authors are the GitHub noreply address). **Never commit** `ssh_priv_key/`,
  `esc-paths.ini`, `tunnel.token`, `data/` or any Cloudflare/GitHub token: the repo is public now.
- Remotes: `origin` = GitHub, `haswell` = the live box (see the sync entry below).

### 2026-11-23: syncing with the live box (haswell) over git + SSH

- **Haswell is the source of truth for what runs**: repo at `C:\Users\usuario\Desktop\esc_programme\esc_programme`
  (note the doubled folder), deployed to `C:\esc`, data in `C:\esc_data`, `tunnel_mode = named`. SSH user is `usuario`
  (key in `ssh_priv_key/`, now gitignored; the box is `100.83.19.74` on Tailscale, default shell is `cmd`, no SFTP/scp).
- **Sync = git over SSH**: remote `haswell` (local config, not committed: `uploadpack`/`receivepack` set to
  `cd /d <repo> & git-upload-pack . & rem` because Windows `cmd` cannot parse git's single-quoted path, and
  `core.sshCommand` pointing at the key). `deploy\sync-haswell.ps1` fetches + fast-forwards ours, pushes to haswell's
  side branch `incoming` (it has `master` checked out) and fast-forwards its `master`. Deploying stays manual.
- Re-create the remote on a fresh clone: see the script header and the two `git config` lines above.

### 2026-11-17: why the relay/tunnel "died by itself" (fixed)

- **Root cause**: `schtasks /create /sc onstart` gives Windows defaults: `ExecutionTimeLimit` 72 h, no restart, stop on battery.
  Both SYSTEM tasks were killed 72 h after boot/deploy (`LastTaskResult 267014` = 0x41306), and the restart loops died with them.
  `install.ps1` now applies `New-ScheduledTaskSettingsSet` (no limit, restart every 1 min x999, ignore power); also applied to the live tasks.
- **This box today**: ESC is in `C:\esc` / `C:\esc_data` (no `D:` drive), `tunnel_mode = named` with `C:\esc\tunnel.token`.
- **Duplicate wrappers**: every manager restart left old `run-relay.ps1` / `run-tunnel.ps1` loops running (as the user, not SYSTEM);
  they fought over port 7878 and deleted each other's `tunnel.log`. Cleaned up; the SYSTEM tasks are the only owners now.
- **Flaky edge**: dials to Cloudflare's IPv6 edge (`2606:4700:a0::x:7844`) time out on this network, and QUIC to region2 fails.
  The named wrapper now runs `--protocol http2 --edge-ip-version 4`, keeps `tunnel.log` (rotated at 5 MB) and appends
  `tunnel-restarts.log` on every `cloudflared` exit. Check those two files first when the tunnel drops.
- **Manager showed OFFLINE while the tunnel was fine**: a non-elevated manager cannot read the command line or path of
  SYSTEM-owned processes, so `Get-EscCloudflared` matched nothing. The tunnel wrapper now writes `tunnel.pid` (cloudflared's
  PID) and `run-tunnel.pid` / `run-relay.pid` (its own PID); `Get-EscCloudflared` trusts `tunnel.pid`.
- **Duplicate wrappers**: the same limitation made the manager's `schtasks /run` fallback spawn a user-owned wrapper on every
  restart. `Test-WrapperAlive` now guards those fallbacks. For stop/restart to really work, run the manager as admin.
- **Network was misconfigured by an old project ("uplink", never to run again)**: Ethernet had a static `192.168.18.5`
  (`Duplicate`) on a network that is really `192.168.2.0/24`, so Windows said "no internet" and everything leaked out over
  IPv6. Ethernet is back on DHCP. **Cloudflare WARP was uninstalled completely** (client, adapter, services, registry) at the
  user's request; `cloudflared` (the ESC tunnel) is separate and stays. Windows Firewall is off on all profiles.
- **Remote admin**: `deploy\remote-ssh.ps1` + `docs\remote-access.md` (OpenSSH Server over Tailscale, key-only, listens on the
  Tailscale IP only). This PC is `haswell` / `100.83.19.74` in the tailnet.
- **Still open**: Task Scheduler history is disabled (no event for a future kill).

### 2026-11-13: tunnel modes (service + named) and a deploy bug fixed

- **`esc-paths.ini` `tunnel_mode`** is `quick` | `named` | `service` | `tailscale`. In `service` mode the Windows
  service `Cloudflared` (the `.msi`) carries a named tunnel: ESC never starts `cloudflared`, the `ESC Tunnel` task is
  disabled and `esc-manager.cmd` shows `CF Service`. In `named` mode ESC runs `cloudflared tunnel run --token-file`
  (token in `D:\esc\tunnel.token`, SYSTEM + Administrators only). Switch in the manager: `[S]` then `[4]` (service) or `[2]` (named).
- **Root cause of "0 requests on the named tunnel"**: `deploy\install.ps1` always rewrote `run-tunnel.ps1` as a quick-tunnel
  wrapper and treated any `cloudflared` as its own. Now it reads `tunnel_mode`, only writes the quick wrapper in `quick`
  mode, and ignores the service's process. The manager's restart, stop and "alive?" checks use `Get-EscCloudflared`
  (ESC's own `cloudflared`, never the service's), and a restart also stops the old wrapper loop (`Stop-TunnelWrappers`).
- **Dashboard** shows `NO DNS` when a named/service tunnel is connected but its domain does not resolve.
- **Production is live on its own domain**: `https://esc.admin.log.br`, carried by the `Cloudflared` Windows service
  (`tunnel_mode = service`; the `ESC Tunnel` task is disabled; no quick tunnel). The Cloudflare route must be
  `http://127.0.0.1:7878` (plain HTTP): `https://` there gives a 502 because the relay does not speak TLS.
  Tunnel `26fcd626-71c9-4839-8c06-aacc0b3cda5e`; the config is remote-managed (Zero Trust, or the API).
- **`relay.ini` `url =`** (permanent `https://` only, never `trycloudflare`) is now announced to the Ring at startup;
  before, only quick-tunnel URLs from the tunnel log were. `Get-EscCloudflared` only matches a `cloudflared` whose
  command line points at ESC's folder or port, so other tunnels on this PC (e.g. `localhost:3000`) are never touched.
- Secrets: never write Cloudflare API tokens or the tunnel token into the repo or docs.

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
