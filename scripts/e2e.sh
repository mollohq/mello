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

# Nakama's environment comes from backend/.env, as on the shared local stack
# (MELLO_ENV_FILE picks another file, as scripts/e2e-oauth-stack.sh does).
# Compose passes SFU_JWT_PRIVATE_KEY and the other keys through from it.
ENV_FILE="${MELLO_ENV_FILE:-backend/.env}"
if [ -f "$ENV_FILE" ]; then
    COMPOSE="docker compose --env-file $ENV_FILE -f backend/docker-compose.yml"
else
    COMPOSE="docker compose -f backend/docker-compose.yml"
fi

# Remote push: Nakama posts to a stub Worker that the push e2e test serves on
# the host (spec 23). Compose recreates Nakama when these change.
MELLO_E2E_PUSH_PORT="${MELLO_E2E_PUSH_PORT:-18787}"
PUSH_WORKER_URL="${PUSH_WORKER_URL:-http://host.docker.internal:$MELLO_E2E_PUSH_PORT}"
PUSH_WORKER_TOKEN="${PUSH_WORKER_TOKEN:-e2e-push-token}"
# A short grace period so the held-push test runs in seconds, not minutes.
PUSH_DESKTOP_GRACE_SECS="${PUSH_DESKTOP_GRACE_SECS:-2}"
export MELLO_E2E_PUSH_PORT PUSH_WORKER_URL PUSH_WORKER_TOKEN PUSH_DESKTOP_GRACE_SECS

# The SFU is the only voice and stream transport: without a signing key,
# voice_join and start_stream return SFU_UNAVAILABLE. The RPC tests only need
# the token, not a running SFU, so a stack with no key gets a key for this run.
# A stack with a key keeps it: a key made here would stay on a --keep stack,
# and the local SFU rejects every token signed with it (its public key is the
# one of backend/.env).
if [ -z "${SFU_JWT_PRIVATE_KEY:-}" ] \
    && ! { [ -f "$ENV_FILE" ] && grep -q '^SFU_JWT_PRIVATE_KEY=' "$ENV_FILE"; }; then
    SFU_JWT_PRIVATE_KEY="$(openssl genrsa 2048 2>/dev/null)"
    export SFU_JWT_PRIVATE_KEY
fi

if ! docker info >/dev/null 2>&1; then
    echo "✗ Docker is not running. Start Docker Desktop and retry." >&2
    exit 1
fi

cleanup() {
    if [ "$KEEP" -eq 0 ]; then
        printf '\n▸ tearing down stack\n'
        $COMPOSE down >/dev/null 2>&1 || true
    else
        printf '\n▸ leaving stack running (--keep). Stop it with:\n'
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

printf '\n✓ e2e suite passed against a live backend\n'
