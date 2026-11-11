<#
.SYNOPSIS
    Installs (or removes) Desktop and Start Menu shortcuts for the goon desktop window.

.DESCRIPTION
    Creates .lnk shortcuts, through the WScript.Shell COM object, that open the goon desktop
    window for a project directory:

        <Desktop>\<Name>.lnk
        <Start Menu>\Programs\<Name>.lnk

    Both live in the current user's profile, so no administrator rights are needed and nothing
    outside the profile is touched. Running the script again updates the shortcuts in place
    instead of creating duplicates; -Uninstall removes exactly the files it manages.

    The shortcut runs the smart launcher (scripts/desktop/goon-desktop.ps1) by default, which
    re-opens the window on an already-running instance rather than starting a second server.
    Use -Direct to install the plain `cmd.exe /c goon.exe web --cwd <project>` shortcut instead.

    Supports -WhatIf (nothing is written) and -Confirm.

.PARAMETER ProjectDir
    Project directory the shortcut opens. Defaults to this checkout's root. Must exist.

.PARAMETER Name
    Shortcut file name without .lnk. Default "goon desktop". Vanilla file-name characters only.

.PARAMETER Uninstall
    Remove the shortcuts this script created. Missing shortcuts are reported, not treated as an
    error, so it is safe to run more than once.

.PARAMETER GoonExe
    Explicit goon.exe. Default search: PATH, target\release, target\debug,
    %USERPROFILE%\.local\bin. Used for the icon (and, with -Direct, as the target binary).

.PARAMETER Direct
    Install the plain shortcut instead of the launcher: target cmd.exe, arguments
    "/c <goon.exe> web --cwd <project>".

.PARAMETER SkipDesktop
    Do not create/remove the Desktop shortcut.

.PARAMETER SkipStartMenu
    Do not create/remove the Start Menu shortcut.

.PARAMETER Help
    Show usage and exit.

.EXAMPLE
    .\install.ps1
    Installs "goon desktop" for this checkout.

.EXAMPLE
    .\install.ps1 -ProjectDir C:\work\api -Name "goon desktop (api)"
    Installs a second shortcut, for another project.

.EXAMPLE
    .\install.ps1 -WhatIf
    Prints what would be created without writing anything.

.EXAMPLE
    .\install.ps1 -Name "goon desktop (api)" -Uninstall
    Removes that shortcut from the Desktop and the Start Menu.

.NOTES
    Windows 10 or later, Windows PowerShell 5.1 (the version this was developed and tested
    against). No administrator rights. See docs/DESKTOP.md for the full guide.
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium')]
param(
    [string]$ProjectDir,
    [string]$Name = 'goon desktop',
    [switch]$Uninstall,
    [string]$GoonExe,
    [switch]$Direct,
    [switch]$SkipDesktop,
    [switch]$SkipStartMenu,
    [Alias('h')][switch]$Help
)

$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'GoonDesktop.Common.ps1')

$repoRoot = Resolve-GoonRepoRoot -ScriptDir $PSScriptRoot
$launcherPath = Join-Path $PSScriptRoot 'goon-desktop.ps1'
$docsPath = Join-Path $repoRoot 'docs\DESKTOP.md'

function Show-GoonInstallUsage {
    Write-Host @'
goon desktop installer

Usage:
  install.ps1 [-ProjectDir <dir>] [-Name <shortcut name>] [-GoonExe <path>] [-Direct]
              [-SkipDesktop] [-SkipStartMenu] [-WhatIf]
  install.ps1 [-Name <shortcut name>] -Uninstall [-SkipDesktop] [-SkipStartMenu] [-WhatIf]

  -ProjectDir <dir>   project the shortcut opens (default: this checkout)
  -Name <name>        shortcut file name without .lnk (default: "goon desktop")
  -Uninstall          remove the shortcuts instead of creating them
  -GoonExe <path>     explicit goon.exe (default: PATH, target\release, target\debug, ~\.local\bin)
  -Direct             install "cmd.exe /c goon.exe web --cwd <project>" instead of the launcher
  -SkipDesktop        leave the Desktop shortcut alone
  -SkipStartMenu      leave the Start Menu shortcut alone
  -WhatIf             show what would change and write nothing
  -Help               show this help

Writes to <Desktop>\<Name>.lnk and <Start Menu>\Programs\<Name>.lnk in the current user's
profile. No administrator rights required. See docs/DESKTOP.md.
'@
}

if ($Help) { Show-GoonInstallUsage; exit 0 }

# ---------------------------------------------------------------------------------------------
# Inputs
# ---------------------------------------------------------------------------------------------

$nameTrimmed = $Name.Trim()
$invalidNameChars = '[<>:"/\\|?*\x00-\x1f]'
if ([string]::IsNullOrWhiteSpace($nameTrimmed)) {
    Write-GoonError '-Name must not be empty.'
    exit 1
}
if ($nameTrimmed -match $invalidNameChars) {
    Write-GoonError "-Name must be a plain file name (no path separators or <>:""/\|?*): '$Name'"
    exit 1
}
if ($nameTrimmed.EndsWith('.')) {
    Write-GoonError "-Name must not end with a dot: '$Name'"
    exit 1
}

