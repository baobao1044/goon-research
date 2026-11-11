# Shared helpers for the goon desktop scripts (install.ps1, goon-desktop.ps1).
#
# Dot-source this file; it only defines functions and never changes the machine on its own.
# Windows PowerShell 5.1 compatible: no ternary operator, no `??`, no pipeline chain operators.
#
# The state file lives at %LOCALAPPDATA%\goon\desktop.json and records the live desktop
# instances (port + pid per project), so a second launch reuses a running instance instead of
# starting a duplicate server. Set GOON_DESKTOP_STATE to point the scripts somewhere else.

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------------------------
# Where things live
# ---------------------------------------------------------------------------------------------

# Repository root: this file lives in <repo>\scripts\desktop\.
function Resolve-GoonRepoRoot {
    param([Parameter(Mandatory = $true)][string]$ScriptDir)
    return [System.IO.Path]::GetFullPath((Join-Path $ScriptDir '..\..'))
}

# Mirrors crate::config::data_dir(): GOON_DATA_DIR | XDG_DATA_HOME | %USERPROFILE%\.local\share,
# always with a final `goon` segment. Used for the Chromium app profile and the state file's
# sibling log directory, so both agree with what the Rust binary picks for itself.
function Get-GoonDataDir {
    $base = $env:GOON_DATA_DIR
    if ([string]::IsNullOrWhiteSpace($base)) { $base = $env:XDG_DATA_HOME }
    if ([string]::IsNullOrWhiteSpace($base)) {
        $homePath = $env:USERPROFILE
        if ([string]::IsNullOrWhiteSpace($homePath)) { $homePath = $env:HOME }
        if ([string]::IsNullOrWhiteSpace($homePath)) { $homePath = $env:SystemDrive + '\' }
        $base = Join-Path $homePath '.local\share'
    }
    return [System.IO.Path]::GetFullPath((Join-Path $base 'goon'))
}

# Desktop state file. Deliberately per-user and machine-local (LOCALAPPDATA, not APPDATA):
# it names PIDs, which are only meaningful on this machine.
function Get-GoonDesktopStatePath {
    $override = $env:GOON_DESKTOP_STATE
    if (-not [string]::IsNullOrWhiteSpace($override)) { return [System.IO.Path]::GetFullPath($override) }
    $root = $env:LOCALAPPDATA
    if ([string]::IsNullOrWhiteSpace($root)) { $root = $env:APPDATA }
    if ([string]::IsNullOrWhiteSpace($root)) {
        $homePath = $env:USERPROFILE
        if ([string]::IsNullOrWhiteSpace($homePath)) { $homePath = $env:HOME }
        $root = Join-Path $homePath '.local\state'
    }
    return [System.IO.Path]::GetFullPath((Join-Path (Join-Path $root 'goon') 'desktop.json'))
}

# Project paths are compared case-insensitively on Windows and without a trailing separator, so
# "C:\Repo" and "c:\repo\" are the same project (and therefore the same reusable instance).
function ConvertTo-GoonProjectKey {
    param([Parameter(Mandatory = $true)][string]$Path)
    $full = [System.IO.Path]::GetFullPath($Path)
    $trimmed = $full.TrimEnd('\', '/')
    if ([string]::IsNullOrEmpty($trimmed)) { $trimmed = $full }
    return $trimmed.ToLowerInvariant()
}

# ---------------------------------------------------------------------------------------------
# Locating goon.exe
# ---------------------------------------------------------------------------------------------

# Search order: explicit -GoonExe, PATH, cargo release build, cargo debug build, ~/.local/bin.
# Returns a hashtable with Path/Source, or $null and fills SearchOrder/SearchLog for the message.
function Resolve-GoonExe {
    param(
        [string]$Explicit,
        [Parameter(Mandatory = $true)][string]$RepoRoot
    )

    $candidates = New-Object System.Collections.ArrayList
    $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase)

    $add = {
        param([string]$Path, [string]$Source)
        if ([string]::IsNullOrWhiteSpace($Path)) { return }
        $full = $Path
        try { $full = [System.IO.Path]::GetFullPath($Path) } catch { }
        if (-not $seen.Add($full)) { return }
        [void]$candidates.Add([pscustomobject]@{ path = $full; source = $Source })
    }

    # An explicit -GoonExe is a promise, not a hint: when it does not exist the caller gets an
    # error instead of silently launching a different binary.
    $explicitMissing = $null
    if (-not [string]::IsNullOrWhiteSpace($Explicit)) {
        $explicitFull = $Explicit
        try { $explicitFull = [System.IO.Path]::GetFullPath($Explicit) } catch { }
        if (Test-Path -LiteralPath $explicitFull -PathType Leaf) {
            $explicitEntry = @([pscustomobject]@{ path = $explicitFull; source = '-GoonExe' })
            return [pscustomobject]@{ path = $explicitFull; source = '-GoonExe'; searched = $explicitEntry; explicitMissing = $null }
        }
        $explicitMissing = $explicitFull
        & $add $explicitFull '-GoonExe (does not exist)'
        return [pscustomobject]@{ path = $null; source = $null; searched = $candidates; explicitMissing = $explicitMissing }
    }

    $onPath = $null
    try { $onPath = Get-Command -Name 'goon.exe' -CommandType Application -ErrorAction Stop } catch { $onPath = $null }
    if ($onPath) { & $add $onPath.Source 'PATH' }

    & $add (Join-Path $RepoRoot 'target\release\goon.exe') 'target\release'
    & $add (Join-Path $RepoRoot 'target\debug\goon.exe') 'target\debug'

    $homePath = $env:USERPROFILE
    if ([string]::IsNullOrWhiteSpace($homePath)) { $homePath = $env:HOME }
    if (-not [string]::IsNullOrWhiteSpace($homePath)) {
        & $add (Join-Path $homePath '.local\bin\goon.exe') '%USERPROFILE%\.local\bin'
    }

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate.path -PathType Leaf) {
            return [pscustomobject]@{ path = $candidate.path; source = $candidate.source; searched = $candidates; explicitMissing = $null }
        }
    }
    return [pscustomobject]@{ path = $null; source = $null; searched = $candidates; explicitMissing = $explicitMissing }
}

