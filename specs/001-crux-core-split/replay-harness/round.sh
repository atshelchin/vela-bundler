#!/bin/zsh
# usage: round.sh <binary> <port> <label>
set -eu
SC=$(cd "$(dirname "$0")" && pwd)
BIN=$1
PORT=$2
LABEL=$3

# fresh state
docker exec vela-sc6-redis redis-cli flushall >/dev/null
docker rm -f vela-sc6-iggy >/dev/null 2>&1 || true
docker run -d --name vela-sc6-iggy --security-opt seccomp=unconfined \
  -e IGGY_TCP_ADDRESS=0.0.0.0:8090 -e IGGY_ROOT_USERNAME=iggy -e IGGY_ROOT_PASSWORD=sc6-local-only \
  -p 127.0.0.1:5190:8090 apache/iggy:latest >/dev/null
for i in {1..30}; do nc -z 127.0.0.1 5190 2>/dev/null && break; sleep 1; done

rm -rf "$SC/run-$LABEL" "$SC/out-$LABEL"
"$SC/run-relay.sh" "$BIN" "$PORT" "$SC/run-$LABEL" &
RELAY_PID=$!
for i in {1..30}; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 2 "http://127.0.0.1:$PORT/readyz" || true)
  [[ "$code" == "204" || "$code" == "200" ]] && break
  sleep 1
done
echo "readyz=$code after ${i}s"

"$SC/replay.sh" "http://127.0.0.1:$PORT" "$SC/out-$LABEL" full
sleep 1
"$SC/dump_redis.sh" "$SC/out-$LABEL/redis.txt"

kill $RELAY_PID 2>/dev/null || true
sleep 1
pkill -f "vela-relay$" 2>/dev/null || true
echo "round $LABEL complete"