$projectPath = $ProjectDir
if ([string]::IsNullOrWhiteSpace($projectPath)) {
    if (-not [string]::IsNullOrWhiteSpace($env:GOON_DESKTOP_PROJECT)) { $projectPath = $env:GOON_DESKTOP_PROJECT }
    else { $projectPath = $repoRoot }
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

function Get-GoonShellFolder {
    param(
        [Parameter(Mandatory = $true)][string]$FolderName,
        [Parameter(Mandatory = $true)][string]$Fallback
    )
    try {
        $resolved = [Environment]::GetFolderPath($FolderName)
        if (-not [string]::IsNullOrWhiteSpace($resolved)) { return $resolved }
    } catch { }
    return $Fallback
}

$desktopHome = $env:USERPROFILE
if ([string]::IsNullOrWhiteSpace($desktopHome)) { $desktopHome = $env:HOME }

$desktopDir = Get-GoonShellFolder -FolderName 'Desktop' -Fallback (Join-Path $desktopHome 'Desktop')
$startMenuDir = Get-GoonShellFolder -FolderName 'Programs' -Fallback (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs')

# The shortcut name is user supplied, so the final path is checked against the shell folder
# itself before anything is written or deleted.
function Assert-GoonShortcutPath {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$BaseDir
    )
    $full = [System.IO.Path]::GetFullPath($Path)
    $base = [System.IO.Path]::GetFullPath($BaseDir).TrimEnd('\') + '\'
    if (-not $full.StartsWith($base, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "refusing to touch '$full': it is outside '$BaseDir'"
    }
    return $full
}

$targets = @()
if (-not $SkipDesktop) {
    try {
        $shortcut = Assert-GoonShortcutPath -Path (Join-Path $desktopDir ($nameTrimmed + '.lnk')) -BaseDir $desktopDir
        $targets += [pscustomobject]@{ kind = 'Desktop'; path = $shortcut; baseDir = $desktopDir }
    } catch {
        Write-GoonWarn "skipping the Desktop shortcut: $(Format-GoonError $_)"
    }
}
if (-not $SkipStartMenu) {
    try {
        $shortcut = Assert-GoonShortcutPath -Path (Join-Path $startMenuDir ($nameTrimmed + '.lnk')) -BaseDir $startMenuDir
        $targets += [pscustomobject]@{ kind = 'Start Menu'; path = $shortcut; baseDir = $startMenuDir }
    } catch {
        Write-GoonWarn "skipping the Start Menu shortcut: $(Format-GoonError $_)"
    }
}

if ($targets.Count -eq 0) {
    Write-GoonError 'nothing to do: both shortcut locations were skipped or unusable.'
    exit 1
}

# ---------------------------------------------------------------------------------------------
# -Uninstall
# ---------------------------------------------------------------------------------------------

if ($Uninstall) {
    $removed = 0
    $missing = 0
    $failed = 0
    foreach ($target in $targets) {
        if (-not (Test-Path -LiteralPath $target.path)) {
            Write-GoonInfo "$($target.kind): not installed ($($target.path))"
            $missing++
            continue
        }
        if ($PSCmdlet.ShouldProcess($target.path, 'remove shortcut')) {
            try {
                Remove-Item -LiteralPath $target.path -Force -ErrorAction Stop
                if (Test-Path -LiteralPath $target.path) {
                    throw 'the file is still present after removal'
                }
                Write-GoonInfo "$($target.kind): removed $($target.path)"
                $removed++
            } catch {
                Write-GoonError "could not remove $($target.path): $(Format-GoonError $_)"
                $failed++
            }
        }
    }
    Write-GoonLine ''
    if ($failed -gt 0) {
        Write-GoonError "finished with $failed failure(s); $removed removed, $missing not installed"
        exit 1
    }
    Write-GoonInfo "uninstall complete: $removed removed, $missing not installed"
    exit 0
}

# ---------------------------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------------------------

$resolved = Resolve-GoonExe -Explicit $GoonExe -RepoRoot $repoRoot
if ([string]::IsNullOrWhiteSpace($resolved.path)) {
    if (-not [string]::IsNullOrWhiteSpace($resolved.explicitMissing)) {
        Write-GoonError "-GoonExe does not exist: $($resolved.explicitMissing)"
        exit 1
    }
    Write-GoonError 'goon.exe was not found, so the shortcut would not start anything.'
    Write-GoonLine 'searched:'
    Write-GoonLine (Format-GoonExeSearch -Result $resolved)
    Write-GoonLine ''
    Write-GoonLine 'Fix one of these, then run again:'
    Write-GoonLine '  - build the binary:        cargo build --release   (from the goon-cli checkout)'
    Write-GoonLine '  - install the npm package: npm install -g @binxgodteli/goon-cli'
    Write-GoonLine '  - point at a binary:       -GoonExe C:\path\to\goon.exe'
    exit 1
}

$iconLocation = $resolved.path + ',0'

# Shortcut target either the smart launcher (default) or the plain cmd.exe form (-Direct).
$useLauncher = (-not $Direct) -and (Test-Path -LiteralPath $launcherPath -PathType Leaf)
if (-not $Direct -and -not $useLauncher) {
    Write-GoonWarn "launcher not found at '$launcherPath'; falling back to the plain goon.exe shortcut"
}

if ($useLauncher) {
    $powershellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $powershellExe -PathType Leaf)) {
        $powershellExe = $null
        try { $powershellExe = (Get-Command -Name 'powershell.exe' -CommandType Application -ErrorAction Stop).Source } catch { $powershellExe = $null }
    }
    if ([string]::IsNullOrWhiteSpace($powershellExe)) {
        Write-GoonWarn 'powershell.exe was not found; falling back to the plain goon.exe shortcut'
        $useLauncher = $false
    }
}

if ($useLauncher) {
    $shortcutTarget = $powershellExe
    $shortcutArguments = '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "{0}" -ProjectDir "{1}"' -f $launcherPath, $projectPath
    $shortcutMode = 'launcher (reuses a running instance)'
} else {
    $shortcutTarget = $env:ComSpec
    if ([string]::IsNullOrWhiteSpace($shortcutTarget)) { $shortcutTarget = Join-Path $env:SystemRoot 'System32\cmd.exe' }
    $shortcutArguments = '/c "{0}" web --cwd "{1}"' -f $resolved.path, $projectPath
    $shortcutMode = 'direct (cmd.exe /c goon.exe web --cwd)'
}

$shortcutDescription = 'goon desktop - ' + $projectPath

function New-GoonDesktopShortcut {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$TargetPath,
        [Parameter(Mandatory = $true)][string]$Arguments,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory,
        [Parameter(Mandatory = $true)][string]$IconLocation,
        [Parameter(Mandatory = $true)][string]$Description,
        [int]$WindowStyle = 7
    )

    $shell = New-Object -ComObject WScript.Shell
    try {
        $shortcut = $shell.CreateShortcut($Path)
        $shortcut.TargetPath = $TargetPath
        $shortcut.Arguments = $Arguments
        $shortcut.WorkingDirectory = $WorkingDirectory
        $shortcut.IconLocation = $IconLocation
        $shortcut.Description = $Description
        # 7 = minimized: the console host must never flash over the app window.
        $shortcut.WindowStyle = $WindowStyle
        $shortcut.Save()
        [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($shortcut)
    } finally {
        [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($shell)
    }
}

Write-GoonLine ''
Write-GoonInfo "project: $projectPath"
Write-GoonInfo "goon.exe: $($resolved.path) (found via $($resolved.source))"
Write-GoonInfo "shortcut mode: $shortcutMode"
Write-GoonLine ''

$created = 0
$updated = 0
$failed = 0

foreach ($target in $targets) {
    $existed = Test-Path -LiteralPath $target.path
    if (-not $PSCmdlet.ShouldProcess($target.path, 'install shortcut')) { continue }
    try {
        New-GoonDesktopShortcut -Path $target.path -TargetPath $shortcutTarget -Arguments $shortcutArguments `
            -WorkingDirectory $projectPath -IconLocation $iconLocation -Description $shortcutDescription
        if (-not (Test-Path -LiteralPath $target.path -PathType Leaf)) {
            throw 'the shortcut file was not created'
        }
        if ($existed) {
            Write-GoonInfo "$($target.kind): updated $($target.path)"
            $updated++
        } else {
            Write-GoonInfo "$($target.kind): created $($target.path)"
            $created++
        }
    } catch {
        Write-GoonError "$($target.kind): could not create '$($target.path)': $(Format-GoonError $_)"
        $failed++
    }
}

Write-GoonLine ''
if ($failed -gt 0) {
    Write-GoonError "finished with $failed failure(s); $created created, $updated updated"
    exit 1
}

if ($WhatIfPreference) {
    Write-GoonInfo 'nothing was written (-WhatIf)'
} else {
    foreach ($target in $targets) {
        if (Test-Path -LiteralPath $target.path -PathType Leaf) {
            Write-GoonInfo "double-click: $($target.path)"
        }
    }
    Write-GoonLine ''
    Write-GoonLine 'Remove these shortcuts with:'
    Write-GoonLine ('  powershell -NoProfile -ExecutionPolicy Bypass -File "{0}" -Name "{1}" -Uninstall' -f $PSCommandPath, $nameTrimmed)
    if (Test-Path -LiteralPath $docsPath -PathType Leaf) {
        Write-GoonLine ("Guide: {0}" -f $docsPath)
    }
}
exit 0