# One readable line listing every place that was searched, for the "not found" error.
function Format-GoonExeSearch {
    param([Parameter(Mandatory = $true)]$Result)
    $lines = New-Object System.Collections.ArrayList
    foreach ($candidate in $Result.searched) {
        [void]$lines.Add("  - $($candidate.source): $($candidate.path)")
    }
    return ($lines -join [Environment]::NewLine)
}

# ---------------------------------------------------------------------------------------------
# State file
# ---------------------------------------------------------------------------------------------

function Read-GoonDesktopState {
    param([Parameter(Mandatory = $true)][string]$Path)

    $state = [pscustomobject]@{ schema = 1; updatedAt = $null; instances = @() }
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $state }

    try {
        $raw = Get-Content -LiteralPath $Path -Raw -Encoding UTF8
        if ([string]::IsNullOrWhiteSpace($raw)) { return $state }
        $data = $raw | ConvertFrom-Json
        if ($null -eq $data) { return $state }
        $instances = @()
        $hasInstances = ($data.PSObject.Properties.Name -contains 'instances')
        if ($hasInstances -and $null -ne $data.instances) {
            foreach ($entry in @($data.instances)) {
                if ($null -ne $entry -and ($entry.PSObject.Properties.Name -contains 'port')) { $instances += $entry }
            }
        }
        $state.instances = @($instances)
    } catch {
        # A corrupt state file must never block launching: warn once and start from empty.
        Write-Warning "goon desktop: ignoring unreadable state file '$Path' ($($_.Exception.Message))"
    }
    return $state
}

