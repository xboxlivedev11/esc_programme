@echo off & title ESC — Star Manager & powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$scriptPath='%~f0'; $repoRoot='%~dp0'.TrimEnd('\'); $cliArgs=@('%*'); Invoke-Expression ((Get-Content -LiteralPath $scriptPath -Encoding UTF8 | Select-Object -Skip 1) -join \"`n\")" & exit /b %ERRORLEVEL%
# ESC — Extremely Simple Communication — Interactive TUI Manager
# Single-file polyglot (.cmd + PowerShell). Double-click in Explorer or run .\esc-manager.cmd in terminal.

[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$Host.UI.RawUI.WindowTitle = "ESC · Star TUI Manager (Polaris)"

$PathsConfigFile = Join-Path $repoRoot "esc-paths.ini"

# Smart default paths:
# 1. If esc.exe + esc_data exist right next to esc-manager.cmd (portable folder), use them.
# 2. Otherwise if D:\ exists, default to D:\esc and D:\esc_data.
# 3. Otherwise (another PC without D:\), default to C:\esc and C:\esc_data.
if ((Test-Path (Join-Path $repoRoot "esc.exe")) -and (Test-Path (Join-Path $repoRoot "esc_data"))) {
    $HomeDir   = $repoRoot
    $DataDir   = Join-Path $repoRoot "esc_data"
    $BackupDir = Join-Path $repoRoot "esc_backups"
} elseif (Test-Path "D:\") {
    $HomeDir   = "D:\esc"
    $DataDir   = "D:\esc_data"
    $BackupDir = "D:\esc_backups"
} else {
    $HomeDir   = "C:\esc"
    $DataDir   = "C:\esc_data"
    $BackupDir = "C:\esc_backups"
}

$TunnelMode  = "quick"
$TunnelToken = ""

# Load user-configured custom paths & tunnel settings from esc-paths.ini if present
if (Test-Path $PathsConfigFile) {
    foreach ($line in (Get-Content $PathsConfigFile -ErrorAction SilentlyContinue)) {
        if ($line -match '^\s*home\s*=\s*(.+)$')         { $HomeDir     = $Matches[1].Trim() }
        if ($line -match '^\s*data\s*=\s*(.+)$')         { $DataDir     = $Matches[1].Trim() }
        if ($line -match '^\s*backup\s*=\s*(.+)$')       { $BackupDir   = $Matches[1].Trim() }
        if ($line -match '^\s*tunnel_mode\s*=\s*(.+)$')  { $TunnelMode  = $Matches[1].Trim().ToLower() }
        if ($line -match '^\s*tunnel_token\s*=\s*(.+)$') { $TunnelToken = $Matches[1].Trim() }
    }
}

$ProdListen = "127.0.0.1:7878"
$DevListen  = "127.0.0.1:7979"
$DevDir     = Join-Path $repoRoot "data\dev_star"
$ServerDir  = Join-Path $repoRoot "server"
$WebDir     = Join-Path $repoRoot "web"
$DeployDir  = Join-Path $repoRoot "deploy"

function Get-TailscaleExe {
    $cmd = (Get-Command tailscale -ErrorAction SilentlyContinue).Source
    if ($cmd -and (Test-Path $cmd)) { return $cmd }
    foreach ($p in @('C:\Program Files\Tailscale\tailscale.exe', 'C:\Program Files (x86)\Tailscale\tailscale.exe')) {
        if (Test-Path $p) { return $p }
    }
    return $null
}

function Get-CloudflaredExe {
    $cmd = (Get-Command cloudflared -ErrorAction SilentlyContinue).Source
    if ($cmd -and (Test-Path $cmd)) { return $cmd }
    foreach ($p in @('C:\Program Files (x86)\cloudflared\cloudflared.exe', 'C:\Program Files\cloudflared\cloudflared.exe')) {
        if (Test-Path $p) { return $p }
    }
    return $null
}

# The Windows service installed by cloudflared's .msi (named "Cloudflared"), or $null.
function Get-CfServiceInfo {
    $svc = Get-CimInstance Win32_Service -Filter "Name='Cloudflared'" -ErrorAction SilentlyContinue
    if (-not $svc) { return $null }
    [pscustomobject]@{ State = $svc.State; Pid = [int]$svc.ProcessId; StartMode = $svc.StartMode }
}

# cloudflared processes that ESC itself launched: their command line points at ESC's folder or relay port.
# Never the Windows service's own process, and never someone else's tunnel (another project on this PC).
function Get-EscCloudflared {
    $svcPid = 0
    $svc = Get-CfServiceInfo
    if ($svc) { $svcPid = $svc.Pid }
    $port = ($ProdListen -split ':')[-1]
    # The SYSTEM boot task owns the process, so a non-elevated manager cannot read its command line:
    # the wrapper (run-tunnel.ps1) records cloudflared's PID in tunnel.pid, which is trusted too.
    $pidFile = 0
    try { $pidFile = [int](Get-Content (Join-Path $HomeDir 'tunnel.pid') -ErrorAction Stop | Select-Object -First 1) } catch {}
    @(Get-CimInstance Win32_Process -Filter "Name='cloudflared.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessId -ne $svcPid -and (($pidFile -and $_.ProcessId -eq $pidFile) -or ($_.CommandLine -and ($_.CommandLine -like "*$HomeDir*" -or $_.CommandLine -like "*:$port*"))) } |
        ForEach-Object { Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue } | Where-Object { $_ })
}

# Stop the restart-loop wrapper(s) of the tunnel (run-tunnel.ps1), so an old copy can never relaunch a stale tunnel.
function Stop-TunnelWrappers {
    Get-Process powershell -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $PID } | ForEach-Object {
        try {
            $cl = (Get-CimInstance Win32_Process -Filter "ProcessId = $($_.Id)" -ErrorAction SilentlyContinue).CommandLine
            if ($cl -and $cl -like "*run-tunnel.ps1*") { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
        } catch {}
    }
}

# Is a restart-loop wrapper (run-relay / run-tunnel) already alive? Each wrapper writes its own PID to <name>.pid.
# Needed because a non-elevated manager cannot read the command line of the SYSTEM-owned wrapper, and starting
# a second copy made the loops fight over port 7878 and tunnel.log.
function Test-WrapperAlive([string]$name) {
    try {
        $id = [int](Get-Content (Join-Path $HomeDir "$name.pid") -ErrorAction Stop | Select-Object -First 1)
        return [bool](Get-Process -Id $id -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -eq 'powershell' })
    } catch { return $false }
}

# Enable or disable the "ESC Tunnel" boot task (disabled in 'service' mode: the Windows service runs the tunnel).
function Set-TunnelTask([bool]$on) {
    $verb = if ($on) { '/enable' } else { '/disable' }
    cmd /c "schtasks /change /tn `"ESC Tunnel`" $verb >nul 2>&1"
}

# Does the public host resolve in DNS? Cached 30 s; a slow lookup counts as "unknown" (true).
$script:PubCheck = @{ At = [datetime]::MinValue; Host = ''; Dns = $true }
function Test-PublicDns([string]$url) {
    if (-not $url) { return $true }
    try { $h = ([uri]$url).DnsSafeHost } catch { return $true }
    if ($script:PubCheck.Host -eq $h -and ((Get-Date) - $script:PubCheck.At).TotalSeconds -lt 30) { return $script:PubCheck.Dns }
    $ok = $false
    try {
        $t = [Net.Dns]::GetHostAddressesAsync($h)
        if ($t.Wait(2000)) { $ok = ($t.Result.Count -gt 0) } else { $ok = $true }
    } catch { $ok = $false }
    $script:PubCheck = @{ At = Get-Date; Host = $h; Dns = $ok }
    return $ok
}

function Write-TunnelRunner {
    New-Item -ItemType Directory -Force $HomeDir | Out-Null
    $port = ($ProdListen -split ':')[-1]
    $cfCmd = Get-CloudflaredExe
    if (-not $cfCmd) { $cfCmd = 'C:\Program Files (x86)\cloudflared\cloudflared.exe' }
    $tsCmd = Get-TailscaleExe
    if (-not $tsCmd) { $tsCmd = 'C:\Program Files\Tailscale\tailscale.exe' }

    if ($script:TunnelMode -eq 'tailscale') {
        @"
while (`$true) {
  & '$tsCmd' funnel --bg $port *> `$null
  Start-Sleep -Seconds 60
}
"@ | Set-Content (Join-Path $HomeDir 'run-tunnel.ps1') -Encoding utf8
    } elseif ($script:TunnelMode -eq 'service') {
        @"
# Tunnel mode 'service': the Windows service 'Cloudflared' (from cloudflared's .msi) carries the tunnel.
# ESC never starts cloudflared in this mode, and the 'ESC Tunnel' task is disabled.
exit
"@ | Set-Content (Join-Path $HomeDir 'run-tunnel.ps1') -Encoding utf8
    } elseif ($script:TunnelMode -eq 'named' -and $script:TunnelToken) {
        # The token lives in a file only SYSTEM and Administrators can read, not on a command line.
        $tokFile = Join-Path $HomeDir 'tunnel.token'
        [IO.File]::WriteAllText($tokFile, $script:TunnelToken)
        & icacls.exe $tokFile /inheritance:r /grant:r '*S-1-5-18:F' '*S-1-5-32-544:F' *> $null
        @"
Set-Content '$HomeDir\run-tunnel.pid' `$PID
while (`$true) {
  # Keep the log (rotated at 5 MB, never deleted) so a drop leaves evidence; http2 over TCP 7844 is steadier than QUIC on flaky links.
  if ((Test-Path '$HomeDir\tunnel.log') -and (Get-Item '$HomeDir\tunnel.log').Length -gt 5MB) { Move-Item '$HomeDir\tunnel.log' '$HomeDir\tunnel.log.old' -Force -ErrorAction SilentlyContinue }
  `$p = Start-Process -FilePath '$cfCmd' -ArgumentList 'tunnel','--no-autoupdate','--protocol','http2','--edge-ip-version','4','--logfile','$HomeDir\tunnel.log','run','--token-file','$tokFile' -NoNewWindow -PassThru
  Set-Content '$HomeDir\tunnel.pid' `$p.Id
  `$p.WaitForExit()
  Add-Content '$HomeDir\tunnel-restarts.log' ("{0} cloudflared exited, code {1}" -f (Get-Date -Format s), `$p.ExitCode)
  Start-Sleep -Seconds 5
}
"@ | Set-Content (Join-Path $HomeDir 'run-tunnel.ps1') -Encoding utf8
    } else {
        @"
while (`$true) {
  Remove-Item '$HomeDir\tunnel.log' -Force -ErrorAction SilentlyContinue
  `$p = Start-Process -FilePath '$cfCmd' -ArgumentList 'tunnel','--no-autoupdate','--url','http://$ProdListen','--logfile','$HomeDir\tunnel.log' -NoNewWindow -PassThru
  while (-not `$p.HasExited) {
    Start-Sleep -Seconds 5
    if (Test-Path '$HomeDir\tunnel.log') {
      `$tail = Get-Content '$HomeDir\tunnel.log' -Tail 25 -ErrorAction SilentlyContinue
      if (`$tail -match 'Unauthorized:\s*Tunnel not found') {
        Stop-Process -Id `$p.Id -Force -ErrorAction SilentlyContinue
        break
      }
    }
  }
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $HomeDir 'run-tunnel.ps1') -Encoding utf8
    }
}

function Save-PathsConfig {
    @(
        "# ESC Star Manager custom paths & tunnel config",
        "home = $HomeDir",
        "data = $DataDir",
        "backup = $BackupDir",
        "tunnel_mode = $($script:TunnelMode)",
        "tunnel_token = $($script:TunnelToken)"
    ) | Set-Content $PathsConfigFile -Encoding UTF8
    # Keep run-relay.ps1 in sync with the chosen HomeDir and DataDir if it exists
    if (Test-Path "$HomeDir\run-relay.ps1") {
        @"
while (`$true) {
  cmd /c "`"$HomeDir\esc.exe`" serve --data `"$DataDir`" --listen $ProdListen --tunnel-log `"$HomeDir\tunnel.log`" >> `"$HomeDir\esc.log`" 2>&1"
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $HomeDir 'run-relay.ps1') -Encoding utf8
    }
    if (Test-Path $HomeDir) {
        Write-TunnelRunner
    }
}

$InnerWidth = 102
$LeftWidth  = 50
$RightWidth = 51

# ──────────────────────────────────────────────────────────────────────────────
# Console Drawing Primitives (Strict 80-col inner width & flicker-free cursor)
# ──────────────────────────────────────────────────────────────────────────────

function Clear-ScreenSafe {
    try { [Console]::Clear() } catch { Clear-Host }
}

function Move-CursorTop {
    try {
        [Console]::SetCursorPosition(0, 0)
    } catch {
        Clear-ScreenSafe
    }
}

function Set-CursorHidden([bool]$hidden) {
    try { [Console]::CursorVisible = (-not $hidden) } catch {}
}

function Write-Seg([string]$text, [ConsoleColor]$fg = [ConsoleColor]::Gray, [ConsoleColor]$bg = [ConsoleColor]::Black) {
    $oldFg = [Console]::ForegroundColor
    $oldBg = [Console]::BackgroundColor
    [Console]::ForegroundColor = $fg
    if ($bg -ne [ConsoleColor]::Black) { [Console]::BackgroundColor = $bg }
    [Console]::Write($text)
    [Console]::ForegroundColor = $oldFg
    [Console]::BackgroundColor = $oldBg
}

function Write-BoxTop([ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    Write-Seg ("  ╔" + ("═" * $InnerWidth) + "╗") $borderColor
    [Console]::WriteLine()
}

function Write-BoxSplitTop([ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    Write-Seg ("  ╠" + ("═" * $LeftWidth) + "╦" + ("═" * $RightWidth) + "╣") $borderColor
    [Console]::WriteLine()
}

function Write-BoxSplitMid([ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    Write-Seg ("  ╠" + ("─" * $LeftWidth) + "╫" + ("─" * $RightWidth) + "╣") $borderColor
    [Console]::WriteLine()
}

function Write-BoxSplitEnd([ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    Write-Seg ("  ╠" + ("═" * $LeftWidth) + "╩" + ("═" * $RightWidth) + "╣") $borderColor
    [Console]::WriteLine()
}

function Write-BoxDivider([string]$ch = "─", [ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    $left = if ($ch -eq "═") { "╠" } else { "╟" }
    $right = if ($ch -eq "═") { "╣" } else { "╢" }
    Write-Seg ("  " + $left + ($ch * $InnerWidth) + $right) $borderColor
    [Console]::WriteLine()
}

function Write-BoxBottom([ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    Write-Seg ("  ╚" + ("═" * $InnerWidth) + "╝") $borderColor
    [Console]::WriteLine()
}

# Writes a single full-width boxed row from an array of @(text, fg, [bg]) segments,
# guaranteed to pad or clip to exactly $InnerWidth characters between ║ and ║.
function Write-BoxRow([array]$segments, [ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    if ($segments.Count -gt 0 -and $segments[0] -is [string]) {
        $segments = @(,$segments)
    }
    Write-Seg "  ║" $borderColor
    $used = 0
    foreach ($seg in $segments) {
        if ($null -eq $seg) { continue }
        $txt = [string]$seg[0]
        if ($used + $txt.Length -gt $InnerWidth) {
            $take = [Math]::Max(0, $InnerWidth - $used)
            $txt = if ($take -gt 0) { $txt.Substring(0, $take) } else { "" }
        }
        if ($txt.Length -gt 0) {
            $fg = if ($seg.Count -ge 2 -and $null -ne $seg[1]) { [ConsoleColor]$seg[1] } else { [ConsoleColor]::Gray }
            $bg = if ($seg.Count -ge 3 -and $null -ne $seg[2]) { [ConsoleColor]$seg[2] } else { [ConsoleColor]::Black }
            Write-Seg $txt $fg $bg
            $used += $txt.Length
        }
    }
    if ($used -lt $InnerWidth) {
        [Console]::Write(" " * ($InnerWidth - $used))
    }
    Write-Seg "║" $borderColor
    [Console]::WriteLine()
}

# Writes a 2-column split row (Left = 44 chars, Divider = ║/│, Right = 45 chars).
function Write-BoxSplitRow([array]$leftSegs, [array]$rightSegs, [string]$midChar = "│", [ConsoleColor]$borderColor = [ConsoleColor]::DarkCyan) {
    if ($leftSegs.Count -gt 0 -and $leftSegs[0] -is [string]) {
        $leftSegs = @(,$leftSegs)
    }
    if ($rightSegs.Count -gt 0 -and $rightSegs[0] -is [string]) {
        $rightSegs = @(,$rightSegs)
    }
    Write-Seg "  ║" $borderColor
    $usedL = 0
    foreach ($seg in $leftSegs) {
        if ($null -eq $seg) { continue }
        $txt = [string]$seg[0]
        if ($usedL + $txt.Length -gt $LeftWidth) {
            $take = [Math]::Max(0, $LeftWidth - $usedL)
            $txt = if ($take -gt 0) { $txt.Substring(0, $take) } else { "" }
        }
        if ($txt.Length -gt 0) {
            $fg = if ($seg.Count -ge 2 -and $null -ne $seg[1]) { [ConsoleColor]$seg[1] } else { [ConsoleColor]::Gray }
            $bg = if ($seg.Count -ge 3 -and $null -ne $seg[2]) { [ConsoleColor]$seg[2] } else { [ConsoleColor]::Black }
            Write-Seg $txt $fg $bg
            $usedL += $txt.Length
        }
    }
    if ($usedL -lt $LeftWidth) {
        [Console]::Write(" " * ($LeftWidth - $usedL))
    }

    Write-Seg $midChar $borderColor

    $usedR = 0
    foreach ($seg in $rightSegs) {
        if ($null -eq $seg) { continue }
        $txt = [string]$seg[0]
        if ($usedR + $txt.Length -gt $RightWidth) {
            $take = [Math]::Max(0, $RightWidth - $usedR)
            $txt = if ($take -gt 0) { $txt.Substring(0, $take) } else { "" }
        }
        if ($txt.Length -gt 0) {
            $fg = if ($seg.Count -ge 2 -and $null -ne $seg[1]) { [ConsoleColor]$seg[1] } else { [ConsoleColor]::Gray }
            $bg = if ($seg.Count -ge 3 -and $null -ne $seg[2]) { [ConsoleColor]$seg[2] } else { [ConsoleColor]::Black }
            Write-Seg $txt $fg $bg
            $usedR += $txt.Length
        }
    }
    if ($usedR -lt $RightWidth) {
        [Console]::Write(" " * ($RightWidth - $usedR))
    }
    Write-Seg "║" $borderColor
    [Console]::WriteLine()
}

function Format-Bytes([long]$bytes) {
    if ($bytes -ge 1GB) { return ("{0:N2} GiB" -f ($bytes / 1GB)) }
    if ($bytes -ge 1MB) { return ("{0:N1} MiB" -f ($bytes / 1MB)) }
    if ($bytes -ge 1KB) { return ("{0:N1} KiB" -f ($bytes / 1KB)) }
    return "$bytes B"
}

function Make-Bar([double]$ratio, [int]$width = 10) {
    $clamped = [Math]::Max(0.0, [Math]::Min(1.0, $ratio))
    $filled = [int][Math]::Round($clamped * $width)
    $empty = $width - $filled
    return ("[" + ("█" * $filled) + ("░" * $empty) + "]")
}

# ──────────────────────────────────────────────────────────────────────────────
# Animated ASCII Logo & Starfield Header
# ──────────────────────────────────────────────────────────────────────────────

function Draw-AnimatedHeader([int]$frame = 0, [string]$starName = "Polaris", [bool]$online = $true) {
    # Shimmer palette across the 6 logo rows
    $palette = @(
        [ConsoleColor]::White,
        [ConsoleColor]::Cyan,
        [ConsoleColor]::Cyan,
        [ConsoleColor]::DarkCyan,
        [ConsoleColor]::Blue,
        [ConsoleColor]::DarkCyan
    )

    # Twinkling constellation frames (strictly 1-cell ASCII/CP437 characters)
    $s1 = @("·", "+", "*", "+")[$frame % 4]
    $s2 = @("*", "·", "+", "·")[($frame + 1) % 4]
    $s3 = @("+", "*", "·", "*")[($frame + 2) % 4]
    $s4 = @("·", "·", "*", "+")[($frame + 3) % 4]

    # Animated orbital wave bar (12 chars wide)
    $waveFrames = @(
        "═══►        ",
        " ═══►       ",
        "  ═══►      ",
        "   ═══►     ",
        "    ═══►    ",
        "     ═══►   ",
        "      ═══►  ",
        "       ═══► ",
        "        ═══►",
        "►        ═══",
        "═►        ══",
        "══►        ═"
    )
    $wave = $waveFrames[$frame % $waveFrames.Count]

    # Beacon pulse
    $beaconFrames = @("(*)", "(+)", "(·)", "(+)")
    $beacon = if ($online) { $beaconFrames[$frame % 4] } else { "(o)" }
    $beaconCol = if ($online) {
        if (($frame % 2) -eq 0) { [ConsoleColor]::Green } else { [ConsoleColor]::Cyan }
    } else {
        [ConsoleColor]::Red
    }

    $c0 = $palette[($frame + 0) % $palette.Count]
    $c1 = $palette[($frame + 1) % $palette.Count]
    $c2 = $palette[($frame + 2) % $palette.Count]
    $c3 = $palette[($frame + 3) % $palette.Count]
    $c4 = $palette[($frame + 4) % $palette.Count]
    $c5 = $palette[($frame + 5) % $palette.Count]

    Write-BoxTop DarkCyan
    Write-BoxRow @(
        @("   $s1  ", [ConsoleColor]::DarkGray),
        @("███████╗ ███████╗  ██████╗", $c0),
        @("   $s2   ", [ConsoleColor]::Yellow),
        @("E X T R E M E L Y   S I M P L E", [ConsoleColor]::White),
        @("         $s3  ", [ConsoleColor]::Cyan),
        @($beacon, $beaconCol)
    )
    Write-BoxRow @(
        @("      ", [ConsoleColor]::DarkGray),
        @("██╔════╝ ██╔════╝ ██╔════╝", $c1),
        @("       ", [ConsoleColor]::DarkGray),
        @("C O M M U N I C A T I O N", [ConsoleColor]::Cyan),
        @("               $s1    ", [ConsoleColor]::DarkCyan)
    )
    Write-BoxRow @(
        @("   $s4  ", [ConsoleColor]::Cyan),
        @("█████╗   ███████╗ ██║     ", $c2),
        @("       ", [ConsoleColor]::DarkGray),
        @("────[", [ConsoleColor]::DarkGray),
        @($wave, [ConsoleColor]::Cyan),
        @("]────────────────────────", [ConsoleColor]::DarkGray)
    )
    Write-BoxRow @(
        @("      ", [ConsoleColor]::DarkGray),
        @("██╔══╝   ╚════██║ ██║     ", $c3),
        @("   $s3   ", [ConsoleColor]::DarkCyan),
        @("S T A R   T U I   M A N A G E R", [ConsoleColor]::Yellow),
        @("       $s2     ", [ConsoleColor]::White)
    )
    Write-BoxRow @(
        @("   $s2  ", [ConsoleColor]::DarkBlue),
        @("███████╗ ███████║ ╚██████╗", $c4),
        @("       ", [ConsoleColor]::DarkGray),
        @("Star: ", [ConsoleColor]::Gray),
        @($starName.PadRight(12), [ConsoleColor]::White),
        @("·  Zero-Dep Rust Relay", [ConsoleColor]::DarkGray)
    )
    Write-BoxRow @(
        @("      ", [ConsoleColor]::DarkGray),
        @("╚══════╝ ╚══════╝  ╚═════╝", $c5),
        @("   $s1   ", [ConsoleColor]::DarkGray),
        @("The Ring of Stars  ·  Edition 2024  ·  v0.1.0", [ConsoleColor]::DarkCyan),
        @("   $s4", [ConsoleColor]::Yellow)
    )
}

# ──────────────────────────────────────────────────────────────────────────────
# Binary Logbook & Relay State Reader
# ──────────────────────────────────────────────────────────────────────────────

function Get-EscLogState([string]$targetDataDir = $DataDir) {
    $logDir = Join-Path $targetDataDir "log"
    $users = [ordered]@{}
    $invites = [ordered]@{}
    $relays = [ordered]@{}
    $settings = [ordered]@{
        default_quota   = 1073741824 # 1 GiB
        flex_pct        = 10
        cleanup_h       = 24
        reserve_pct     = 5
        reserve_min     = 2147483648 # 2 GiB
        signup          = "invite"
        member_invites  = 0
        req_pending_max = 20
        req_daily_max   = 50
        smtp_host       = ""
        smtp_port       = 0
        smtp_user       = ""
        smtp_from       = ""
        smtp_tls        = ""
    }
    $msgCount = 0
    $convCount = 0
    $eventCount = 0

    if (Test-Path $logDir) {
        foreach ($file in (Get-ChildItem $logDir -Filter "*.log" -File -ErrorAction SilentlyContinue)) {
            try {
                $fs = [System.IO.File]::Open($file.FullName, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
                try {
                    $bytes = New-Object byte[] $fs.Length
                    [void]$fs.Read($bytes, 0, $bytes.Length)
                } finally {
                    $fs.Close()
                }
                $pos = 0
                while ($pos + 18 -le $bytes.Length) {
                    $len = [BitConverter]::ToUInt32($bytes, $pos)
                    if ($len -lt 10 -or ($pos + 8 + $len) -gt $bytes.Length) { break }
                    $ts = [BitConverter]::ToUInt64($bytes, $pos + 8)
                    $kind = [BitConverter]::ToUInt16($bytes, $pos + 16)
                    $eventCount++

                    if ($kind -in @(1, 2, 10, 11, 14, 16, 30, 40)) {
                        if ($kind -eq 30) { $convCount++ }
                        elseif ($kind -eq 40) { $msgCount++ }
                        else {
                            $jsonLen = [int]$len - 10
                            $jsonStr = [System.Text.Encoding]::UTF8.GetString($bytes, $pos + 18, $jsonLen)
                            $obj = $jsonStr | ConvertFrom-Json -ErrorAction SilentlyContinue
                            if ($obj) {
                                switch ($kind) {
                                    1 {
                                        if ($obj.r) {
                                            $relays[[string]$obj.r] = [PSCustomObject]@{
                                                Id      = [string]$obj.r
                                                Name    = if ($obj.n) { [string]$obj.n } else { "-" }
                                                Url     = if ($obj.u) { [string]$obj.u } else { "(no public URL)" }
                                                Updated = [DateTimeOffset]::FromUnixTimeMilliseconds([long]$ts).ToLocalTime().ToString("yyyy-MM-dd HH:mm")
                                            }
                                        }
                                    }
                                    2 {
                                        if ($obj.k -and $null -ne $obj.v) {
                                            $k = ([string]$obj.k).Replace('.', '_')
                                            if ($settings.Contains($k)) { $settings[$k] = $obj.v }
                                        }
                                    }
                                    10 {
                                        if ($obj.a) {
                                            $id = [string]$obj.a
                                            if (-not $users.Contains($id)) {
                                                $created = [DateTimeOffset]::FromUnixTimeMilliseconds([long]$ts).ToLocalTime().ToString("yyyy-MM-dd HH:mm")
                                                $users[$id] = [PSCustomObject]@{
                                                    Callsign    = $id
                                                    Name        = if ($obj.n) { [string]$obj.n } else { "" }
                                                    Admin       = [bool]$obj.adm
                                                    Disabled    = $false
                                                    DoorOpen    = $true
                                                    AutoRecycle = $true
                                                    Quota       = $null
                                                    Created     = $created
                                                }
                                            }
                                            if ($obj.inv -and $invites.Contains([string]$obj.inv)) {
                                                $inv = $invites[[string]$obj.inv]
                                                if ($inv.Uses -gt 0) { $inv.Uses-- }
                                            }
                                        }
                                    }
                                    11 {
                                        if ($obj.a -and $users.Contains([string]$obj.a)) {
                                            $u = $users[[string]$obj.a]
                                            if ($null -ne $obj.n)   { $u.Name        = [string]$obj.n }
                                            if ($null -ne $obj.adm) { $u.Admin       = [bool]$obj.adm }
                                            if ($null -ne $obj.dis) { $u.Disabled    = [bool]$obj.dis }
                                            if ($null -ne $obj.acc) { $u.DoorOpen    = [bool]$obj.acc }
                                            if ($null -ne $obj.ar)  { $u.AutoRecycle = [bool]$obj.ar }
                                            if ($obj.PSObject.Properties.Match('q').Count -gt 0) { $u.Quota = $obj.q }
                                        }
                                    }
                                    14 {
                                        if ($obj.c) {
                                            $code = [string]$obj.c
                                            $invites[$code] = [PSCustomObject]@{
                                                Code    = $code
                                                By      = if ($obj.by) { [string]$obj.by } else { "system" }
                                                Uses    = if ($null -ne $obj.u) { [int]$obj.u } else { 1 }
                                                Admin   = [bool]$obj.adm
                                                Created = [DateTimeOffset]::FromUnixTimeMilliseconds([long]$ts).ToLocalTime().ToString("yyyy-MM-dd HH:mm")
                                            }
                                        }
                                    }
                                    16 {
                                        if ($obj.c) { $invites.Remove([string]$obj.c) }
                                    }
                                }
                            }
                        }
                    }
                    $pos += 8 + [int]$len
                }
            } catch {}
        }
    }

    $activeInvites = @($invites.Values | Where-Object { $_.Uses -gt 0 })
    return [PSCustomObject]@{
        Users      = @($users.Values)
        Invites    = $activeInvites
        Relays     = @($relays.Values)
        Settings   = $settings
        MsgCount   = $msgCount
        ConvCount  = $convCount
        EventCount = $eventCount
    }
}

function Test-LocalRelay([string]$listenAddr) {
    try {
        $req = [System.Net.WebRequest]::Create("http://$listenAddr/api/info")
        $req.Timeout = 1200
        $resp = $req.GetResponse()
        $code = [int]$resp.StatusCode
        $resp.Close()
        return ($code -eq 200)
    } catch {
        return $false
    }
}

function Restart-ZombieTunnel {
    if ($script:TunnelMode -eq 'service') {
        Restart-Service Cloudflared -ErrorAction SilentlyContinue
        return
    }
    Get-EscCloudflared | Stop-Process -Force -ErrorAction SilentlyContinue
    Stop-TunnelWrappers
    Set-TunnelTask $true
    Start-Sleep -Milliseconds 250
    Remove-Item "$HomeDir\tunnel.log" -Force -ErrorAction SilentlyContinue
    cmd /c "schtasks /run /tn `"ESC Tunnel`" >nul 2>&1"
    if ($LASTEXITCODE -ne 0 -and (Test-Path "$HomeDir\run-tunnel.ps1")) {
        $hasRunner = $false
        Get-Process powershell -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $PID } | ForEach-Object {
            try {
                $cl = (Get-CimInstance Win32_Process -Filter "ProcessId = $($_.Id)" -ErrorAction SilentlyContinue).CommandLine
                if ($cl -and $cl -like "*run-tunnel.ps1*") { $hasRunner = $true }
            } catch {}
        }
        if (-not $hasRunner -and -not (Test-WrapperAlive 'run-tunnel')) {
            Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$HomeDir\run-tunnel.ps1`"" -WindowStyle Hidden
        }
    }
}

function Get-StarStatus {
    $logState = Get-EscLogState $DataDir
    $st = [ordered]@{
        StarName     = "Polaris"
        StarId       = "-"
        ProdListen   = $ProdListen
        ConfigUrl    = ""
        Seeds        = ""
        RelayProc    = $null
        RelayHealthy = $false
        RelayMemMB   = 0.0
        TunnelMode   = $script:TunnelMode
        TunnelProc   = $null
        TunnelState  = "OFFLINE"
        PublicUrl    = $null
        AdminTicket  = $null
        Events       = $logState.EventCount
        DevProc      = $null
        DevMemMB     = 0.0
        DevTicket    = $null
        DiskFreeGB   = 0.0
        DiskTotalGB  = 1.0
        DataSizeMB   = 0.0
        BlobCount    = 0
        UserCount    = $logState.Users.Count
        ConvCount    = $logState.ConvCount
        MsgCount     = $logState.MsgCount
        LogState     = $logState
        AutoStart    = $false
        GitDirty     = 0
    }

    if (Test-Path "$DataDir\relay.ini") {
        foreach ($line in (Get-Content "$DataDir\relay.ini" -ErrorAction SilentlyContinue)) {
            if ($line -match '^\s*name\s*=\s*(.*)$')   { $st.StarName   = $Matches[1].Trim() }
            if ($line -match '^\s*id\s*=\s*(.*)$')     { $st.StarId     = $Matches[1].Trim() }
            if ($line -match '^\s*listen\s*=\s*(.*)$') { $st.ProdListen = $Matches[1].Trim() }
            if ($line -match '^\s*url\s*=\s*(.*)$')    { $st.ConfigUrl  = $Matches[1].Trim() }
            if ($line -match '^\s*seeds\s*=\s*(.*)$')  { $st.Seeds      = $Matches[1].Trim() }
        }
    }

    $allEsc = @(Get-Process esc, esc-dev -ErrorAction SilentlyContinue)
    $st.RelayProc = $allEsc | Where-Object { $_.Name -eq 'esc' -and ($_.Path -like "$HomeDir*" -or -not $_.Path) } | Select-Object -First 1
    if (-not $st.RelayProc) {
        $st.RelayProc = $allEsc | Where-Object { $_.Name -eq 'esc' -and $_.Path -notlike "$DevDir*" } | Select-Object -First 1
    }
    if ($st.RelayProc) {
        $st.RelayMemMB = [math]::Round($st.RelayProc.WorkingSet64 / 1MB, 1)
        $st.RelayHealthy = Test-LocalRelay $st.ProdListen
    }

    $st.DevProc = $allEsc | Where-Object { $_.Name -eq 'esc-dev' -or ($_.Path -and $_.Path -like "$DevDir*") } | Select-Object -First 1
    if ($st.DevProc) {
        $st.DevMemMB = [math]::Round($st.DevProc.WorkingSet64 / 1MB, 1)
    }

    if ($script:TunnelMode -eq 'tailscale') {
        $st.TunnelProc = Get-Process tailscaled -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($st.ConfigUrl) {
            $st.PublicUrl = $st.ConfigUrl
        } else {
            $tsCmd = Get-TailscaleExe
            if ($tsCmd) {
                $tsJson = (& $tsCmd status --json 2>$null) -join "`n"
                if ($tsJson -match '"DNSName"\s*:\s*"([^"]+)\."') {
                    $st.PublicUrl = "https://$($Matches[1])"
                }
            }
        }
        if ($st.TunnelProc -and $st.PublicUrl) {
            $st.TunnelState = "ONLINE"
        } elseif ($st.TunnelProc) {
            $st.TunnelState = "STARTING"
        }
    } elseif ($script:TunnelMode -eq 'service') {
        $svc = Get-CfServiceInfo
        if ($svc -and $svc.Pid -gt 0) { $st.TunnelProc = Get-Process -Id $svc.Pid -ErrorAction SilentlyContinue }
        if ($st.ConfigUrl) { $st.PublicUrl = $st.ConfigUrl }
        $st.TunnelState = if (-not $svc) { "NO SERVICE" } elseif ($svc.State -eq 'Running') { "ONLINE" } else { "OFFLINE" }
    } elseif ($script:TunnelMode -eq 'named') {
        $st.TunnelProc = Get-EscCloudflared | Select-Object -First 1
        if ($st.ConfigUrl) { $st.PublicUrl = $st.ConfigUrl }
        if ($st.TunnelProc) {
            if (Test-Path "$HomeDir\tunnel.log") {
                $tailLines = @(Get-Content "$HomeDir\tunnel.log" -Tail 25 -ErrorAction SilentlyContinue)
                $tailText = $tailLines -join "`n"
                if ($tailText -match 'Registered tunnel connection') {
                    $st.TunnelState = "ONLINE"
                } elseif ($tailText -match 'Retrying connection|Serve tunnel error') {
                    $st.TunnelState = "RECONNECTING"
                } else {
                    $st.TunnelState = "STARTING"
                }
            } else {
                $st.TunnelState = "ONLINE"
            }
        }
    } else {
        $st.TunnelProc = Get-EscCloudflared | Select-Object -First 1

        if (Test-Path "$HomeDir\tunnel.log") {
            $m = Select-String -Path "$HomeDir\tunnel.log" -Pattern 'https://[a-z0-9-]+\.trycloudflare\.com' -AllMatches -ErrorAction SilentlyContinue | Select-Object -Last 1
            if ($m) { $st.PublicUrl = $m.Matches[-1].Value }

            $tailLines = @(Get-Content "$HomeDir\tunnel.log" -Tail 30 -ErrorAction SilentlyContinue)
            $tailText = $tailLines -join "`n"
            if ($tailText -match 'Unauthorized:\s*Tunnel not found') {
                $st.TunnelState = "HEALING"
                $st.PublicUrl = $null
                if ($st.TunnelProc -or $st.RelayProc) {
                    Restart-ZombieTunnel
                }
            } elseif ($st.TunnelProc) {
                if ($tailText -match ' Serve tunnel error|Retrying connection' -and $tailLines[-1] -notmatch 'Registered tunnel connection') {
                    $st.TunnelState = "RECONNECTING"
                } elseif ($st.PublicUrl) {
                    $st.TunnelState = "ONLINE"
                } else {
                    $st.TunnelState = "STARTING"
                }
            } else {
                $st.PublicUrl = $null
            }
        } elseif ($st.TunnelProc) {
            $st.TunnelState = "STARTING"
        }
    }

    # Connected to Cloudflare is not the same as reachable: without a DNS record the domain never reaches the tunnel.
    if ($script:TunnelMode -in 'named', 'service' -and $st.TunnelState -eq 'ONLINE' -and -not (Test-PublicDns $st.PublicUrl)) {
        $st.TunnelState = "NO DNS"
    }

    if (Test-Path "$HomeDir\esc.log") {
        $lines = @(Get-Content "$HomeDir\esc.log" -ErrorAction SilentlyContinue)
        $lastBoot = 0
        for ($i = $lines.Count - 1; $i -ge 0; $i--) {
            if ($lines[$i] -match '^ESC .* Star ') {
                $lastBoot = $i
                break
            }
        }
        $bootLines = $lines[$lastBoot..($lines.Count - 1)]
        foreach ($l in $bootLines) {
            if ($l -match '#ticket=([A-Z0-9]+)') { $st.AdminTicket = $Matches[1] }
            if ($l -match 'public\s+(https://[a-z0-9-]+\.trycloudflare\.com)') {
                if (-not $st.PublicUrl -and $st.TunnelProc -and $st.TunnelState -ne "HEALING") { $st.PublicUrl = $Matches[1] }
            }
        }
    }

    if (Test-Path "$DevDir\dev.log") {
        $devLines = @(Get-Content "$DevDir\dev.log" -ErrorAction SilentlyContinue)
        foreach ($l in $devLines) {
            if ($l -match '#ticket=([A-Z0-9]+)') { $st.DevTicket = $Matches[1] }
        }
    }

    $driveLetter = ($DataDir -split ':')[0]
    $drv = Get-PSDrive -Name $driveLetter -ErrorAction SilentlyContinue
    if ($drv) {
        $st.DiskFreeGB  = [math]::Round($drv.Free / 1GB, 1)
        $st.DiskTotalGB = [math]::Round(($drv.Free + $drv.Used) / 1GB, 1)
    }
    if (Test-Path $DataDir) {
        $files = @(Get-ChildItem $DataDir -Recurse -File -ErrorAction SilentlyContinue)
        $bytes = ($files | Measure-Object -Property Length -Sum).Sum
        if ($bytes) { $st.DataSizeMB = [math]::Round($bytes / 1MB, 2) }
        $blobDir = Join-Path $DataDir "blobs"
        if (Test-Path $blobDir) {
            $st.BlobCount = @(Get-ChildItem $blobDir -Recurse -File -ErrorAction SilentlyContinue).Count
        }
    }

    cmd /c "schtasks /query /tn `"ESC Relay`" >nul 2>&1"
    $st.AutoStart = ($LASTEXITCODE -eq 0)

    $gitOut = @(git -C $repoRoot status --porcelain 2>$null)
    $st.GitDirty = $gitOut.Count

    return $st
}

# ──────────────────────────────────────────────────────────────────────────────
# Main Dashboard Renderer (Strict 2-Column Telemetry + Highlighted Menu)
# ──────────────────────────────────────────────────────────────────────────────

function Draw-Dashboard($st, [int]$selected, [string]$flashMsg, [int]$frame = 0, [bool]$inPlace = $false) {
    if ($inPlace) { Move-CursorTop } else { Clear-ScreenSafe }

    $isOnline = ($null -ne $st.RelayProc)
    Draw-AnimatedHeader -frame $frame -starName $st.StarName -online $isOnline

    $pulseDot = if (($frame % 2) -eq 0) { "●" } else { "◉" }
    $cfg = $st.LogState.Settings
    $tankStr = Format-Bytes ([long]$cfg.default_quota)
    $diskUsedRatio = if ($st.DiskTotalGB -gt 0) { ($st.DiskTotalGB - $st.DiskFreeGB) / $st.DiskTotalGB } else { 0.1 }
    $diskBar = Make-Bar $diskUsedRatio 8

    # 2-Column Telemetry Grid
    Write-BoxSplitTop DarkCyan
    Write-BoxSplitRow @(
        @(" STAR IDENTITY & ACTIVITY", [ConsoleColor]::Cyan)
    ) @(
        @(" STORAGE & CONTROL TOWER", [ConsoleColor]::Cyan)
    ) "║"

    Write-BoxSplitMid DarkCyan

    Write-BoxSplitRow @(
        @(" Star Name : ", [ConsoleColor]::Gray),
        @($st.StarName, [ConsoleColor]::White),
        @(" ($($st.StarId))", [ConsoleColor]::DarkGray)
    ) @(
        @(" Data Path : ", [ConsoleColor]::Gray),
        @($DataDir, [ConsoleColor]::White),
        @(" ($($st.DataSizeMB) MB)", [ConsoleColor]::DarkGray)
    )

    Write-BoxSplitRow @(
        @(" Accounts  : ", [ConsoleColor]::Gray),
        @("$($st.UserCount) Users", [ConsoleColor]::Cyan),
        @(" · ", [ConsoleColor]::DarkGray),
        @("$($st.ConvCount) Convs", [ConsoleColor]::White),
        @(" · ", [ConsoleColor]::DarkGray),
        @("$($st.MsgCount) Msgs", [ConsoleColor]::White)
    ) @(
        @(" Hold/Disk : ", [ConsoleColor]::Gray),
        @("$($st.BlobCount) blobs ", [ConsoleColor]::Cyan),
        @($diskBar, [ConsoleColor]::DarkCyan),
        @(" $($st.DiskFreeGB)G free", [ConsoleColor]::Gray)
    )

    Write-BoxSplitRow @(
        @(" Logbook   : ", [ConsoleColor]::Gray),
        @("$($st.Events) events", [ConsoleColor]::Green),
        @(" · Boot Task: ", [ConsoleColor]::Gray),
        @($(if ($st.AutoStart) { "ON" } else { "OFF" }), $(if ($st.AutoStart) { [ConsoleColor]::Green } else { [ConsoleColor]::DarkGray }))
    ) @(
        @(" Tank Rule : ", [ConsoleColor]::Gray),
        @("$tankStr (+$($cfg.flex_pct)%)", [ConsoleColor]::White),
        @(" · ", [ConsoleColor]::DarkGray),
        @($cfg.signup.ToUpper(), [ConsoleColor]::Yellow)
    )

    Write-BoxSplitEnd DarkCyan

    # Live Services Section
    if ($st.RelayProc -and $st.RelayHealthy) {
        Write-BoxRow @(
            @(" Prod Star : ", [ConsoleColor]::Gray),
            @("[$pulseDot] RUNNING ", [ConsoleColor]::Green),
            @("PID $($st.RelayProc.Id) · $($st.RelayMemMB) MB RAM", [ConsoleColor]::White),
            @("  ─►  ", [ConsoleColor]::DarkCyan),
            @("http://$($st.ProdListen)", [ConsoleColor]::Cyan)
        )
    } elseif ($st.RelayProc) {
        Write-BoxRow @(
            @(" Prod Star : ", [ConsoleColor]::Gray),
            @("[!] STALLED ", [ConsoleColor]::Yellow),
            @("PID $($st.RelayProc.Id) is not responding on http://$($st.ProdListen)", [ConsoleColor]::Yellow)
        )
    } else {
        Write-BoxRow @(
            @(" Prod Star : ", [ConsoleColor]::Gray),
            @("[○] STOPPED ", [ConsoleColor]::Red),
            @("(listen configured on http://$($st.ProdListen))", [ConsoleColor]::DarkGray)
        )
    }

    $tunLabel = switch ($st.TunnelMode) {
        'tailscale' { " TS Funnel : " }
        'named'     { " CF Named  : " }
        'service'   { " CF Service: " }
        default     { " CF Tunnel : " }
    }
    $tunExtra = switch ($st.TunnelMode) {
        'tailscale' { "Permanent  ─►  " }
        'named'     { "Permanent PID $($st.TunnelProc.Id)  ─►  " }
        'service'   { "Windows service PID $($st.TunnelProc.Id)  ─►  " }
        default     { "PID $($st.TunnelProc.Id)  ─►  " }
    }
    switch ($st.TunnelState) {
        "ONLINE" {
            Write-BoxRow @(
                @($tunLabel, [ConsoleColor]::Gray),
                @("[$pulseDot] ONLINE  ", [ConsoleColor]::Green),
                @($tunExtra, [ConsoleColor]::DarkCyan),
                @($st.PublicUrl, [ConsoleColor]::Yellow)
            )
        }
        "NO DNS" {
            Write-BoxRow @(
                @($tunLabel, [ConsoleColor]::Gray),
                @("[!] NO DNS  ", [ConsoleColor]::Yellow),
                @("connected, but the domain has no DNS yet (set its nameservers to Cloudflare)", [ConsoleColor]::DarkYellow)
            )
        }
        "NO SERVICE" {
            Write-BoxRow @(
                @($tunLabel, [ConsoleColor]::Gray),
                @("[○] MISSING ", [ConsoleColor]::Red),
                @("Windows service 'Cloudflared' not found (install the .msi, then [S] -> [4])", [ConsoleColor]::DarkGray)
            )
        }
        "HEALING" {
            Write-BoxRow @(
                @($tunLabel, [ConsoleColor]::Gray),
                @("[!] HEALING ", [ConsoleColor]::Yellow),
                @("Cloudflare expired old tunnel ('Tunnel not found') -> rotating now...", [ConsoleColor]::Yellow)
            )
        }
        "RECONNECTING" {
            $u = if ($st.PublicUrl) { $st.PublicUrl } else { "reconnecting to tunnel edge..." }
            Write-BoxRow @(
                @($tunLabel, [ConsoleColor]::Gray),
                @("[!] RETRY   ", [ConsoleColor]::Yellow),
                @("PID $($st.TunnelProc.Id) reconnecting  ─►  $u", [ConsoleColor]::DarkYellow)
            )
        }
        "STARTING" {
            Write-BoxRow @(
                @($tunLabel, [ConsoleColor]::Gray),
                @("[·] STARTING", [ConsoleColor]::Cyan),
                @(" Negotiating public tunnel connection...", [ConsoleColor]::Gray)
            )
        }
        default {
            Write-BoxRow @(
                @($tunLabel, [ConsoleColor]::Gray),
                @("[○] OFFLINE ", [ConsoleColor]::Red),
                @("(public tunnel is not running — press [1] or configure in [S])", [ConsoleColor]::DarkGray)
            )
        }
    }

    if ($st.DevProc) {
        $ticketSuffix = if ($st.DevTicket) { "/#ticket=$($st.DevTicket)" } else { "" }
        Write-BoxRow @(
            @(" Dev Mode  : ", [ConsoleColor]::Gray),
            @("[$pulseDot] RUNNING ", [ConsoleColor]::Green),
            @("PID $($st.DevProc.Id) ($($st.DevMemMB) MB)  ─►  ", [ConsoleColor]::White),
            @("http://$DevListen$ticketSuffix", [ConsoleColor]::Cyan)
        )
    } else {
        $repoInfo = if ($st.GitDirty -gt 0) { "$($st.GitDirty) modified/untracked files" } else { "working tree clean" }
        Write-BoxRow @(
            @(" Dev Mode  : ", [ConsoleColor]::Gray),
            @("[·] STANDBY ", [ConsoleColor]::DarkGray),
            @("(port 7979, live web/)  ·  Git: ", [ConsoleColor]::DarkGray),
            @($repoInfo, $(if ($st.GitDirty -gt 0) { [ConsoleColor]::DarkYellow } else { [ConsoleColor]::DarkGreen }))
        )
    }

    if ($st.AdminTicket) {
        $base = if ($st.PublicUrl) { $st.PublicUrl } else { "http://$($st.ProdListen)" }
        Write-BoxRow @(
            @(" Admin Key : ", [ConsoleColor]::Magenta),
            @("$base/#ticket=$($st.AdminTicket)", [ConsoleColor]::Yellow),
            @(" (first admin)", [ConsoleColor]::DarkGray)
        )
    }

    Write-BoxDivider "═" DarkCyan

    # Interactive Menu Items
    $items = @(
        @{ Key = "1"; Label = "Start / Restart Production Star ($($st.StarName) + Cloudflare Tunnel)" },
        @{ Key = "2"; Label = "Stop Production Star & Cloudflare Tunnel" },
        @{ Key = "3"; Label = "Build Release & Deploy to Production (cargo build --release -> $HomeDir)" },
        @{ Key = "4"; Label = $(if ($st.DevProc) { "Stop Dev Server (port 7979)" } else { "Start Dev Server on port 7979 (serves live web/ without rebuild)" }) },
        @{ Key = "5"; Label = "Copy Public Tunnel URL to Clipboard & Open in Browser" },
        @{ Key = "6"; Label = "Open Local Star URL in Browser (http://$($st.ProdListen) or Dev 7979)" },
        @{ Key = "7"; Label = "Live Logs Monitor (real-time esc.log & tunnel.log stream)" },
        @{ Key = "8"; Label = "Run Cargo Clippy & Release Unit Tests (126+ tests)" },
        @{ Key = "9"; Label = "Show All Users in Relay ($($st.UserCount) accounts: Callsign, Name, Role & Tank)" },
        @{ Key = "J"; Label = "Join an Existing Ring (esc join wizard — connect this PC to another Star)" },
        @{ Key = "S"; Label = "Settings & Control Tower (Star Config, Quotas, Tickets, Backup & Boot)" },
        @{ Key = "R"; Label = "Force Refresh Telemetry Now" },
        @{ Key = "Q"; Label = "Quit ESC Star Manager" }
    )

    for ($i = 0; $i -lt $items.Count; $i++) {
        $it = $items[$i]
        if ($i -eq $selected) {
            $selBar = " ► [$($it.Key)] $($it.Label)".PadRight($InnerWidth - 2)
            Write-BoxRow @(
                @(" ", [ConsoleColor]::Black),
                @($selBar, [ConsoleColor]::Black, [ConsoleColor]::Cyan),
                @(" ", [ConsoleColor]::Black)
            )
        } else {
            Write-BoxRow @(
                @("   [", [ConsoleColor]::DarkGray),
                @($it.Key, [ConsoleColor]::Cyan),
                @("] ", [ConsoleColor]::DarkGray),
                @($it.Label, [ConsoleColor]::Gray)
            )
        }
    }

    Write-BoxDivider "─" DarkCyan

    $clock = (Get-Date).ToString("HH:mm:ss")
    if ($flashMsg) {
        Write-BoxRow @(
            @(" * ", [ConsoleColor]::Yellow),
            @($flashMsg.PadRight(87), [ConsoleColor]::Yellow),
            @("[$clock]", [ConsoleColor]::DarkGray)
        )
    } else {
        Write-BoxRow @(
            @(" [Up/Down + Enter] ", [ConsoleColor]::Cyan),
            @("or ", [ConsoleColor]::DarkGray),
            @("[1-9] Actions  ·  [J]oin Ring  ·  [U]sers  ·  [S]ettings  ·  [R]efresh  ·  [Q]uit", [ConsoleColor]::Gray),
            @(" [$clock]", [ConsoleColor]::DarkGray)
        )
    }

    Write-BoxBottom DarkCyan
}

# ──────────────────────────────────────────────────────────────────────────────
# Boot Intro Animation (runs once on interactive launch)
# ──────────────────────────────────────────────────────────────────────────────

function Play-BootIntro {
    Clear-ScreenSafe
    Set-CursorHidden $true
    for ($f = 0; $f -lt 4; $f++) {
        Move-CursorTop
        Draw-AnimatedHeader -frame $f -starName "Polaris" -online $true
        Write-BoxBottom DarkCyan
        Start-Sleep -Milliseconds 65
    }
}

# ──────────────────────────────────────────────────────────────────────────────
# Commands & Actions
# ──────────────────────────────────────────────────────────────────────────────

function Join-ExistingRing {
    Set-CursorHidden $false
    Clear-ScreenSafe
    Draw-AnimatedHeader -frame 0 -starName "New Star" -online $false
    Write-BoxDivider "═" DarkCyan
    Write-BoxRow @( ,@(" JOIN AN EXISTING RING (esc join Wizard)", [ConsoleColor]::Cyan) )
    Write-BoxDivider "─" DarkCyan
    Write-BoxRow @( ,@(" 1. On your existing Star (e.g. Polaris), open Control Tower -> The Ring -> Light a new Star.", [ConsoleColor]::Gray) )
    Write-BoxRow @( ,@(" 2. Paste either the full 'esc join ...' command OR the Star URL below.", [ConsoleColor]::Gray) )
    Write-BoxBottom DarkCyan

    Write-Host "`n   Star URL or full 'esc join <url> <spark>' command (leave blank to cancel):" -ForegroundColor Cyan
    Write-Host "   > " -ForegroundColor Yellow -NoNewline
    $rawInput = (Read-Host).Trim()
    if (-not $rawInput) {
        Clear-ScreenSafe
        return "Join Ring cancelled."
    }

    $peerUrl = ""
    $spark   = ""
    if ($rawInput -match '(https?://\S+)\s+([A-Za-z0-9_-]+)') {
        $peerUrl = $Matches[1].TrimEnd('/')
        $spark   = $Matches[2].Trim()
        Write-Host "   Detected Star URL : $peerUrl" -ForegroundColor Green
        Write-Host "   Detected Spark    : $spark" -ForegroundColor Green
    } else {
        $peerUrl = $rawInput.TrimEnd('/')
        Write-Host "`n   Enter the one-time Spark token from Control Tower (leave blank to cancel):" -ForegroundColor Cyan
        Write-Host "   > " -ForegroundColor Yellow -NoNewline
        $spark = (Read-Host).Trim()
        if (-not $spark) {
            Clear-ScreenSafe
            return "Join Ring cancelled."
        }
    }

    Write-Host "`n   Target Data Folder on this PC [Enter for $DataDir]: " -ForegroundColor Cyan -NoNewline
    $customData = (Read-Host).Trim()
    if ($customData) {
        $script:DataDir = $customData
        if (-not (Test-Path $PathsConfigFile)) {
            $driveRoot = Split-Path -Path $customData -Qualifier -ErrorAction SilentlyContinue
            if ($driveRoot) {
                $script:HomeDir   = Join-Path $driveRoot "\esc"
                $script:BackupDir = Join-Path $driveRoot "\esc_backups"
            }
        }
    }

    if (Test-Path (Join-Path $DataDir "relay.ini")) {
        Write-Host "`n   [!] $DataDir already holds a Star (relay.ini exists)." -ForegroundColor Yellow
        Write-Host "   Move existing $DataDir to backup folder before joining? [y/N]: " -ForegroundColor Yellow -NoNewline
        $ans = (Read-Host).Trim()
        if ($ans -match '^[Yy]') {
            [void](Stop-ProdStar)
            $bak = "${DataDir}_old_$(Get-Date -Format 'yyyyMMdd-HHmmss')"
            Move-Item $DataDir $bak -Force
            Write-Host "   Moved old data folder to $bak" -ForegroundColor Gray
        } else {
            Clear-ScreenSafe
            return "Join Ring cancelled ($DataDir already holds a Star)."
        }
    }

    Write-Host "`n   Optional Star Name (press Enter to let the Ring pick Vega, Sirius, etc.): " -ForegroundColor Cyan -NoNewline
    $customName = (Read-Host).Trim()

    # Locate esc.exe
    $exeCandidates = @(
        (Join-Path $HomeDir "esc.exe"),
        (Join-Path $repoRoot "esc.exe"),
        (Join-Path $ServerDir "target\release\esc.exe")
    )
    $exePath = $exeCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $exePath -and (Test-Path (Join-Path $ServerDir "Cargo.toml"))) {
        Write-Host "`n   esc.exe not found yet; building release binary..." -ForegroundColor Cyan
        Push-Location $ServerDir
        & cargo build --release
        Pop-Location
        $exePath = Join-Path $ServerDir "target\release\esc.exe"
    }
    if (-not $exePath -or -not (Test-Path $exePath)) {
        Write-Host "`n   [!] Could not find esc.exe. Place esc.exe next to esc-manager.cmd or in $HomeDir." -ForegroundColor Red
        Write-Host "   Press any key to return..." -ForegroundColor Gray
        [void][Console]::ReadKey($true)
        Clear-ScreenSafe
        return "Join Ring failed: esc.exe not found."
    }

    New-Item -ItemType Directory -Force $HomeDir | Out-Null
    $homeExe = Join-Path $HomeDir "esc.exe"
    if ($exePath -ne $homeExe) {
        Copy-Item $exePath $homeExe -Force -ErrorAction SilentlyContinue
        if (Test-Path $homeExe) { $exePath = $homeExe }
    }

    $joinArgs = @("join", $peerUrl, $spark, "--data", $DataDir)
    if ($customName) { $joinArgs += @("--name", $customName) }

    Write-Host "`n   Connecting to $peerUrl and joining the Ring..." -ForegroundColor Cyan
    & $exePath @joinArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "`n   [!] 'esc join' failed (exit code $LASTEXITCODE). Check the URL and Spark." -ForegroundColor Red
        Write-Host "   Press any key to return..." -ForegroundColor Gray
        [void][Console]::ReadKey($true)
        Clear-ScreenSafe
        return "Join Ring failed."
    }

    Save-PathsConfig
    # Ensure run-relay.ps1 and run-tunnel.ps1 exist in $HomeDir so [1] Start works on a standalone PC
    if (-not (Test-Path "$HomeDir\run-relay.ps1")) {
        @"
while (`$true) {
  cmd /c "`"$HomeDir\esc.exe`" serve --data `"$DataDir`" --listen $ProdListen --tunnel-log `"$HomeDir\tunnel.log`" >> `"$HomeDir\esc.log`" 2>&1"
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $HomeDir 'run-relay.ps1') -Encoding utf8
    }
    $cfCmd = (Get-Command cloudflared -ErrorAction SilentlyContinue).Source
    if (-not $cfCmd) { $cfCmd = 'C:\Program Files (x86)\cloudflared\cloudflared.exe' }
    if (-not (Test-Path "$HomeDir\run-tunnel.ps1") -and (Test-Path $cfCmd)) {
        @"
while (`$true) {
  Remove-Item '$HomeDir\tunnel.log' -Force -ErrorAction SilentlyContinue
  `$p = Start-Process -FilePath '$cfCmd' -ArgumentList 'tunnel','--no-autoupdate','--url','http://$ProdListen','--logfile','$HomeDir\tunnel.log' -NoNewWindow -PassThru
  while (-not `$p.HasExited) {
    Start-Sleep -Seconds 5
    if (Test-Path '$HomeDir\tunnel.log') {
      `$tail = Get-Content '$HomeDir\tunnel.log' -Tail 25 -ErrorAction SilentlyContinue
      if (`$tail -match 'Unauthorized:\s*Tunnel not found') {
        Stop-Process -Id `$p.Id -Force -ErrorAction SilentlyContinue
        break
      }
    }
  }
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $HomeDir 'run-tunnel.ps1') -Encoding utf8
    }

    Write-Host "`n   [OK] Successfully joined the Ring! Start this Star now? [Y/n]: " -ForegroundColor Green -NoNewline
    $startNow = (Read-Host).Trim()
    if ($startNow -eq '' -or $startNow -match '^[Yy]') {
        return (Start-ProdStar)
    }
    Clear-ScreenSafe
    return "Joined the Ring in $DataDir. Press [1] to start serving."
}

