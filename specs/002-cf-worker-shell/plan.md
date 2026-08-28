# Implementation Plan: Cloudflare Worker Shell (second deployment target)

**Branch**: `002-cf-worker-shell` | **Date**: 2026-08-28 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/002-cf-worker-shell/spec.md`

## Summary

Add a second shell — `vela-relay-cf`, a Rust Cloudflare Worker — that consumes
`vela-relay-core` unchanged. Durable Objects replace Redis for everything that
needs strong consistency and serialization (lifecycle records, lane execution,
treasury funding, delayed inbox, prepared intents); Cloudflare Queues replace
Iggy for the admission→execution hand-off; KV holds caches only; DO alarms and
queue retry delays replace resident timer loops. The platform's per-object
serialization structurally supplies the guarantees the docker shell builds from
leases and Lua guards — the core programs run verbatim, answered by different
arms. Byte-parity of the external surface is made structural by promoting
JSON-RPC envelope parsing/rendering into an additive pure `wire` module in the
core, consumed by both shells and gated by the existing replay battery. The
docker deployment stays behaviorally unchanged.

## Technical Context

**Language/Version**: Rust, edition 2024 throughout. New shell crate compiles
to `wasm32-unknown-unknown` via `workers-rs` (`worker = "0.8.5"` measured) and
`worker-build`/wrangler; feasibility pre-verified — `vela-relay-core` + `worker`
compile together, and a probe exercising real core paths (both crux apps,
hashing, lifecycle, hold) builds to a 749 KB wasm (222 KB gzipped), far under
platform size limits (research.md R1).

**Primary Dependencies**: NEW member `vela-relay-cf`: `worker` 0.8.x,
`vela-relay-core` (path), `getrandom` with the wasm backend feature (consumer-
side only; core untouched), `serde_json`. No tokio of its own, no axum, no
redis/iggy. Existing crates unchanged.

**Storage**: Durable Objects (three classes — RecordDO per operation, LaneDO
per (chain, lane), TreasuryDO per chain; see data-model.md) for all lifecycle
state, intents, locks, and the delayed inbox; Cloudflare Queues (+ DLQ) for the
envelope transport; KV for loss-harmless caches (chain metadata, market price,
gas quotes); Worker secrets/vars for configuration.

**Testing**: unchanged native gates for core + docker shell (`cargo fmt`,
`clippy`, `cargo test --locked` — the wasm-only member is excluded from the
native default build, so existing CI semantics hold). New: `cargo check
--target wasm32-unknown-unknown -p vela-relay-cf` gate; native unit tests for
the additive core `wire` module (byte-pinning the envelope); the 001 replay
battery run against `wrangler dev` (workerd emulates DO/Queues/KV locally) and
byte-compared against the docker deployment (SC-001); fault-injection scripts
for redelivery/reorder/DO-restart (SC-005).

**Target Platform**: Cloudflare Workers (V8 isolates, wasm), Durable Objects,
Queues, KV, Cron Triggers — plus the existing Linux/Docker target, unchanged.

**Project Type**: same JSON-RPC service, second deployment target; repo grows
from two to three workspace members (`vela-relay`, `vela-relay-core`,
`vela-relay-cf`).

**Performance Goals**: SC-004 — ≥1,000 accepted submissions/s and ≥10,000
reads/s aggregate, p95 submit <500 ms, p95 read <200 ms from three continents;
intake is stateless-per-request (unbounded); execution parallelism = chains ×
lanes (protocol-inherent), one LaneDO each.

**Constraints**: core additive-only (FR-003); every state class with guard
semantics on strongly consistent storage (FR-006 — KV is caches-only);
time-driven behaviors within declared tolerances without resident processes
(FR-007, tolerances in contracts/platform-bindings.md); execution ownership
disjointness (FR-010); platform limits respected (CPU/subrequest budgets per
batch — research.md R3/R5).

**Scale/Scope**: 5 stories; new crate estimated ~3–5 k LOC (thin arms — the
decisions already exist); core gains one additive `wire` module; docker shell
change is limited to delegating envelope rendering to `wire` (behavior-
preserving, battery-gated).

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | How the plan complies |
|---|---|---|
| I. Core/Shell Separation | PASS | `vela-relay-cf` is a SECOND shell: DO/Queues/KV/fetch wiring only; every decision consumed from `vela-relay-core`. The core stays I/O-free (wasm needs no exception — the randomness backend feature lives in the consumer crate). A PATCH amendment names the second shell in Principle I's illustrative list (task in Phase 2). |
| II. Determinism by Injection | PASS | Worker time (`Date.now` equivalent), generated tokens, chain context, and prices enter via start events/operation results exactly as today; secrets stay in shell arms (signing fns receive key bytes per call, unchanged). |
| III. Failure Is Data | PASS | The CF arms fold platform failures (DO fetch errors, queue send failures, RPC/fetch errors) into the SAME outcome variants; no new error channel. The absent-operation guarantees carry over (no record deletion op; `Interrupted` is simply never produced on this shell — vocabulary unchanged). |
| IV. Single Source of Truth | PASS | No rule moves into wrangler config, DO code, or JS glue: transition tables, schedules, budgets, parsers stay core-only. The `wire` module ADDS envelope vocabulary to the core precisely so neither shell re-encodes response bytes. |
| V. Infrastructure-Free Core Tests First | PASS | Core programs already pinned (99 tests). The `wire` module gets native byte-pinning tests BEFORE the docker shell delegates to it. CF arms are kept thin; their correctness is covered by the battery + fault-injection integration gates (shell-with-fakes testing where workers-rs permits; research.md R6). |
| VI. Behavior-Preserving Migration | PASS | Applies to the docker deployment: its only change is the `wire` delegation, landed behind green gates + unchanged battery output. The CF deployment's declared platform deltas (readyz semantics, ack granularity, alarm tolerances) are enumerated in contracts/deployment-parity.md and platform-bindings.md — the FR-012 equivalence-note skeleton. |

**Post-Phase-1 re-check**: PASS — the design introduces no violation. Two
liberties are recorded: (a) the `wire` module widens the core's scope from
business decisions to frozen wire vocabulary — justified because response bytes
ARE contract, drift between two shells is the exact disease Principle IV
exists to prevent, and the module stays pure; (b) LaneDO answers lease
operations with structural truths — a strictly stronger guarantee, declared in
the bindings contract rather than silently diverging.

## Project Structure

### Documentation (this feature)

```text
specs/002-cf-worker-shell/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output — DO catalog, queue topology, mapping table
├── quickstart.md        # Phase 1 output — the 002 gates
├── contracts/
│   ├── deployment-parity.md    # frozen external surface + declared platform deltas
│   └── platform-bindings.md    # Operation → CF primitive mapping + tolerances
├── checklists/requirements.md
└── tasks.md             # Phase 2 output (/speckit-tasks — not created here)
```

### Source Code (repository root)

```text
Cargo.toml                    # [workspace] members += "vela-relay-cf" (wasm-only member;
                              # native default build unchanged)
