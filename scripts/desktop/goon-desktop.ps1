<#
.SYNOPSIS
    Launches (or reuses) the goon desktop window for one project directory.

.DESCRIPTION
    `goon web` serves the desktop UI on a loopback port and opens a Chromium app window for it.
    This launcher adds the parts a shortcut needs to behave like an application:

      * reuse - if a live goon desktop instance is already serving the same project, the window is
        re-opened against that instance instead of starting a second server;
      * state - the port and PID of every instance are recorded in
        %LOCALAPPDATA%\goon\desktop.json (override with -StateFile or GOON_DESKTOP_STATE);
      * lifecycle - -List shows the recorded instances, -Stop terminates them and removes the
        state file.

    A recorded instance whose process is gone (stale PID) is detected, reported and pruned instead
    of being reused. The launcher never needs administrator rights: everything it touches is in
    the current user's profile.

.PARAMETER ProjectDir
    Project workspace to open. Defaults to GOON_DESKTOP_PROJECT when set (bin\goon-desktop.cmd
    passes it that way), otherwise the current directory. Must already exist.

.PARAMETER Port
    Fixed loopback port for the instance. Default 0 lets the server pick a free ephemeral port.
    A fixed port is only reused by an instance that serves that same port.

.PARAMETER Stop
    Stop the running instance(s) recorded in the state file and delete the file. With -ProjectDir
    or -Port, only matching instances are stopped. Reports politely when nothing is running.

.PARAMETER List
    Print the recorded instances with their live/stale status and exit. Changes nothing.

.PARAMETER NoOpen
    Start or reuse the server without opening a window (used by scripts/CI). The URL is printed.

.PARAMETER GoonExe
    Explicit path to goon.exe, overriding the normal search (PATH, target\release, target\debug,
    %USERPROFILE%\.local\bin).

.PARAMETER StateFile
    Alternate state file path instead of %LOCALAPPDATA%\goon\desktop.json (tests, multiple users).

.PARAMETER StartupTimeoutSec
    How long to wait for the instance to report its listening URL. Default 30 seconds.

.PARAMETER Help
    Show usage and exit.

.EXAMPLE
    .\goon-desktop.ps1 -ProjectDir C:\work\app
    Opens the desktop window for C:\work\app, reusing the instance if it is already running.

.EXAMPLE
    .\goon-desktop.ps1 -ProjectDir C:\work\app -Port 7788
    Forces a fixed port, which is handy behind a firewall rule or a bookmark.

.EXAMPLE
    .\goon-desktop.ps1 -Stop
    Stops every recorded instance and removes the state file.

.EXAMPLE
    .\goon-desktop.ps1 -List
    Shows which instances are recorded, with live or stale status.

.NOTES
    Windows PowerShell 5.1 (the version this was developed and tested against). No administrator
    rights required. See docs/DESKTOP.md for the full guide.
#>
[CmdletBinding()]
param(
    # Positional so that "goon-desktop.ps1 C:\work\app -Port 7788" works the same as
    # "goon-desktop.ps1 -ProjectDir C:\work\app -Port 7788" (bin\goon-desktop.cmd forwards verbatim).
    [Parameter(Position = 0)][string]$ProjectDir,
    [int]$Port = 0,
    [switch]$Stop,
    [switch]$List,
    [switch]$NoOpen,
    [string]$GoonExe,
    [string]$StateFile,
    [ValidateRange(1, 600)][int]$StartupTimeoutSec = 30,
    [Alias('h')][switch]$Help
)

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'GoonDesktop.Common.ps1')

$repoRoot = Resolve-GoonRepoRoot -ScriptDir $PSScriptRoot

