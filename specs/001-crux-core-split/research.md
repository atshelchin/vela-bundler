# Phase 0 Research: Crux Core/Shell Split

**Date**: 2026-08-28. Research was performed as three deep code surveys before this
plan: (1) `p256-index` (`/Volumes/data/production/p256-index`) — the server-side crux
reference; (2) `crux-demo` (`/Volumes/data/production/crux-demo`) — crux 0.19 usage
patterns and methodology docs; (3) the current vela-relay codebase — full
architecture, IO inventory, and entanglement map. No NEEDS CLARIFICATION markers
remained in the spec; the research items below record the decisions that shaped the
design.

---

## R1 — Core paradigm: per-unit-of-work cores, not long-lived state machines

**Decision**: Follow `p256-index`: one `Core::new()` per unit of work (one HTTP
admission, one consumed lane batch), a `Model` that holds `Option<Outcome>`, the
whole business program written as a sequential async `Command` that `await`s
`ctx.request_from_shell(op)` per step and settles via a `Settled` event; the shell
reads `view().outcome` and discards the core.

**Rationale**: vela-relay's work is naturally unit-shaped (a request, a batch).
Long-lived in-process state (Iggy sessions, lane actors, lease heartbeats) is
operational, not business, and p256-index demonstrates it belongs in the shell.
One-shot cores keep the Model trivially auditable and make crash-recovery reasoning
identical to today's (durable state lives in Redis, not in a resident core).

**Alternatives considered**: crux-demo's long-lived core with correlation ids
(request_id/mutation_id guards) — right for UI sessions, wrong here: a resident
model would duplicate durable Redis state and add a second source of truth,
violating Constitution IV in spirit. Rejected except for its correlation-guard idea,
which we keep where the shell resumes prepared intents.

## R2 — Engine and effect vocabulary: crux_core 0.19, custom Operations, no stock capabilities

**Decision**: `crux_core = "0.19"` with the Command/Operation API. Each app defines
its own `Operation` enum (`impl crux_core::capability::Operation { type Output = …Result; }`)
wrapped in a single-variant `#[effect] pub enum …Effect { Work(…Operation) }`.
No `crux_http`/`crux_kv`/`crux_time`, no typegen, no foreign shells.

**Rationale**: both reference repos converge on exactly this shape; the stock
capability crates model browser/mobile IO, not Redis/Iggy/EVM-RPC. A custom
vocabulary lets the effect set be deliberately incomplete as a safety property
(Constitution III): e.g. no operation exists to un-admit a durably admitted
operation.

**Alternatives considered**: legacy capability-struct API (pre-0.19) — deprecated
shape, more boilerplate. Multi-variant effect enums per infrastructure kind —
rejected; dispatch on the inner Operation enum is simpler and matches both
references.

## R3 — Failure mapping convention

**Decision**: Shell executors never return `Err` to the core. Every `…Result`
enumerates failure variants; the flat executor `match` folds each infrastructure
error into one (`StoreUnavailable`, `QueueUnavailable`, `ChainReadFailed`,
`TxOutcome::ReceiptUncertain{…}`, …). Core programs use `Result<T, Outcome>` purely
as an early-return channel, collapsed with `Ok(o) | Err(o) => o`.

**Rationale**: the *meaning* of a failure is business policy (vela-relay today:
chain pre-checks fail-open, settlement holds on missing price where currently
fail-closed, batch retries without offset advance). p256-index proves the pattern
and its tests pin exactly these policies.

**Alternatives considered**: `anyhow`-style error bubbling to the shell — rejected:
policy would migrate back into the shell, silently, per call site.

## R4 — Workspace layout

**Decision**: Root package stays `vela-relay` (shell) at the repo root;
`vela-relay-core/` is added as a workspace member. `Cargo.toml` gains
`[workspace] members = ["vela-relay-core"]`, dependency direction
`vela-relay → vela-relay-core` only.

**Rationale**: avoids moving ~21.7k lines into a subdirectory — preserves git
blame/history, CI paths, `Dockerfile` build contexts, and the
`src/bin/deploy_simulations.rs` `#[path]` include. The constitutional property
(dependency discipline + manifest doctrine) is layout-independent.

**Alternatives considered**: pure-members layout like p256-index
(`vela-relay-core/` + `vela-relay/`) — cleaner symmetry, but the mass move is pure
churn in a behavior-preserving migration and can be done later as a trivial
follow-up if desired.

## R5 — Lua reduction strategy (Stories 1–2)

**Decision**: Move the lifecycle transition table (`PATCH_RECORD_SCRIPT`'s `allowed`
table, `src/app/user_operation_store.rs:45-49`) into `vela-relay-core::lifecycle` as
the production rule; promote/delete the `#[cfg(test)] transition_is_allowed` mirror
(`:1630`). The Lua script keeps only mechanical guards that protect against
concurrent writers (current-status compare, ownership-token compare) — the *decision*
of whether a transition is legal is made in Rust before the call, and the script's
compare acts as an optimistic-concurrency check, not a policy. Same treatment for
the delayed-inbox backoff schedule (`:207-217`, `:283-293`): the core computes the
deferral delay; the script stores what it is told.

**Rationale**: Constitution IV. Concurrency guards must stay server-side (two
processes can race), but guards are mechanical: "is the stored status still X",
"does the token match" — no business table needed in Lua.

**Alternatives considered**: keeping the Lua table and generating it from Rust at
build time — rejected: still two runtimes executing policy, and generation machinery
is more complex than the CAS reduction. Removing Lua entirely (plain
GET/compare/SET from Rust) — rejected: loses atomicity against concurrent writers.

## R6 — What migrates, what stays

