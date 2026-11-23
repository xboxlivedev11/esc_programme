# Sync this repo with the live ESC box ("haswell", Tailscale 100.83.19.74) over git + SSH.
# Run from anywhere inside the repo. Needs the `haswell` git remote (see docs/remote-access.md, "Syncing the code")
# and ssh_priv_key\id_ed25519_haswell_ssh_key. Never rebuilds or restarts the relay: deploying is a separate step.
#
#   1. fetch haswell/master and fast-forward ours (refuses if histories diverged: merge by hand);
#   2. push our master to haswell's side branch `incoming` (haswell has master checked out, so git refuses a direct push);
#   3. fast-forward haswell's master to `incoming` over SSH (refuses if its working tree has edits).
$ErrorActionPreference = 'Stop'
$repo = (git rev-parse --show-toplevel).Trim()
Set-Location $repo
$key  = Join-Path $repo 'ssh_priv_key\id_ed25519_haswell_ssh_key'
$sshO = @('-i', $key, '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes')
$dir  = 'C:\Users\usuario\Desktop\esc_programme'

git fetch haswell master
if ($LASTEXITCODE) { throw 'fetch failed (is haswell online in Tailscale?)' }
git merge --ff-only haswell/master
if ($LASTEXITCODE) { throw 'history diverged: merge haswell/master by hand, then run again' }

git push haswell master:refs/heads/incoming --force-with-lease
if ($LASTEXITCODE) { throw 'push failed' }
ssh @sshO usuario@100.83.19.74 "cd /d $dir & git merge --ff-only incoming & git log --oneline -1"
if ($LASTEXITCODE) { throw 'fast-forward on haswell failed (uncommitted edits there?)' }
Write-Host 'In sync. To deploy: stop the ESC tasks on haswell, cargo build --release there, run deploy\install.ps1.'
