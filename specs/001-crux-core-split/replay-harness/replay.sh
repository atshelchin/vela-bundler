#!/bin/zsh
# usage: replay.sh <base_url> <outdir> <mode: full|safe> [chain_id]
set -eu
BASE=$1
OUT=$2
MODE=$3
CHAIN=${4:-42161}
SC=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$OUT"

get() { # name path
  curl -s -m 15 -D "$OUT/$1.headers" -o "$OUT/$1.body" "$BASE$2" || echo "CURL-FAIL $1" >> "$OUT/errors.txt"
}

get root /
get health /health
get healthz /healthz
get readyz /readyz
get version /version

for f in "$SC"/battery/*-*.json; do
  name=$(basename "$f" .json)
  safe=$(python3 -c "import json,sys; m=json.load(open('$SC/battery/manifest.json')); print([e['safe_for_prod'] for e in m if e['file']=='$(basename "$f")'][0])")
  if [[ "$MODE" == "safe" && "$safe" != "True" ]]; then continue; fi
  curl -s -m 20 -D "$OUT/$name.headers" -o "$OUT/$name.body" \
    -X POST "$BASE/$CHAIN" -H 'content-type: application/json' \
    --data-binary @"$f" || echo "CURL-FAIL $name" >> "$OUT/errors.txt"
done

# normalize headers: keep only status line + x-vela-rpc-domain + content-type
for h in "$OUT"/*.headers; do
  grep -iE "^HTTP/|^x-vela-rpc-domain|^content-type" "$h" | tr -d '\r' > "$h.norm" && mv "$h.norm" "$h"
done
echo "replay done -> $OUT"