function Start-ProdStar {
    Set-CursorHidden $false
    Write-Host "`n  Starting / Restarting Production Star..." -ForegroundColor Cyan
    if (-not (Test-Path "$HomeDir\esc.exe")) {
        Write-Host "  $HomeDir\esc.exe not found. Running deploy\install.ps1..." -ForegroundColor Yellow
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $DeployDir "install.ps1") -Home_ $HomeDir -Data $DataDir
        if ($LASTEXITCODE -ne 0) {
            Write-Host "`n  Install FAILED (exit $LASTEXITCODE). Read the message above (run the manager as Administrator; build first with 'cargo build --release' in server\). Press any key..." -ForegroundColor Red
            [void][Console]::ReadKey($true)
            Clear-ScreenSafe
            return "Install failed: see the error above."
        }
        if (-not (Get-CloudflaredExe) -and $script:TunnelMode -in 'quick','named') {
            Write-Host "`n  Relay started, but cloudflared is NOT installed, so there is no public tunnel. Run: winget install --id Cloudflare.cloudflared -e   then press [1] again. Press any key..." -ForegroundColor Yellow
            [void][Console]::ReadKey($true)
        }
        Clear-ScreenSafe
        return "Production Star installed and started."
    }
    # 'service' mode: the Windows service owns the tunnel. Otherwise only cloudflared that ESC launched counts.
    $tunnelAlive = ($script:TunnelMode -eq 'service') -or ((Get-EscCloudflared).Count -gt 0)
    if ($script:TunnelMode -eq 'service') {
        Set-TunnelTask $false
        Remove-Item "$HomeDir\tunnel.log" -Force -ErrorAction SilentlyContinue
        $cfs = Get-Service Cloudflared -ErrorAction SilentlyContinue
        if ($cfs -and $cfs.Status -ne 'Running') { Start-Service Cloudflared -ErrorAction SilentlyContinue }
    } else {
        Set-TunnelTask $true
    }
    cmd /c "schtasks /end /tn `"ESC Relay`" >nul 2>&1"
    Get-Process esc -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like "$HomeDir*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 600

    cmd /c "schtasks /run /tn `"ESC Relay`" >nul 2>&1"
    $startedRelay = ($LASTEXITCODE -eq 0)
    if (-not $startedRelay -and (Test-Path "$HomeDir\run-relay.ps1") -and -not (Test-WrapperAlive 'run-relay')) {
        Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$HomeDir\run-relay.ps1`"" -WindowStyle Hidden
    }

    if (-not $tunnelAlive) {
        cmd /c "schtasks /run /tn `"ESC Tunnel`" >nul 2>&1"
        if ($LASTEXITCODE -ne 0 -and (Test-Path "$HomeDir\run-tunnel.ps1") -and -not (Test-WrapperAlive 'run-tunnel')) {
            Start-Process powershell.exe -ArgumentList "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$HomeDir\run-tunnel.ps1`"" -WindowStyle Hidden
        }
    }
    Write-Host "  Waiting for relay and Cloudflare quick tunnel..." -ForegroundColor Gray
    for ($w = 0; $w -lt 10; $w++) {
        Start-Sleep -Seconds 1
        $s = Get-StarStatus
        if ($s.RelayProc -and $s.PublicUrl) { break }
    }
    Clear-ScreenSafe
    return "Production Star running (Cloudflare tunnel URL preserved)."
}

