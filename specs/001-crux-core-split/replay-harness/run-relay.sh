#!/bin/zsh
# usage: run-relay.sh <binary> <port> <workdir>
set -eu
BIN=$1
PORT=$2
WORKDIR=$3
mkdir -p "$WORKDIR"
cd "$WORKDIR"
export VELA_RELAY_REDIS_URL="redis://127.0.0.1:6390/0"
export VELA_RELAY_IGGY_URL="iggy+tcp://iggy:sc6-local-only@127.0.0.1:5190?reconnection_retries=5&reconnection_interval=1s&reestablish_after=5s&heartbeat_interval=3s&nodelay=true"
export VELA_RELAY_EXECUTOR_ENABLED=false
export VELA_RELAY_LISTEN_ADDR="127.0.0.1:$PORT"
export VELA_RELAY_SETTLEMENT_RECIPIENT="0x00000000000000000000000000000000000000fe"
exec "$BIN" > relay.log 2>&1
