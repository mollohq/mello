#!/bin/sh
# Switch the local Docker stack to the e2e OAuth profile and back
# (backend/docker-compose.e2e.yml, plans/E2E-QA.md §8).
#
#   scripts/e2e-oauth-stack.sh up     # Nakama + fake OAuth provider
#   scripts/e2e-oauth-stack.sh down   # the normal stack again
#
# While the profile runs, the local Nakama cannot reach the real Discord,
# Twitch, Steam or Google. Postgres and MinIO keep their data: they use the
# named volumes of the "backend" project in both modes.
#
# MELLO_ENV_FILE: the backend .env to use (default: backend/.env of this
# checkout). A git worktree has no .env of its own; point this at the main
# checkout's, or Nakama starts with a default HTTP key and the client gets
# "HTTP key invalid".
set -eu
cd "$(dirname "$0")/.."

ENV_FILE="${MELLO_ENV_FILE:-backend/.env}"
if [ ! -f "$ENV_FILE" ]; then
    echo "✗ no env file at $ENV_FILE. Set MELLO_ENV_FILE to your backend/.env." >&2
    exit 1
fi

base="docker compose -p backend --env-file $ENV_FILE -f backend/docker-compose.yml"
case "${1:-}" in
    up)
        $base -f backend/docker-compose.e2e.yml up -d --build
        until curl -sf -o /dev/null http://127.0.0.1:18080/healthz; do sleep 1; done
        until curl -sf -o /dev/null http://127.0.0.1:7350/healthcheck; do sleep 1; done
        echo "✓ e2e OAuth profile up: fake provider on http://127.0.0.1:18080"
        ;;
    down)
        $base up -d --remove-orphans
        docker volume rm backend_fake-oauth-trust >/dev/null 2>&1 || true
        until curl -sf -o /dev/null http://127.0.0.1:7350/healthcheck; do sleep 1; done
        echo "✓ normal stack again"
        ;;
    *)
        echo "usage: $0 up|down" >&2
        exit 2
        ;;
esac
