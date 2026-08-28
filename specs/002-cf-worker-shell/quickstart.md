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
