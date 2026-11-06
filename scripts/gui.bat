@echo off
rem Build and start the Ferrow desktop app. Double-click it, or run:
rem   scripts\gui.bat [--release]
rem The app reads the index at %LOCALAPPDATA%\ferrow\index.bin; build one
rem with "scripts\run.bat index <folder>" or the button in the app.
setlocal
cd /d "%~dp0.."
set "PROFILE="
if /i "%~1"=="--release" set "PROFILE=--release"
cargo run --package ferrow-gui %PROFILE%
if errorlevel 1 (
    echo Ferrow did not start. See the messages above.
    pause
    exit /b 1
)
