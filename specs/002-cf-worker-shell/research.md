# Research: Cloudflare Worker Shell

Decisions with rationale and alternatives. Platform facts verified against
Cloudflare docs and workers-rs 0.8.5 on 2026-08-28; local feasibility probes
run the same day.

## R1 — Feasibility: the core compiles and fits

**Decision**: build the second shell on `worker = "=0.8.5"` (pinned; pre-1.0
API churn is real) over `wasm32-unknown-unknown`.

**Evidence**: `vela-relay-core` + `worker` 0.8.5 compile together unchanged;
a probe exercising real paths (both crux apps, `user_operation_hash`,
lifecycle, hold) builds to **749 KB wasm / 222 KB gzipped** with
`opt-level="z"`+LTO — against a 10 MB (Paid) compressed limit. The only
tree-level accommodation is the consumer-side randomness backend
(`getrandom` 0.2 `js` feature; k256 signing is RFC6979-deterministic so
entropy is compile-time-only). secp256k1 signing in wasm is ~0.3–2 ms —
negligible against the 30 s (→5 min) Paid CPU budget. **Workers Paid is
required** (Free's 10 ms CPU cannot host crypto paths).

**Alternatives considered**: JS/TS shell calling a wasm core (rejected: a
second language reintroduces the drift disease 001 eliminated); containers on
CF (rejected: that's the docker shell again, without the platform's scaling
model).

## R2 — State home: SQLite-backed Durable Objects; KV for caches only

**Decision**: all lifecycle state, intents, locks, and the delayed inbox live
in Durable Objects (SQLite-backed classes, `new_sqlite_classes`); KV holds
only loss-harmless caches (chain metadata, market price, gas quotes).

**Rationale**: DOs are strongly consistent, transactional, single-threaded
per object, with output gates confirming writes before outbound messages —
they structurally supply what the docker shell builds from Lua CAS + leases
(FR-006). KV is eventually consistent (~60 s propagation, 1 write/s per key,
no read-modify-write) — explicitly disqualified for records/locks by FR-006,
ideal for the read-heavy caches whose loss today is also harmless.

**Alternatives**: D1 (a single global database — no per-entity serialization,
reintroduces cross-request races the DO model eliminates); KV with
version-stamps (cannot provide CAS; rejected).

**Facts that shaped the object catalog** (data-model.md): one alarm per DO
(set overwrite semantics) → each class packs its schedules as earliest-of;
storage get/put batched ≤128 keys; key+value ≤2 MB (records are ~2–10 KB ✓);
~1,000 req/s soft ceiling per object → per-operation RecordDOs and per-lane
LaneDOs shard naturally, no hot object; DO requests have no wall-clock limit
(alarms 15 min); 6 simultaneous outbound connections per object — a perfect
match for the crux driver's strictly sequential one-operation-in-flight
invariant.

## R3 — Serialization: structural for lanes, explicit lock for treasury

**Decision**: LaneDO's single-threaded input gate IS the lane lease —
`AcquireLaneLease`/`EnsureLaneLease` answer `true` structurally and the
interrupt path is unreachable on this shell. The treasury keeps a REAL lock
(holder token + deadline in TreasuryDO storage) because the core program holds
it across a multi-operation session invoked from different LaneDOs.

**Rationale**: the lease/heartbeat/interrupt machinery exists because Redis is
shared-mutable; a DO serializes per entity for free — a strictly stronger
guarantee, declared in contracts/platform-bindings.md rather than silently
diverged. The treasury case genuinely spans requests, so the existing lease
vocabulary (acquired:false → funding-defer) maps to explicit lock state whose
races the DO's single thread eliminates.

**Alternatives**: forwarding whole funding sub-sequences into TreasuryDO
(rejected: splits one core program across two drivers); global lock DO
(rejected: needless cross-chain coupling).

## R4 — Queue semantics: at-least-once, unordered — sequence in DOs

**Decision**: one Queues queue (+ DLQ) carries the frozen envelope JSON;
the consumer groups by (chain, lane) via `relayer_index_for_sender` and
forwards each group to its LaneDO; ack per message from `ItemResolution`
(`Durable` → ack, `Failed` → retry).

**Facts**: Queues guarantee at-least-once and **no ordering**; batching ≤100
msgs/batch (default 10), consumer autoscales to 250 concurrent invocations;
per-message `ack()`/`retry()` with `delaySeconds` ≤24 h; DLQ after
max_retries; 5,000 msg/s per queue; 128 KB/message (envelopes are KBs ✓);
one active consumer Worker per queue — the CF shell owns its queue
exclusively (the docker shell never consumes it; structural FR-010 aid).

**Rationale**: reordering and redelivery are already absorbed by business
rules (durable-status skip, dedupe, nonce triage future→delay / stale→reject)
— the spec was written for exactly this transport class. Sequencing lives in
LaneDO, so consumer concurrency scales intake without touching correctness.
Per-message ack replaces Iggy's contiguous-durable-prefix: durability still
gates acknowledgment; only redundant redelivery of already-durable items
disappears (declared delta, deployment-parity.md).

**Alternatives**: per-lane queues (rejected: one-consumer-per-queue ×
chains×10 lanes explodes config and deploys for zero correctness gain);
`max_concurrency=1` global serialization (rejected: kills intake scale and
still doesn't order); relying on Rust-side `message.attempts` for hold
accounting (rejected: exposure uncertain in workers-rs — attempt counters
stay in DO storage as today they stay in Redis).

## R5 — Time-driven behaviors: DO alarms, packed earliest-of

**Decision**: RecordDO alarm = min(TTL expiry, next receipt check); LaneDO
alarm = min(delayed-inbox due, reconcile-while-intent-exists). Cron trigger
retained only as a coarse sweep/backstop if implementation finds an orphan
class (decided in tasks).

**Facts**: alarms are at-least-once with auto-retry (2 s backoff, ≤6
retries), 15-min handler wall-clock, ms precision, billed as one request.
At-least-once alarm firing is safe: every alarm action re-derives from stored
state via core rules (idempotent by construction).

**Alternatives**: queue `delaySeconds` for the hold ladder (workable — ≤24 h
covers the 300 s cap — but splits delayed-inbox state between queue and DO;
rejected for auditability: the DO owns payload+attempts+due exactly like
Redis does today); global cron sweeps (rejected as primary: wasteful scans,
coarse granularity; kept as optional backstop).

## R6 — Testing strategy

**Decision**: (a) core + new `wire` module: native `cargo test`
(byte-pinning envelope tests land BEFORE the docker shell delegates —
Constitution V); (b) CF shell e2e: `wrangler dev` (workerd emulates DO
storage+alarms, Queues, KV locally) driven by the 001 replay harness for
byte-parity (Gate 2) and by fault-injection scripts (Gate 3); cron via
`wrangler dev --test-scheduled`; (c) keep CF arms thin so shell-only logic
approaches zero.

**Facts**: workers-rs has no Rust-native DO test harness; the endorsed
integration path is miniflare/workerd-based e2e. Accepted: our replay battery
IS that harness, already built and already the parity oracle.

## R7 — Workspace mechanics

**Decision**: third member `vela-relay-cf`, excluded from native default
builds: `workspace.default-members` keeps today's members; CI adds
`cargo check -p vela-relay-cf --target wasm32-unknown-unknown` and any
`--workspace` invocations gain `--exclude vela-relay-cf`. Wasm-only deps sit
under `[target.'cfg(target_arch = "wasm32")'.dependencies]`; entry points are
`#[cfg(target_arch = "wasm32")]`-gated so an accidental native check compiles
an empty crate. `worker` pinned `=0.8.5`.

**Rationale**: every existing gate keeps its exact meaning (FR-003); the
resolver-2 feature isolation (edition 2024) keeps `getrandom/js` out of
native builds.

## R8 — Byte-parity is structural: the core `wire` module

**Decision**: promote JSON-RPC envelope parsing + response rendering (the
eight methods, error codes/messages, GET bodies) into an additive pure
`vela_relay_core::wire` module consumed by both shells; the docker shell
delegates behavior-preservingly (its tests + replay battery pin the bytes).

**Rationale**: response bytes ARE the frozen contract; two hand-maintained
renderings would be the exact drift disease Principle IV exists to prevent.
A pure module keeps Constitution I intact (no I/O enters the core).

**Alternatives**: a third `vela-relay-api` crate (rejected: adds a workspace
member without adding isolation the module doesn't already have); per-shell
duplication with battery-only enforcement (rejected: the battery catches
drift after the fact; the module prevents it).

## R9 — Cost and limits envelope

Paid plan assumed. Per-batch subrequests (10–30 RPC + ~10 RecordDO calls)
vs 10,000/invocation: no issue. DO duration bills wall-clock at a flat
128 MB while a lane awaits chain RPCs (~$1.6e-6 per lane-second) —
negligible at relay scale; noted because LaneDO deliberately hosts the RPC
waits to preserve serialization. Intake fetch handlers are stateless and
scale unbounded; the platform's stated ceilings (5,000 msg/s/queue, ~1,000
req/s/DO) sit far above SC-004 targets with per-entity sharding.

## R11 — Lane width: 10 by default, up to the pool's 100 as deployment policy

**Facts**: `OPERATOR_SECRET` derives a pool of `RELAYER_POOL_SIZE = 100`
relayer EOAs (`relayer-#0..99`, chain-agnostic addresses); the docker shell
routes across `RELAYER_ROUTING_WIDTH = 10` of them, and its config REJECTS any
other width solely because "the Iggy queue uses fixed routing" (fixed
partition count). The 100-EOA pool is already core vocabulary.

**Decision**: the CF shell keeps width 10 as its default (posture parity with
the docker deployment) but treats `VELA_RELAY_RELAYER_COUNT` as a genuine
1..=100 deployment policy: routing is computed per envelope
(`relayer_index_for_sender(sender, width)`), LaneDOs are addressed
`{chain}:{lane}`, and no fixed-partition constraint exists on this platform.
Raising width multiplies execution parallelism per chain (up to 100 LaneDOs)
without touching any business rule.

**Trade-offs (operator-facing, documented in the deploy checklist)**: more
active EOAs = proportionally more float capital locked per chain and a longer
cold-start funding phase (treasury funding stays serialized per chain — one
outstanding funding transaction, by existing core rule); width is fixed per
deployment lifetime (changing it remaps sender→lane and strands in-flight
lane state), so it must be chosen at provisioning time. Invisible to API
consumers; no parity impact because the CF deployment holds its own keys
(FR-010).

## R10 — Execution ownership (FR-010)

**Decision**: a per-deployment `EXECUTION_CHAINS` allowlist consulted by
`CheckChainSupported`'s arm before any directory lookup; deploy checklist
asserts global disjointness for any shared key material. The
one-consumer-per-queue platform rule plus per-deployment queues/stores makes
cross-deployment execution of one envelope structurally impossible; the
allowlist closes the remaining risk (same chain configured with the same
keys in both deployments' env).