vela-relay-core/
└── src/
    └── wire.rs               # NEW (additive): JSON-RPC envelope parse/render for the
                              # eight methods + GET bodies; native byte-pinning tests

vela-relay-cf/                # NEW — the Cloudflare shell (wasm)
├── Cargo.toml                # worker 0.8.x + core + getrandom wasm backend; doctrine
│                             # comment: decisions live in vela-relay-core, never here
├── wrangler.jsonc            # DO classes + migrations, queue producer/consumer + DLQ,
│                             # KV namespace, cron trigger, vars/secrets
└── src/
    ├── lib.rs                # #[event(fetch)] / #[event(queue)] / #[event(scheduled)]
    ├── http.rs               # routing → wire dispatch → AdmissionApp driver + reads
    ├── record_do.rs          # RecordDO: record storage, guarded writes, TTL/receipt alarms
    ├── lane_do.rs            # LaneDO: ExecutionApp driver + executor arms, delayed inbox,
    │                         # prepared intent, broadcast cache, reconcile alarm
    ├── treasury_do.rs        # TreasuryDO: funding lock, funding intent, nonce serialization
    ├── arms/
    │   ├── rpc.rs            # chain RPC failover over fetch (transport-only policy)
    │   ├── market.rs         # Binance + metadata fetches with KV caches
    │   └── telegram.rs       # alert delivery (gating decided by core)
    └── config.rs             # env/secrets → validated policy values (data into core)

src/                          # docker shell: unchanged except rpc handlers delegating
                              # envelope bytes to vela_relay_core::wire (battery-gated)
```

**Structure Decision**: third workspace member, wasm-only, excluded from the
native default build so every existing gate keeps its meaning; the CF shell
gets its own build/deploy gates (quickstart.md). The core remains the only
shared code path between shells — plus the new `wire` module, which exists so
the shared surface includes the response bytes themselves.

## Complexity Tracking

No constitution violations to justify. The two recorded liberties (core `wire`
module scope; structural lease answers) are declared design decisions with
rationale in the Constitution Check and research.md, not gate violations.