function Show-GoonDesktopUsage {
    Write-Host @'
goon desktop launcher

Usage:
  goon-desktop.ps1 [-ProjectDir <dir>] [-Port <n>] [-NoOpen] [-GoonExe <path>]
  goon-desktop.ps1 -List
  goon-desktop.ps1 -Stop [-ProjectDir <dir>] [-Port <n>]

  -ProjectDir <dir>   project to open (default: %GOON_DESKTOP_PROJECT% or the current directory)
  -Port <n>           fixed loopback port (default 0: the server picks a free one)
  -NoOpen             start/reuse the server without opening a window
  -GoonExe <path>     explicit goon.exe (default: PATH, target\release, target\debug, ~\.local\bin)
  -StateFile <path>   state file (default: %LOCALAPPDATA%\goon\desktop.json)
  -List               list recorded instances and their status
  -Stop               stop the recorded instance(s) and delete the state file
  -Help               show this help

Re-running for the same project re-opens the window on the instance that is already running
instead of starting a second server. See docs/DESKTOP.md.
'@
}

if ($Help) { Show-GoonDesktopUsage; exit 0 }

$statePath = Get-GoonDesktopStatePath
if (-not [string]::IsNullOrWhiteSpace($StateFile)) { $statePath = [System.IO.Path]::GetFullPath($StateFile) }

$state = Read-GoonDesktopState -Path $statePath
$instances = @($state.instances)

# Entries written by an older/edited state file may lack projectKey or port; normalise here so the
# rest of the script can trust those fields.
function Get-GoonEntryPort {
    param($Entry)
    $value = 0
    if ($null -eq $Entry) { return 0 }
    if ([int]::TryParse([string]$Entry.port, [ref]$value) -and $value -gt 0 -and $value -le 65535) { return $value }
    return 0
}

function Get-GoonEntryKey {
    param($Entry)
    if ($null -eq $Entry) { return $null }
    $key = [string]$Entry.projectKey
    if (-not [string]::IsNullOrWhiteSpace($key)) { return $key }
    $dir = [string]$Entry.projectDir
    if ([string]::IsNullOrWhiteSpace($dir)) { return $null }
    try { return (ConvertTo-GoonProjectKey -Path $dir) } catch { return $null }
}

function Get-GoonEntryPid {
    param($Entry)
    $value = 0
    if ($null -eq $Entry) { return 0 }
    if ([int]::TryParse([string]$Entry.pid, [ref]$value) -and $value -gt 0) { return $value }
    return 0
}

function Test-GoonEntryLive {
    param($Entry)
    $entryPort = Get-GoonEntryPort -Entry $Entry
    if ($entryPort -le 0) { return $false }
    return (Test-GoonServer -Port $entryPort)
}

# Terminates one instance, waits for the process to disappear and the port to be released.
# Returns the outcome so -Stop can report each entry honestly.
function Stop-GoonRecordedInstance {
    param(
        [int]$ProcessId,
        [int]$EntryPort
    )

    if ($ProcessId -le 0) {
        if ($EntryPort -gt 0) { $ProcessId = Get-GoonListenerPid -Port $EntryPort }
        if ($ProcessId -le 0) { return 'unknown' }
    }

    if (-not (Test-GoonPid -Id $ProcessId)) { return 'stale' }

    try {
        Stop-Process -Id $ProcessId -Force -ErrorAction Stop
    } catch {
        return "failed: $(Format-GoonError $_)"
    }

    $deadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $deadline) {
        if (-not (Test-GoonPid -Id $ProcessId)) { break }
        Start-Sleep -Milliseconds 150
    }

    if (Test-GoonPid -Id $ProcessId) { return "failed: process $ProcessId did not exit" }

    if ($EntryPort -gt 0) {
        $deadline = (Get-Date).AddSeconds(5)
        while ((Get-Date) -lt $deadline) {
            if (-not (Test-GoonPort -Port $EntryPort)) { break }
            Start-Sleep -Milliseconds 150
        }
    }
    return 'stopped'
}

function Get-GoonProjectSlug {
    param([string]$Key)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($Key))
    } finally {
        $sha.Dispose()
    }
    $parts = @()
    for ($i = 0; $i -lt 4; $i++) { $parts += $bytes[$i].ToString('x2') }
    return ($parts -join '')
}

