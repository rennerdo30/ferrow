@echo off
rem Run the same checks as CI: format, clippy, tests.
rem Usage: scripts\check.bat
setlocal
cd /d "%~dp0.."
echo ^> cargo fmt --all --check
cargo fmt --all --check || goto failed
echo ^> cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --locked -- -D warnings || goto failed
echo ^> cargo test --workspace --locked
cargo test --workspace --locked || goto failed
echo All checks passed.
exit /b 0
:failed
echo Checks failed.
exit /b 1
