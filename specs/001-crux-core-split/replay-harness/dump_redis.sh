#!/bin/zsh
# Dump all keys of the local sc6 redis, values pretty-printed, volatile fields masked.
set -eu
OUT=$1
docker exec vela-sc6-redis redis-cli --scan | sort | while read -r key; do
  type=$(docker exec vela-sc6-redis redis-cli type "$key")
  ttl=$(docker exec vela-sc6-redis redis-cli ttl "$key")
  # bucket TTLs: -1 stays, else round to nearest 60s to absorb run-time skew
  if [[ "$ttl" != "-1" && "$ttl" != "-2" ]]; then ttl=$(( (ttl + 30) / 60 * 60 )); fi
  echo "== $key type=$type ttl~${ttl}s"
  case "$type" in
    string)
      docker exec vela-sc6-redis redis-cli get "$key" | python3 -c '
import json,sys,re
raw = sys.stdin.read()
try:
    value = json.loads(raw)
    def mask(obj):
        if isinstance(obj, dict):
            return {k: ("<ms>" if re.search(r"AtMs$", k) else mask(v)) for k, v in obj.items()}
        if isinstance(obj, list):
            return [mask(item) for item in obj]
        return obj
    print(json.dumps(mask(value), sort_keys=True, separators=(",", ":")))
except Exception:
    print(raw.strip())
';;
    set) docker exec vela-sc6-redis redis-cli smembers "$key" | sort;;
    zset) docker exec vela-sc6-redis redis-cli zrange "$key" 0 -1;;
    hash) docker exec vela-sc6-redis redis-cli hgetall "$key";;
    *) echo "(unhandled type)";;
  esac
done > "$OUT"
echo "redis dump -> $OUT"
