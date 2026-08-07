#!/bin/sh
# Run the end-to-end suite against a local Nakama stack.
#
# Brings up backend/docker-compose.yml (Postgres + MinIO + Nakama), seeds it,
# runs tools/e2e, and by default tears the stack down again.
#
# This is the only thing in the repo that exercises Go RPC handlers and hooks.
# All 106 backend unit tests are pure functions — nothing else touches a
# registered RPC, so a changed payload shape or a broken hook passes every
# other check and fails only in production.
#
# Usage:
#   scripts/e2e.sh            # up, seed, test, down
#   scripts/e2e.sh --keep     # leave the stack running afterwards
#   scripts/e2e.sh --no-seed  # skip seeding (faster if already seeded)
set -eu

cd "$(dirname "$0")/.."

KEEP=0
SEED=1
for arg in "$@"; do
    case "$arg" in
        --keep) KEEP=1 ;;
        --no-seed) SEED=0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

COMPOSE="docker compose -f backend/docker-compose.yml"

if ! docker info >/dev/null 2>&1; then
    echo "✗ Docker is not running. Start Docker Desktop and retry." >&2
    exit 1
fi

cleanup() {
    if [ "$KEEP" -eq 0 ]; then
        echo "\n▸ tearing down stack"
        $COMPOSE down >/dev/null 2>&1 || true
    else
        echo "\n▸ leaving stack running (--keep). Stop it with:"
        echo "  $COMPOSE down"
    fi
}
trap cleanup EXIT

# The first run builds the Nakama Go plugin inside an amd64 container, which is
# emulated on Apple Silicon and slow. Later runs reuse the cached image.
echo "▸ starting stack (first run builds the Nakama plugin; can take minutes)"
$COMPOSE up -d --wait

echo "▸ waiting for Nakama"
i=0
until curl -sf -o /dev/null http://127.0.0.1:7350/healthcheck; do
    i=$((i + 1))
    if [ "$i" -gt 60 ]; then
        echo "✗ Nakama did not become healthy within 60s" >&2
        $COMPOSE logs --tail 40 nakama >&2
        exit 1
    fi
    sleep 1
done
echo "  healthy"

if [ "$SEED" -eq 1 ]; then
    echo "▸ seeding"
    ./scripts/seed.sh >/dev/null || echo "  ! seeding failed; discovery tests may see no crews"
fi

echo "▸ running e2e tests"
# MELLO_E2E makes the tests actually run. Without it they skip, so that
# `cargo test --workspace` stays hermetic — but with it set and no backend
# reachable they fail rather than skipping, so this can never pass vacuously.
MELLO_E2E=1 CI=true cargo test -p e2e --test backend -- --nocapture

echo "\n✓ e2e suite passed against a live backend"
