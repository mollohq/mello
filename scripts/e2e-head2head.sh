#!/bin/sh
# Integration test of the head2head feed entry against a local Nakama.
#
# Runs tools/e2e/tests/head2head_feed.rs: two fresh accounts, one crew,
# library_report for both, crew_feed as the first account, a belt move, and
# game_session_end with tz_offset_min. A second test posts clips, sessions,
# moments and voice stints, and checks the six session titles and the recap
# belts. About 70 seconds, because the holders doc debounces the session
# trigger for 60 seconds. The two tests run in parallel.
#
# Voice in the second test takes the SFU path: the crew is switched to the SFU
# and the join must answer in SFU mode. The own stack gets a throwaway signing
# key (SFU_JWT_PRIVATE_KEY) and the local SFU address. No media runs.
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
# The push test (belt moved, friend landed) serves a stub push Worker on the
# host. The own stack posts to it through host.docker.internal. With --existing,
# the stack needs PUSH_WORKER_URL and PUSH_WORKER_TOKEN as scripts/e2e.sh sets
# them, and MELLO_E2E_PUSH_PORT must name the same port. Else that test skips.
#
# Local only. It never reads a remote URL, and --existing refuses a host that
# is not loopback. The second test needs MELLO_ENABLE_DEV_FAULT=1 and
# SFU_JWT_PRIVATE_KEY on the stack. The own stack sets both. With --existing,
# set them yourself.
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
    # Voice joins in the test must take the SFU path (P2P is being removed).
    # SFU mode needs a signing key on Nakama. A throwaway key is enough: the
    # test makes no media connection. The endpoint is the local SFU, never the
    # production one that the compose file defaults to.
    command -v openssl >/dev/null 2>&1 || { echo "openssl is needed for the throwaway SFU key" >&2; exit 1; }
    SFU_JWT_PRIVATE_KEY="$(openssl genrsa 2048 2>/dev/null)"
    export SFU_JWT_PRIVATE_KEY
    # Nakama posts push requests to the stub Worker that the push test serves.
    MELLO_E2E_PUSH_PORT="${MELLO_E2E_PUSH_PORT:-18787}"
    PUSH_WORKER_URL="http://host.docker.internal:$MELLO_E2E_PUSH_PORT"
    PUSH_WORKER_TOKEN="e2e-push-token"
    export MELLO_E2E_PUSH_PORT PUSH_WORKER_URL PUSH_WORKER_TOKEN
    export SFU_ENDPOINT_EU="ws://127.0.0.1:8080/ws"
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
    # The session titles test backdates voice joins and generates the recap
    # through the dev_fault RPC.
    environment:
      - MELLO_ENABLE_DEV_FAULT=1
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
