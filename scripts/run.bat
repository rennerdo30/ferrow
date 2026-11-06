@echo off
rem Build and run the Ferrow CLI. Usage:
rem   scripts\run.bat [--release] [ferrow arguments...]
rem Examples:
rem   scripts\run.bat search report
rem   scripts\run.bat --release index C:\Users
setlocal
cd /d "%~dp0.."
set "PROFILE="
if /i "%~1"=="--release" (
    set "PROFILE=--release"
    shift
)
set "ARGS="
:collect
if "%~1"=="" goto run
set ARGS=%ARGS% %1
shift
goto collect
:run
cargo run --package ferrow %PROFILE% -- %ARGS%
exit /b %ERRORLEVEL%
