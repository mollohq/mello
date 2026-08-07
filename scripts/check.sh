#!/bin/sh
# Fast pre-push gate: everything that runs without Docker or a backend.
#
# Answers "did I break a critical user journey?" — Rust unit tests, the
# headless UI flow tests, the screen-state invariants, and the cross-language
# RPC contract check.
#
# For the slower lanes (C++ ctest, Go, dockerised integration) use
# scripts/check-full.sh.
#
# Usage:  ./scripts/check.sh
set -eu

cd "$(dirname "$0")/.."

# Hardware-dependent voice/video tests block forever waiting on real capture
# devices without this. Mandatory for `cargo test --workspace`.
export CI=true

step() {
    printf '\n\033[1m▸ %s\033[0m\n' "$1"
}

FAILED=0
run() {
    if ! "$@"; then
        FAILED=1
        printf '\033[31m  ✗ failed: %s\033[0m\n' "$*"
    fi
}

START=$(date +%s)

step "fmt"
run cargo fmt --all -- --check

step "clippy"
run cargo clippy --all-targets -- -D warnings

step "tests (workspace + UI flows + RPC contract)"
run cargo test --workspace

ELAPSED=$(( $(date +%s) - START ))

printf '\n'
if [ "$FAILED" -eq 0 ]; then
    printf '\033[32m━━━ all checks passed in %ss ━━━\033[0m\n' "$ELAPSED"
    printf 'Not covered here: C++ ctest, Go tests, live backend.\n'
    printf 'Run ./scripts/check-full.sh before a release.\n'
else
    printf '\033[31m━━━ checks FAILED after %ss ━━━\033[0m\n' "$ELAPSED"
    exit 1
fi
