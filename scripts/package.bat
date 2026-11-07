@echo off
rem Build the Windows setup (artifacts\installer\Ferrow-<version>-setup.exe).
rem Usage: scripts\package.bat <path to makensis.exe>
rem   or set FERROW_MAKENSIS first. Builds only; installs nothing.
setlocal
cd /d "%~dp0.."
if not "%~1"=="" set "FERROW_MAKENSIS=%~1"
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts\package.ps1
exit /b %ERRORLEVEL%