function Stop-ProdStar {
    Write-Host "`n  Stopping Production Star & Tunnel..." -ForegroundColor Yellow
    foreach ($t in 'ESC Relay', 'ESC Tunnel') {
        cmd /c "schtasks /end /tn `"$t`" >nul 2>&1"
    }
    Get-Process powershell -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne $PID } | ForEach-Object {
        try {
            $cmdLine = (Get-CimInstance Win32_Process -Filter "ProcessId = $($_.Id)" -ErrorAction SilentlyContinue).CommandLine
            if ($cmdLine -and ($cmdLine -like "*run-relay.ps1*" -or $cmdLine -like "*run-tunnel.ps1*")) {
                Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
            }
        } catch {}
    }
    Get-EscCloudflared | Stop-Process -Force -ErrorAction SilentlyContinue
    Get-Process esc -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -like "$HomeDir*" } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    Clear-ScreenSafe
    if ($script:TunnelMode -eq 'service') { return "Production Star stopped. The Cloudflared Windows service keeps running." }
    return "Production Star & Tunnel stopped."
}

function Rotate-TunnelUrl {
    Write-Host "`n  Rotating Cloudflare Quick Tunnel URL (restarting cloudflared only)..." -ForegroundColor Cyan
    Restart-ZombieTunnel
    for ($w = 0; $w -lt 12; $w++) {
        Start-Sleep -Seconds 1
        $s = Get-StarStatus
        if ($s.TunnelProc -and $s.PublicUrl -and $s.TunnelState -eq 'ONLINE') {
            return "New Tunnel URL: $($s.PublicUrl)"
        }
    }
    return "Cloudflare tunnel restarted (check status in a moment for the new URL)."
}

function Build-AndDeploy {
    Set-CursorHidden $false
    Clear-ScreenSafe
    Draw-AnimatedHeader -frame 0 -starName "Polaris" -online $true
    Write-BoxBottom DarkCyan
    Write-Host "`n  [1/2] Building release binary (cargo build --release)..." -ForegroundColor Cyan
    Push-Location $ServerDir
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) {
            Write-Host "`n  Build failed! Press any key to return..." -ForegroundColor Red
            [void][Console]::ReadKey($true)
            Clear-ScreenSafe
            return "Build failed."
        }
    } finally {
        Pop-Location
    }
    Write-Host "`n  [2/2] Deploying to $HomeDir via deploy\install.ps1..." -ForegroundColor Cyan
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $DeployDir "install.ps1") -Home_ $HomeDir -Data $DataDir
    Write-Host "`n  Done! Press any key to return to the manager..." -ForegroundColor Green
    [void][Console]::ReadKey($true)
    Clear-ScreenSafe
    return "Built release binary and deployed to Production."
}

