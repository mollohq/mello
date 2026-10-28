#!/bin/sh
# Run the art Worker (mello-art-worker) on this machine for the e2e journeys.
#
#   scripts/e2e-art-worker.sh up      build the fixture index, start wrangler dev
#   scripts/e2e-art-worker.sh down    stop it
#
# The index is the six games of qa/fixtures/art-index, turned into KV items by
# the Worker's own scripts/build-index.ts and loaded into a local KV store
# (`wrangler kv bulk put --local`, so the Worker repo needs no change). The
# state lives in $STATE, not in the Worker repo.
#
# The Worker still fetches the images from images.igdb.com (the fixture uses
# real IGDB image ids), so this machine needs internet access. Nothing here
# reaches art.m3llo.app.
#
# The driver points the apps at it: tools/mello-driver launchEnv sets
# MELLO_ART_BASE to http://127.0.0.1:8787 (MELLO_E2E_ART_BASE overrides).
#
# A journey that uses this Worker declares `needs: ["art"]`. `mello-driver run`
# then checks <base>/health once before the first journey, only on a local
# address, and stops with the fix when it does not answer.
#
# wrangler runs in its own process group. A caller that runs `up` in a process
# group and kills the group when `up` returns (the QA harness does) would
# otherwise kill the Worker. macOS has no setsid, so a perl setpgrp wrapper
# starts it, with job control (set -m) as the fallback. `down` kills the pid
# (the group leader) and its children. On Windows (Git Bash) there is no
# process group kill: the Worker survives the caller, and `down` uses
# taskkill /T on the pid to end the tree.
set -eu

HERE=$(cd "$(dirname "$0")/.." && pwd)
WORKER="${MELLO_ART_WORKER_DIR:-$HERE/../mello-art-worker}"
PORT="${MELLO_E2E_ART_PORT:-8787}"
STATE="${MELLO_E2E_ART_STATE:-/tmp/m3llo-art-e2e}"

case "${1:-}" in
up)
    [ -d "$WORKER/node_modules" ] || { echo "no $WORKER/node_modules: run npm install there" >&2; exit 1; }
    if [ -f "$STATE/wrangler.pid" ] && kill -0 "$(cat "$STATE/wrangler.pid")" 2>/dev/null; then
        echo "art Worker already running (pid $(cat "$STATE/wrangler.pid"))"
        exit 0
    fi
    rm -rf "$STATE"
    mkdir -p "$STATE/kv-state"
    (cd "$WORKER" && node scripts/build-index.ts "$HERE/qa/fixtures/art-index" "$STATE/index-out")
    (cd "$WORKER" && npx wrangler kv bulk put "$STATE/index-out/kv-000.json" \
        --binding ART_INDEX --local --persist-to "$STATE/kv-state")
    if command -v perl >/dev/null 2>&1; then
        (cd "$WORKER" && exec perl -e 'setpgrp(0,0); exec @ARGV' -- \
            npx wrangler dev --port "$PORT" --ip 127.0.0.1 \
            --persist-to "$STATE/kv-state" >"$STATE/wrangler.log" 2>&1) &
        echo $! >"$STATE/wrangler.pid"
    else
        # No perl: job control gives the background job its own group.
        (
            set -m
            (cd "$WORKER" && exec npx wrangler dev --port "$PORT" --ip 127.0.0.1 \
                --persist-to "$STATE/kv-state" >"$STATE/wrangler.log" 2>&1) &
            echo $! >"$STATE/wrangler.pid"
        )
    fi
    i=0
    while [ $i -lt 60 ]; do
        if curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
            echo "art Worker on http://127.0.0.1:$PORT (log $STATE/wrangler.log)"
            exit 0
        fi
        i=$((i + 1))
        sleep 1
    done
    echo "art Worker did not answer in 60 s; see $STATE/wrangler.log" >&2
    exit 1
    ;;
down)
    if [ -f "$STATE/wrangler.pid" ]; then
        pid=$(cat "$STATE/wrangler.pid")
        # wrangler runs workerd as a child: stop the process group members.
        # Git Bash on Windows has no pkill; taskkill ends the process tree there.
        if command -v pkill >/dev/null 2>&1; then
            pkill -P "$pid" 2>/dev/null || true
        elif command -v taskkill.exe >/dev/null 2>&1; then
            taskkill.exe //PID "$pid" //T //F >/dev/null 2>&1 || true
        fi
        kill "$pid" 2>/dev/null || true
        rm -f "$STATE/wrangler.pid"
    fi
    echo "art Worker stopped"
    ;;
*)
    echo "usage: $0 up|down" >&2
    exit 2
    ;;
esac