# ---------------------------------------------------------------------------------------------
# -List
# ---------------------------------------------------------------------------------------------

if ($List) {
    if ($instances.Count -eq 0) {
        Write-GoonInfo "no instance recorded in $statePath"
        exit 0
    }
    Write-GoonLine "recorded instances ($statePath):"
    foreach ($entry in $instances) {
        $entryPort = Get-GoonEntryPort -Entry $entry
        $entryPid = Get-GoonEntryPid -Entry $entry
        if (Test-GoonEntryLive -Entry $entry) {
            $status = 'running'
        } elseif (Test-GoonPid -Id $entryPid) {
            $status = 'stale (process up, port closed)'
        } else {
            $status = 'stale (process gone)'
        }
        $entryUrl = [string]$entry.url
        if ([string]::IsNullOrWhiteSpace($entryUrl) -and $entryPort -gt 0) { $entryUrl = "http://127.0.0.1:$entryPort/" }
        Write-GoonLine ("  [{0}] port {1} pid {2}  {3}  {4}" -f $status, $entryPort, $entryPid, [string]$entry.projectDir, $entryUrl)
    }
    exit 0
}

# ---------------------------------------------------------------------------------------------
# -Stop
# ---------------------------------------------------------------------------------------------

if ($Stop) {
    $stopKey = $null
    if (-not [string]::IsNullOrWhiteSpace($ProjectDir)) {
        $stopKey = ConvertTo-GoonProjectKey -Path ([System.IO.Path]::GetFullPath($ProjectDir))
    }

    $targets = @()
    foreach ($entry in $instances) {
        if ($null -ne $stopKey) {
            if ((Get-GoonEntryKey -Entry $entry) -ne $stopKey) { continue }
        }
        if ($Port -gt 0 -and (Get-GoonEntryPort -Entry $entry) -ne $Port) { continue }
        $targets += $entry
    }

    if ($targets.Count -eq 0) {
        Write-GoonInfo "no running instance recorded in $statePath - nothing to stop."
        exit 0
    }

    $keep = @()
    foreach ($entry in $instances) {
        $isTarget = $false
        foreach ($candidate in $targets) { if ([object]::ReferenceEquals($entry, $candidate)) { $isTarget = $true; break } }
        if (-not $isTarget) { $keep += $entry }
    }

    $failed = 0
    foreach ($entry in $targets) {
        $entryPort = Get-GoonEntryPort -Entry $entry
        $entryPid = Get-GoonEntryPid -Entry $entry
        $label = "port $entryPort / pid $entryPid ($([string]$entry.projectDir))"
        $outcome = Stop-GoonRecordedInstance -ProcessId $entryPid -EntryPort $entryPort
        if ($outcome -eq 'stopped') {
            Write-GoonInfo "stopped $label"
        } elseif ($outcome -eq 'stale') {
            Write-GoonInfo "cleaned stale entry $label (process was already gone)"
        } elseif ($outcome -eq 'unknown') {
            Write-GoonWarn "could not identify the process behind $label; removed it from the state file only"
        } else {
            $failed++
            Write-GoonError "could not stop $label - $outcome"
            $keep += $entry
        }
    }

    $wrote = Save-GoonDesktopState -Path $statePath -Instances $keep
    if ($wrote) {
        Write-GoonInfo "state updated: $statePath"
    } else {
        Write-GoonInfo "state file removed (no instance left): $statePath"
    }
    if ($failed -gt 0) { exit 1 }
    exit 0
}

# ---------------------------------------------------------------------------------------------
# Normal launch: reuse a live instance, otherwise start one
# ---------------------------------------------------------------------------------------------

