@echo off
rem goon desktop: opens the local UI in a Chromium app window (Windows).
rem
rem This is the double-click entry point. It forwards every argument to the smart launcher
rem (scripts\desktop\goon-desktop.ps1) so re-launching reuses a running instance instead of
rem starting a second server. When PowerShell or the launcher is unavailable it falls back to
rem running the binary directly, which is the pre-launcher behaviour.
rem
rem Usage:  goon-desktop.cmd [project-dir] [launcher options]
rem         goon-desktop.cmd --help
rem Docs:   docs\DESKTOP.md
setlocal EnableExtensions DisableDelayedExpansion
set "RC=0"

rem --- help / usage (handled here so it never needs the binary) ------------------------------
if /i "%~1"=="--help" goto :usage
if /i "%~1"=="-h" goto :usage
if /i "%~1"=="-?" goto :usage
if /i "%~1"=="/?" goto :usage

rem --- locations ----------------------------------------------------------------------------
for %%I in ("%~dp0..") do set "GOON_REPO=%%~fI"
set "GOON_LAUNCHER=%GOON_REPO%\scripts\desktop\goon-desktop.ps1"
set "GOON_DOCS=%GOON_REPO%\docs\DESKTOP.md"
set "GOON_PROJECT=%GOON_REPO%"
set "GOON_ARGS="

rem --- argument handling --------------------------------------------------------------------
rem A single positional argument that is a directory means "open that project" (the historical
rem goon-desktop.cmd . behaviour). Anything else is forwarded verbatim to the launcher.
if "%~1"=="" goto :resolve
if not "%~2"=="" goto :forward
set "GOON_FIRST=%~1"
if "%GOON_FIRST:~0,1%"=="-" goto :forward
if not exist "%~1\" goto :forward
set "GOON_PROJECT=%~f1"
goto :resolve

:forward
set "GOON_ARGS=%*"

:resolve
rem The launcher takes -ProjectDir from this variable unless it is given on the command line,
rem so forwarding "%*" cannot produce a duplicate parameter.
set "GOON_DESKTOP_PROJECT=%GOON_PROJECT%"

if not exist "%GOON_LAUNCHER%" goto :fallback

set "GOON_PS=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
if not exist "%GOON_PS%" set "GOON_PS=powershell.exe"

"%GOON_PS%" -NoProfile -ExecutionPolicy Bypass -File "%GOON_LAUNCHER%" %GOON_ARGS%
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" goto :failed
goto :done

rem --- fallback: run the binary directly ----------------------------------------------------
:fallback
set "GOON_EXE="
for %%I in (goon.exe) do if not defined GOON_EXE set "GOON_EXE=%%~$PATH:I"
if not defined GOON_EXE if exist "%GOON_REPO%\target\release\goon.exe" set "GOON_EXE=%GOON_REPO%\target\release\goon.exe"
if not defined GOON_EXE if exist "%GOON_REPO%\target\debug\goon.exe" set "GOON_EXE=%GOON_REPO%\target\debug\goon.exe"
if not defined GOON_EXE if exist "%USERPROFILE%\.local\bin\goon.exe" set "GOON_EXE=%USERPROFILE%\.local\bin\goon.exe"
if not defined GOON_EXE goto :missing

echo goon desktop: launcher not found, running "%GOON_EXE%" directly.
"%GOON_EXE%" web --cwd "%GOON_PROJECT%" %GOON_ARGS%
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" goto :failed
goto :done

:missing
echo.
echo goon desktop: goon.exe was not found.
echo searched: PATH, "%GOON_REPO%\target\release", "%GOON_REPO%\target\debug", "%USERPROFILE%\.local\bin"
echo.
echo Build the binary:      cargo build --release
echo Or install the npm package: npm install -g @binxgodteli/goon-cli
set "RC=1"
goto :failed

:failed
echo.
echo goon desktop failed with exit code %RC%.
if exist "%GOON_DOCS%" echo Guide: %GOON_DOCS%

:maybe_pause
rem Keep the window open only when this script was started by a double-click or shortcut, so the
rem error stays readable. GOON_DESKTOP_NO_PAUSE=1 forces the non-interactive behaviour.
if defined GOON_DESKTOP_NO_PAUSE goto :done
echo %cmdcmdline% | findstr /i /c:"goon-desktop.cmd" >nul 2>nul
if errorlevel 1 goto :done
echo.
echo Press any key to close this window...
pause >nul
goto :done

:usage
echo goon desktop - open the goon coding agent as a desktop window
echo.
echo Usage:
echo   goon-desktop.cmd                        open this checkout as the project
echo   goon-desktop.cmd C:\path\to\project     open that project
echo   goon-desktop.cmd -Port 7788             fixed port instead of an ephemeral one
echo   goon-desktop.cmd -NoOpen                start/reuse the server, print the URL
echo   goon-desktop.cmd -List                  list recorded instances and their status
echo   goon-desktop.cmd -Stop                  stop the recorded instance(s)
echo   goon-desktop.cmd --help                 this text
echo.
echo Arguments are forwarded to scripts\desktop\goon-desktop.ps1. Re-running for the same
echo project re-opens the window on the running instance instead of starting a second one.
echo State is kept in %%LOCALAPPDATA%%\goon\desktop.json.
if exist "%GOON_DOCS%" echo.
if exist "%GOON_DOCS%" echo Guide: %GOON_DOCS%
set "RC=0"
goto :done

:done
endlocal & exit /b %RC%
