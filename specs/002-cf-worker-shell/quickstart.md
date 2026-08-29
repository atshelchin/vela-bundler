# Quickstart: Validating the Cloudflare Worker Shell

How to prove, at any merge point, that the second deployment target is on
contract. See [data-model.md](data-model.md) for the object catalog and
[contracts/](contracts/) for the frozen surfaces.

## Gate 0 — Existing deployments untouched (every commit)

```bash
cargo fmt --check
cargo clippy --all-targets --locked     # baseline warnings, add none
cargo test --locked                     # docker shell suite
cargo test -p vela-relay-core --locked  # core suite (includes new wire tests)
```

Expected: green, with the docker shell's behavior unchanged (FR-003/SC-002).

## Gate 1 — Wasm build + purity audit

```bash
cargo check -p vela-relay-cf --target wasm32-unknown-unknown --locked
cargo tree -p vela-relay-core -e normal   # still zero IO/runtime crates
grep -rn "transition_is_allowed\|retry_delay\|parse_reimbursement" vela-relay-cf/src/  # expect: only core:: calls, no local definitions (SC-003)
```

## Gate 2 — Local platform emulation + replay battery (SC-001)

```bash
cd vela-relay-cf && npx wrangler dev &          # workerd: DO + Queues + KV emulated
../specs/001-crux-core-split/replay-harness/replay.sh http://127.0.0.1:8787 out-cf full
# docker side (same battery, same fixtures):
../specs/001-crux-core-split/replay-harness/round.sh <docker-shell-binary> 4601 docker
diff -r out-cf out-docker    # bodies byte-identical for every deterministic surface
```

Expected: byte-identical response bodies; RecordDO record JSON identical to the
Redis record (masked-timestamp normalization as in the harness).

## Gate 3 — Execution dispositions under fault injection (SC-005)

Scripted against `wrangler dev`:

- duplicate delivery of one envelope → second delivery resolves via
  durable-status skip; one nonce consumed;
- reordered nonces from one sender → future nonce parks in the delayed inbox
  (LaneDO alarm set), stale nonce rejects — same reasons as the core tests pin;
- kill/restart between RecordDO create and queue send → crash-window behavior
  (record retained, idempotent resubmission);
- forced DO restart mid-batch → prepared-intent resume, no
  same-nonce-different-bytes broadcast;
- concurrent consumer scale-out on one lane → single LaneDO serializes; no
  interleaved execution.

### Gate 3 as run (T018, anvil rig)

Rig: anvil claiming chain 42161 (`--block-time 3`), the real EntryPoint v0.7
runtime installed via `anvil_setCode`, trivially-valid account contracts at
the fixture senders, the treasury funded 10 ETH, and a JSON-RPC shaper between
the worker and anvil (`scratchpad/t018/shaper.py`) that (a) restores the
production error shapes anvil strips — anvil returns FailedOp custom-error
reverts with empty `data` in every tier, so the shaper re-derives the AA25
cause from `EntryPoint.getNonce` and surfaces it exactly as geth's tracer
would — and (b) injects faults on demand. Local chain metadata was seeded
into the dev KV (`chainmeta:42161`, `rpc: []`) so no executor traffic can
leave localhost. Results:

- **Full batches landed**: four complete treasury funding cycles (treasury
  nonce 0→4, each `SET NX`-guarded intent saved/probed/cleared through
  TreasuryDO) and four mined `handleOps` bundles across four lanes — via BOTH
  simulation tiers (`eth_simulateV1`, and `debug_traceCall` with the shaper
  refusing simulateV1). Records reached `submitted` with the mined hashes.
- **Nonce triage**: future nonce → `future account nonce moved to durable
  delayed inbox … user_nonce=3 onchain_nonce=1 attempt=1` (parked, alarm
  set); stale nonce → `stale account nonce rejected … user_nonce=0
  onchain_nonce=1` (terminal). Frozen texts, real probe values.
- **Same-lane burst**: six same-sender ops (nonces 0–5) sent concurrently →
  exactly one outer transaction (relayer nonce advanced by exactly 1), the
  five future ops held; single LaneDO serialized the whole burst.
- **Restart + resume, zero double-broadcast**: the lane's prepared intent and
  the parked delayed-inbox entries survived a hard kill of every wrangler and
  workerd process; after restart, redeliveries re-ran `ResumeBundleIntent`
  17 more times (≈100 times pre-restart) — the relayer nonce never moved and
  no second funding/bundle transaction ever appeared. At-least-once
  redelivery and the queue's `max_retries` → DLQ backstop (observed at 101
  attempts) both behaved.
- **Known pre-US3 limitation (by design)**: with receipt confirmation not yet
  landed (T020), a submitted bundle's members never reach a terminal state,
  so the lane's intent is retained and new work on that lane redelivers until
  the DLQ backstop — the core's resume-first rule working as specified; US3
  closes it.
- **Found and fixed by this gate**: `PreparedFundingIntent.amount_wei: u128`
  degraded to a float crossing the TreasuryDO boundary (workers-rs
  `Request::json` parses via JS `JSON.parse` → JsValue). The whole TreasuryDO
  boundary now speaks JSON text and stores the funding intent as its JSON
  string (see platform-bindings.md).
- Not externally injectable: a kill between RecordDO create and queue send
  (sub-millisecond window); the crash-window contract is declared on the
  `Enqueue` row of platform-bindings.md and matches the docker shell's.

## Gate 4 — Time-driven tolerances (SC-006)

Park an operation at attempt *k*; assert redelivery within the tolerance
declared in [contracts/platform-bindings.md](contracts/platform-bindings.md);
observe a reconcile alarm pass and a receipt-check alarm against the emulator's
clock.

## Gate 5 — Load and isolation (SC-004/SC-007, pre-production)

Deployed environment: sustained-rate submission/read load from three regions
(k6 or equivalent), 30 minutes, with the SC-004 targets; then saturate one
chain and assert other chains' p95 degradation <10%.

## Gate 6 — Ownership review (FR-010)

```bash
# per deployment: the execution allowlist
grep EXECUTION_CHAINS vela-relay-cf/wrangler.jsonc .env* 
```

Expected: no (chain, key set) enabled for execution in more than one
deployment; recorded in the deploy checklist.

## Story completion map

| Story | Done when |
|---|---|
| 1 Edge intake/reads | Gates 0–2 pass with execution disabled |
| 2 Edge execution | Gate 3 passes; a full batch lands on a testnet via `wrangler dev`/staging |
| 3 Time-driven | Gate 4 passes |
| 4 Scale | Gate 5 passes |
| 5 Coexistence | Gate 0 on the final merge + Gate 6 review |