# Writes the state file atomically (temp + replace). When no instance is left the file is removed
# instead of leaving an empty stub, so "no instance running" and "no state file" stay equivalent.
function Save-GoonDesktopState {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        $Instances
    )

    $list = @()
    foreach ($entry in @($Instances)) { if ($null -ne $entry) { $list += $entry } }

    $dir = Split-Path -Parent $Path
    if ([string]::IsNullOrWhiteSpace($dir)) { $dir = '.' }

    if ($list.Count -eq 0) {
        if (Test-Path -LiteralPath $Path -PathType Leaf) { Remove-Item -LiteralPath $Path -Force }
        return $false
    }

    if (-not (Test-Path -LiteralPath $dir -PathType Container)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }

    $payload = [pscustomobject][ordered]@{
        schema    = 1
        updatedAt = (Get-Date).ToUniversalTime().ToString('o')
        instances = $list
    }
    $json = ConvertTo-Json -InputObject $payload -Depth 8
    $tmp = $Path + '.tmp'
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($tmp, $json, $utf8NoBom)
    Move-Item -LiteralPath $tmp -Destination $Path -Force
    return $true
}

# ---------------------------------------------------------------------------------------------
# Liveness
# ---------------------------------------------------------------------------------------------

function Test-GoonPid {
    param($Id)
    if ($null -eq $Id) { return $false }
    $pidValue = 0
    if (-not [int]::TryParse([string]$Id, [ref]$pidValue)) { return $false }
    if ($pidValue -le 0) { return $false }
    try {
        $process = Get-Process -Id $pidValue -ErrorAction Stop
        return (-not $process.HasExited)
    } catch {
        return $false
    }
}

# True only when something accepts a loopback TCP connection on that port.
function Test-GoonPort {
    param(
        [int]$Port,
        [int]$TimeoutMs = 700
    )
    if ($Port -le 0 -or $Port -gt 65535) { return $false }
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $async = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
        if (-not $async.AsyncWaitHandle.WaitOne($TimeoutMs, $false)) { return $false }
        $client.EndConnect($async)
        return $true
    } catch {
        return $false
    } finally {
        try { $client.Close() } catch { }
    }
}

# A goon desktop instance answers GET /api/meta with JSON that carries a "version" field, so the
# launcher can tell its own server apart from an unrelated process that happens to hold the port.
function Test-GoonServer {
    param(
        [int]$Port,
        [int]$TimeoutMs = 1500
    )
    if (-not (Test-GoonPort -Port $Port -TimeoutMs $TimeoutMs)) { return $false }

    $client = New-Object System.Net.Sockets.TcpClient
    $reader = $null
    try {
        $client.Connect('127.0.0.1', $Port)
        $stream = $client.GetStream()
        $stream.ReadTimeout = $TimeoutMs
        $stream.WriteTimeout = $TimeoutMs
        $request = "GET /api/meta HTTP/1.1`r`nHost: 127.0.0.1`r`nConnection: close`r`n`r`n"
        $bytes = [System.Text.Encoding]::ASCII.GetBytes($request)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush()
        $reader = New-Object System.IO.StreamReader($stream)
        $response = $reader.ReadToEnd()
        return ($response -match '"version"')
    } catch {
        return $false
    } finally {
        if ($null -ne $reader) { try { $reader.Dispose() } catch { } }
        try { $client.Close() } catch { }
    }
}

# Finds the PID listening on a loopback port, so a lost/stale state file can still be recovered
# (and -Stop still works when the recorded PID is missing). 0 means "unknown".
function Get-GoonListenerPid {
    param([int]$Port)
    if ($Port -le 0) { return 0 }

    try {
        $connection = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop | Select-Object -First 1
        if ($null -ne $connection -and $connection.OwningProcess -gt 0) { return [int]$connection.OwningProcess }
    } catch {
        # Get-NetTCPConnection is absent on some SKUs; fall through to netstat.
    }

    try {
        $lines = & netstat.exe -ano -p tcp 2>$null
        foreach ($line in @($lines)) {
            if ($line -match '^\s*TCP\s+\S+:(\d+)\s+\S+\s+LISTENING\s+(\d+)\s*$') {
                if ([int]$matches[1] -eq $Port) { return [int]$matches[2] }
            }
        }
    } catch { }
    return 0
}

