# SC-004 / SC-007 load harness (T022)

k6 scripts proving the scale success criteria against a REAL deployed
`vela-relay-cf` environment. Gate 5 in
[../quickstart.md](../quickstart.md).

## Target environment posture (mandatory)

- A dedicated load deployment on Workers Paid (`wrangler deploy` per
  [docs/cloudflare.md](../../../docs/cloudflare.md)), never the production
  worker.
- `VELA_RELAY_EXECUTOR_ENABLED=false` — SC-004 measures intake and reads;
  accepted operations must not reach any real chain. With the executor off,
  the queue consumer retries messages until the DLQ backstop; that is
  expected and exercises the durable path.
- A throwaway `OPERATOR_SECRET`; pass its derived vault address (the
  `settlement vault initialized vault_address=…` value, or
  `vault::derive_address`) as `RECIPIENT` so submissions pass the admission
  minimum-payment check — only ACCEPTED submissions count toward SC-004.
- Queue/KV/DO bindings provisioned as in docs; note queue backlog growth is
  expected for the 30-minute window (executor off) and is itself a useful
  observation.

## SC-004 — sustained load (three regions, 30 min)

Run ONE instance per region (e.g. three cloud VMs on different continents),
splitting the aggregate targets (≥1,000 submits/s, ≥10,000 reads/s):

```sh
k6 run -e BASE_URL=https://<worker-host> \
       -e RECIPIENT=<derived-vault-40hex> \
       -e CHAIN_ID=42161 \
       -e SUBMIT_RATE=334 -e READ_RATE=3334 \
       -e DURATION=30m -e REGION=a \
       load.js
```

Use `REGION=a|b|c` per instance (it salts the nonce space so hashes never
collide across regions). Pass criteria (encoded as k6 thresholds):

- p95 `http_req_duration{scenario:submits}` < 500 ms;
- p95 `http_req_duration{scenario:reads}` < 200 ms;
- `http_req_failed` rate < 0.1% (zero capacity-caused failures);
- `accepted_submissions` ≈ SUBMIT_RATE × duration (refusals ≈ 0);
- zero operator scaling actions during the window (Workers scale
  themselves; record that none were taken).

## SC-007 — per-chain isolation

Two runs from one region; compare the victim chain's p95s:

```sh
# 1. baseline: victim traffic only
k6 run -e BASE_URL=… -e RECIPIENT=… -e VICTIM_CHAIN=8453 \
       -e SATURATE_RATE=0 -e DURATION=5m isolation.js
# 2. experiment: saturate a different chain at full tilt
k6 run -e BASE_URL=… -e RECIPIENT=… -e VICTIM_CHAIN=8453 \
       -e SATURATED_CHAIN=42161 -e SATURATE_RATE=1500 -e DURATION=5m isolation.js
```

Pass: `http_req_duration{chain:victim}` p95 (submits AND reads) degrades by
<10% between run 1 and run 2.

## Smoke check (local, not the gate)

The scripts run unmodified against `wrangler dev` at tiny rates to verify
wiring (localhost cannot host the real rates or regions):

```sh
k6 run -e BASE_URL=http://127.0.0.1:8787 -e RECIPIENT=<dev vault> \
       -e SUBMIT_RATE=5 -e READ_RATE=20 -e DURATION=30s load.js
```

## Result record

| Run | Date | Regions | Submits/s (agg) | Reads/s (agg) | p95 submit | p95 read | Failures | Verdict |
|---|---|---|---|---|---|---|---|---|
| SC-004 | _pending real deployment_ | | | | | | | |
| SC-007 baseline | _pending_ | | | | | | | |
| SC-007 saturated | _pending_ | | | | | | | |
