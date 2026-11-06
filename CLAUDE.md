# ESC programme — rules for every session

ESC = Extremely Simple Communication. The spec is the source of truth:
`docs/superpowers/specs/2026-11-06-esc-design.md`. Update it whenever a decision changes, and say
who decided.

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
- **Ask the user** about product, privacy and their accounts/infrastructure (recommended option
  first), and keep working on everything that doesn't depend on the answer. The user likes to read
  what is being done: keep this file and the spec current, and narrate.
- The user writes English as a second language (Brazilian, pt-BR); read for intent.

## Layout

- `server/`: crate `esc`. `src/lib.rs` wires the modules; `src/main.rs` is the CLI
  (`init`, `serve`, `join`, `admin`).
- `web/`: static app, embedded into the binary at compile time (`--web <dir>` serves from disk in dev).
- `docs/superpowers/specs/`: design.

## Commands

- Tests: `cd server; cargo test --release`
- Lint: `cargo clippy --all-targets -- -D warnings`
- Release build: `cargo build --release` → `server/target/release/esc.exe`
- Dev data folder: `D:\esc_data` (D: has the free space; C: is tight)

## Current state (newest first)

### 2026-11-06

- **M0 done:**
  - The user decided: Rust std-only, full-mirror ring, Cloudflare quick tunnel (no domain yet),
    server-trusted storage, 4-char IDs, master-only.
  - Spec written. Crypto module delegated to the Sonnet agent.
- **Next:** M1 relay core.
