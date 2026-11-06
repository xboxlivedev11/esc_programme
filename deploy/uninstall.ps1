# Stop ESC and remove the boot tasks. Keeps D:\esc_data (your accounts, messages and files).
$ErrorActionPreference = 'Continue'
foreach ($t in 'ESC Relay', 'ESC Tunnel') {
  schtasks /end /tn $t 2>$null | Out-Null
  schtasks /delete /tn $t /f 2>$null | Out-Null
}
Get-Process esc, cloudflared -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host "ESC stopped and removed from boot. Data kept in D:\esc_data."
