# Tasks: Crux Core/Shell Split (vela-relay-core extraction)

**Input**: Design documents from `/specs/001-crux-core-split/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: INCLUDED — Constitution V mandates infrastructure-free core tests that pin
migrated behavior BEFORE old code is deleted. Test tasks precede implementation
tasks within every story.

**Organization**: One phase per user story (US1–US6, priority order from spec.md).
Every story ends with the full pre-existing suite green (FR-010) and, for
money-path stories, an equivalence note in the PR (FR-011).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelizable (different files, no dependency on an incomplete task)
- **[Story]**: US1–US6 for story-phase tasks

## Path Conventions

Two-member workspace (plan.md Structure Decision): root package `vela-relay`
(shell, `src/…`) + new member `vela-relay-core/` (core). Dependency direction:
`vela-relay → vela-relay-core` only.

---

## Phase 1: Setup (Workspace + core crate scaffold)

**Purpose**: Create the workspace and the empty, doctrine-carrying core crate.

- [x] T001 Convert root `Cargo.toml` to a workspace: add `[workspace] members = ["vela-relay-core"]` while keeping the root `[package] vela-relay` intact; verify `cargo test --locked` still passes untouched (161 tests incl. 1 ignored)
- [x] T002 Scaffold `vela-relay-core/Cargo.toml` (edition 2024) with the no-I/O doctrine comment (mirroring `p256-registrar`), deps: `crux_core = "0.19"`, `serde`, `serde_json`, `hex`, `sha2`, `sha3`, `k256`, `alloy` (`default-features = false`, `sol-types` only), `anyhow`; and `vela-relay-core/src/lib.rs` with the crate-level doctrine rustdoc and business-domain module map (plan.md source tree)
- [x] T003 Add `vela-relay-core = { path = "vela-relay-core" }` to the shell's dependencies; run quickstart Gate 0 + Gate 1 (`cargo tree -p vela-relay-core -e normal` shows computation crates only — verified, 0 runtime/IO crates). *Adjusted: the shell needs no direct `crux_core` dependency until US4/US5 introduce the driver loops.*

**Checkpoint**: workspace builds; core crate exists, empty but doctrinally correct.

---

## Phase 2: Foundational (Shared vocabulary — blocks all stories)

**Purpose**: The types every story speaks.

- [x] T004 Create `vela-relay-core/src/task.rs`: `UserOperationStatus` enum with frozen wire names plus the `is_terminal`/`is_durable` predicates; re-exported from `src/app/rpc/types.rs` as `UserOperationStatusKind` so no call site changes. *Adjusted during implementation: `StoredUserOperation`/`RoutedUserOperation` drag the whole `UserOperation` wire-type web with them and no US1 decision consumes them, so they migrate with the story that first needs them (US4/US5), per the put-it-in-the-earliest-story-that-needs-it rule.*
- [x] T005 [P] Migrate already-pure, story-independent helpers into `vela-relay-core`: `src/utils/vault.rs` → `vela-relay-core/src/vault.rs` (HKDF derivation + lane routing, golden-vector tests moved; `src/bin/deploy_simulations.rs` now uses the crate instead of a `#[path]` include; the two secret-key derivations became workspace-`pub` — neither crate is published), `src/utils/tempo.rs` → core, `src/utils/alchemy.rs` → core; shell keeps path-stable re-export shims
- [x] T006 [P] Migrate gas-price arithmetic into `vela-relay-core/src/gas_math.rs`: `GasPrice`/`GasPriceError`/`GasPricePolicy`/`GasPriceTiers`/`FeeHistory` types plus `price_from_fee_history`, `tiers`, `scale_price`, `fallback_priority_fee`, `median_priority_fee`, `parse_quantity`, `legacy_price_from_result` (5 tests moved); `GasPriceManager` keeps polling/caching/failover and calls the core math; all shell paths preserved via re-exports

**Checkpoint**: `cargo test -p vela-relay-core` runs the migrated pure tests with zero
infrastructure; full shell suite still green. ✅ (core 26 / shell 145 after US2)

---

## Phase 3: User Story 1 — One authoritative lifecycle state machine (P1) 🎯 MVP

**Goal**: The status transition table exists exactly once, in Rust, in the core;
Lua reduces to mechanical guarded writes.

**Independent Test**: quickstart Gate 3 transition greps pass; core tests pin every
legal/illegal transition; altering one table entry fails a test.