function Toggle-DevServer($st) {
    if ($st.DevProc) {
        Stop-Process -Id $st.DevProc.Id -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 400
        return "Dev server on port 7979 stopped."
    }
    $releaseExe = Join-Path $ServerDir "target\release\esc.exe"
    if (-not (Test-Path $releaseExe)) {
        Write-Host "`n  Building release binary first..." -ForegroundColor Cyan
        Push-Location $ServerDir
        & cargo build --release
        Pop-Location
        if ($LASTEXITCODE -ne 0) { return "Build failed; could not start dev server." }
    }
    New-Item -ItemType Directory -Force $DevDir | Out-Null
    $devExe = Join-Path $DevDir "esc-dev.exe"
    $devData = Join-Path $DevDir "esc_data"
    $devLog = Join-Path $DevDir "dev.log"
    Copy-Item $releaseExe $devExe -Force
    Remove-Item $devLog -ErrorAction SilentlyContinue
    $cmdArgs = "/c `"`"$devExe`" serve --data `"$devData`" --listen $DevListen --web `"$WebDir`" > `"$devLog`" 2>&1`""
    Start-Process cmd.exe -ArgumentList $cmdArgs -WindowStyle Hidden
    Start-Sleep -Seconds 1
    Clear-ScreenSafe
    return "Dev server started on http://$DevListen (using live web/ directory)."
}

