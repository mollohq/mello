#!/bin/sh
# Spike: launch one isolated mello user. Usage: launch.sh USER MCP_PORT [DEEPLINK]
U=$1; P=$2; LINK=${3:-}
D=${MELLO_E2E_RUN_DIR:-/tmp/mello-e2e}/$U; mkdir -p $D/config
cd "$(dirname "$0")/../../.."
MELLO_CONFIG_DIR=$D/config MELLO_SESSION_KEY=e2e-$U SLINT_MCP_PORT=$P MELLO_E2E_STATE_PORT=$((P+100)) MELLO_E2E_SESSION_FILE=$D/session.token \
NAKAMA_SERVER_KEY=mello_dev_key RUST_LOG=info,mello=debug,mello_core=debug \
  nohup target/debug/mello $LINK --instance e2e-$U > $D/app.log 2>&1 &
echo $! > $D/pid; echo "launched $U pid=$(cat $D/pid) mcp=$P"
