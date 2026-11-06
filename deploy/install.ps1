# ESC on this box: copy the relay to D:\esc, run it (and a Cloudflare quick tunnel) at every boot.
# Run from an elevated PowerShell:  powershell -ExecutionPolicy Bypass -File deploy\install.ps1
# Undo:                             powershell -ExecutionPolicy Bypass -File deploy\uninstall.ps1
param(
  [string]$Home_ = 'D:\esc',
  [string]$Data = 'D:\esc_data',
  [string]$Listen = '127.0.0.1:7878'
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'server\target\release\esc.exe'
if (-not (Test-Path $exe)) { throw "Build first: cd server; cargo build --release" }
$cloudflared = (Get-Command cloudflared -ErrorAction SilentlyContinue).Source
if (-not $cloudflared) { $cloudflared = 'C:\Program Files (x86)\cloudflared\cloudflared.exe' }
if (-not (Test-Path $cloudflared)) { throw "cloudflared missing: winget install --id Cloudflare.cloudflared -e" }

New-Item -ItemType Directory -Force $Home_, $Data | Out-Null
# Stop running copies so the binary can be replaced.
foreach ($t in 'ESC Relay', 'ESC Tunnel') { schtasks /end /tn $t 2>$null | Out-Null }
Get-Process esc, cloudflared -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$Home_*" -or $_.Path -eq $cloudflared } | Stop-Process -Force
Start-Sleep -Milliseconds 500
Copy-Item $exe (Join-Path $Home_ 'esc.exe') -Force

# Wrappers: restart on exit, append everything to a log (Task Scheduler has no console).
@"
while (`$true) {
  # cmd's redirection keeps the log as plain UTF-8 bytes (Windows PowerShell 5.1 would re-encode it).
  cmd /c "`"$Home_\esc.exe`" serve --data `"$Data`" --listen $Listen --tunnel-log `"$Home_\tunnel.log`" >> `"$Home_\esc.log`" 2>&1"
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $Home_ 'run-relay.ps1') -Encoding utf8
@"
while (`$true) {
  Remove-Item '$Home_\tunnel.log' -ErrorAction SilentlyContinue
  & '$cloudflared' tunnel --no-autoupdate --url 'http://$Listen' --logfile '$Home_\tunnel.log' *> `$null
  Start-Sleep -Seconds 5
}
"@ | Set-Content (Join-Path $Home_ 'run-tunnel.ps1') -Encoding utf8
Copy-Item (Join-Path $PSScriptRoot 'status.ps1') (Join-Path $Home_ 'status.ps1') -Force

foreach ($pair in @(@('ESC Relay', 'run-relay.ps1'), @('ESC Tunnel', 'run-tunnel.ps1'))) {
  $cmd = "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$Home_\$($pair[1])`""
  schtasks /create /tn $pair[0] /tr $cmd /sc onstart /ru SYSTEM /rl HIGHEST /f | Out-Null
  schtasks /run /tn $pair[0] | Out-Null
}
Write-Host "Installed. Waiting for the tunnel..."
Start-Sleep -Seconds 8
& (Join-Path $Home_ 'status.ps1') -Home_ $Home_
