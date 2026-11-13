# Where is ESC right now? Prints the public URL, process state, and the first-admin Ticket (until an admin exists).
param([string]$Home_ = 'D:\esc')
$url = $null
if (Test-Path "$Home_\tunnel.log") {
  $m = Select-String -Path "$Home_\tunnel.log" -Pattern 'https://[a-z0-9-]+\.trycloudflare\.com' -AllMatches | Select-Object -Last 1
  if ($m) { $url = $m.Matches[-1].Value }
}
# A permanent URL (named tunnel, service or Tailscale) lives in relay.ini; quick tunnels only show in tunnel.log.
$ini = @('D:\esc_data', 'C:\esc_data') | ForEach-Object { Join-Path $_ 'relay.ini' } | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($ini) { foreach ($l in Get-Content $ini) { if ($l -match '^\s*url\s*=\s*(https?://\S+)') { $url = $Matches[1] } } }
$relay = Get-Process esc -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$Home_*" }
$svc = Get-CimInstance Win32_Service -Filter "Name='Cloudflared'" -ErrorAction SilentlyContinue
$tunnel = Get-Process cloudflared -ErrorAction SilentlyContinue
Write-Host ("Relay   : " + $(if ($relay) { "running (pid $($relay.Id -join ','))" } else { 'NOT running' }))
Write-Host ("Tunnel  : " + $(if ($tunnel) { "running (pid $($tunnel.Id -join ','))" } else { 'NOT running' }))
if ($svc) { Write-Host ("Service : Cloudflared Windows service is " + $svc.State) }
Write-Host ("Public  : " + $(if ($url) { $url } else { 'no URL yet (see tunnel.log)' }))
if (Test-Path "$Home_\esc.log") {
  $lines = @(Get-Content "$Home_\esc.log" -ErrorAction SilentlyContinue)
  $lastBoot = 0
  for ($i = $lines.Count - 1; $i -ge 0; $i--) {
    if ($lines[$i] -match '^ESC .* Star ') { $lastBoot = $i; break }
  }
  $ticket = $null
  foreach ($l in $lines[$lastBoot..($lines.Count - 1)]) {
    if ($l -match '#ticket=([A-Z0-9]+)') { $ticket = $Matches[1] }
  }
  if ($ticket) {
    Write-Host ""
    Write-Host "No admin yet. Create YOUR admin account (one use):"
    Write-Host ("  " + $(if ($url) { "$url/#ticket=$ticket" } else { "http://127.0.0.1:7878/#ticket=$ticket" }))
  }
}
