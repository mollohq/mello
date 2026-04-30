#!/bin/bash
# Start voice-test-client in PRODUCTION mode (remote Nakama)
# Uses the same Nakama keys as client-prod.sh unless overridden by env.

NAKAMA_SERVER_KEY="sjA4e8JJDVsqitgvGCrK7eTU79iIp0JM44r9WfrORjM="
NAKAMA_HTTP_KEY="nTQ8dvc04gc6nd/seEssQej1iyKMV9u4kIgVPJY2sYY="

export VOICE_TEST_PRODUCTION=1
export RUST_LOG="info,mello_core=debug,libmello=debug"

if [ -n "$NAKAMA_SERVER_KEY" ]; then
    export NAKAMA_SERVER_KEY
fi

if [ -n "$NAKAMA_HTTP_KEY" ]; then
    export NAKAMA_HTTP_KEY
fi

cd "$(dirname "$0")/tools/voice-test-client" && cargo run "$@"