$projectPath = $ProjectDir
if ([string]::IsNullOrWhiteSpace($projectPath)) {
    if (-not [string]::IsNullOrWhiteSpace($env:GOON_DESKTOP_PROJECT)) { $projectPath = $env:GOON_DESKTOP_PROJECT }
    else { $projectPath = (Get-Location).Path }
}

try {
    $projectPath = [System.IO.Path]::GetFullPath($projectPath)
} catch {
    Write-GoonError "invalid project directory '$projectPath'"
    exit 1
}

if (-not (Test-Path -LiteralPath $projectPath -PathType Container)) {
    Write-GoonError "project directory does not exist: $projectPath"
    exit 1
}

$projectKey = ConvertTo-GoonProjectKey -Path $projectPath
$profileDir = Join-Path (Get-GoonDataDir) 'desktop-profile'

# 1. Reuse: same project (and same port when one was requested) and the port still answers like a
#    goon desktop server. A stale PID alone never blocks a fresh launch.
$liveEntry = $null
foreach ($entry in $instances) {
    if ((Get-GoonEntryKey -Entry $entry) -ne $projectKey) { continue }
    $entryPort = Get-GoonEntryPort -Entry $entry
    if ($entryPort -le 0) { continue }
    if ($Port -gt 0 -and $entryPort -ne $Port) { continue }
    if (-not (Test-GoonServer -Port $entryPort)) { continue }
    $liveEntry = $entry
    break
}

if ($null -ne $liveEntry) {
    $livePort = Get-GoonEntryPort -Entry $liveEntry
    $liveUrl = [string]$liveEntry.url
    if ([string]::IsNullOrWhiteSpace($liveUrl)) { $liveUrl = "http://127.0.0.1:$livePort/" }
    $livePid = Get-GoonEntryPid -Entry $liveEntry
    $adoptPid = $livePid

    if (-not (Test-GoonPid -Id $livePid)) {
        # The server answers but the recorded PID is gone (state edited, PID reused, or a previous
        # launcher died before writing it): ask Windows who owns the listening socket.
        $adoptPid = Get-GoonListenerPid -Port $livePort
    }

    Write-GoonInfo "reusing the running instance for '$projectPath' - $liveUrl (pid $adoptPid)"

    if ($adoptPid -ne $livePid -and $adoptPid -gt 0) {
        $liveEntry | Add-Member -NotePropertyName pid -NotePropertyValue $adoptPid -Force
        Save-GoonDesktopState -Path $statePath -Instances $instances | Out-Null
        Write-GoonInfo "recovered and recorded the listener pid ($adoptPid)"
    }

    if ($NoOpen) {
        Write-GoonInfo "no window requested (-NoOpen); open $liveUrl"
        exit 0
    }

    $opened = Open-GoonBrowserWindow -Url $liveUrl -ProfileDir $profileDir
    if ($null -eq $opened) {
        Write-GoonWarn "could not open a window; open $liveUrl manually"
        exit 1
    }
    if ($opened -eq 'default-browser') {
        Write-GoonInfo "no Chrome/Edge found; opened $liveUrl in the default browser (no app window)"
    } else {
        Write-GoonInfo "opened a window with $opened"
    }
    exit 0
}

# Prune dead entries for this project before writing a new one, so the state file only ever
# describes instances that answer.
$keep = @()
$pruned = 0
foreach ($entry in $instances) {
    if ((Get-GoonEntryKey -Entry $entry) -eq $projectKey -and $Port -le 0) {
        if (-not (Test-GoonEntryLive -Entry $entry)) { $pruned++; continue }
    }
    if ((Get-GoonEntryKey -Entry $entry) -eq $projectKey -and $Port -gt 0 -and (Get-GoonEntryPort -Entry $entry) -eq $Port) {
        if (-not (Test-GoonEntryLive -Entry $entry)) { $pruned++; continue }
    }
    $keep += $entry
}
if ($pruned -gt 0) { Write-GoonInfo "pruned $pruned stale state entr$(if ($pruned -eq 1) { 'y' } else { 'ies' })" }