function Show-Logs {
    Clear-ScreenSafe
    $f = 0
    while (-not [Console]::KeyAvailable) {
        Move-CursorTop
        Draw-AnimatedHeader -frame $f -starName "Polaris" -online $true
        Write-BoxDivider "═" DarkCyan
        Write-BoxRow @( @(" LIVE RELAY LOG ($HomeDir\esc.log)", [ConsoleColor]::Yellow) )
        Write-BoxDivider "─" DarkCyan
        if (Test-Path "$HomeDir\esc.log") {
            $rLines = @(Get-Content "$HomeDir\esc.log" -Tail 10 -ErrorAction SilentlyContinue)
            foreach ($l in $rLines) {
                $rowText = " " + $l
                Write-BoxRow @( ,@($rowText, [ConsoleColor]::Gray) )
            }
            for ($p = $rLines.Count; $p -lt 10; $p++) { Write-BoxRow @() }
        } else {
            Write-BoxRow @( ,@(" (no esc.log file found)", [ConsoleColor]::DarkGray) )
        }

        Write-BoxDivider "═" DarkCyan
        Write-BoxRow @( ,@(" LIVE CLOUDFLARE TUNNEL LOG ($HomeDir\tunnel.log)", [ConsoleColor]::Yellow) )
        Write-BoxDivider "─" DarkCyan
        if (Test-Path "$HomeDir\tunnel.log") {
            $tLines = @(Get-Content "$HomeDir\tunnel.log" -Tail 8 -ErrorAction SilentlyContinue)
            foreach ($l in $tLines) {
                $rowText = " " + $l
                Write-BoxRow @( ,@($rowText, [ConsoleColor]::DarkGray) )
            }
            for ($p = $tLines.Count; $p -lt 8; $p++) { Write-BoxRow @() }
        } else {
            Write-BoxRow @( ,@(" (no tunnel.log file found)", [ConsoleColor]::DarkGray) )
        }
        Write-BoxDivider "─" DarkCyan
        Write-BoxRow @(
            @(" [LIVE STREAM] ", [ConsoleColor]::Green),
            @("Refreshing every 500ms  ·  Press any key to return to main menu", [ConsoleColor]::Gray)
        )
        Write-BoxBottom DarkCyan
        $f++
        Start-Sleep -Milliseconds 500
    }
    [void][Console]::ReadKey($true)
    Clear-ScreenSafe
}

