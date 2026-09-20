#!/bin/sh
# Fast pre-push gate: everything that runs without Docker or a backend.
#
# Answers "did I break a critical user journey?" — Rust unit tests, the
# headless UI flow tests, the screen-state invariants, and the cross-language
# RPC contract check.
#
# For the slower lanes (C++ ctest, dockerised integration) use
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
# --workspace, not just the default members. Without it clippy skips every
# crate under tools/, which is how a lint error reached release.yml (the only
# place that used --workspace) and blocked a release.
run cargo clippy --workspace --all-targets -- -D warnings

step "tests (workspace + UI flows + RPC contract)"
run cargo test --workspace

step "backend (Go fmt / vet / tests)"
# Seconds, not minutes: this is what stands between a push and a 30-minute
# cross-platform CI round-trip on a formatting slip. Skips cleanly where Go
# is not installed; check-full.sh and PR CI run the same lane.
if command -v go >/dev/null 2>&1; then
    (
        cd backend/nakama/data/modules
        # .gitattributes pins LF for *.go, but a working copy checked out
        # before that still has CRLF, which gofmt always flags. Compare with
        # CR stripped: that is what git stores and what Linux CI sees.
        unformatted=$(
            find . -name '*.go' -print | while IFS= read -r f; do
                if [ -n "$(tr -d '\r' < "$f" | gofmt -l)" ]; then
                    printf '%s\n' "$f"
                fi
            done
        )
        if [ -n "$unformatted" ]; then
            printf '\033[31m  ? gofmt:\033[0m\n'
            printf '%s\n' "$unformatted" | while IFS= read -r f; do
                printf '\033[31m      %s\033[0m\n' "$f"
            done
            exit 1
        fi
        go vet ./...
        go test ./...
    ) || FAILED=1
else
    printf '  ! go not installed, skipping backend tests\n'
fi

ELAPSED=$(( $(date +%s) - START ))

printf '\n'
if [ "$FAILED" -eq 0 ]; then
    printf '\033[32m━━━ all checks passed in %ss ━━━\033[0m\n' "$ELAPSED"
    printf 'Not covered here: C++ ctest, live backend.\n'
    printf 'Run ./scripts/check-full.sh before a release.\n'
else
    printf '\033[31m━━━ checks FAILED after %ss ━━━\033[0m\n' "$ELAPSED"
    exit 1
fi
