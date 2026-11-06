# ESC — Extremely Simple Communication

A tiny, very fast web app for talking to people: text forever, files within a storage Tank.
It runs on a Ring of relays called Stars. Each Star is one native Rust binary (1.3 MB, the web
app included) with **zero dependencies**, and every Star mirrors everything.

- **Design, decisions and the names glossary:** [docs/superpowers/specs/2026-11-06-esc-design.md](docs/superpowers/specs/2026-11-06-esc-design.md)
- **Rules for the next session and current state:** [CLAUDE.md](CLAUDE.md)

## How it works, in one minute

- **Joining**: you join with a **Ticket** and get a 4-character **Callsign** (like `K7Q2`), drawn as
  signal flags. Your callsign is the only way anyone can reach you.
- **Knocks**: people **Knock** on your callsign; nothing happens unless you **open the door**.
  You then share a **Line**. Groups are **Crews** of up to 128, and you join only by accepting.
- **The Tank**: files count against your Tank (1 GB by default, set in the **Control Tower**);
  text never counts.
  - With auto-recycling on, **Dustin** recycles your oldest files (never **Keepers**), so new files
    always arrive.
  - With it off, a **Red Alert** warns you at 95 %, and files that don't fit **Bounce**: you can
    **Catch** them later.
- **Your files**: the **Stash** shows all your files. The **Cleaning Room** sorts and filters them
  so you can free space. **Peek** views pictures; videos open in the browser's own player.

## Layout

| Path | What |
| --- | --- |
| `server/` | The relay: Rust 1.98, edition 2024, std only (no crates). `cargo test --release` runs 120+ unit tests and end-to-end tests that spawn real Stars |
| `web/` | The web app: plain HTML/CSS/JS, no libraries, no build step; embedded into the binary |
| `deploy/` | Run this box as Star Polaris behind a Cloudflare quick tunnel, at every boot |
| `docs/` | Design spec |

## Run it

```powershell
cd server
cargo build --release
.\target\release\esc.exe serve --data D:\esc_data      # prints a one-time admin Ticket link
```

**Put this box online** (elevated PowerShell; starts at every boot, behind a free Cloudflare
quick tunnel):

```powershell
powershell -ExecutionPolicy Bypass -File deploy\install.ps1     # install + start + print URL and Ticket
powershell -ExecutionPolicy Bypass -File D:\esc\status.ps1      # current public URL (changes on restart)
powershell -ExecutionPolicy Bypass -File deploy\uninstall.ps1   # stop + remove from boot (keeps data)
```

**Add a Star to the Ring** (on another computer that has a reachable URL):

1. Control Tower → The Ring → **Light a new Star** gives you a one-time **Spark**.
2. On the new computer, copy `esc.exe`, then run:

   ```powershell
   esc join https://<an existing Star> <spark> --data D:\esc_data --url https://<this computer's address>
   esc serve --data D:\esc_data
   ```

   It copies the whole Ring and serves the same accounts, messages and files.
