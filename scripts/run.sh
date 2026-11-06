#!/usr/bin/env sh
# Build and run Ferrow. Usage:
#   ./scripts/run.sh [--release] [ferrow arguments...]
# Examples:
#   ./scripts/run.sh
#   ./scripts/run.sh --release '*.rs'
set -eu
cd "$(dirname "$0")/.."

profile=""
if [ "${1:-}" = "--release" ]; then
    profile="--release"
    shift
fi

# shellcheck disable=SC2086 # $profile is empty or one word
exec cargo run --package ferrow $profile -- "$@"
