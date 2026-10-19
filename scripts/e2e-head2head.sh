#!/bin/sh
# Integration test of the head2head feed entry against a local Nakama.
#
# Runs tools/e2e/tests/head2head_feed.rs: two fresh accounts, one crew,
# library_report for both, crew_feed as the first account, a belt move, and
# game_session_end with tz_offset_min. About 70 seconds, because the holders
# doc debounces the session trigger for 60 seconds.
#
# The Nakama module must be the one in THIS checkout. The stack that other
# worktrees leave on port 7350 may mount a different checkout, so the default
# is an own stack on port 17350 that mounts this tree and is torn down after.
#
# Usage:
#   scripts/e2e-head2head.sh              # own stack on 17350, torn down after
#   scripts/e2e-head2head.sh --keep       # leave the own stack running
#   scripts/e2e-head2head.sh --existing   # use the stack on NAKAMA_HOST:NAKAMA_PORT
#                                         # (default 127.0.0.1:7350)
#
# Local only. It never reads a remote URL, and --existing refuses a host that
# is not loopback.
set -eu

cd "$(dirname "$0")/.."

KEEP=0
EXISTING=0
for arg in "$@"; do
    case "$arg" in
        --keep) KEEP=1 ;;
        --existing) EXISTING=1 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

if ! docker info >/dev/null 2>&1; then
    echo "Docker is not running. Start Docker Desktop and retry." >&2
    exit 1
fi

if [ "$EXISTING" -eq 1 ]; then
    HOST="${NAKAMA_HOST:-127.0.0.1}"
    PORT="${NAKAMA_PORT:-7350}"
    case "$HOST" in
        127.0.0.1|localhost|::1) ;;
        *) echo "refusing to run against $HOST: local stacks only" >&2; exit 1 ;;
    esac
else
    HOST=127.0.0.1
    PORT="${H2H_NAKAMA_PORT:-17350}"
    TMP="$(mktemp -d)"
    PROJECT=mello-h2h
    # Distinct container names and ports, so it lives next to the main stack.
    cat >"$TMP/override.yml" <<YML
services:
  postgres:
    container_name: mello-h2h-postgres
    ports: !reset []
  minio:
    container_name: mello-h2h-minio
    ports: !reset []
  minio-init:
    container_name: mello-h2h-minio-init
  nakama-build:
    container_name: mello-h2h-nakama-build
  nakama:
    container_name: mello-h2h-nakama
    ports: !override
      - "$PORT:7350"
YML
    COMPOSE="docker compose -p $PROJECT -f backend/docker-compose.yml -f $TMP/override.yml"
    cleanup() {
        if [ "$KEEP" -eq 0 ]; then
            echo "tearing down the own stack"
            $COMPOSE down -v >/dev/null 2>&1 || true
        else
            echo "own stack left running on port $PORT. Stop it with:"
            echo "  $COMPOSE down -v"
        fi
        rm -rf "$TMP"
    }
    trap cleanup EXIT
    echo "starting the own stack (the first run builds the Nakama plugin, which is slow on Apple Silicon)"
    $COMPOSE up -d --wait
fi

echo "waiting for Nakama on $HOST:$PORT"
i=0
until curl -sf -o /dev/null "http://$HOST:$PORT/healthcheck"; do
    i=$((i + 1))
    if [ "$i" -gt 60 ]; then
        echo "Nakama did not become healthy within 60s" >&2
        exit 1
    fi
    sleep 1
done

NAKAMA_HOST="$HOST" NAKAMA_PORT="$PORT" MELLO_E2E=1 CI=true \
    cargo test -p e2e --test head2head_feed -- --nocapture

echo "head2head integration test passed"