**Decision — migrates into `vela-relay-core` essentially unchanged** (already pure):
`worker/executor/settlement.rs` (evaluation + repricing math),
`worker/executor/{abi,cost,receipt}.rs`, `worker/executor/transaction.rs` (signing
math; key bytes injected per call), `utils/{vault,tempo,alchemy}.rs`,
`app/rpc/handlers/in_band_settlement.rs` (merged into core settlement, Story 6),
`gas_price` math helpers (`price_from_fee_history`, `median_priority_fee`, `scale`),
and the ~25 pure free functions at the bottom of `engine.rs` (including
`admission_action`, `treasury_affordable_top_up`, `is_durable_status`,
`settlement_hold_reason`, `settlement_rejection_reason`).

**Decision — stays in the shell**: Iggy session/reconnect
(`utils/iggy.rs`, `consumer.rs` rebuild logic), lane actor pool and mpsc backpressure,
Redis lease heartbeat driver (`run_with_lease_heartbeat` — though *whether/when* to
renew is policy the core states via its operation sequence), tokio runtimes and job
supervision, config env parsing (validated policy values passed into the core as
data), RPC endpoint failover/cooldown, HTTP middleware, Telegram delivery
(dedup fingerprint *text* comes from the core; delivery and claim storage are shell),
key custody (HKDF derivation math is core; secret material handling is shell).

**Rationale**: matches the constitutional shell-owned-concerns list and p256-index's
division; the pure modules already have the strongest test coverage — moving them is
low-risk and immediately gives the core crate its vocabulary.

## R7 — Executor decomposition seam (Story 4)

**Decision**: Grow the ExecutionApp out of the existing
`trait UserOperationHandler::handle_batch` seam (`src/worker/consumer.rs:60`): the
consumer keeps calling the same trait; its implementation becomes a driver loop
(`Core::new()` → `process_event(Start{envelope_ids})` → execute/resolve →
`BatchVerdict`). `execute_with_lane_lease`'s ~460 interleaved lines decompose into
the core program's sequential steps; the flat Operation executor holds the IO.
`BatchVerdict::Advance/Retry` maps onto the existing contiguous-durable-prefix
offset rule (`batch_result`, offset commit) unchanged.

**Rationale**: the trait contract ("result vector same length/order, Ok = durable
outcome, must be idempotent") is already an event/response protocol; p256-index's
`SubmissionApp`/worker pair is the exact template, down to the verdict enum.

**Alternatives considered**: rewriting the consumer loop as part of Story 4 —
rejected: the consumer orchestration (parse/route/dispatch/collect/commit) is
intrinsically shell and already close to correct; churn without payoff.

## R8 — Core testing harness

**Decision**: Replicate p256-index's in-module `Driver` harness per app: holds
`Core<App>` + `VecDeque<Request<Operation>>`, `step(expected_op, scripted_result)`
asserts the requested operation and resolves it, `assert_settled(outcome)` asserts
the queue is empty and the view settled. Assert the strictly-sequential invariant
(`queue.len() <= 1`) in `absorb`. Admission tests generate real signatures in-test
where validation needs them (p256 does this with P-256; here, Safe/4337 signature
shapes as needed).

**Rationale**: proven pattern, zero infrastructure, millisecond runtime, and the
harness doubles as executable documentation of the shell conversation.

**Alternatives considered**: crux's `AppTester`/`testing` feature — neither
reference repo uses it for this style; the hand-rolled driver is smaller and asserts
the sequencing invariant directly.

## R9 — Determinism injection points

**Decision**: The shell supplies, per unit of work: `now_ms` (and any deadline
inputs), generated ids (task id, bundle token — today `SystemTime`-based
`unique_token` at `engine.rs:3625` moves behind an injected value), chain id,
registry/entry-point addresses, validated policy config (markup bps, floor bps,
hold budget, float targets, caps), and pre-fetched market prices where a decision
needs them. Redis server-side time (Lua `TIME`) stops being a *policy* input: the
core precomputes the complete backoff schedule and the scripts perform a
mechanical table lookup. *(Adjusted during US2: the due-time clock anchor stays
Redis `TIME` — the attempt counter lives server-side and the claim reader also
uses `TIME`, so anchoring writer and reader to one clock is behavior-preserving,
while injecting process time would introduce clock-skew behavior that does not
exist today. Every delay VALUE comes from `vela_relay_core::hold`.)*

**Rationale**: Constitution II; p256-index's Submit-event injection is the template.
Cache TTL expiry driven by `Instant::now()` inside shell caches remains shell
behavior (it's infrastructure freshness, not business policy) — except the hold
ladder and any user-visible timing, which are core.

**Alternatives considered**: a `Time` operation the core requests — usable, but
injecting at program start keeps programs shorter; a mid-program time re-read is
only added where today's behavior actually re-reads the clock (it does not on the
decision paths in scope).

## R10 — Behavior-equivalence verification method

**Decision**: Three layers. (a) Core tests pin migrated behavior — reason strings,
thresholds, orderings — before old code is deleted (Constitution V/VI). (b) The
full existing suite (160 tests) runs unmodified at every merge point; HTTP-layer
tests already exercise routing/validation with a store-less AppState and must not
change. (c) Money-path PRs carry an equivalence note mapping old path → new
decision, per FR-011; where practical, migration commits keep old and new
implementations side by side for one commit with a comparison test before deletion
(p256-index's rationale commit records "~110 injected mutants, all killed" as the
bar for this style of audit).

**Rationale**: FR-003/SC-002/SC-006 need more than review; pinned-first tests plus
an unmodified legacy suite give mechanical evidence.

**Alternatives considered**: full production-traffic replay harness — valuable but
out of scope for this feature; SC-006 is written so an operator can perform it with
existing tooling when desired.
