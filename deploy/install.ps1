# ESC on this box: copy the relay to D:\esc and run it at every boot, plus the tunnel the manager chose
# (esc-paths.ini: tunnel_mode = quick | named | tailscale | service; default quick). It only ever writes the
# quick-tunnel wrapper; the other modes keep the wrapper esc-manager.cmd wrote, and 'service' runs no tunnel task.
# Run from an elevated PowerShell:  powershell -ExecutionPolicy Bypass -File deploy\install.ps1
# Undo:                             powershell -ExecutionPolicy Bypass -File deploy\uninstall.ps1
param(
  [string]$Home_ = $(if (Test-Path 'D:\') { 'D:\esc' } else { 'C:\esc' }),
  [string]$Data = $(if (Test-Path 'D:\') { 'D:\esc_data' } else { 'C:\esc_data' }),
  [string]$Listen = '127.0.0.1:7878'
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'server\target\release\esc.exe'
if (-not (Test-Path $exe)) { throw "Build first: cd server; cargo build --release" }
# Which tunnel did the manager choose?
$mode = 'quick'
$ini = Join-Path $repo 'esc-paths.ini'
if (Test-Path $ini) {
  foreach ($l in Get-Content $ini) { if ($l -match '^\s*tunnel_mode\s*=\s*(\w+)') { $mode = $Matches[1].ToLower() } }
}
$cloudflared = (Get-Command cloudflared -ErrorAction SilentlyContinue).Source
if (-not $cloudflared) { $cloudflared = 'C:\Program Files (x86)\cloudflared\cloudflared.exe' }
if (($mode -in 'quick', 'named') -and -not (Test-Path $cloudflared)) {
  # Not fatal: the relay still installs and runs locally; only the public tunnel is missing.
  Write-Warning "cloudflared not found. The relay will start, but there is NO public tunnel until you run: winget install --id Cloudflare.cloudflared -e  (then press [1] again)"
  $noCloudflared = $true
}

New-Item -ItemType Directory -Force $Home_, $Data | Out-Null
# Windows PowerShell 5.1 turns any stderr of a native command into a fatal error under 'Stop':
# from here on native calls are checked by exit code instead.
$ErrorActionPreference = 'Continue'
# Stop running relay so esc.exe can be replaced, but keep ESC's own cloudflared alive if it is already running
# so the public *.trycloudflare.com quick-tunnel URL does not change on a relay update!
# The Windows service 'Cloudflared' (cloudflared's .msi) is not ESC's: it never counts and is never touched.
$svc = Get-CimInstance Win32_Service -Filter "Name='Cloudflared'" -ErrorAction SilentlyContinue
$svcPid = if ($svc) { [int]$svc.ProcessId } else { 0 }
$listenPort = ($Listen -split ':')[-1]
$tunnelRunning = Get-CimInstance Win32_Process -Filter "Name='cloudflared.exe'" -ErrorAction SilentlyContinue |
  Where-Object { $_.ProcessId -ne $svcPid -and $_.CommandLine -and ($_.CommandLine -like "*$Home_*" -or $_.CommandLine -like "*:$listenPort*") }
cmd /c "schtasks /end /tn `"ESC Relay`" >nul 2>&1"
Get-Process esc -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$Home_*" } | Stop-Process -Force
if (-not $tunnelRunning) {
  cmd /c "schtasks /end /tn `"ESC Tunnel`" >nul 2>&1"
}
Start-Sleep -Milliseconds 500
Copy-Item $exe (Join-Path $Home_ 'esc.exe') -Force

# Wrappers: restart on exit, append everything to a log (Task Scheduler has no console).
@"
Set-Content '$Home_\run-relay.pid' `$PID
while (`$true) {
  # cmd's redirection keeps the log as plain UTF-8 bytes (Windows PowerShell 5.1 would re-encode it).
  cmd /c "`"$Home_\esc.exe`" serve --data `"$Data`" --listen $Listen --tunnel-log `"$Home_\tunnel.log`" >> `"$Home_\esc.log`" 2>&1"
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $Home_ 'run-relay.ps1') -Encoding utf8
if ($mode -eq 'quick' -or -not (Test-Path (Join-Path $Home_ 'run-tunnel.ps1'))) {
if ($mode -ne 'quick') { Write-Warning "tunnel_mode is '$mode' but there is no run-tunnel.ps1 yet: run esc-manager.cmd, [S], to write it." }
@"
while (`$true) {
  Remove-Item '$Home_\tunnel.log' -Force -ErrorAction SilentlyContinue
  `$p = Start-Process -FilePath '$cloudflared' -ArgumentList 'tunnel','--no-autoupdate','--url','http://$Listen','--logfile','$Home_\tunnel.log' -NoNewWindow -PassThru
  while (-not `$p.HasExited) {
    Start-Sleep -Seconds 5
    if (Test-Path '$Home_\tunnel.log') {
      `$tail = Get-Content '$Home_\tunnel.log' -Tail 25 -ErrorAction SilentlyContinue
      if (`$tail -match 'Unauthorized:\s*Tunnel not found') {
        Stop-Process -Id `$p.Id -Force -ErrorAction SilentlyContinue
        break
      }
    }
  }
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $Home_ 'run-tunnel.ps1') -Encoding utf8
}
Copy-Item (Join-Path $PSScriptRoot 'status.ps1') (Join-Path $Home_ 'status.ps1') -Force

foreach ($pair in @(@('ESC Relay', 'run-relay.ps1'), @('ESC Tunnel', 'run-tunnel.ps1'))) {
  $cmd = "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$Home_\$($pair[1])`""
  schtasks /create /tn $pair[0] /tr $cmd /sc onstart /ru SYSTEM /rl HIGHEST /f | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Could not create the '$($pair[0])' task (run this from an elevated PowerShell)." }
  # schtasks defaults kill the task after 72 h, never restart it and stop it on battery: that silently
  # took the relay and the tunnel down every 3 days. Run forever, restart on failure, ignore power state.
  $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -StartWhenAvailable -MultipleInstances IgnoreNew
  Set-ScheduledTask -TaskName $pair[0] -Settings $settings | Out-Null
  if ($pair[0] -eq 'ESC Tunnel') { schtasks /change /tn $pair[0] $(if ($mode -eq 'service') { '/disable' } else { '/enable' }) | Out-Null }
  if ($pair[0] -eq 'ESC Relay' -or ($mode -ne 'service' -and -not $tunnelRunning -and -not $noCloudflared)) {
    schtasks /run /tn $pair[0] | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not start the '$($pair[0])' task." }
  }
}
Write-Host "Installed. Waiting for the relay..."
if ($mode -ne 'service' -and -not $tunnelRunning) { Start-Sleep -Seconds 8 } else { Start-Sleep -Seconds 2 }
& (Join-Path $Home_ 'status.ps1') -Home_ $Home_