# 2. A fixed port that already serves goon without being recorded (state file lost): adopt it
#    rather than starting a server that would immediately fail to bind.
if ($Port -gt 0 -and (Test-GoonServer -Port $Port)) {
    $adoptPid = Get-GoonListenerPid -Port $Port
    $adoptUrl = "http://127.0.0.1:$Port/"
    $entry = [pscustomobject][ordered]@{
        projectDir = $projectPath
        projectKey = $projectKey
        port       = $Port
        pid        = $adoptPid
        url        = $adoptUrl
        goonExe    = $null
        logFile    = $null
        startedAt  = (Get-Date).ToUniversalTime().ToString('o')
        adopted    = $true
    }
    $keep += $entry
    Save-GoonDesktopState -Path $statePath -Instances $keep | Out-Null
    Write-GoonInfo "port $Port already serves goon desktop - adopted it (pid $adoptPid)"
    if ($NoOpen) { Write-GoonInfo "open $adoptUrl"; exit 0 }
    $opened = Open-GoonBrowserWindow -Url $adoptUrl -ProfileDir $profileDir
    if ($null -eq $opened) { Write-GoonWarn "could not open a window; open $adoptUrl manually"; exit 1 }
    Write-GoonInfo "opened a window with $opened"
    exit 0
}

# 3. Start a new instance.
$resolved = Resolve-GoonExe -Explicit $GoonExe -RepoRoot $repoRoot
if ([string]::IsNullOrWhiteSpace($resolved.path)) {
    if (-not [string]::IsNullOrWhiteSpace($resolved.explicitMissing)) {
        Write-GoonError "-GoonExe does not exist: $($resolved.explicitMissing)"
        exit 1
    }
    Write-GoonError 'goon.exe was not found.'
    Write-GoonLine 'searched:'
    Write-GoonLine (Format-GoonExeSearch -Result $resolved)
    Write-GoonLine ''
    Write-GoonLine 'Fix one of these, then run again:'
    Write-GoonLine '  - build the binary:      cargo build --release   (from the goon-cli checkout)'
    Write-GoonLine '  - install the npm package: npm install -g @binxgodteli/goon-cli'
    Write-GoonLine '  - point at a binary:     -GoonExe C:\path\to\goon.exe'
    exit 1
}

$stateDir = Split-Path -Parent $statePath
if ([string]::IsNullOrWhiteSpace($stateDir)) { $stateDir = '.' }
$logDir = Join-Path $stateDir 'logs'
if (-not (Test-Path -LiteralPath $logDir -PathType Container)) { New-Item -ItemType Directory -Path $logDir -Force | Out-Null }
$slug = Get-GoonProjectSlug -Key $projectKey
$errLog = Join-Path $logDir "desktop-$slug.err.log"
$outLog = Join-Path $logDir "desktop-$slug.out.log"

$arguments = @('web', '--port', [string]$Port, '--cwd', $projectPath)
if ($NoOpen) { $arguments += '--no-open' }

Write-GoonInfo "starting '$($resolved.path)' (found via $($resolved.source)) for '$projectPath'"