- [x] T007 [US1] Write pinning tests in `vela-relay-core/src/lifecycle.rs` (in-module `mod tests`): every pair of the transition table from data-model.md §2 (legal transitions, illegal refusals, same-status field-merge always allowed, missing record, terminal-state guards incl. terminal same-status merges); port the assertions of the `#[cfg(test)] transition_is_allowed` tests from `src/app/user_operation_store.rs:1630`+ 
- [x] T008 [US1] Implement `vela-relay-core/src/lifecycle.rs`: the transition table, `UserOperationStatus::is_terminal()`, and `decide_patch(current, requested) -> PatchDecision { Apply | RefuseIllegalTransition }` per data-model.md §2 (*Apply carries no merged fields: the merge is mechanical and stays in the guarded script; record-missing is observed by the shell's read, not decided by the core*)
- [x] T009 [US1] Write pinning tests for the bundle-submission policy in `vela-relay-core/src/lifecycle.rs`: eligible (`queued`/`not_submitted` + same chain → `submitted` with tx hash + `admitted=true`), idempotent re-index (`submitted` + same hash), skip (wrong chain incl. the fail-closed legacy chain-id fallback, terminal states, different hash) — behavior lifted from `MARK_BUNDLE_SUBMITTED_SCRIPT` (`src/app/user_operation_store.rs:63-98`)
- [x] T010 [US1] Implement `decide_bundle_submission(status, record_tx_hash, record_chain_id, record_chain_id_text, bundle_chain_id, bundle_tx_hash) -> BundleSubmissionDecision { Transition | IndexOnly | Skip }` in `vela-relay-core/src/lifecycle.rs`
- [x] T011 [US1] Reduce `PATCH_RECORD_SCRIPT` in `src/app/user_operation_store.rs` to a guarded CAS (apply iff stored status still equals the status the decision was computed against); rework the store's `patch` path to: read record → `lifecycle::decide_patch` → guarded write → on conflict re-read and re-decide (≤ 4 rounds); keep key naming/TTL (`KEEPTTL`) identical
- [x] T012 [US1] Reduce `MARK_BUNDLE_SUBMITTED_SCRIPT` the same way: shell MGETs the records, calls `decide_bundle_submission` per record, and the script performs only observed-state-guarded writes + set-index bookkeeping; the `:70-72` chain-comparison semantics (decimal text, fail-closed legacy fallback) moved into the core decision with its own pinning tests
- [x] T013 [US1] Deduplicate the durability predicate and delete the mirror. *Corrected during implementation: `is_durable_status` includes `Submitted` and is NOT the terminal predicate — the core enum now carries both `is_durable()` (offset semantics, engine delegates to it) and `is_terminal()` (transition semantics), pinned distinct by `terminal_and_durable_predicates_stay_distinct`.* The `#[cfg(test)] transition_is_allowed` mirror and the policy-asserting script-text tests are deleted; replaced by `patch_lua_is_a_mechanical_guarded_merge` / `submitted_lua_applies_core_decisions_behind_observed_state_guards`, which assert the scripts contain NO policy
- [x] T014 [US1] Run quickstart Gates 0–3 (fmt OK; clippy 9 warnings = baseline, none new; 7 core + 160 shell tests green; core dep tree has 0 runtime/IO crates; `allowed = {` and `transition_is_allowed` grep clean in `src/`); equivalence note written to `specs/001-crux-core-split/equivalence-notes/us1-lifecycle.md`

**Checkpoint**: MVP — the highest-risk drift point is structurally eliminated;
merge as its own PR.

---

## Phase 4: User Story 2 — Unified, testable fee-hold policy (P2)

**Goal**: Backoff schedule + hold budget + reason strings are one pure decision.

**Independent Test**: core tests enumerate the full ladder (attempts 1→13) with no
infrastructure; the doubling loop is gone from Lua.

- [x] T015 [US2] Pinning tests in `vela-relay-core/src/hold.rs` (ladder attempts 1..=13/20, schedule table, budget boundary 12/12 vs 13/12) and `vela-relay-core/src/settlement.rs` (byte-identical hold + rejection reason strings; engine's `explains_an_insufficient_in_band_reimbursement` moved along)
- [x] T016 [US2] Implement `vela-relay-core/src/hold.rs` (`retry_delay_ms`, `retry_delay_schedule_ms`, `decide_hold -> Hold{reason} | RejectBudgetExhausted`) and move both reason builders to `vela-relay-core/src/settlement.rs`. *Adjusted: no `HoldLadder` struct or `due_at_ms` in the decision — the attempt counter lives in Redis, so the core exports the schedule as a lookup table and judges the post-increment attempt; see T017 note on the clock anchor.*
- [x] T017 [US2] Reduce both delayed-inbox scripts: doubling loop replaced by a table lookup over core-supplied delays (`append_retry_schedule`); attempt bookkeeping, claim-token guards, TTL mechanics unchanged. *Adjusted from the plan: the due-time anchor deliberately stays Redis `TIME` — the claim reader uses the same clock, so switching the writer to process time would introduce clock-skew behavior that does not exist today (research.md R9, updated). Script-text tests now pin "no `delay * 2` in Lua".*
- [x] T018 [US2] `hold_for_affordable_market` now matches on `vela_relay_core::hold::decide_hold`; the engine-side budget check and both local reason builders are deleted (engine imports `settlement_rejection_reason` from the core)
- [x] T019 [US2] Gates 0–3 green (fmt OK; clippy 7 warnings — two pre-existing dead-code warnings vanished with the vault move, none added; core 26 + shell 145 tests, 5 genuinely new; backoff grep clean); equivalence note at `specs/001-crux-core-split/equivalence-notes/us2-hold.md`

**Checkpoint**: hold policy readable and testable as one unit; merge as its own PR.

---

## Phase 5: User Story 3 — Settlement decision as a pure verdict (P3)

**Goal**: accept / reprice / hold / reject is a single pure function consuming
pre-fetched inputs.

**Independent Test**: table-driven verdict tests incl. boundary fees, missing
market price, mixed multi-op batches — no infrastructure.

- [x] T020 [US3] `src/worker/executor/settlement.rs` (evaluation, repricing math, log verification + 17 tests) moved wholesale to `vela-relay-core/src/settlement.rs` (`pub(crate)` → `pub`); worker module is a re-export shim. *Also pulled forward from T025: `cost.rs` (`allocate_bundle_gas`/`native_cost` + 2 tests) → `vela-relay-core/src/cost.rs`, because the verdict recomputes per-op costs at the repriced fee.*
- [x] T021 [US3] Six verdict pinning tests in `vela-relay-core/src/settlement.rs`: fully-funded → KeepQuote(all accepted); shortfall-above-floor → Reprice with the exact affordable fee (700/1400 paid → 500 from a 1000 quote); affordable-below-floor → FloorUnfundable with exact `{affordable: 500, floor: 600}`; uncurable rejection (malformed calldata) disables repricing for the batch; cost overflow with the byte-frozen error text; stablecoin-payment detection from calldata alone
- [x] T022 [US3] Implemented `decide_settlement(recipient, chain_assets, call_datas, allocations, native_usd_price, FeeContext) -> SettlementDecision { KeepQuote | FloorUnfundable{affordable, floor} | Reprice{fee_per_gas, evaluation} }`. *Adjusted from the planned shape: the verdict does not fold in Hold/Reject — those remain the downstream gate's per-operation outcomes (US2's `decide_hold` + the rejection path), exactly as in the old control flow; `FloorUnfundable` exists as its own arm so the shell can emit the byte-identical diagnostic log.*
- [x] T023 [US3] `settle_at_affordable_fee` now pre-fetches the Binance price once (calldata-only `has_stablecoin_payment`, moved to core) and applies the core verdict; `evaluate_settlement` and the engine-local decision flow deleted; log messages and field names preserved
- [x] T024 [US3] Gates green (fmt OK; clippy 7 warnings = current baseline, none new; shell 126 + core 51 tests, 6 genuinely new); equivalence note at `specs/001-crux-core-split/equivalence-notes/us3-settlement.md`

**Checkpoint**: the revenue/safety core is exhaustively pinned; merge as its own PR.

---

## Phase 6: User Story 4 — Executor batch pipeline testable end-to-end (P4)

**Goal**: `ExecutionApp` drives a lane batch from envelope ids to
`BatchVerdict::Advance|Retry` behind the existing `UserOperationHandler` seam;
`engine.rs` decomposes into pure decisions + a flat operation executor.

**Independent Test**: Driver-harness walk of a full batch asserting operation order;
failure-injection variants settle to today's retry/advance/dead-letter outcomes.

- [x] T025 [P] [US4] Pure bundle math moved to the core as separate business-domain modules (matching how `cost.rs` went, instead of one `bundle.rs`): `abi.rs` and `receipt.rs` with their tests; this pulled the wire vocabulary (`UserOperation`/`UserOperationV0_7`/`UserOperationV0_6`/`Eip7702Authorization`/`UserOperationEvent` + `Address`/`HexData`/`Quantity`/`TransactionHash` aliases) into `vela-relay-core/src/task.rs` per the T004 note — US4 is the first consumer. The engine-side `receipt_succeeded` match migrates with the reconcile decision (T034)
- [x] T026 [P] [US4] `transaction.rs` → `vela-relay-core/src/signing.rs` (byte-for-byte signing tests moved; key custody stays in the shell); core alloy grew computation-only features (`consensus`, `eips`, `network`, `serde`, `signer-local`) — dep tree still has 0 IO/runtime crates
- [x] T027 [P] [US4] `vela-relay-core/src/broadcast.rs`: `nonce_too_low`, `parse_hex_bytes`, `validate_raw_transaction` (frozen error texts) and `resolve_unproven_broadcast(reason, transaction_known) -> Confirmed | CheckStaleNonce | RetainOutbox` — the judgement shared by the Ambiguous/Rejected arms, with pinning tests. *The arms themselves still sequence their probes in `engine.rs`; they become core-driven operation sequences with the ExecutionApp (T032/T034)*
- [x] T028 [P] [US4] `vela-relay-core/src/funding.rs`: `plan_native_top_up` (target/deficit/cap with frozen error texts), `native_top_up_reserve`, `treasury_affordable_top_up`, `native_amount_for_usd_cap`, `plan_tempo_top_up`, `TOP_UP_GAS_LIMIT`/`NATIVE_TOP_UP_USD_CAP`; also pulled in: `parse_market_usd_price` + `USD_PRICE_SCALE` + `marked_tempo_cost` → core `settlement.rs`, `tempo_cost_in_path_usd` + `buffered_top_up_gas_limit` + `TEMPO_TOP_UP_GAS_BUFFER_BPS` → core `tempo.rs`; engine keeps thin error-mapping wrappers so call sites and error strings are untouched
- [x] T029 [US4] `ExecutionOperation`/`ExecutionOutcome` vocabulary + `#[effect]` wrapper + `ExecutionApp` in `vela-relay-core/src/execution.rs`. *Adjusted from the plan: `Start` carries the routed envelopes + validated `ExecutionPolicy` + shell-generated lease token (not bare envelope ids — the triage decisions consume the payloads); the settled outcome is a per-item `Vec<ItemResolution { Durable | Failed{reason} }>` matching the `UserOperationHandler` results-vector contract, not a single `BatchVerdict` (the contiguous-durable-prefix offset rule consumes per-item durability). Five nested pipelines remain composite operations for T034: `ResumeBundleIntent`, `ResolveNonceMismatches`, `EnsureRelayerFunded`, `BroadcastBundle`, `ExecuteTempoBundle`.*
- [x] T030 [US4] Driver harness (p256 pattern incl. the `queue.len() <= 1` sequential invariant) + `a_fully_funded_batch_walks_the_pipeline_to_submission`: an 18-step walk pinning the exact operation order — support check → assets → records → lease → prepared-intent probe → simulate per-op → renew → simulate bundle → renew → transaction context → market price (top-up cap, fail-open to the static cap) → renew → sign → renew → persist intent → broadcast → mark submitted → all durable
- [x] T031 [US4] Failure-injection Driver tests: store outage at `LoadRecords` → every item fails without touching the lane; lane lease unavailable → `lease`-stage diagnostic (no Telegram — expected hand-off) + finish reason preserved; already-durable record advances without execution; unaffordable market → `DeferOperation` → in-budget hold with the byte-identical hold reason + Telegram notify → durable. *(Deeper injections for the composite operations land with T034 when they are decomposed.)*
- [x] T032 [US4] The `drive_batch`/`execute_with_lane_lease` program implemented in the core, composing US1–US3 decisions (`is_durable`, `admission_action`, `queued_operation_from_routed`/`candidate_from_record`/`queue_record_matches` — moved here as pure fns — dedupe by hash/sender, truncate, `decide_settlement`, `decide_hold`, log verification, gas/prefund math, USD top-up cap fail-open, funding-precedence check, save-race resume, indexed-count guard). **Additive so far: the shell still runs its old path; T033 swaps the driver.**
- [x] T033 [US4] Shell driver + flat executor landed: `handle_lane_batch` is now `Core::<ExecutionApp>::new()` → execute/resolve loop (`BatchShell` with lease-heartbeat task and asset caching); per-item `ItemResolution`s map onto the existing results-vector/offset rule (`consumer.rs` untouched; the delayed-inbox path flows through the same entry). Executor arms fold infra errors into result variants; historical logs emitted from the arms. *Declared divergence: heartbeat renewal failure no longer aborts mid-await — the existing `EnsureLaneLease` checkpoints catch the loss (see the equivalence note).*
- [~] T034 [US4] Composite decomposition — **first batch landed**: `BroadcastBundle` is now a core-driven sequence (`CheckBroadcastSeen`/`BroadcastRaw`/`Remember`/`Forget`/`ProbeTransactionKnown`/`ProbeStaleNonce`/`ClearStaleIntent`/`RecordUnprovenBroadcast`) judged by `crate::broadcast::resolve_unproven_broadcast`, with raw-byte validation in-program; `ResolveNonceMismatches` decomposed into `FetchAccountNonces` + per-item `DeferOperation{FutureNonce}`/`MarkRejected{StaleNonce}` decisions; the Gnosis xDAI peg moved into the core (`settlement::pegged_native_usd_price` — the program never asks a pegged chain for a market price); the `2×base+tip` quote and legacy-gas-price tip rules moved to `gas_math::{quoted_outer_fee, tip_from_legacy_gas_price}`. Three new Driver injections: future-nonce defer, stale-nonce reject, nonce-too-low broadcast → stale-lane clear (21 steps). **Remaining**: `EnsureRelayerFunded` (treasury lease/probe/sign/broadcast), `ExecuteTempoBundle`, `ResumeBundleIntent` + `reconcile_prepared_bundles` (the shell's `broadcast_bundle_intent` composite survives only for these resume paths)
- [x] T035 [US4] Old paths deleted: `handle_lane_batch` (old), `execute_with_lane_lease`, `execute_tempo_bundle`, `resolve_nonce_mismatches`, `settle_at_affordable_fee`, `hold_for_affordable_market`, `record_*_deferred` helpers, `native_top_up_cap`, `Candidate`, `admission_action`, `queued_operation_from_routed`, `candidate_from_record`, `queue_record_matches`, `is_durable_status`, `should_notify_executor_deferred`, `finish_results` — engine.rs ≈3.1k lines, no business decisions left on the batch path. Gates green (clippy 7 = baseline; core suite 0.07 s); equivalence note at `specs/001-crux-core-split/equivalence-notes/us4-execution.md`

**Checkpoint**: the monolith is decomposed; the currently untested main path has
scripted coverage; merge as its own PR (or two: T025–T028 prep PR, then T029–T035).

---

## Phase 7: User Story 5 — Admission protocol testable (P5)

**Goal**: the two-phase enqueue is an `AdmissionApp` program with pinned
crash-window behavior.

**Independent Test**: Driver tests for first-submit / duplicate / queue-down /
store-down; existing HTTP handler tests unchanged.

- [x] T036 [US5] `AdmissionOperation`/`AdmissionResult` + `AdmissionApp` in `vela-relay-core/src/admission.rs`; `PreparedUserOperation` (validation + userOpHash), `existing_admission_action`, `admission_fingerprint`, the zero-fee/minimum-reimbursement validators, and the `SUPPORTED_ENTRY_POINTS` list all moved into the core with their 7 tests (+ the 2 HTTP parser tests, now against `admission::string_reimbursement`). *Adjusted: validation needs directory/RPC data, so `LoadSettlementAssets`/`FetchTokenDecimals` are operations rather than pre-injected inputs; no `new_task_id`/`now_ms` exist on this path (the hash is the identity).*
- [x] T037 [US5] Six Driver walks: create→enqueue→admit order with today's acknowledgement; duplicate-admitted idempotency; conflicting payload refusal; queue-outage-after-create crash window (record retained, no further operations — the vocabulary has no delete); underpayment rejection with the frozen string; zero-fee refusal before any operation
- [x] T038 [US5] `drive_admission` program implemented (early-settlement `Err` channel, p256 style); the missing-queue-before-write ordering preserved via a `QueueUnavailable` answer to `CreateQueued`
- [x] T039 [US5] Shell handler is now driver + flat executor + renderer producing byte-identical JSON-RPC responses and logs; the old linear `accept` and the HTTP-side reimbursement parser adapter are deleted (US6 reaches its final form: parsing exists ONLY in the core)
- [x] T040 [US5] Gates green (fmt OK; clippy 7 = baseline; shell 101 + core 90 = 191 tests, +6 new; HTTP routing/validation tests unmodified); equivalence note at `specs/001-crux-core-split/equivalence-notes/us5-admission.md`

**Checkpoint**: admission crash-consistency is executable, not prose; merge as its
own PR.

---

## Phase 8: User Story 6 — One reimbursement interpretation (P6)

**Goal**: exactly one parser for in-band reimbursement calldata.

**Independent Test**: both former test suites pass against the single function;
`TRUSTED_MULTISEND` grep shows one crate.

- [x] T041 [US6] The HTTP copy is now a string-facing adapter over `vela_relay_core::settlement::parse_reimbursement`; core `minimum_amount` made `pub` with thin `minimum_native_amount`/`minimum_stablecoin_amount` adapters; `is_tempo_chain` re-exported from core tempo; single production `TRUSTED_MULTISEND` (a test-fixture string remains in the adapter's `#[cfg(test)]`)
- [x] T042 [US6] *Adjusted: the module is kept (as the adapter) rather than deleted — its 3 tests now exercise the adapter → core path, and the handler call sites keep their `in_band_settlement::` paths unchanged; `parse_address`/`decode_hex` stay as shell transport helpers*
- [x] T043 [US6] Gates green (fmt OK; clippy 7 = baseline; 177 tests, shell 126 / core 51); equivalence note at `specs/001-crux-core-split/equivalence-notes/us6-reimbursement.md` — documents the two resolved RPC-vs-executor divergences (u128 saturation, zero-amount stable legs), both settled toward the executor's authoritative semantics

**Checkpoint**: last known duplication closed; merge as its own PR.

---

## Phase 9: Polish & Cross-Cutting

- [x] T044 [P] `README.md` gained an Architecture section describing the two-crate core/shell split, the crux per-unit-of-work drivers, and the Spec Kit governance; `vela-relay-core/src/lib.rs` carries the doctrine rustdoc and the root `Cargo.toml` the shell doctrine comment (p256 style)
- [x] T045 [P] Shim audit: every re-export shim has live users (`utils::tempo` via the engine's module alias; vault/alchemy/abi/receipt/transaction/cost/settlement via their historical paths) — all kept deliberately, each carrying a moved-to-core doc comment
- [x] T046 Full gate pass 2026-08-28: fmt OK; clippy 7 warnings (= baseline; started at 9); **core 94 + shell 101 = 195 tests** (baseline 160), core suite **0.07 s** with a dependency tree containing **0 IO/runtime crates** (SC-001/SC-003/SC-005 met)
- [x] T047 SC-004 verified: transition tables in shell 0, test-only FSM mirror 0, backoff doubling in Lua 0, `parse_reimbursement` defined only in the core (the one extra `TRUSTED_MULTISEND` string is a core test fixture); commands recorded in quickstart Gate 3

---

## Dependencies

```
Phase 1 (T001–T003) → Phase 2 (T004–T006) → US1 (T007–T014) 🎯 MVP
US1 → US2 (T015–T019)        # hold reasons referenced by settlement tests
US2 → US3 (T020–T024)        # verdict embeds hold/reject reasons
US1+US2+US3 → US4 (T025–T035)  # program composes lifecycle+hold+settlement
US1 → US5 (T036–T040)        # admission writes lifecycle-governed records
                             # (US5 is otherwise independent of US2–US4)
US3 → US6 (T041–T043)        # unifies INTO the migrated settlement module
US4, US5, US6 → Phase 9 (T044–T047)
```

US5 may proceed in parallel with US2–US4 once US1 lands. US6 may proceed in
parallel with US4 once US3 lands.

## Parallel Execution Examples

- **Phase 2**: T005 and T006 in parallel (different files) after T004.
- **US4 prep**: T025, T026, T027, T028 all in parallel (four independent core
  modules), then T029→T030→T031 sequentially, T032 after T031, T033–T035 after T032.
- **After US1 lands**: one track runs US2→US3→US4 while a second track runs US5.
- **After US3 lands**: US6 can run alongside US4.

## Implementation Strategy

- **MVP first**: Phases 1–3 (T001–T014) deliver Story 1 — the drift-risk
  elimination — as a complete, independently valuable PR.
- **Incremental delivery**: each story is its own branch/PR with the full
  pre-existing suite green and an equivalence note for money-path changes
  (FR-010/FR-011); pinning tests always land before old code is deleted
  (Constitution V/VI).
- **Suggested order**: sequential US1→US6 for a single implementer; the two-track
  split above if parallelizing.
- **Largest risk**: US4 (T032–T035). Mitigation: T030's happy-path walk pins the
  operation order before decomposition begins, and T025–T028 land as a separate
  low-risk prep PR.
