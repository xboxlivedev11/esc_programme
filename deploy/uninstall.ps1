# Stop ESC and remove the boot tasks. Keeps D:\esc_data (your accounts, messages and files).
$ErrorActionPreference = 'Continue'
foreach ($t in 'ESC Relay', 'ESC Tunnel') {
  schtasks /end /tn $t 2>$null | Out-Null
  schtasks /delete /tn $t /f 2>$null | Out-Null
}
# The Windows service 'Cloudflared' (cloudflared's .msi) is not ESC's: leave its process alone.
$svc = Get-CimInstance Win32_Service -Filter "Name='Cloudflared'" -ErrorAction SilentlyContinue
$svcPid = if ($svc) { [int]$svc.ProcessId } else { 0 }
Get-Process esc, cloudflared -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $svcPid } | Stop-Process -Force
Write-Host "ESC stopped and removed from boot. Data kept in D:\esc_data."
