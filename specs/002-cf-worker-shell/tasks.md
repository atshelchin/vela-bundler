# Tasks: Cloudflare Worker Shell (second deployment target)

**Input**: Design documents from `/specs/002-cf-worker-shell/`

**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: INCLUDED — Constitution V mandates pinning tests before behavior
lands: the `wire` module's byte-pinning tests precede the docker shell's
delegation, and every story ends with its quickstart gate.

**Organization**: One phase per user story (US1–US5, priority order from
spec.md). Every story ends with the full pre-existing suite green (FR-003) and
the story's gate from quickstart.md; transport-mapping changes update
contracts/platform-bindings.md in the same PR (FR-012).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelizable (different files, no dependency on an incomplete task)
- **[Story]**: US1–US5 for story-phase tasks

## Path Conventions

Three-member workspace (plan.md Structure Decision): root `vela-relay` (docker
shell) + `vela-relay-core/` (core) + NEW `vela-relay-cf/` (Cloudflare shell,
wasm-only, excluded from native default builds). Dependency directions:
`vela-relay → vela-relay-core` and `vela-relay-cf → vela-relay-core` only.

---

## Phase 1: Setup (workspace member + wrangler scaffold)

**Purpose**: the wasm-only member exists, builds, and boots under `wrangler dev`
without touching any native gate.

- [x] T001 Add `vela-relay-cf` to the workspace: root `Cargo.toml` `members += "vela-relay-cf"` (NO default-members change — verify `cargo test --locked` still runs exactly the docker-shell suite and `cargo clippy --all-targets --locked` output is unchanged); create `vela-relay-cf/Cargo.toml` (edition 2024; doctrine comment "business decisions live in vela-relay-core — this crate wires them to Cloudflare primitives and must not define rules"; `[target.'cfg(target_arch = "wasm32")'.dependencies]` worker `"=0.8.5"`, vela-relay-core path, serde/serde_json, getrandom `{ version = "0.2", features = ["js"] }`) and `vela-relay-cf/src/lib.rs` with `#[cfg(target_arch = "wasm32")]`-gated module tree so a native `cargo check -p vela-relay-cf` compiles an empty crate; gate: `cargo check -p vela-relay-cf --target wasm32-unknown-unknown --locked`
- [x] T002 [P] `vela-relay-cf/wrangler.jsonc`: `[build]` via worker-build; DO bindings RECORDS/LANES/TREASURY with `new_sqlite_classes` migration; queue producer+consumer for `vela-relay-ops` + `vela-relay-dlq` (max_batch_size/timeout/max_retries/dead_letter_queue per research R4); KV namespace CACHE; `[limits] cpu_ms`; vars skeleton (policy values, optional EXECUTION_CHAINS, VELA_RELAY_RELAYER_COUNT per R11); `.dev.vars.example` (no real secrets); boot a stub fetch handler under `npx wrangler dev`
- [x] T003 [P] Record the 002 gates in CI-facing docs: quickstart Gate 0/1 command list verified once end-to-end; note in `docs/docker.md`-style `docs/cloudflare.md` stub that deploys require Workers Paid (research R1)

**Checkpoint**: workspace builds native + wasm; wrangler dev boots; zero native-gate drift.

---

## Phase 2: Foundational (the wire module — blocks US1 for both shells)

**Purpose**: one executable definition of the JSON-RPC envelope bytes.

- [x] T004 Byte-pinning tests FIRST in `vela-relay-core/src/wire.rs` `mod tests`: request-envelope parsing (valid, malformed JSON, wrong jsonrpc version, unknown method, batch/odd id types) and response rendering for every envelope class (`result`, `-32602 invalid params` + data, `-32500 UserOperation simulation failed` + data, method-not-found, parse error) — golden vectors lifted from the 001 replay battery's captured bodies so the pins are production bytes
- [x] T005 Implement `vela-relay-core/src/wire.rs` (additive, pure): envelope types, the eight-method dispatch vocabulary, params extraction for each method (reusing the existing core wire types), and outcome→body renderers (including the admission `AdmissionOutcome` renderer moved from the shell where it is already core-shaped); re-export nothing that drags IO
- [x] T006 Docker shell delegates envelope parse/render to `vela_relay_core::wire` (`src/app/rpc/mod.rs` + handlers): behavior-preserving — full suite green, then `001/replay-harness/round.sh` before/after with `diff -r` empty; write `specs/002-cf-worker-shell/equivalence-notes/wire.md` mapping old shell rendering → wire fns

**Checkpoint**: both shells CAN share bytes; docker deployment provably unchanged.

---

## Phase 3: User Story 1 — Edge intake and reads (P1) 🎯 MVP

**Goal**: the full external surface served from the edge; accepted operations
durably recorded (RecordDO) + queued (Queues); execution disabled.

**Independent Test**: quickstart Gate 2 — replay battery byte-identical vs the
docker deployment under `wrangler dev`; crash-window scenario matches.

