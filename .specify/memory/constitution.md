<!--
Sync Impact Report
- Version change: 1.0.1 → 1.0.2 (PATCH: core crate renamed vela-bundler →
  vela-relay-core by user decision — "vela-bundler" is the name of the
  predecessor system referenced throughout the code (HKDF salt, golden-vector
  tests) and collides with it; naming bullet updated accordingly.
  Earlier 1.0.0 → 1.0.1: toolchain gate corrected to match the repository's
  actual CI — clippy runs without -D warnings, 9 pre-existing warnings on main)
- Modified principles: none (Architectural Constraints bullets only)
- Added sections: none
- Removed sections: none
- Follow-up TODOs: none
-->

# Vela Relay Constitution

## Core Principles

### I. Core/Shell Separation (NON-NEGOTIABLE)

All business vocabulary and decision rules live in the `vela-relay-core` crate (the Core).
The Core is deliberately I/O-free: no Redis, no Iggy, no HTTP clients, no tokio, no
clocks, no environment access. The `vela-relay` crate (the Shell) wires Core decisions
to real infrastructure: Axum, Redis, Iggy, EVM JSON-RPC, Binance, Telegram.

- Adding an I/O dependency to `vela-relay-core` is a design violation, not a convenience.
  The crate manifest MUST state this rule in a comment, mirroring `p256-registrar`.
- Modules in the Core are business domains (admission, settlement, execution, funding),
  never architectural layers.
- Logic that does not fit the crux engine still obeys the split: the Core returns a
  pure plan or verdict value; the Shell executes it. Core/Shell is a whole-repo
  discipline, not a crux-only artifact.

Rationale: this is the load-bearing property that makes every business decision
deterministically testable without infrastructure, proven in `p256-index`.

### II. Determinism by Injection

The Core MUST NOT read wall-clock time, generate randomness or UUIDs, read env vars,
or observe any other nondeterministic source. Every nondeterministic input (timestamps,
task/bundle identifiers, chain id, contract addresses, derived signer addresses,
market prices) enters the Core through the start Event or through an Operation result
supplied by the Shell. Sensitive raw inputs (private keys, unhashed client IPs) MUST
NOT cross into the Core at all; the Shell pre-derives or pre-hashes them.

Rationale: determinism is what turns the Core into a replayable, millisecond-fast
state machine; a single leaked `now()` breaks it.

### III. Failure Is Data, Not an Error Channel

Shell executors never surface infrastructure failure to the Core as `Err`. Every
Operation's `Output` type enumerates failure as ordinary variants
(`StoreUnavailable`, `ChainReadFailed`, `QueueUnavailable`, `ReceiptUncertain`, …),
and the Core alone decides what each failure means — fail-open, fail-closed, retry,
hold, or reject. The effect vocabulary is deliberately minimal: an action the system
must never take (e.g. un-admitting a durably admitted operation) MUST NOT exist as an
Operation at all.

Rationale: failure policy is business policy; keeping it in the Core makes it visible,
testable, and impossible to bypass. An absent Operation is a stronger guarantee than a
guarded one.

### IV. Single Source of Truth for State Machines

Every business state transition table lives in Rust types inside the Core. No
transition rule, backoff schedule, retry budget, or guard condition may live in Redis
Lua scripts, configuration strings, or any other second language. Lua scripts are
reduced to mechanical guarded writes (compare-and-set, ownership-token checks) whose
decisions were already made by the Core. A `#[cfg(test)]`-only mirror of a production
rule is forbidden — the mirror must BE the production rule.

Rationale: the pre-refactor codebase carries its primary FSM in a Lua string with a
hand-maintained Rust test mirror that can silently drift; this class of defect is
eliminated structurally, not by review.

### V. Infrastructure-Free Core Tests First

Every Core decision path MUST be exercised by pure state-machine tests that run with
no Redis, no Iggy, no chain, and no tokio runtime, using a Driver harness that scripts
the Shell side of the conversation (`expected Operation` in, scripted `Result` back,
settled outcome asserted). Sequential programs MUST assert the "at most one operation
in flight" invariant. The Shell is tested separately with fakes: given an Operation,
does it call the right infrastructure. The two kinds of tests are never mixed.