$started = $null
try {
    # Redirecting the child's streams keeps the console quiet and lets the launcher read the
    # listening URL that `goon web` prints on stderr. Both streams go to the state directory so a
    # failed start can be diagnosed after the fact.
    $started = Start-Process -FilePath $resolved.path `
        -ArgumentList $arguments `
        -WorkingDirectory $projectPath `
        -RedirectStandardError $errLog `
        -RedirectStandardOutput $outLog `
        -PassThru -ErrorAction Stop
} catch {
    Write-GoonError "could not start '$($resolved.path)': $(Format-GoonError $_)"
    exit 1
}

$deadline = (Get-Date).AddSeconds($StartupTimeoutSec)
$actualPort = 0
$actualUrl = $null
$exited = $false

while ((Get-Date) -lt $deadline) {
    if ($started.HasExited) { $exited = $true; break }
    $logText = ''
    if (Test-Path -LiteralPath $errLog -PathType Leaf) {
        try { $logText = Get-Content -LiteralPath $errLog -Raw -Encoding UTF8 -ErrorAction Stop } catch { $logText = '' }
    }
    if ($logText -match '(?i)listening on (http://127\.0\.0\.1:(\d+)/)') {
        $actualUrl = $matches[1]
        $actualPort = [int]$matches[2]
        break
    }
    Start-Sleep -Milliseconds 250
}

if ($actualPort -gt 0) {
    $entry = [pscustomobject][ordered]@{
        projectDir = $projectPath
        projectKey = $projectKey
        port       = $actualPort
        pid        = $started.Id
        url        = $actualUrl
        goonExe    = $resolved.path
        logFile    = $errLog
        startedAt  = (Get-Date).ToUniversalTime().ToString('o')
        adopted    = $false
    }
    $keep += $entry
    Save-GoonDesktopState -Path $statePath -Instances $keep | Out-Null

    Write-GoonInfo "running on $actualUrl (pid $($started.Id))"
    Write-GoonInfo "state file: $statePath"
    if ($NoOpen) {
        Write-GoonInfo 'no window requested (-NoOpen); open the URL above'
    } else {
        Write-GoonInfo 'the window opens by itself (goon web launches Chrome/Edge with --app)'
    }
    if (Test-Path -LiteralPath $errLog -PathType Leaf) { Write-GoonInfo "log: $errLog" }
    exit 0
}

if ($exited) {
    # Windows PowerShell 5.1 does not expose ExitCode for processes started without -Wait, so the
    # code is reported only when the host provides it; the log tail always shows the real reason.
    $exitCode = $null
    try {
        $started.WaitForExit()
        $exitCode = $started.ExitCode
    } catch {
        $exitCode = $null
    }
    if ($null -ne $exitCode) {
        Write-GoonError "goon exited with code $exitCode before it started listening"
    } else {
        Write-GoonError 'goon exited before it started listening'
    }
    $tail = Get-GoonLogTail -Path $errLog -Lines 12
    if ($tail.Count -gt 0) {
        Write-GoonLine ''
        Write-GoonLine "last lines of ${errLog}:"
        foreach ($line in $tail) { Write-GoonLine "  $line" }
        Write-GoonLine ''
    }
    $tailText = ($tail -join "`n")
    if ($tailText -match '(?i)address already in use|10048|bind') {
        Write-GoonLine 'The port is taken. Pick another one (-Port 7900) or free it:'
        Write-GoonLine '  netstat -ano | findstr :<port>'
        Write-GoonLine 'If another goon desktop instance already serves this project, use it instead of starting a new one.'
    } elseif ($tailText -match '(?i)access is denied|permission') {
        Write-GoonLine 'The binary could not be executed. Check that it is not blocked by antivirus or policy.'
    } elseif ($tailText -match '(?i)panic|error') {
        Write-GoonLine 'See the log above; `goon web --no-open` in a terminal shows the same output live.'
    }
    exit 1
}

# Timed out: the process may still come up, so it is left running and recorded with port 0.
# -Stop still finds it by PID, and the log shows what happened.
$entry = [pscustomobject][ordered]@{
    projectDir = $projectPath
    projectKey = $projectKey
    port       = 0
    pid        = $started.Id
    url        = $null
    goonExe    = $resolved.path
    logFile    = $errLog
    startedAt  = (Get-Date).ToUniversalTime().ToString('o')
    adopted    = $false
    status     = 'starting'
}
$keep += $entry
Save-GoonDesktopState -Path $statePath -Instances $keep | Out-Null
Write-GoonWarn "no listening URL after ${StartupTimeoutSec}s; pid $($started.Id) is still running, see $errLog"
Write-GoonWarn 'use -Stop to terminate it, or -StartupTimeoutSec 60 on a slow machine'
exit 1
