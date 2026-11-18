# Remote admin for the ESC box: OpenSSH Server reachable ONLY over Tailscale, public-key login only.
# Run from an elevated PowerShell:
#   powershell -ExecutionPolicy Bypass -File deploy\remote-ssh.ps1 -PublicKey "ssh-ed25519 AAAA... me@laptop"
# Needs Tailscale logged in (it must have a 100.x address). Safe to re-run: it adds keys, never opens other networks.
# Undo: powershell -ExecutionPolicy Bypass -File deploy\remote-ssh.ps1 -Remove
param(
  [string]$PublicKey,
  [string]$User = $env:USERNAME,
  [string]$Home_ = $(if (Test-Path 'D:\') { 'D:\esc' } else { 'C:\esc' }),
  [switch]$NoLauncher,   # do not open esc-manager automatically when you log in
  [switch]$Remove
)
$ErrorActionPreference = 'Stop'
$sshDir = "$env:SystemRoot\System32\OpenSSH"
$cfg = 'C:\ProgramData\ssh\sshd_config'
$rule = 'ESC SSH (Tailscale only)'

if ($Remove) {
  Stop-Service sshd -ErrorAction SilentlyContinue
  Set-Service sshd -StartupType Disabled
  Get-NetFirewallRule -DisplayName $rule -ErrorAction SilentlyContinue | Remove-NetFirewallRule
  Write-Host "SSH access removed (the OpenSSH Server feature is still installed, stopped and disabled)."
  return
}

if ($PublicKey -notmatch '^(ssh-ed25519|ecdsa-sha2-nistp\d+|ssh-rsa|sk-ssh-ed25519@openssh\.com) [A-Za-z0-9+/=]+') {
  throw "Pass -PublicKey with the contents of your .pub file (ssh-keygen -t ed25519 on the other machine)."
}
$ts = 'C:\Program Files\Tailscale\tailscale.exe'
$ip = (& $ts ip -4 2>$null | Select-Object -First 1)
if ($ip -notmatch '^100\.') { throw "Tailscale has no IP yet. Log in first:  & '$ts' up   (open the printed link), then re-run." }

if ((Get-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0').State -ne 'Installed') {
  Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' | Out-Null
}
# The installer opens port 22 to every network: replace it with a Tailscale-only rule.
Get-NetFirewallRule -Name '*OpenSSH-Server*' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
if (-not (Get-NetFirewallRule -DisplayName $rule -ErrorAction SilentlyContinue)) {
  New-NetFirewallRule -DisplayName $rule -Direction Inbound -Action Allow -Protocol TCP -LocalPort 22 `
    -RemoteAddress '100.64.0.0/10' -InterfaceAlias 'Tailscale' -Profile Any | Out-Null
}
if (-not (Test-Path "C:\ProgramData\ssh\ssh_host_ed25519_key")) { & "$sshDir\ssh-keygen.exe" -A | Out-Null }
# sshd refuses to start if a host key is readable by a normal user (ssh-keygen gives it to the creator): strip everyone but SYSTEM + Administrators.
Get-ChildItem C:\ProgramData\ssh\ssh_host_*_key | ForEach-Object {
  icacls.exe $_.FullName /setowner '*S-1-5-18' | Out-Null   # the service runs as SYSTEM and rejects keys owned by a user
  icacls.exe $_.FullName /inheritance:r | Out-Null
  icacls.exe $_.FullName /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' | Out-Null
  foreach ($line in (icacls.exe $_.FullName)) {
    if ($line -match '^\S*\s*(\S+\\' + [regex]::Escape($env:USERNAME) + '):') { icacls.exe $_.FullName /remove $Matches[1] | Out-Null }
  }
}

$lines = @(
  '# ESC remote admin: reachable only over Tailscale, public keys only.',
  "ListenAddress $ip", 'AddressFamily inet',
  'PubkeyAuthentication yes', 'PasswordAuthentication no', 'KbdInteractiveAuthentication no',
  'PermitEmptyPasswords no', 'AuthenticationMethods publickey', "AllowUsers $User",
  'MaxAuthTries 3', 'LoginGraceTime 30', 'MaxSessions 4', 'ClientAliveInterval 30', 'ClientAliveCountMax 4',
  'AllowTcpForwarding no', 'AllowAgentForwarding no', 'X11Forwarding no')
# Administrators' keys live in one file that only SYSTEM and Administrators may touch. The stock config reaches it through
# "Match Group administrators"; this config has none, so point sshd at it explicitly or an admin's key is never found.
$isAdmin = (Get-LocalGroupMember -SID S-1-5-32-544 | Where-Object { $_.Name -like "*\$User" }) -ne $null
if ($isAdmin) { $lines += 'AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys' }
# Shortcut: a login starts in the user's home folder and cmd searches the current folder first, so a tiny m.cmd there means
# typing  m  opens esc-manager. (No PATH change, no auto-launch.) -NoLauncher skips it.
if (-not $NoLauncher) {
  $manager = Join-Path (Split-Path -Parent $PSScriptRoot) 'esc-manager.cmd'
  $profileDir = (Get-CimInstance Win32_UserProfile | Where-Object { $_.LocalPath -like "*\$User" } | Select-Object -First 1).LocalPath
  if (-not $profileDir) { $profileDir = $env:USERPROFILE }
  Set-Content (Join-Path $profileDir 'm.cmd') "@echo off`r`ncall `"$manager`"`r`n" -Encoding ascii
}
Set-Content $cfg $lines -Encoding ascii

$ak = if ($isAdmin) { 'C:\ProgramData\ssh\administrators_authorized_keys' } else { "$env:USERPROFILE\.ssh\authorized_keys" }
New-Item -ItemType Directory -Force (Split-Path $ak) | Out-Null
if (-not (Test-Path $ak) -or -not (Select-String -Path $ak -SimpleMatch $PublicKey.Trim() -Quiet)) { Add-Content $ak $PublicKey.Trim() -Encoding ascii }
if ($isAdmin) { icacls.exe $ak /setowner '*S-1-5-18' | Out-Null; icacls.exe $ak /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' | Out-Null }

& "$sshDir\sshd.exe" -t; if ($LASTEXITCODE -ne 0) { throw "sshd_config did not validate." }
# Delayed start + restart on failure: sshd binds to the Tailscale IP, so it must be retried if it starts before Tailscale is up.
sc.exe config sshd start= delayed-auto | Out-Null
sc.exe failure sshd reset= 86400 actions= restart/30000/restart/60000/restart/60000 | Out-Null
Restart-Service sshd
Write-Host "Done. From another machine on your tailnet:  ssh $User@$ip   then run esc-manager.cmd"
