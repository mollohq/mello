#!/usr/bin/env bash
# Start Mello client in DEVELOPMENT mode (local Nakama), release build.
# Use this for perf baselines — not client-dev.sh (debug + verbose logs + MCP metadata).

NAKAMA_SERVER_KEY="mello_dev_key"

export RUST_LOG="${RUST_LOG:-info}"

if [ -n "$NAKAMA_SERVER_KEY" ]; then
    export NAKAMA_SERVER_KEY
fi

cd "$(dirname "$0")/client" && cargo run --release --no-default-features --features development "$@"
