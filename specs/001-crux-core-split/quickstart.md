# Quickstart: Validating the Crux Core/Shell Split

How to prove, at any merge point, that the refactor is on contract. See
[data-model.md](data-model.md) for types and [contracts/](contracts/) for the
frozen surfaces.

## Prerequisites

- Rust toolchain (edition 2024), same as today's CI.
- No infrastructure needed for the core checks — that is the point.
- Optional gated checks need Redis / Iggy endpoints (same env vars as today).

## Gate 0 — Repository health (every commit)

```bash
cargo fmt --check
cargo clippy --all-targets --locked   # matches CI; 9 pre-existing warnings on main, add none
cargo test --locked
```

Expected: all green, with the pre-existing 160 shell tests unmodified (SC-002).

## Gate 1 — Core purity audit (SC-003)

```bash
cargo tree -p vela-relay-core -e normal
```

Expected: computation crates only — **no** tokio, axum, redis, iggy, reqwest, or
any client/runtime crate. The `vela-relay-core/Cargo.toml` header comment states the
no-I/O rule (Constitution I).

## Gate 2 — Infrastructure-free core suite (SC-001)

```bash
time cargo test -p vela-relay-core --locked
```

Expected: completes in under 10 seconds, no network, no running services; includes
Driver-harness walks of the admission program and the lane-batch program, each with
at least one failure-injection scenario (SC-005), and each asserting the
one-operation-in-flight invariant.

## Gate 3 — Single source of truth (SC-004)

```bash
# Transition tables: only the core defines one.
grep -rn "allowed = {" src/ vela-relay-core/src/ | grep -v test
# The test-only mirror must be gone:
grep -rn "transition_is_allowed" src/
# Backoff doubling must not remain in Lua:
grep -n "delay \* 2" src/app/user_operation_store.rs
# Reimbursement parsing exists once:
grep -rln "TRUSTED_MULTISEND" src/ vela-relay-core/src/
```

Expected: transition table and backoff live only under `vela-relay-core/src/`;
`TRUSTED_MULTISEND` appears in exactly one crate (Story 6 done).

## Gate 4 — Behavior equivalence (per story)

For each landed story, its PR's equivalence note (FR-011) maps old path → new core
decision. Spot-check reason strings survived byte-identically:

```bash
# Example: hold/rejection reasons (Story 2/3)
grep -rn "settlement" vela-relay-core/src/ --include="*.rs" | grep -i reason
```

Compare against the strings previously in `src/worker/executor/engine.rs`
(`settlement_hold_reason` / `settlement_rejection_reason`).

## Gate 5 — Gated integration checks (unchanged from today, optional)

```bash
# Live Iggy producer test (pre-existing, still the only #[ignore] in-tree):
cargo test --locked -- --ignored   # with P256-style env: the repo's documented
                                   # VELA_RELAY_* test endpoints configured
```

Expected: same behavior as before the refactor.

## Gate 6 — End-to-end smoke (operator, SC-006)

Run old and new builds against the same staging infrastructure and replay
production-shaped traffic (the eight JSON-RPC methods, `contracts/external-api.md`).
Expected: byte-identical HTTP responses; identical stored lifecycle records;
identical on-chain calldata for identical inputs.

## Story completion map

| Story | Done when |
|---|---|
| 1 Lifecycle | Gate 3 transition greps pass; core tests pin all legal/illegal transitions; Lua reduced to guarded CAS |
| 2 Hold | Ladder tests enumerate attempts 1→13; backoff gone from Lua |
| 3 Settlement | Verdict table-tests (accept/reprice/hold/reject + boundaries) pass with no infra |
| 4 Execution | Driver walk of a full batch + failure injections; `handle_batch` drives the core; Gate 2 time still <10 s |
| 5 Admission | Driver walk incl. crash-window scenario; HTTP responses unchanged (existing handler tests) |
| 6 Reimbursement | Gate 3 `TRUSTED_MULTISEND` grep shows one crate; both former test suites pass against the one parser |
