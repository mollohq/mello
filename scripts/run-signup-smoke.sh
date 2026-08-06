#!/bin/sh
# Drive a *built* Mello binary through a real signup and fail if it cannot.
#
# This is the only check that exercises the shipped artifact rather than the
# source. Keys such as NAKAMA_HTTP_KEY are baked in at compile time via
# option_env!, so a build made with the wrong secret is indistinguishable from
# a correct one until a user tries to sign up. Running the real binary against
# a real server is the only way to catch that before release.
#
# Complements tools/canary: the canary proves the *CI secret* matches the
# server, this proves the *built binary* does.
#
# Usage:
#   scripts/run-signup-smoke.sh <path-to-mello-binary>
#
# Environment:
#   NAKAMA_HOST / NAKAMA_PORT / NAKAMA_SSL   optional target override
#
# Exit 0 = a new user can sign up with this binary.
set -eu

BINARY="${1:-}"
if [ -z "$BINARY" ] || [ ! -x "$BINARY" ]; then
    echo "usage: $0 <path-to-mello-binary>" >&2
    exit 2
fi

cd "$(dirname "$0")/.."
SCENARIO="$(pwd)/tools/perf-harness/scenarios/signup_smoke.json"

SIGNAL_DIR="$(mktemp -d)"
trap 'rm -rf "$SIGNAL_DIR"' EXIT

# Unique crew name per run so repeated runs never collide.
MELLO_SMOKE_ID="$(date +%s)-$$"
export MELLO_SMOKE_ID

# Isolated config dir: the smoke run must not read or clobber whatever
# onboarding state exists on the machine (a runner may have a completed
# onboarding persisted, which would skip the very flow under test).
MELLO_CONFIG_DIR="$SIGNAL_DIR/config"
mkdir -p "$MELLO_CONFIG_DIR"
export MELLO_CONFIG_DIR

echo "▸ signup smoke: $BINARY"
echo "  scenario: $SCENARIO"

MELLO_PERF_MODE=1 \
MELLO_PERF_SCENARIO="$SCENARIO" \
MELLO_PERF_SIGNAL_DIR="$SIGNAL_DIR" \
RUST_LOG="${RUST_LOG:-info}" \
    "$BINARY" || true

DONE="$SIGNAL_DIR/done.json"
if [ ! -f "$DONE" ]; then
    echo "✗ signup smoke FAILED: the client exited without reporting a result." >&2
    echo "  It likely crashed or quit before the scenario finished." >&2
    exit 1
fi

echo "  result: $(cat "$DONE")"

# The scenario runner reports through this file rather than the process exit
# code — it quits the Slint event loop and returns 0 either way — so the status
# has to be translated here or a failure would pass CI silently.
if grep -q '"status":"ok"' "$DONE"; then
    echo "✓ signup smoke passed: this binary can create a new account."
    exit 0
fi

echo "✗ signup smoke FAILED — do not release this build." >&2
echo "  A new user installing it would not be able to sign up." >&2
echo "  Most likely: NAKAMA_HTTP_KEY baked into the build does not match the server." >&2
exit 1
