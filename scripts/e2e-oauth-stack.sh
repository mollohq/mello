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
# The stack belongs to one checkout: the one that last started Postgres (its
# compose working directory). `up` adds the profile from THIS checkout and
# `down` restores from the stack's own checkout, with that checkout's
# backend/.env. So a git worktree can run `up` and `down` and leave the stack
# as it was.
#
# MELLO_ENV_FILE: the backend .env to use. Default: backend/.env of the
# stack's checkout. Without the right .env, Nakama starts with a default HTTP
# key and the client gets "HTTP key invalid".
set -eu

here="$(cd "$(dirname "$0")/.." && pwd)"

# The checkout that owns the stack: Postgres runs from it and is never
# recreated by this script.
owner="$(docker inspect mello-postgres \
    --format '{{index .Config.Labels "com.docker.compose.project.working_dir"}}' 2>/dev/null || true)"
owner="${owner%/backend}"
if [ -z "$owner" ] || [ ! -f "$owner/backend/docker-compose.yml" ]; then
    owner="$here"
fi

env_file="${MELLO_ENV_FILE:-$owner/backend/.env}"
case "$env_file" in
    /*) ;;
    *) env_file="$(pwd)/$env_file" ;;
esac
if [ ! -f "$env_file" ]; then
    echo "✗ no env file at $env_file. Set MELLO_ENV_FILE to your backend/.env." >&2
    exit 1
fi

wait_for() {
    i=0
    until curl -sf -o /dev/null "$1"; do
        i=$((i + 1))
        if [ "$i" -gt 120 ]; then
            echo "✗ $1 did not answer within 120 s" >&2
            exit 1
        fi
        sleep 1
    done
}

case "${1:-}" in
    up)
        cd "$here"
        docker compose -p backend --env-file "$env_file" \
            -f backend/docker-compose.yml -f backend/docker-compose.e2e.yml up -d --build
        wait_for http://127.0.0.1:18080/healthz
        wait_for http://127.0.0.1:7350/healthcheck
        echo "✓ e2e OAuth profile up: fake provider on http://127.0.0.1:18080"
        echo "  Restore with: $0 down   (restores from $owner)"
        ;;
    down)
        cd "$owner"
        docker compose -p backend --env-file "$env_file" \
            -f backend/docker-compose.yml up -d --remove-orphans
        docker volume rm backend_fake-oauth-trust >/dev/null 2>&1 || true
        wait_for http://127.0.0.1:7350/healthcheck
        echo "✓ normal stack again, from $owner"
        ;;
    *)
        echo "usage: $0 up|down" >&2
        exit 2
        ;;
esac
