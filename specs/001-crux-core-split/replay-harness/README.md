# SC-006 local replay harness

Approximates quickstart Gate 6 without staging infrastructure: two builds run
sequentially against identical fresh local infra (Docker Redis + Iggy), an
enqueue-only configuration (`VELA_RELAY_EXECUTOR_ENABLED=false`, so no chain
access, no keys, no broadcasts), and the same request battery; every response
body, normalized header set, and the Redis dump are compared byte-for-byte.
The production endpoint receives only the read-only / rejected-by-validation
subset (`safe_for_prod` in the manifest) — never an accepted operation.

```sh
python3 make_fixtures.py                       # writes battery/
./round.sh <old-binary> 4601 old               # fresh infra + full battery
./round.sh <new-binary> 4602 new
diff -r out-old out-new                        # expect empty
./replay.sh https://vela-relay.getvela.app out-prod safe
for f in out-prod/*.body; do cmp -s "$f" "out-new/$(basename "$f")" || echo "DIFF $f"; done
```

`round.sh` creates throwaway containers `vela-sc6-redis` (127.0.0.1:6390) and
`vela-sc6-iggy` (127.0.0.1:5190, `apache/iggy:latest` with
`--security-opt seccomp=unconfined` for io_uring and harness-local root
credentials). The battery covers: supported entry points, unknown method,
malformed JSON, wrong jsonrpc version, the nonzero-fee refusal, the
minimum-reimbursement rejection (empty calldata and one-wei-short), a valid
accept (fees `0x0`, native leg exactly `10^13` wei to the configured
recipient), its idempotent duplicate, status/byHash/receipt on the accepted
and an unknown hash, a bad EntryPoint, and the GET endpoints.

Verified 2026-08-28 (old = main @ 4e176db-era build, new = 8cf4a3e): local
old-vs-new fully byte-identical (21 responses + headers + Redis record);
production-vs-local identical on all 15 deterministic surfaces, the only
difference being `/version`'s build-sha field (CI embeds the full commit
hash, local builds a short one). Gate 5's `#[ignore]` Iggy producer test also
passes against the local `apache/iggy` container (SDK 0.10.3-edge.1 ↔ server
0.8.0).

What this does NOT cover: the executor money path (simulation, settlement,
funding, broadcast) — that surface is pinned by the 99 infrastructure-free
core tests; an on-chain E2E against staging remains the full Gate 6.
