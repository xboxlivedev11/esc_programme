# Where is ESC right now? Prints the public URL, process state, and the first-admin Ticket (until an admin exists).
param([string]$Home_ = 'D:\esc')
$url = $null
if (Test-Path "$Home_\tunnel.log") {
  $m = Select-String -Path "$Home_\tunnel.log" -Pattern 'https://[a-z0-9-]+\.trycloudflare\.com' -AllMatches | Select-Object -Last 1
  if ($m) { $url = $m.Matches[-1].Value }
}
$relay = Get-Process esc -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$Home_*" }
$tunnel = Get-Process cloudflared -ErrorAction SilentlyContinue
Write-Host ("Relay   : " + $(if ($relay) { "running (pid $($relay.Id -join ','))" } else { 'NOT running' }))
Write-Host ("Tunnel  : " + $(if ($tunnel) { "running (pid $($tunnel.Id -join ','))" } else { 'NOT running' }))
Write-Host ("Public  : " + $(if ($url) { $url } else { 'no URL yet (see tunnel.log)' }))
if (Test-Path "$Home_\esc.log") {
  $t = Select-String -Path "$Home_\esc.log" -Pattern '#ticket=([A-Z0-9]+)' | Select-Object -Last 1
  $admin = Select-String -Path "$Home_\esc.log" -Pattern 'No admin yet' | Select-Object -Last 1
  if ($t -and $admin -and $admin.LineNumber -ge ($t.LineNumber - 2)) {
    $code = $t.Matches[0].Groups[1].Value
    Write-Host ""
    Write-Host "No admin yet. Create YOUR admin account (one use):"
    Write-Host ("  " + $(if ($url) { "$url/#ticket=$code" } else { "http://127.0.0.1:7878/#ticket=$code" }))
  }
}
