@echo off
rem goon desktop: opens the local UI in a Chromium app window.
rem Usage: goon-desktop.cmd [project-dir]
setlocal
set "GOON_EXE=%~dp0goon.exe"
if not exist "%GOON_EXE%" set "GOON_EXE=goon"

if "%~1"=="" (
  "%GOON_EXE%" web
) else (
  "%GOON_EXE%" web --cwd "%~1"
)

if errorlevel 1 (
  echo.
  echo goon desktop exited with code %errorlevel%.
  pause
)
endlocal
