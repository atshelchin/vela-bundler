# Implementation Plan: Crux Core/Shell Split (vela-relay-core extraction)

**Branch**: `001-crux-core-split` | **Date**: 2026-08-28 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/001-crux-core-split/spec.md`

## Summary

Extract every business decision in vela-relay into a new I/O-free core crate
`vela-relay-core`, driven by the crux `Command`/`Operation` engine in the per-unit-of-work
paradigm proven by `p256-index`. The existing crate becomes the shell: Axum, Redis,
Iggy, EVM RPC, Binance, and Telegram execute the core's requested operations and feed
results back as data. The migration is behavior-preserving (byte-identical responses,
records, reason strings, thresholds) and lands as six independently mergeable stories
ordered by risk-removed-per-effort: lifecycle FSM out of Lua → unified hold policy →
pure settlement verdict → executor batch program → admission program → single
reimbursement parser.

## Technical Context

**Language/Version**: Rust, edition 2024 (existing toolchain; CI: `cargo fmt --check` + `cargo clippy --all-targets --locked` + `cargo test --locked`; no new warnings allowed)

**Primary Dependencies**: NEW `crux_core = "0.19"` (core + shell). Existing, shell-only:
axum 0.8, tokio 1, redis 0.32, iggy 0.10.3-edge.1, alloy 2.2, reqwest 0.12, k256,
hkdf/sha2/sha3. Core crate depends only on computation crates (alloy `sol-types`
subset, serde, hex, sha2/sha3, k256 for deterministic signing math) — no runtime, no
network, no storage clients.

**Storage**: Redis (shell only; 16 Lua scripts reduced to mechanical guarded writes),
Iggy streams (shell only). The core never sees a connection.

**Testing**: `cargo test`. Core: pure state-machine tests with a scripted `Driver`
harness (expected Operation in → scripted Result back → settled outcome asserted;
"at most one operation in flight" invariant). Shell: existing 160 tests stay green
unmodified at every merge point; gated e2e tests unchanged.

**Target Platform**: Linux server (Docker) / macOS dev. Single process, two tokio
runtimes (HTTP + worker) — unchanged.

**Project Type**: JSON-RPC web service + background worker; repo converts from a
single crate to a two-member Cargo workspace.

**Performance Goals**: No regression. Core decisions are in-process state-machine
calls (`Core::process_event`/`resolve` resolve synchronously; no serialization
crosses the boundary), so per-request overhead is negligible versus the network
calls that dominate today.

**Constraints**: Behavior-preserving money path (FR-003); every merge point green
(FR-010); equivalence notes for money-path PRs (FR-011); core test suite completes
in under 10 s with zero infrastructure (SC-001).

**Scale/Scope**: ~21.7k LOC today; ~6 stories; the executor monolith
(`src/worker/executor/engine.rs`, 3,827 lines) is the largest single decomposition.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | How the plan complies |
|---|---|---|
| I. Core/Shell Separation | PASS | New `vela-relay-core` crate is I/O-free with the manifest-comment rule; modules are business domains (lifecycle, hold, settlement, execution, admission, funding, broadcast). Plan-style pure functions used where the crux engine doesn't fit. |
| II. Determinism by Injection | PASS | Timestamps, generated ids, chain context, market prices enter via start events or Operation results (see contracts/core-shell-operations.md). No clock/randomness/env in core; raw keys and raw IPs never cross. |
| III. Failure Is Data | PASS | Every Operation `Output` enumerates failure variants (`StoreUnavailable`, `ChainReadFailed`, `QueueUnavailable`, `ReceiptUncertain`, …); today's per-case policy (e.g. fail-open chain pre-checks) is preserved in core decisions. No "undo admission" operation exists. |
| IV. Single Source of Truth | PASS | Story 1 moves the lifecycle table from `PATCH_RECORD_SCRIPT` Lua into the core and deletes the `#[cfg(test)]` mirror; Story 2 moves the hold backoff schedule out of Lua. Scripts become guarded CAS writes. |
| V. Infrastructure-Free Core Tests First | PASS | Each story lands Driver-harness tests pinning migrated behavior before old code is deleted; sequential-invariant assertion included. |
| VI. Behavior-Preserving Migration | PASS | Spec declares zero behavior changes; the one candidate change found (placeholder jobs gating readiness) is explicitly deferred. Equivalence notes required on money-path PRs. |

**Post-Phase-1 re-check**: PASS — the data model and contracts introduce no new
violation; the only structural liberty taken (root package + member crate instead of
p256's pure-members layout) is a layout choice the constitution does not govern,
justified in research.md R4.

## Project Structure

### Documentation (this feature)

```text
specs/001-crux-core-split/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   ├── core-shell-operations.md   # internal Operation/Result vocabularies per app
│   └── external-api.md            # frozen external HTTP/JSON-RPC contract
├── checklists/requirements.md
└── tasks.md             # Phase 2 output (/speckit-tasks — not created here)
```

### Source Code (repository root)

```text
Cargo.toml                    # gains [workspace] members = ["vela-relay-core"];
                              # root [package] remains vela-relay (the shell)
vela-relay-core/                 # NEW — the decision core (I/O-free)
├── Cargo.toml                # computation deps only + the "no I/O" doctrine comment
└── src/
    ├── lib.rs                # doctrine + business-domain module map
    ├── task.rs               # shared vocabulary: routed/stored operation, status enum
    ├── lifecycle.rs          # Story 1: the single status transition table
    ├── hold.rs               # Story 2: hold ladder (backoff schedule + budget + reasons)
    ├── settlement.rs         # Story 3+6: settlement verdict + single reimbursement parser
    │                         #   (absorbs worker/executor/settlement.rs and the HTTP copy)
    ├── execution.rs          # Story 4: ExecutionApp — per-lane batch program
    ├── admission.rs          # Story 5: AdmissionApp — two-phase enqueue program
    ├── funding.rs            # Story 4: relayer/treasury top-up plan (pure)
    ├── broadcast.rs          # Story 4: broadcast-outcome classification table (pure)
    ├── bundle.rs             # Story 4: abi packing, userOpHash, gas allocation, receipts
    │                         #   (absorbs abi.rs, cost.rs, receipt.rs)
    └── signing.rs            # deterministic tx signing math (absorbs transaction.rs;
                              #   key bytes injected by the shell per call)

src/                          # the shell (existing crate, thinned)
├── app/…                     # Axum transport; rpc handlers call AdmissionApp/core fns
├── worker/…                  # consumer, lane pool, jobs; executor becomes the
│                             #   Operation executor + engine driver loop
├── gas_price/…               # fee-history polling stays; math helpers migrate to core
└── utils/…                   # config env-parsing, rpc failover, iggy reconnect stay;
                              #   vault/tempo/alchemy pure helpers migrate to core
```

**Structure Decision**: two-member workspace with the root package remaining
`vela-relay` at the repo root and `vela-relay-core/` as a new member directory. This
differs from p256-index's pure-members layout deliberately: it avoids moving ~21.7k
lines (preserving git history, CI paths, Dockerfile contexts) while achieving the
identical dependency discipline — the direction of dependency is
`vela-relay → vela-relay-core`, never the reverse, and `vela-relay-core`'s manifest carries
the no-I/O rule (Constitution I). See research.md R4.

## Complexity Tracking

No constitution violations to justify. (The workspace-layout deviation from the
reference project is a style choice, not a gate violation; recorded in research.md.)