# ---------------------------------------------------------------------------------------------
# Opening the window
# ---------------------------------------------------------------------------------------------

# Same candidate order as src/web/mod.rs::browser_candidates() on Windows.
function Get-GoonBrowserCandidates {
    $candidates = New-Object System.Collections.ArrayList
    foreach ($root in @($env:PROGRAMFILES, ${env:PROGRAMFILES(X86)}, $env:LOCALAPPDATA)) {
        if ([string]::IsNullOrWhiteSpace($root)) { continue }
        [void]$candidates.Add((Join-Path $root 'Google\Chrome\Application\chrome.exe'))
        [void]$candidates.Add((Join-Path $root 'Microsoft\Edge\Application\msedge.exe'))
    }
    return @($candidates)
}

# Opens `url` as a Chromium app window (no address bar, own taskbar entry). Falls back to the
# default browser when neither Chrome nor Edge is installed. Returns the launcher used, or $null.
function Open-GoonBrowserWindow {
    param(
        [Parameter(Mandatory = $true)][string]$Url,
        [string]$ProfileDir
    )

    $arguments = @("--app=$Url", '--window-size=1280,860', '--no-first-run', '--no-default-browser-check')
    if (-not [string]::IsNullOrWhiteSpace($ProfileDir)) {
        try {
            if (-not (Test-Path -LiteralPath $ProfileDir -PathType Container)) {
                New-Item -ItemType Directory -Path $ProfileDir -Force | Out-Null
            }
            $arguments += "--user-data-dir=$ProfileDir"
        } catch {
            # A read-only profile directory only costs session isolation, not the window.
        }
    }

    foreach ($browser in Get-GoonBrowserCandidates) {
        if (-not (Test-Path -LiteralPath $browser -PathType Leaf)) { continue }
        try {
            Start-Process -FilePath $browser -ArgumentList $arguments -ErrorAction Stop | Out-Null
            return $browser
        } catch {
            continue
        }
    }

    try {
        Start-Process -FilePath $Url -ErrorAction Stop | Out-Null
        return 'default-browser'
    } catch {
        return $null
    }
}

# ---------------------------------------------------------------------------------------------
# Console output
# ---------------------------------------------------------------------------------------------

function Write-GoonLine {
    param([string]$Message = '')
    Write-Host $Message
}

function Write-GoonInfo {
    param([string]$Message)
    Write-Host "goon desktop: $Message"
}

function Write-GoonWarn {
    param([string]$Message)
    Write-Host "goon desktop: warning: $Message" -ForegroundColor Yellow
}

function Write-GoonError {
    param([string]$Message)
    Write-Host "goon desktop: error: $Message" -ForegroundColor Red
}

# Collapses an exception into one printable line and keeps the innermost cause visible.
function Format-GoonError {
    param($ErrorRecord)
    $message = [string]$ErrorRecord
    if ($null -ne $ErrorRecord -and $null -ne $ErrorRecord.Exception) { $message = $ErrorRecord.Exception.Message }
    return ($message -replace '\s+', ' ').Trim()
}

# Last few lines of a log file, for when goon refused to start and the reason is in its stderr.
function Get-GoonLogTail {
    param(
        [string]$Path,
        [int]$Lines = 12
    )
    if ([string]::IsNullOrWhiteSpace($Path)) { return @() }
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return @() }
    try {
        $all = @(Get-Content -LiteralPath $Path -Encoding UTF8 -ErrorAction Stop)
        if ($all.Count -eq 0) { return @() }
        $start = [Math]::Max(0, $all.Count - $Lines)
        return @($all[$start..($all.Count - 1)])
    } catch {
        return @()
    }
}
