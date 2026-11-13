# Stop ESC and remove the boot tasks. Keeps D:\esc_data (your accounts, messages and files).
$ErrorActionPreference = 'Continue'
foreach ($t in 'ESC Relay', 'ESC Tunnel') {
  schtasks /end /tn $t 2>$null | Out-Null
  schtasks /delete /tn $t /f 2>$null | Out-Null
}
# The Windows service 'Cloudflared' (cloudflared's .msi) is not ESC's: leave its process alone.
$svc = Get-CimInstance Win32_Service -Filter "Name='Cloudflared'" -ErrorAction SilentlyContinue
$svcPid = if ($svc) { [int]$svc.ProcessId } else { 0 }
# ...nor anyone else's tunnel: only a cloudflared that points at ESC's folder or port 7878.
Get-Process esc -ErrorAction SilentlyContinue | Stop-Process -Force
Get-CimInstance Win32_Process -Filter "Name='cloudflared.exe'" -ErrorAction SilentlyContinue |
  Where-Object { $_.ProcessId -ne $svcPid -and $_.CommandLine -and ($_.CommandLine -like "*D:\esc*" -or $_.CommandLine -like "*C:\esc*" -or $_.CommandLine -like "*:7878*") } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Write-Host "ESC stopped and removed from boot. Data kept in D:\esc_data."
