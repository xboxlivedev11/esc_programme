# Remote access to the ESC box (esc-manager over SSH, via Tailscale)

Goal: run `esc-manager.cmd` from another machine, encrypted end to end, with no new software (OpenSSH Server is a
Windows feature; Tailscale is already used by the box).

## How it is locked down
- **Tailscale** (WireGuard) carries the traffic: nothing is exposed to the LAN or the internet.
- **Firewall**: one inbound rule, `ESC SSH (Tailscale only)`, port 22 from `100.64.0.0/10` on the Tailscale interface only.
  The OpenSSH installer's own rule (port 22 from any network) is deleted.
- **sshd** listens only on the box's Tailscale IP, public-key login only, no passwords, one allowed user, no forwarding.
- **Keys**: the box holds only the *public* key. Protect the private key on your machine with a passphrase: that is the password.
- It survives the Cloudflare tunnel being down, so it is the way in when the tunnel is broken.

## Set up (once)
1. On the other machine: `ssh-keygen -t ed25519` (set a passphrase), and copy the `.pub` line.
2. On the ESC box, log Tailscale in if needed: `& 'C:\Program Files\Tailscale\tailscale.exe' up` and open the link.
   Install Tailscale on the other machine and log into the same tailnet.
3. On the ESC box, elevated PowerShell:
   `powershell -ExecutionPolicy Bypass -File deploy\remote-ssh.ps1 -PublicKey "ssh-ed25519 AAAA... you@laptop"`
4. From the other machine: `ssh usuario@<tailscale ip or name>`, then `esc-manager.cmd` (or `C:\...\esc-manager.cmd`).

Every administrator session is elevated, so the manager can stop and restart the SYSTEM-owned relay and tunnel
(without admin it can only read status).

## Opening the manager faster (phone)
After logging in, type **`m`** to open `esc-manager.cmd`. `remote-ssh.ps1` writes a tiny `m.cmd` in the user's home folder (an
SSH login starts there and cmd searches the current folder first, so no PATH change). Nothing opens by itself; `-NoLauncher` skips it.
Also: an admin key must be reachable through `AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys`
(the script writes it; without it every login fails).

## If `sshd` will not start (learned the hard way, 2026-11-17)
- The service runs as SYSTEM and refuses host keys that are **owned by a user** or readable by one ("UNPROTECTED PRIVATE KEY
  FILE" in a SYSTEM debug run, no log, just "terminated unexpectedly"). `ssh-keygen -A` run from your account creates them
  owned by you: the script now sets the owner to SYSTEM and strips every other entry.
- To see the real error, run `sshd.exe -d -e` as SYSTEM (a one-off scheduled task with `/ru SYSTEM`), not from your own shell,
  which hides the problem.
- The firewall on this box is **off** on all profiles, so the real protection is `ListenAddress <tailscale ip>`: if the node
  gets a new Tailscale IP, re-run the script. Check with `Get-NetTCPConnection -LocalPort 22 -State Listen` (only `100.x`).

## Undo
`powershell -ExecutionPolicy Bypass -File deploy\remote-ssh.ps1 -Remove` stops sshd, disables it and deletes the firewall rule.