- [x] T007 [P] [US1] `vela-relay-cf/src/config.rs`: env/secrets → validated policy values (same defaults/bounds as the docker parser; shared numeric defaults promoted to core consts where absent — additive); dynamic-chain posture per R10 (EXECUTION_CHAINS optional), lane width 1..=100 per R11
- [x] T008 [P] [US1] `vela-relay-cf/src/record_do.rs`: RecordDO — serde fetch protocol (`create_queued` if-absent via the core's `queued_record` — promoted additively so both shells persist one initial shape — `get`, `mark_admitted`; *adjusted: `patch`/`mark_bundle_member_submitted` land with their first consumer, the US2 LaneDO (T014), per the earliest-story-that-needs-it rule*); TTL alarm cleanup (earliest-of packing per data-model §1); storage layout documented in the file header
- [x] T009 [US1] `vela-relay-cf/src/http.rs`: fetch handler routing (`/`, `/health`, `/healthz`, `/readyz` as binding checks per deployment-parity.md, `/version`, `POST /{chain_id}`) → wire dispatch; reads (supportedEntryPoints, status, byHash, receipt) via RecordDO + wire renderers
- [x] T010 [US1] AdmissionApp driver in the fetch path + arms: `LoadSettlementAssets` (chain-directory fetch + KV cache), `FetchTokenDecimals` (RPC fetch via arms/rpc.rs), `CreateQueued`/`LoadExisting`/`MarkAdmitted` → RecordDO, `Enqueue` → queue producer (send failure → `QueueUnavailable`, crash-window semantics preserved); responses rendered by wire
- [ ] T011 [P] [US1] `vela-relay-cf/src/arms/rpc.rs` + `arms/market.rs`: chain RPC failover over fetch (user-header → Alchemy → directory fallback, same order; timeouts via AbortSignal race) and Binance/metadata fetches with KV caches — read-side methods (estimate, gasPrice, in-band quote) wired through them
- [x] T012 [US1] Gate 2 run: replay battery against `wrangler dev` AND the docker build, `diff -r` on bodies; scripted crash-window check (forced queue-send failure → response + RecordDO state match docker's); fix until byte-clean (2026-08-29: 16/16 RPC bodies byte-identical incl. valid accept + idempotent duplicate + accepted-record reads; GETs identical except the two declared deltas; statuses all equal). *Adjusted: the scripted crash-window forcing joins T011's change set; the crash-window DECISION is already pinned by the core Driver tests and the arm maps send-failure → QueueUnavailable verbatim*

**Checkpoint**: MVP — a globally deployable enqueue-only relay with byte-identical surface; merge as its own PR.

---

## Phase 4: User Story 2 — Edge execution with identical dispositions (P2)

**Goal**: LaneDO drives `ExecutionApp` verbatim; queue consumer routes and acks.

**Independent Test**: quickstart Gate 3 fault-injection set + a full testnet batch.

- [ ] T013 [US2] `vela-relay-cf/src/lib.rs` `#[event(queue)]` consumer + `vela-relay-cf/src/lane_do.rs` scaffold: group batch by (chainId, `relayer_index_for_sender`), forward per-lane groups to LaneDO over the serde fetch protocol, map returned `ItemResolution`s → per-message `ack()`/`retry()`; DLQ flow for `DeadLetterRouted`
- [ ] T014 [US2] LaneDO ExecutionApp driver + state arms in `lane_do.rs`: `LoadRecords`/`ReloadRecord`/`Mark*` → RecordDO subrequests; prepared-intent save (put-if-absent)/load/guarded-clear in DO storage; broadcast-seen cache; lease ops answered structurally (`true`); `RecordDeferred`/`NotifyIssue`/`EmitDiagnostic` arms (same gating — decided by core)
- [ ] T015 [P] [US2] Chain-IO arms in `lane_do.rs` + `arms/rpc.rs`: `SimulateIndividually`/`SimulateBundle`/`FetchAccountNonces`/`FetchTransactionContext`/`FetchTempoContext`/`BroadcastRaw`/`ProbeTransactionKnown`/`ProbeStaleNonce` over the failover transport; signing arms via core signing fns with secret bindings (keys never enter core); `FetchMarketPrice` with KV cache and unchanged fail directions
- [ ] T016 [P] [US2] `vela-relay-cf/src/treasury_do.rs`: TreasuryDO — lock state (holder token + deadline; acquire/ensure/release arms; store error → `Failed`, batch-fatal per bindings contract), `PreparedFundingIntent` storage, receipt-probe lock, `Record*`/`NoteFundingReceipt` arms with the frozen texts
- [ ] T017 [US2] Tempo tail arms: `FetchTempoTreasuryContext`, `SignTempoBundle`, `SignTreasuryPathUsd` — the pathUSD twin over the same transports; verify against the core's tempo Driver walks' operation sequences
- [ ] T018 [US2] Gate 3: scripted fault injection under `wrangler dev` (duplicate delivery → durable-skip, reordered nonces → delayed-inbox/reject, DO restart mid-batch → prepared-intent resume with zero double-broadcast, consumer scale-out on one lane → serialized) + one full batch landed on a testnet; record results in quickstart; update bindings contract as-built

**Checkpoint**: complete relay on the new platform; merge as its own PR.

---

## Phase 5: User Story 3 — Time-driven behavior without resident processes (P3)

**Goal**: alarms fire the hold ladder, receipt checks, TTLs, reconcile — within
declared tolerances.

**Independent Test**: quickstart Gate 4 tolerance assertions under emulation.

- [ ] T019 [US3] LaneDO delayed inbox + alarm in `lane_do.rs`: `DeferOperation` arm stores payload + post-increment attempt + due (core schedule values), packs the alarm earliest-of(delayed due, reconcile-while-intent); alarm handler re-drives due items through the same batch entry (idempotent re-derivation from storage per R5)
- [ ] T020 [US3] RecordDO receipt/TTL alarms in `record_do.rs`: receipt fetch → core receipt rules → lifecycle transition via `patch`; reschedule at the same interval values; TTL cleanup never-early; LaneDO reconcile alarm applies `audit_bundle_replay` + resume/mark/clear exactly as the shell composite
- [ ] T021 [US3] Gate 4: park at attempt k → redelivery within max(30 s, 10%) tolerance; reconcile + receipt alarm observations under `wrangler dev`; document measured tolerances in `contracts/platform-bindings.md`'s table

**Checkpoint**: no resident processes anywhere; merge as its own PR.

---

## Phase 6: User Story 4 — Scale and latency (P4)

**Goal**: SC-004/SC-007 evidenced on a deployed environment.

**Independent Test**: quickstart Gate 5.

- [ ] T022 [US4] Load harness (k6 or fetch-based) in `specs/002-cf-worker-shell/load/`: sustained ≥1,000 submits/s + ≥10,000 reads/s from three regions, 30 min, p95 targets; then per-chain isolation experiment (SC-007); tune queue batch size/consumer concurrency from results; record numbers in quickstart

**Checkpoint**: scale evidence recorded; merge as its own PR.

---

## Phase 7: User Story 5 — Coexistence, ops parity, governance (P5)

**Goal**: two deployments, one repo, operational parity, explicit ownership.

**Independent Test**: Gate 0 on the final merge + Gate 6 ownership review.

- [ ] T023 [P] [US5] `vela-relay-cf/src/arms/telegram.rs` + diagnostics: alert delivery with the core-decided gating, structured logs carrying the historical field names (EmitDiagnostic arm parity with the docker shell's)
- [ ] T024 [P] [US5] `docs/cloudflare.md`: deploy workflow (wrangler secrets, vars, Paid-plan requirement, EXECUTION_CHAINS semantics incl. the shared-key rule, lane-width provisioning note per R11, Gate 6 checklist); README architecture section gains the three-member picture
- [ ] T025 [US5] Constitution PATCH amendment PR: Principle I's illustrative shell list + Architectural Constraints name the second shell (`vela-relay-cf` wiring Cloudflare primitives); Sync Impact Report + version bump per governance
- [ ] T026 [US5] Finalize FR-012 equivalence notes: `specs/002-cf-worker-shell/equivalence-notes/transport.md` (queue ack granularity vs offset rule, structural lease answers, alarm tolerances, treasury lock mapping) + platform-bindings.md as-built pass

**Checkpoint**: feature complete; final merge PR.

---

## Phase 8: Polish & Cross-Cutting

- [ ] T027 Full gate pass: Gates 0–4 re-run, SC-003 rule-duplication audit recorded (grep set from quickstart Gate 1), suite counts recorded; `specs/002-cf-worker-shell/checklists/requirements.md` re-verified; memory of measured wasm size + battery result in quickstart

---

## Dependencies

```text
Phase 1 (T001–T003) → Phase 2 (T004–T006) → US1 (T007–T012) 🎯 MVP
US1 → US2 (T013–T018)      # execution consumes US1's records + queue
US2 → US3 (T019–T021)      # alarms extend LaneDO/RecordDO built in US2
US1 → US4 (T022, intake portion may start after US1; full run after US3)
US2 → US5 (T023–T026)      # ops parity needs execution surfaces
US4, US5 → Phase 8 (T027)
```

## Parallel Execution Examples

- **Phase 1**: T002 and T003 in parallel after T001.
- **US1**: T007, T008, T011 in parallel; then T009 → T010 → T012.
- **US2**: T015 and T016 in parallel after T014; T017 after T016.
- **After US2 lands**: one track runs US3 while another starts US5's docs/alert tasks.

## Implementation Strategy

- **MVP first**: Phases 1–3 deliver a globally distributed enqueue-only relay
  whose surface is byte-identical to production — independently valuable and
  zero on-chain risk.
- **Incremental delivery**: each story is its own branch/PR with the full
  pre-existing suite green (FR-003) and bindings-contract updates in the same
  PR (FR-012).
- **Largest risk**: US2 (T014–T018) — mitigated by the core Driver suite
  already pinning every disposition, the bindings contract fixing each arm's
  guarantee up front, and Gate 3's fault injection before any mainnet exposure.
