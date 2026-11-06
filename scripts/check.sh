#!/usr/bin/env sh
# Run the same checks as CI: format, clippy, tests.
# Usage: ./scripts/check.sh
set -eu
cd "$(dirname "$0")/.."

echo "> cargo fmt --all --check"
cargo fmt --all --check
echo "> cargo clippy --workspace --all-targets --locked -- -D warnings"
cargo clippy --workspace --all-targets --locked -- -D warnings
echo "> cargo test --workspace --locked"
cargo test --workspace --locked
echo "All checks passed."