function Run-Checks {
    Set-CursorHidden $false
    Clear-ScreenSafe
    Draw-AnimatedHeader -frame 0 -starName "Polaris" -online $true
    Write-BoxBottom DarkCyan
    Write-Host "`n  → cargo clippy --all-targets -- -D warnings" -ForegroundColor Yellow
    Push-Location $ServerDir
    try {
        & cargo clippy --all-targets -- -D warnings
        $clippyCode = $LASTEXITCODE
        Write-Host "`n  → cargo test --release --lib" -ForegroundColor Yellow
        & cargo test --release --lib
        $testCode = $LASTEXITCODE
        if ($clippyCode -eq 0 -and $testCode -eq 0) {
            Write-Host "`n  [OK] All clippy & unit test checks passed!" -ForegroundColor Green
        } else {
            Write-Host "`n  [ERR] Checks reported errors (clippy=$clippyCode, test=$testCode)." -ForegroundColor Red
        }
    } finally {
        Pop-Location
    }
    Write-Host "`n  Press any key to return to the manager..." -ForegroundColor Gray
    [void][Console]::ReadKey($true)
    Clear-ScreenSafe
    return "Finished running clippy & unit tests."
}

function Draw-UsersBox([string]$targetDataDir = $DataDir, [string]$label = "Production Star ($DataDir)") {
    $logState = Get-EscLogState $targetDataDir
    $users = $logState.Users
    $defaultQuota = [long]$logState.Settings.default_quota

    Write-BoxDivider "═" DarkCyan
    Write-BoxRow @(
        @(" ACCOUNTS ON $($label.ToUpper()) ", [ConsoleColor]::Cyan),
        @("($($users.Count) registered)", [ConsoleColor]::DarkGray)
    )
    Write-BoxDivider "─" DarkCyan

    if ($users.Count -eq 0) {
        Write-BoxRow @( @("   (no accounts registered yet)", [ConsoleColor]::DarkGray) )
        return
    }

    Write-BoxRow @(
        @("  CALLSIGN   ", [ConsoleColor]::Yellow),
        @("NAME                    ", [ConsoleColor]::Yellow),
        @("ROLE             ", [ConsoleColor]::Yellow),
        @("DOOR     ", [ConsoleColor]::Yellow),
        @("DUSTIN   ", [ConsoleColor]::Yellow),
        @("TANK        ", [ConsoleColor]::Yellow),
        @("CREATED", [ConsoleColor]::Yellow)
    )
    Write-BoxDivider "─" DarkCyan

    foreach ($u in $users) {
        $dispName = if ([string]::IsNullOrWhiteSpace($u.Name)) { "-" } else { $u.Name }
        if ($dispName.Length -gt 22) { $dispName = $dispName.Substring(0, 21) + "." }
        $role = if ($u.Disabled) { "Disabled" } elseif ($u.Admin) { "Admin (Tower)" } else { "Member" }
        $roleColor = if ($u.Disabled) { [ConsoleColor]::Red } elseif ($u.Admin) { [ConsoleColor]::Magenta } else { [ConsoleColor]::Gray }
        $door = if ($u.DoorOpen) { "Open" } else { "Closed" }
        $dustin = if ($u.AutoRecycle) { "Auto" } else { "Off" }
        $qBytes = if ($null -ne $u.Quota) { [long]$u.Quota } else { $defaultQuota }
        $qStr = Format-Bytes $qBytes
        $callCol = "  " + $u.Callsign.PadRight(11)

        Write-BoxRow @(
            @($callCol, [ConsoleColor]::Green),
            @($dispName.PadRight(24), [ConsoleColor]::White),
            @($role.PadRight(17), $roleColor),
            @($door.PadRight(9), [ConsoleColor]::Gray),
            @($dustin.PadRight(9), [ConsoleColor]::Cyan),
            @($qStr.PadRight(12), [ConsoleColor]::White),
            @($u.Created, [ConsoleColor]::DarkGray)
        )
    }
}

function Show-Users {
    Clear-ScreenSafe
    $f = 0
    while (-not [Console]::KeyAvailable) {
        Move-CursorTop
        Draw-AnimatedHeader -frame $f -starName "Polaris" -online $true
        Draw-UsersBox -targetDataDir $DataDir -label "Production Star ($DataDir)"
        $devData = Join-Path $DevDir "esc_data"
        if (Test-Path (Join-Path $devData "log")) {
            Draw-UsersBox -targetDataDir $devData -label "Dev Star ($devData)"
        }
        Write-BoxDivider "─" DarkCyan
        Write-BoxRow @(
            @(" Press any key to return to the main menu...", [ConsoleColor]::Gray)
        )
        Write-BoxBottom DarkCyan
        $f++
        Start-Sleep -Milliseconds 160
    }
    [void][Console]::ReadKey($true)
    Clear-ScreenSafe
    return "Displayed relay accounts."
}

function Update-RelayIni([string]$key, [string]$newValue) {
    $iniPath = Join-Path $DataDir "relay.ini"
    if (-not (Test-Path $iniPath)) { return $false }
    $lines = @(Get-Content $iniPath -ErrorAction SilentlyContinue)
    $found = $false
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match "^\s*$key\s*=") {
            $lines[$i] = "$key = $newValue"
            $found = $true
        }
    }
    if (-not $found) { $lines += "$key = $newValue" }
    $lines | Set-Content $iniPath -Encoding UTF8
    return $true
}