Rationale: the pre-refactor executor monolith has zero tests on its main paths because
they require live infrastructure; the split exists precisely to make those paths
testable in milliseconds.

### VI. Behavior-Preserving Migration

The refactor is a 1:1 behavioral migration. Every reason string, error message, HTTP
status, JSON shape, threshold, backoff value, and timing budget is kept byte-identical
unless a spec explicitly declares the change. Each migration step MUST be
independently mergeable, MUST keep the full existing test suite green, and SHOULD add
Core tests that pin the migrated behavior before the old implementation is deleted.
Behavioral equivalence is verified by review against the old code path, not assumed.

Rationale: this service moves user funds on-chain; the refactor must never be the
cause of a behavior change that was not consciously specified.

## Architectural Constraints

- **Crux engine**: `crux_core = "0.19"` using the Command/Operation API
  (`Command::request_from_shell(op).then_send(Event)`, `Core::process_event` /
  `resolve` / `view`). Official capability crates (`crux_http`, `crux_kv`,
  `crux_time`) are not used; each app defines its own `Operation` enum with a
  single-variant `#[effect]` wrapper.
- **Per-unit-of-work Cores**: server-side apps follow the `p256-index` paradigm — one
  `Core::new()` per HTTP request or per consumed batch, a `Model` holding the settled
  outcome, discarded after the Shell reads `view()`. Long-lived in-process state
  (connection sessions, lane actor pools, lease heartbeats, tokio supervision) stays
  in the Shell.
- **Naming**: the Core crate is `vela-relay-core` (user decision, 2026-08-28) —
  subordinate to the service name so the pairing with `vela-relay` is obvious.
  `vela-core` and `vela-bundler` were both rejected: the latter is the predecessor
  system's name, still referenced by the HKDF salt and golden-vector tests, and
  must not be reused. Core *modules* are still named for business domains
  (admission, settlement, lifecycle), never for architectural layers.
- **Shell-owned concerns**: transport (body limits, JSON envelopes, IP extraction),
  Redis key naming, TTL values, Iggy topology and reconnection, RPC endpoint failover
  and cooldown, key custody and signing execution, process lifecycle.
- **Toolchain**: Rust edition 2024; the repository CI gates — `cargo fmt --check`,
  `cargo clippy --all-targets --locked`, and `cargo test --locked` — MUST pass on
  every commit that lands. Warnings-as-errors is not currently a CI gate (9
  pre-existing warnings exist on main); migration changes MUST NOT add new
  warnings.

## Development Workflow & Quality Gates

- All refactor work flows through Spec Kit: constitution → specify → (clarify) → plan
  → tasks → implement, with artifacts under `specs/`. No implementation begins before
  its spec and plan exist.
- Work is sequenced as small, independently mergeable steps ordered by
  risk-removed-per-effort; each step lands as its own branch/PR with the full suite
  green.
- A migration step that touches money-path logic (admission, settlement, funding,
  broadcast) requires a written equivalence note in the PR description mapping old
  code path → new Core decision, including where each reason string went.
- Duplicated business logic discovered during migration (e.g. parallel reimbursement
  parsers) MUST be collapsed into a single Core function rather than migrated twice.

## Governance

This constitution supersedes ad-hoc practice for all work in this repository.
Amendments are made by PR that edits this file, states the version bump and rationale
in the Sync Impact Report comment, and is approved by the repository owner.
Versioning is semantic: MAJOR for principle removals or redefinitions, MINOR for new
or materially expanded principles/sections, PATCH for clarifications. Every
`/speckit-plan` execution re-reads this file and records a Constitution Check; a plan
that violates a principle MUST either be revised or carry an explicit, justified
complexity-tracking entry. Reviews of migration PRs verify Principles I–VI
explicitly.

**Version**: 1.0.2 | **Ratified**: 2026-08-28 | **Last Amended**: 2026-08-28
