# ESC — Extremely Simple Communication

A tiny, very fast web app for talking to people: text forever, files within a storage quota.
It runs on a ring of relays. Each relay is one native Rust binary with **zero dependencies**, and
every relay mirrors everything.

- **Design (read this first):** [docs/superpowers/specs/2026-11-06-esc-design.md](docs/superpowers/specs/2026-11-06-esc-design.md)
- **Status:** M0 (design) done; M1 (relay core) in progress. See the "Current state" section in [CLAUDE.md](CLAUDE.md).

## Layout

| Path | What |
| --- | --- |
| `server/` | The relay: Rust 1.98, edition 2024, std only (no crates) |
| `web/` | The web app: plain HTML/CSS/JS, no libraries, no build step; embedded into the binary |
| `docs/` | Design spec and notes |

## Build and run (dev)

```powershell
cd server
cargo test --release            # all unit tests
cargo build --release           # -> server\target\release\esc.exe
.\target\release\esc.exe init   --data D:\esc_data   # first time: creates the ring secret, prints the admin sign-up link
.\target\release\esc.exe serve  --data D:\esc_data   # serves http://127.0.0.1:7878
```

(The commands above are the target interface; they land during M1.)