function Backup-StarData {
    New-Item -ItemType Directory -Force $BackupDir | Out-Null
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $zipPath = Join-Path $BackupDir "esc_backup_$stamp.zip"
    $stageDir = Join-Path $BackupDir "stage_$stamp"
    New-Item -ItemType Directory -Force $stageDir | Out-Null
    try {
        foreach ($sub in @("relay.ini", "secret", "log", "blobs")) {
            $src = Join-Path $DataDir $sub
            if (Test-Path $src) {
                Copy-Item $src (Join-Path $stageDir $sub) -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
        Compress-Archive -Path "$stageDir\*" -DestinationPath $zipPath -Force
        return "Backup saved: $zipPath"
    } catch {
        return "Backup failed: $($_.Exception.Message)"
    } finally {
        Remove-Item $stageDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Export-PortableBundle {
    New-Item -ItemType Directory -Force $BackupDir | Out-Null
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $zipPath = Join-Path $BackupDir "esc_portable_$stamp.zip"
    $stageDir = Join-Path $BackupDir "portable_$stamp"
    $stageData = Join-Path $stageDir "esc_data"
    New-Item -ItemType Directory -Force $stageDir, $stageData | Out-Null
    try {
        $exeSrc = if (Test-Path "$HomeDir\esc.exe") { "$HomeDir\esc.exe" } else { Join-Path $ServerDir "target\release\esc.exe" }
        if (Test-Path $exeSrc) { Copy-Item $exeSrc (Join-Path $stageDir "esc.exe") -Force }
        Copy-Item $scriptPath (Join-Path $stageDir "esc-manager.cmd") -Force
        foreach ($sub in @("relay.ini", "secret", "log", "blobs")) {
            $src = Join-Path $DataDir $sub
            if (Test-Path $src) {
                Copy-Item $src (Join-Path $stageData $sub) -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
        @"
while (`$true) {
  cmd /c "`"`$PSScriptRoot\esc.exe`" serve --data `"`$PSScriptRoot\esc_data`" --listen $ProdListen --tunnel-log `"`$PSScriptRoot\tunnel.log`" >> `"`$PSScriptRoot\esc.log`" 2>&1"
  Start-Sleep -Seconds 3
}
"@ | Set-Content (Join-Path $stageDir "run-relay.ps1") -Encoding utf8
        Compress-Archive -Path "$stageDir\*" -DestinationPath $zipPath -Force
        return "Portable PC bundle saved: $zipPath"
    } catch {
        return "Portable export failed: $($_.Exception.Message)"
    } finally {
        Remove-Item $stageDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Draw-SettingsBox($st) {
    $cfg = $st.LogState.Settings
    Write-BoxSplitTop DarkCyan
    Write-BoxSplitRow @(
        @(" LOCAL STAR CONFIG (relay.ini)", [ConsoleColor]::Cyan)
    ) @(
        @(" CONTROL TOWER LIVE POLICY", [ConsoleColor]::Cyan)
    ) "║"
    Write-BoxSplitMid DarkCyan

    Write-BoxSplitRow @(
        @(" Star Name : ", [ConsoleColor]::Gray),
        @($st.StarName, [ConsoleColor]::White)
    ) @(
        @(" Sign-up   : ", [ConsoleColor]::Gray),
        @($cfg.signup.ToUpper(), [ConsoleColor]::Yellow),
        @(" (Mbr Inv: $($cfg.member_invites))", [ConsoleColor]::DarkGray)
    )

    Write-BoxSplitRow @(
        @(" Star ID   : ", [ConsoleColor]::Gray),
        @($st.StarId, [ConsoleColor]::DarkCyan)
    ) @(
        @(" Tank Size : ", [ConsoleColor]::Gray),
        @((Format-Bytes ([long]$cfg.default_quota)), [ConsoleColor]::White),
        @(" (+$($cfg.flex_pct)% flex)", [ConsoleColor]::Cyan)
    )

    Write-BoxSplitRow @(
        @(" Listen    : ", [ConsoleColor]::Gray),
        @($st.ProdListen, [ConsoleColor]::White)
    ) @(
        @(" Dustin    : ", [ConsoleColor]::Gray),
        @("$($cfg.cleanup_h)h grace round", [ConsoleColor]::White)
    )

    $tunDesc = switch ($st.TunnelMode) {
        'tailscale' { "Tailscale Funnel ($($st.ConfigUrl))" }
        'named'     { "CF Named Token, run by ESC ($($st.ConfigUrl))" }
        'service'   { "CF Named Tunnel, Windows service ($($st.ConfigUrl))" }
        default     { if ($st.ConfigUrl) { $st.ConfigUrl } else { "Quick Tunnel (auto-heal)" } }
    }
    Write-BoxSplitRow @(
        @(" Tunnel    : ", [ConsoleColor]::Gray),
        @($tunDesc, [ConsoleColor]::Yellow)
    ) @(
        @(" Disk Guard: ", [ConsoleColor]::Gray),
        @("$($cfg.reserve_pct)% / $(Format-Bytes ([long]$cfg.reserve_min))", [ConsoleColor]::White)
    )

    $bootStr = if ($st.AutoStart) { "Enabled (SYSTEM)" } else { "Disabled" }
    $smtpStr = if ($cfg.smtp_host) { "$($cfg.smtp_host):$($cfg.smtp_port)" } else { "Codes only (no SMTP)" }
    Write-BoxSplitRow @(
        @(" Boot Task : ", [ConsoleColor]::Gray),
        @($bootStr, $(if ($st.AutoStart) { [ConsoleColor]::Green } else { [ConsoleColor]::DarkGray }))
    ) @(
        @(" Knocks    : ", [ConsoleColor]::Gray),
        @("$($cfg.req_pending_max) pend / $($cfg.req_daily_max) day", [ConsoleColor]::White)
    )

    $seedStr = if ($st.Seeds) { $st.Seeds } else { "(genesis Star)" }
    Write-BoxSplitRow @(
        @(" Ring Seeds: ", [ConsoleColor]::Gray),
        @($seedStr, [ConsoleColor]::DarkGray)
    ) @(
        @(" Lifeline  : ", [ConsoleColor]::Gray),
        @($smtpStr, [ConsoleColor]::DarkGray)
    )

    Write-BoxSplitEnd DarkCyan

    if ($st.LogState.Invites.Count -gt 0) {
        foreach ($inv in $st.LogState.Invites) {
            $base = if ($st.PublicUrl) { $st.PublicUrl } else { "http://$($st.ProdListen)" }
            Write-BoxRow @(
                @(" Ticket [", [ConsoleColor]::Gray),
                @($inv.Code, [ConsoleColor]::Green),
                @("] ($($inv.Uses) left) : ", [ConsoleColor]::Gray),
                @("$base/#ticket=$($inv.Code)", [ConsoleColor]::Yellow)
            )
        }
    } else {
        Write-BoxRow @(
            @(" Active Invite Tickets : ", [ConsoleColor]::Gray),
            @("None (create Tickets inside Control Tower in the web app)", [ConsoleColor]::DarkGray)
        )
    }

    foreach ($r in $st.LogState.Relays) {
        Write-BoxRow @(
            @(" Ring Star : ", [ConsoleColor]::Gray),
            @("$($r.Name) ", [ConsoleColor]::White),
            @("($($r.Id))  ─►  ", [ConsoleColor]::DarkGray),
            @($r.Url, [ConsoleColor]::Cyan)
        )
    }
}

function Configure-PublicTunnel($st) {
    Set-CursorHidden $false
    Clear-ScreenSafe
    Draw-AnimatedHeader -frame 0 -starName $st.StarName -online ($null -ne $st.RelayProc)
    Write-BoxRow @(
        @(" CONFIGURE PUBLIC HTTPS TUNNEL MODE", [ConsoleColor]::Cyan),
        @(" — Permanent or Ephemeral Public URL", [ConsoleColor]::Gray)
    )
    Write-BoxDivider "─" DarkCyan
    Write-BoxRow @(
        @("   [1] ", [ConsoleColor]::Cyan),
        @("Tailscale Funnel (Recommended — Permanent Free URL: https://<pc>.<tailnet>.ts.net)", [ConsoleColor]::Green)
    )
    Write-BoxRow @(
        @("       Free, no custom domain needed, never changes or expires on sleep/network drops.", [ConsoleColor]::DarkGray)
    )
    Write-BoxRow @(
        @("   [2] ", [ConsoleColor]::Cyan),
        @("Cloudflare Named Tunnel Token (Permanent Custom Domain: https://esc.yourdomain.com)", [ConsoleColor]::Yellow)
    )
    Write-BoxRow @(
        @("       Uses a permanent token from Cloudflare Zero Trust (requires your own domain).", [ConsoleColor]::DarkGray)
    )
    Write-BoxRow @(
        @("   [3] ", [ConsoleColor]::Cyan),
        @("Cloudflare Quick Tunnel (Default — Ephemeral *.trycloudflare.com + Auto-Heal)", [ConsoleColor]::White)
    )
    Write-BoxRow @(
        @("       Zero account setup, but URL changes if the PC sleeps or internet drops.", [ConsoleColor]::DarkGray)
    )
    Write-BoxRow @(
        @("   [4] ", [ConsoleColor]::Cyan),
        @("Cloudflare Named Tunnel run by the installed Windows service (cloudflared .msi)", [ConsoleColor]::Yellow)
    )
    Write-BoxRow @(
        @("       Same permanent domain as [2]; the service carries it, ESC never launches cloudflared.", [ConsoleColor]::DarkGray)
    )
    Write-BoxBottom DarkCyan

    Write-Host "`n   Select tunnel mode [1/2/3/4, or Enter to cancel]: " -ForegroundColor Cyan -NoNewline
    $choice = (Read-Host).Trim()
    $port = ($st.ProdListen -split ':')[-1]

    if ($choice -eq '1') {
        $tsCmd = Get-TailscaleExe
        if (-not $tsCmd) {
            Write-Host "`n   Tailscale is not installed yet. Install now via winget? [Y/n]: " -ForegroundColor Yellow -NoNewline
            $ans = (Read-Host).Trim()
            if ($ans -eq '' -or $ans -match '^[Yy]') {
                Write-Host "   Installing Tailscale..." -ForegroundColor Cyan
                & winget install --id Tailscale.Tailscale -e --accept-package-agreements --accept-source-agreements
                $tsCmd = Get-TailscaleExe
            }
        }
        if (-not $tsCmd) {
            Set-CursorHidden $true
            Clear-ScreenSafe
            return "Tailscale installation not found. Install from https://tailscale.com/download and retry."
        }

        Write-Host "   Checking Tailscale login & MagicDNS certificate status..." -ForegroundColor Cyan
        $tsJson = (& $tsCmd status --json 2>$null) -join "`n"
        if ($tsJson -match '"BackendState"\s*:\s*"NeedsLogin"') {
            Write-Host "   Opening browser for Tailscale sign-in..." -ForegroundColor Yellow
            & $tsCmd login
            $tsJson = (& $tsCmd status --json 2>$null) -join "`n"
        }

        $dnsName = $null
        if ($tsJson -match '"DNSName"\s*:\s*"([^"]+)\."') {
            $dnsName = $Matches[1]
        }
        if (-not $dnsName) {
            Write-Host "   Enter your machine's Tailscale domain (e.g. polaris.tail1234.ts.net): " -ForegroundColor Cyan -NoNewline
            $dnsName = ((Read-Host).Trim() -replace '^https?://', '').TrimEnd('/')
        }
        if (-not $dnsName) {
            Set-CursorHidden $true
            Clear-ScreenSafe
            return "Cancelled Tailscale Funnel setup."
        }

        $permUrl = "https://$dnsName"
        Write-Host "   Enabling Tailscale Funnel on port $port -> $permUrl ..." -ForegroundColor Cyan
        & $tsCmd funnel --bg $port
        if ($LASTEXITCODE -ne 0) {
            Write-Host "`n   [!] Note: If Tailscale printed an HTTPS/Funnel enable link above, open it once in your browser," -ForegroundColor Yellow
            Write-Host "       click Enable Funnel, and press Enter here to retry: " -ForegroundColor Yellow -NoNewline
            [void](Read-Host)
            & $tsCmd funnel --bg $port
        }

        $script:TunnelMode  = "tailscale"
        $script:TunnelToken = ""
        [void](Update-RelayIni "url" $permUrl)
        Save-PathsConfig
        Get-EscCloudflared | Stop-Process -Force -ErrorAction SilentlyContinue
        Stop-TunnelWrappers
        [void](Start-ProdStar)
        Set-CursorHidden $true
        Clear-ScreenSafe
        return "Tailscale Funnel enabled! Permanent URL: $permUrl"
    } elseif ($choice -eq '2') {
        Write-Host "`n   1. In Cloudflare Zero Trust -> Networks -> Tunnels, create a tunnel and copy its Token." -ForegroundColor Gray
        Write-Host "   2. Point its Public Hostname (e.g. esc.yourdomain.com) to http://$($st.ProdListen)" -ForegroundColor Gray
        Write-Host "`n   Paste Cloudflare Tunnel Token (eyJh...): " -ForegroundColor Cyan -NoNewline
        $tok = (Read-Host).Trim()
        # If user pasted 'cloudflared.exe service install eyJh...', extract just the token
        if ($tok -match '([A-Za-z0-9_-]{40,}=*)') { $tok = $Matches[1] }
        if (-not $tok) {
            Set-CursorHidden $true
            Clear-ScreenSafe
            return "Cancelled (no token provided)."
        }
        Write-Host "   Enter your permanent Public HTTPS URL (e.g. https://esc.yourdomain.com): " -ForegroundColor Cyan -NoNewline
        $customUrl = (Read-Host).Trim().TrimEnd('/')
        if ($customUrl -and $customUrl -notmatch '^https?://') { $customUrl = "https://$customUrl" }
        if (-not $customUrl) {
            Set-CursorHidden $true
            Clear-ScreenSafe
            return "Cancelled (no public URL provided)."
        }

        $script:TunnelMode  = "named"
        $script:TunnelToken = $tok
        [void](Update-RelayIni "url" $customUrl)
        Save-PathsConfig
        Restart-ZombieTunnel
        [void](Start-ProdStar)
        Set-CursorHidden $true
        Clear-ScreenSafe
        return "Configured Named Cloudflare Tunnel for $customUrl!"
    } elseif ($choice -eq '3') {
        $tsCmd = Get-TailscaleExe
        if ($tsCmd -and $script:TunnelMode -eq 'tailscale') {
            & $tsCmd funnel --bg off *> $null
        }
        $script:TunnelMode  = "quick"
        $script:TunnelToken = ""
        [void](Update-RelayIni "url" "")
        Save-PathsConfig
        Restart-ZombieTunnel
        [void](Start-ProdStar)
        Set-CursorHidden $true
        Clear-ScreenSafe
        return "Switched to Cloudflare Quick Tunnel (*.trycloudflare.com with auto-heal)."
    } elseif ($choice -eq '4') {
        $svc = Get-CfServiceInfo
        if (-not $svc) {
            Set-CursorHidden $true
            Clear-ScreenSafe
            return "Windows service 'Cloudflared' not found. Install the cloudflared .msi, then run: cloudflared service install <token>"
        }
        $cur = if ($st.ConfigUrl -and $st.ConfigUrl -notmatch 'trycloudflare|\.ts\.net') { $st.ConfigUrl } else { "" }
        $hint = if ($cur) { " [Enter = $cur]" } else { "" }
        Write-Host "`n   The service already holds the tunnel token; its Public Hostname must point to http://$($st.ProdListen)" -ForegroundColor Gray
        Write-Host "   Enter your permanent Public HTTPS URL (e.g. https://esc.yourdomain.com)${hint}: " -ForegroundColor Cyan -NoNewline
        $u = (Read-Host).Trim().TrimEnd('/')
        if (-not $u) { $u = $cur }
        if ($u -and $u -notmatch '^https?://') { $u = "https://$u" }
        if (-not $u) {
            Set-CursorHidden $true
            Clear-ScreenSafe
            return "Cancelled (no public URL provided)."
        }
        $tsCmd = Get-TailscaleExe
        if ($tsCmd -and $script:TunnelMode -eq 'tailscale') { & $tsCmd funnel --bg off *> $null }
        $script:TunnelMode  = "service"
        $script:TunnelToken = ""
        # Retire everything ESC itself used to run for the tunnel: task, wrapper loop, cloudflared, token file.
        cmd /c "schtasks /end /tn `"ESC Tunnel`" >nul 2>&1"
        Stop-TunnelWrappers
        Get-EscCloudflared | Stop-Process -Force -ErrorAction SilentlyContinue
        Set-TunnelTask $false
        Remove-Item "$HomeDir\tunnel.log", "$HomeDir\tunnel.token" -Force -ErrorAction SilentlyContinue
        [void](Update-RelayIni "url" $u)
        Save-PathsConfig
        [void](Start-ProdStar)
        Set-CursorHidden $true
        Clear-ScreenSafe
        return "Tunnel now runs from the Cloudflared Windows service: $u"
    }

    Set-CursorHidden $true
    Clear-ScreenSafe
    return "Tunnel configuration unchanged."
}

function Show-SettingsMenu {
    Clear-ScreenSafe
    $subSel = 0
    $subMsg = ""
    $f = 0
    $st = Get-StarStatus

    while ($true) {
        Move-CursorTop
        Draw-AnimatedHeader -frame $f -starName $st.StarName -online ($null -ne $st.RelayProc)
        Draw-SettingsBox $st
        Write-BoxDivider "═" DarkCyan

        $subItems = @(
            @{ Key = "1"; Label = "Change Star Name in relay.ini (currently: $($st.StarName))" },
            @{ Key = "2"; Label = "Configure Public Tunnel Mode (Tailscale Funnel / Cloudflare Token / Quick)" },
            @{ Key = "3"; Label = "Rotate Cloudflare Quick Tunnel URL (restart cloudflared for a new URL)" },
            @{ Key = "4"; Label = $(if ($st.AutoStart) { "Disable Windows Boot Auto-Start (remove Task Scheduler tasks)" } else { "Enable Windows Boot Auto-Start (install Task Scheduler tasks)" }) },
            @{ Key = "5"; Label = "Create Snapshot Backup ($DataDir -> $BackupDir\*.zip)" },
            @{ Key = "6"; Label = "Export Portable Bundle for Another PC (esc.exe + manager + esc_data)" },
            @{ Key = "7"; Label = "Change esc_data & Relay Folder Paths (Data: $DataDir | Bin: $HomeDir)" },
            @{ Key = "8"; Label = "Copy First Active Invite Ticket Link to Clipboard" },
            @{ Key = "B"; Label = "Back to Main Menu" }
        )

        for ($i = 0; $i -lt $subItems.Count; $i++) {
            $it = $subItems[$i]
            if ($i -eq $subSel) {
                $selBar = " ► [$($it.Key)] $($it.Label)".PadRight($InnerWidth - 2)
                Write-BoxRow @(
                    @(" ", [ConsoleColor]::Black),
                    @($selBar, [ConsoleColor]::Black, [ConsoleColor]::Cyan),
                    @(" ", [ConsoleColor]::Black)
                )
            } else {
                Write-BoxRow @(
                    @("   [", [ConsoleColor]::DarkGray),
                    @($it.Key, [ConsoleColor]::Cyan),
                    @("] ", [ConsoleColor]::DarkGray),
                    @($it.Label, [ConsoleColor]::Gray)
                )
            }
        }

        Write-BoxDivider "─" DarkCyan
        if ($subMsg) {
            Write-BoxRow @(
                @(" * ", [ConsoleColor]::Yellow),
                @($subMsg, [ConsoleColor]::Yellow)
            )
        } else {
            Write-BoxRow @(
                @(" [Up/Down + Enter] ", [ConsoleColor]::Cyan),
                @("or ", [ConsoleColor]::DarkGray),
                @("[1-8] Actions  ·  [B]ack to Main Menu", [ConsoleColor]::Gray)
            )
        }
        Write-BoxBottom DarkCyan

        # Wait up to 140ms for key so the header keeps animating smoothly
        $waited = 0
        while (-not [Console]::KeyAvailable -and $waited -lt 140) {
            Start-Sleep -Milliseconds 35
            $waited += 35
        }
        $f++
        if (-not [Console]::KeyAvailable) { continue }

        $k = [Console]::ReadKey($true)
        $act = $null
        switch ($k.Key) {
            'UpArrow'   { $subSel = ($subSel - 1 + $subItems.Count) % $subItems.Count; continue }
            'DownArrow' { $subSel = ($subSel + 1) % $subItems.Count; continue }
            'Enter'     { $act = $subSel }
            'D1'        { $act = 0 }
            'NumPad1'   { $act = 0 }
            'D2'        { $act = 1 }
            'NumPad2'   { $act = 1 }
            'D3'        { $act = 2 }
            'NumPad3'   { $act = 2 }
            'D4'        { $act = 3 }
            'NumPad4'   { $act = 3 }
            'D5'        { $act = 4 }
            'NumPad5'   { $act = 4 }
            'D6'        { $act = 5 }
            'NumPad6'   { $act = 5 }
            'D7'        { $act = 6 }
            'NumPad7'   { $act = 6 }
            'D8'        { $act = 7 }
            'NumPad8'   { $act = 7 }
            'B'         { $act = 8 }
            'Q'         { $act = 8 }
            'Escape'    { $act = 8 }
        }
        if ($null -eq $act) { continue }
        $subSel = $act

        switch ($act) {
            0 {
                Set-CursorHidden $false
                Write-Host "`n   Enter new Star name (leave blank to cancel): " -ForegroundColor Cyan -NoNewline
                $newName = (Read-Host).Trim()
                Set-CursorHidden $true
                Clear-ScreenSafe
                if ($newName) {
                    if (Update-RelayIni "name" $newName) {
                        $subMsg = "Updated Star name to '$newName' in relay.ini (restart Star to apply)."
                    } else {
                        $subMsg = "Could not update relay.ini."
                    }
                } else {
                    $subMsg = "Cancelled."
                }
                $st = Get-StarStatus
            }
            1 {
                $subMsg = Configure-PublicTunnel $st
                $st = Get-StarStatus
            }
            2 {
                $subMsg = Rotate-TunnelUrl
                Clear-ScreenSafe
                $st = Get-StarStatus
            }
            3 {
                if ($st.AutoStart) {
                    foreach ($t in 'ESC Relay', 'ESC Tunnel') {
                        cmd /c "schtasks /delete /tn `"$t`" /f >nul 2>&1"
                    }
                    $subMsg = "Removed Windows boot auto-start tasks."
                } else {
                    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $DeployDir "install.ps1") -Home_ $HomeDir -Data $DataDir
                    $subMsg = "Installed & started Windows boot auto-start tasks."
                }
                Clear-ScreenSafe
                $st = Get-StarStatus
            }
            4 {
                $subMsg = Backup-StarData
                Clear-ScreenSafe
            }
            5 {
                $subMsg = Export-PortableBundle
                Clear-ScreenSafe
            }
            6 {
                Set-CursorHidden $false
                Write-Host "`n   Current Data Folder (esc_data) : $DataDir" -ForegroundColor Gray
                Write-Host "   Enter new Data Folder path (e.g. C:\esc_data, or Enter to keep): " -ForegroundColor Cyan -NoNewline
                $newData = (Read-Host).Trim()
                Write-Host "   Current Relay Bin Folder       : $HomeDir" -ForegroundColor Gray
                Write-Host "   Enter new Relay Bin Folder (e.g. C:\esc, or Enter to keep): " -ForegroundColor Cyan -NoNewline
                $newHome = (Read-Host).Trim()
                $oldData = $DataDir
                if ($newData) { $script:DataDir = $newData }
                if ($newHome) { $script:HomeDir = $newHome }
                if ($newData -or $newHome) {
                    New-Item -ItemType Directory -Force $script:DataDir, $script:HomeDir | Out-Null
                    if ($newData -and ($oldData -ne $script:DataDir) -and (Test-Path "$oldData\relay.ini") -and -not (Test-Path "$($script:DataDir)\relay.ini")) {
                        Write-Host "   Copy existing data from $oldData to $($script:DataDir)? [Y/n]: " -ForegroundColor Yellow -NoNewline
                        $ans = (Read-Host).Trim()
                        if ($ans -eq '' -or $ans -match '^[Yy]') {
                            foreach ($sub in @("relay.ini", "secret", "log", "blobs")) {
                                $src = Join-Path $oldData $sub
                                if (Test-Path $src) {
                                    Copy-Item $src (Join-Path $script:DataDir $sub) -Recurse -Force -ErrorAction SilentlyContinue
                                }
                            }
                        }
                    }
                    Save-PathsConfig
                    $subMsg = "Saved paths to esc-paths.ini (Data: $($script:DataDir) | Bin: $($script:HomeDir))."
                } else {
                    $subMsg = "No path changes made."
                }
                Set-CursorHidden $true
                Clear-ScreenSafe
                $st = Get-StarStatus
            }
            7 {
                if ($st.LogState.Invites.Count -gt 0) {
                    $inv = $st.LogState.Invites[0]
                    $base = if ($st.PublicUrl) { $st.PublicUrl } else { "http://$($st.ProdListen)" }
                    $link = "$base/#ticket=$($inv.Code)"
                    Set-Clipboard -Value $link -ErrorAction SilentlyContinue
                    $subMsg = "Copied Ticket link: $link"
                } else {
                    $subMsg = "No active Invite Tickets found in Logbook."
                }
            }
            8 {
                Clear-ScreenSafe
                return "Returned from Settings & Control Tower."
            }
        }
    }
}

# ──────────────────────────────────────────────────────────────────────────────
# One-Shot Non-Interactive CLI Modes: .\esc-manager.cmd status | users | settings
# ──────────────────────────────────────────────────────────────────────────────

if ($cliArgs -and ($cliArgs[0] -match 'users|--users|-u')) {
    $st = Get-StarStatus
    Draw-AnimatedHeader -frame 0 -starName $st.StarName -online ($null -ne $st.RelayProc)
    Draw-UsersBox -targetDataDir $DataDir -label "Production Star ($DataDir)"
    Write-BoxBottom DarkCyan
    return
}
if ($cliArgs -and ($cliArgs[0] -match 'settings|--settings|-c')) {
    $st = Get-StarStatus
    Draw-AnimatedHeader -frame 0 -starName $st.StarName -online ($null -ne $st.RelayProc)
    Draw-SettingsBox $st
    Write-BoxBottom DarkCyan
    return
}
if ($cliArgs -and ($cliArgs[0] -match 'status|--status|-s')) {
    $st = Get-StarStatus
    Draw-Dashboard -st $st -selected -1 -flashMsg "One-shot status check complete." -frame 0 -inPlace $false
    return
}

# ──────────────────────────────────────────────────────────────────────────────
# Interactive Animated Event Loop
# ──────────────────────────────────────────────────────────────────────────────

$selected = 0
$menuCount = 13
$flash = ""
$frame = 0
$st = Get-StarStatus

try {
    Play-BootIntro
    Clear-ScreenSafe
    Set-CursorHidden $true

    while ($true) {
        if (($frame % 25) -eq 0 -and $frame -gt 0) {
            $st = Get-StarStatus
        }
        Draw-Dashboard -st $st -selected $selected -flashMsg $flash -frame $frame -inPlace $true

        # Non-blocking wait (140ms per animation tick)
        $waited = 0
        while (-not [Console]::KeyAvailable -and $waited -lt 140) {
            Start-Sleep -Milliseconds 35
            $waited += 35
        }
        $frame++
        if (-not [Console]::KeyAvailable) { continue }

        $key = [Console]::ReadKey($true)
        $action = $null

        switch ($key.Key) {
            'UpArrow'   { $selected = ($selected - 1 + $menuCount) % $menuCount; continue }
            'DownArrow' { $selected = ($selected + 1) % $menuCount; continue }
            'Enter'     { $action = $selected }
            'D1'        { $action = 0 }
            'NumPad1'   { $action = 0 }
            'D2'        { $action = 1 }
            'NumPad2'   { $action = 1 }
            'D3'        { $action = 2 }
            'NumPad3'   { $action = 2 }
            'D4'        { $action = 3 }
            'NumPad4'   { $action = 3 }
            'D5'        { $action = 4 }
            'NumPad5'   { $action = 4 }
            'D6'        { $action = 5 }
            'NumPad6'   { $action = 5 }
            'D7'        { $action = 6 }
            'NumPad7'   { $action = 6 }
            'D8'        { $action = 7 }
            'NumPad8'   { $action = 7 }
            'D9'        { $action = 8 }
            'NumPad9'   { $action = 8 }
            'U'         { $action = 8 }
            'J'         { $action = 9 }
            'S'         { $action = 10 }
            'D0'        { $action = 10 }
            'NumPad0'   { $action = 10 }
            'R'         { $action = 11 }
            'Q'         { $action = 12 }
            'Escape'    { $action = 12 }
        }

        if ($null -eq $action) { continue }
        $selected = $action

        switch ($action) {
            0 { $flash = Start-ProdStar; $st = Get-StarStatus }
            1 { $flash = Stop-ProdStar;  $st = Get-StarStatus }
            2 { $flash = Build-AndDeploy; $st = Get-StarStatus }
            3 { $flash = Toggle-DevServer $st; $st = Get-StarStatus }
            4 {
                if ($st.PublicUrl) {
                    $targetUrl = if ($st.AdminTicket) { "$($st.PublicUrl)/#ticket=$($st.AdminTicket)" } else { $st.PublicUrl }
                    Set-Clipboard -Value $targetUrl -ErrorAction SilentlyContinue
                    Start-Process $targetUrl
                    $flash = "Copied & opened: $targetUrl"
                } else {
                    $flash = "No public Cloudflare tunnel URL is active right now."
                }
            }
            5 {
                $localUrl = if ($st.DevProc) {
                    if ($st.DevTicket) { "http://$DevListen/#ticket=$($st.DevTicket)" } else { "http://$DevListen" }
                } else {
                    if ($st.AdminTicket) { "http://$($st.ProdListen)/#ticket=$($st.AdminTicket)" } else { "http://$($st.ProdListen)" }
                }
                Set-Clipboard -Value $localUrl -ErrorAction SilentlyContinue
                Start-Process $localUrl
                $flash = "Copied & opened: $localUrl"
            }
            6 { Show-Logs; $st = Get-StarStatus }
            7 { $flash = Run-Checks; $st = Get-StarStatus }
            8 { $flash = Show-Users; $st = Get-StarStatus }
            9 { $flash = Join-ExistingRing; $st = Get-StarStatus }
            10 { $flash = Show-SettingsMenu; $st = Get-StarStatus }
            11 {
                $st = Get-StarStatus
                $flash = "Telemetry refreshed at $(Get-Date -Format 'HH:mm:ss')."
            }
            12 {
                Set-CursorHidden $false
                Clear-ScreenSafe
                Write-Host "ESC Star Manager closed." -ForegroundColor Cyan
                break
            }
        }
    }
} finally {
    Set-CursorHidden $false
}
