#!/usr/bin/env sh
# Run the same checks as CI: format, clippy, tests.
# Usage: ./scripts/check.sh [--package ferrow-query ferrow ...]
set -eu
cd "$(dirname "$0")/.."

if [ "$#" -gt 0 ]; then
    if [ "$1" != --package ] || [ "$#" -lt 2 ]; then
        echo "usage: $0 [--package <crate>...]" >&2
        exit 2
    fi
    shift
    # Rotate the original arguments into repeated cargo package selectors.
    count=$#
    while [ "$count" -gt 0 ]; do
        package=$1
        shift
        case "$package" in
            ''|-*) echo "invalid package: $package" >&2; exit 2 ;;
        esac
        set -- "$@" -p "$package"
        count=$((count - 1))
    done
else
    set -- --workspace
fi

echo "> cargo fmt --all --check"
cargo fmt --all --check
echo "> cargo clippy --workspace --all-targets --locked -j 8 -- -D warnings"
cargo clippy --workspace --all-targets --locked -j 8 -- -D warnings
echo "> cargo test --locked -j 8 $*"
cargo test --locked -j 8 "$@"
echo "All checks passed."
