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
- [x] T011 [P] [US1] `vela-relay-cf/src/arms/rpc.rs` + `arms/market.rs`: chain RPC failover over fetch (user-header → Alchemy → directory fallback, same order; per-attempt deadlines via Delay races — metadata 10 s, Binance 2 s, RPC 2 s, mirroring the docker clients) and Binance/metadata fetches with KV caches. *Adjusted (rule-duplication avoidance, same pattern as wire): the three read methods' COMPUTATION was promoted additively to the core — `gas_math` consumed directly for tiers, NEW `quote` module (Multicall3 encode/decode, quote assembly, exact-decimal USD ordering, minimum adapters; 4 tests moved + 3 new), NEW `estimate` module (plan/finish split, simulation calldata + overrides + vendored bytecode moved, revert decoding; 8 tests moved) — and BOTH shells now drive those modules (docker handlers became thin drivers, battery-verified byte-identical; SUPPORTED_ENTRY_POINTS de-duplicated to the core const). Live-verified on the CF worker: gasPrice real tiers, quote real Arbitrum prices, estimate frozen validation errors*
- [x] T012 [US1] Gate 2 run: replay battery against `wrangler dev` AND the docker build, `diff -r` on bodies; scripted crash-window check (forced queue-send failure → response + RecordDO state match docker's); fix until byte-clean (2026-08-29: 16/16 RPC bodies byte-identical incl. valid accept + idempotent duplicate + accepted-record reads; GETs identical except the two declared deltas; statuses all equal). *Adjusted: the scripted crash-window forcing joins T011's change set; the crash-window DECISION is already pinned by the core Driver tests and the arm maps send-failure → QueueUnavailable verbatim*

**Checkpoint**: MVP — a globally deployable enqueue-only relay with byte-identical surface; merge as its own PR.

---

## Phase 4: User Story 2 — Edge execution with identical dispositions (P2)

**Goal**: LaneDO drives `ExecutionApp` verbatim; queue consumer routes and acks.

**Independent Test**: quickstart Gate 3 fault-injection set + a full testnet batch.

- [x] T013 [US2] `vela-relay-cf/src/lib.rs` `#[event(queue)]` consumer + `vela-relay-cf/src/lane_do.rs` scaffold: group batch by (chainId, `relayer_index_for_sender`), forward per-lane groups to LaneDO over the serde fetch protocol, map returned `ItemResolution`s → per-message `ack()`/`retry()`; DLQ flow for `DeadLetterRouted` AND malformed envelopes (docker `handle_malformed` parity: dead-letter durably before ack). *Verified end-to-end under wrangler dev: send → queue → grouped consumer → LaneDO → core defer('rpc', frozen reason) → RecordDO diagnostic patch → retry; envelope travels as JSON text (bindings contract updated)*
- [x] T014 [US2] LaneDO ExecutionApp driver + state arms in `lane_do.rs`: `LoadRecords`/`ReloadRecord`/`Mark*` → RecordDO subrequests; prepared-intent save (put-if-absent)/load/guarded-clear in DO storage; broadcast-seen cache; lease ops answered structurally (`true`); `RecordDeferred`/`NotifyIssue`/`EmitDiagnostic` arms (same gating — decided by core). *Also delivered here: RecordDO gained `Patch` (core `decide_patch` + mechanical merge), `RestoreQueued`, and `MarkBundleMemberSubmitted` (core `decide_bundle_submission` + the Lua 't' merge); CfConfig carries the full executor policy (docker names/defaults/bounds) and derives treasury + per-lane relayer via core vault; the delayed inbox stores payload+attempts+due with an earliest-of alarm. Chain-IO arms answer a staging Failed and `CheckChainSupported` stays false until T015 wires the trusted transport — every batch defers with the frozen 'chain has no trusted executor RPC' (verified live). NotifyIssue logs pending T023 Telegram*
- [x] T015 [P] [US2] Chain-IO arms in `lane_do.rs` + `arms/rpc.rs`: `SimulateIndividually`/`SimulateBundle`/`FetchAccountNonces`/`FetchTransactionContext`/`FetchTempoContext`/`BroadcastRaw`/`ProbeTransactionKnown`/`ProbeStaleNonce` over the failover transport; signing arms via core signing fns with secret bindings (keys never enter core); `FetchMarketPrice` with KV cache and unchanged fail directions. *As built: the executor transport is `arms/trusted.rs` (docker `TrustedRpcClient` ported onto fetch — explicit `VELA_RELAY_EXECUTOR_RPC_URLS` → Alchemy → directory, per-URL `eth_chainId` validation, item-level batch failover, broadcast classification; per-isolate caches; `Delay`-raced deadline from `VELA_RELAY_EXECUTOR_RPC_TIMEOUT_SECS`); simulation orchestration is `arms/simulate.rs` with all interpretation promoted to the NEW core `simulation` module (parsers, verdicts, reason strings, CREATE2 derivation — docker delegates) and broadcast/upstream-error classification promoted into core `broadcast` (docker delegates); no auto-deploy on this shell (declared in bindings contract; `Pending` verdict never produced). `FetchTempoContext` stays staged for T017; `FetchMarketPrice` uses a per-isolate 60 s cache (same TTL/error texts). `ResumeBundleIntent` composite, `ClearStaleIntent` guarded-clear gating, and `LoadChainAssets` missing-decimals batch ported line-for-line. Live-verified on wrangler dev against real Arbitrum via BOTH the explicit-URL tier and the pure directory tier: accepted send → queue → LaneDO → real `eth_simulateV1` → core `Rejected("handleOps reverted during simulation")` → record status `rejected` (no broadcast). Docker refactor re-verified byte-identical (42/42 battery + identical redis dump); Gate 2 CF vs docker 42/42; suites shell 83 + core 125; native clippy baseline 5; wasm clippy 0. Also fixed latent CF bug: metadata KV `put.execute()` future was never awaited (cache never persisted)*
- [x] T016 [P] [US2] `vela-relay-cf/src/treasury_do.rs`: TreasuryDO — lock state (holder token + deadline; acquire/ensure/release arms; store error → `Failed`, batch-fatal per bindings contract), `PreparedFundingIntent` storage, receipt-probe lock, `Record*`/`NoteFundingReceipt` arms with the frozen texts. *As built: one DO per chain; lease = docker store semantics exactly (acquire `SET NX PX`, renew/release token-guarded, expiry judged on read); the docker background heartbeat is replaced by renewal-on-touch — every TDO request from the holder piggybacks a lease extension (`TreasuryRequest::renew`, declared in bindings contract). Funding storage put-if-absent + hash-guarded clear (Lua parity); receipt probe = expiring `probe:{txhash}` throttle slot, deleted as housekeeping when its intent clears. The whole treasury arm set landed in `lane_do.rs`, including `FetchTreasuryContext` (2-call trusted batch, docker error texts), `FetchTransactionReceipt`, and BOTH treasury signing arms (`SignTreasuryTransfer` via core `sign_eip1559` + `TOP_UP_GAS_LIMIT`; `SignTreasuryPathUsd` via core `sign_tempo`, pathUSD/tip-0) with `vault::derive_treasury_secret_key` — keys never enter core. CfConfig gained `VELA_RELAY_EXECUTOR_LEASE_TTL_SECS` (30) and `VELA_RELAY_EXECUTOR_RECEIPT_POLL_SECS` (3). wrangler v3 migration + TREASURY binding live-verified under wrangler dev; Gate 2 battery re-verified 42/42; suites shell 83 + core 125; wasm clippy 0. The end-to-end funding drive (lease contention, save/clear, receipt probe against a mining chain) lands with T018's Gate 3 anvil rig — funding only triggers once real survivors exist, which fixture ops cannot produce*
- [x] T017 [US2] Tempo tail arms: `FetchTempoTreasuryContext`, `SignTempoBundle`, `SignTreasuryPathUsd` — the pathUSD twin over the same transports; verify against the core's tempo Driver walks' operation sequences. *As built (`SignTreasuryPathUsd` had already landed with T016's treasury signing pair): `FetchTempoContext` = the docker four-call batch (block → gasPrice → pinned-constant base-fee fallback chain, relayer nonce, relayer pathUSD balance), `FetchTempoTreasuryContext` = the three-call batch including the exact-transfer `eth_estimateGas` with the `feeToken` field (raw estimate — the buffer is core-applied), `SignTempoBundle` = core `sign_tempo` 0x76 envelope (pathUSD fee token, tip 0) over the per-lane vault key, `RecordTempoTreasuryShortfall` log with the frozen text and `reserve_path_usd = TEMPO_TREASURY_FLOOR`. Every error string byte-identical to the docker engine. The core's tempo Driver walks pin the operation sequences these arms answer (they run in the shared suites); live Tempo-chain exercise belongs to T018's rig. Gate 2 re-verified 42/42; suites 208; wasm clippy 0*
- [x] T018 [US2] Gate 3: scripted fault injection under `wrangler dev` (duplicate delivery → durable-skip, reordered nonces → delayed-inbox/reject, DO restart mid-batch → prepared-intent resume with zero double-broadcast, consumer scale-out on one lane → serialized) + one full batch landed on a testnet; record results in quickstart; update bindings contract as-built. *As run: anvil rig claiming chain 42161 (real EntryPoint v0.7 runtime via setCode, funded treasury, JSON-RPC shaper restoring the production error shapes anvil strips + injecting faults; metadata KV-seeded with `rpc:[]` so no executor traffic leaves localhost — full details in quickstart.md "Gate 3 as run"). Landed: four treasury funding cycles + four mined handleOps bundles across four lanes via BOTH simulation tiers; future-nonce park with `attempt=1` + alarm; stale-nonce terminal reject; six-op same-sender burst → exactly one outer transaction; hard-kill of every wrangler/workerd process → prepared intent AND parked delayed-inbox entries survived, ~117 post-broadcast resume redeliveries with the relayer nonce frozen (zero double-broadcast); queue max_retries → DLQ backstop observed at 101 attempts. The gate CAUGHT A REAL BUG: u128 `amount_wei` degraded to a float across the TreasuryDO boundary (workers-rs `Request::json` = JS JSON.parse → JsValue) — the TDO boundary now speaks JSON text end-to-end (bindings contract updated). Known pre-US3 limitation documented: without T020 receipt confirmation, submitted members never reach terminal state so their lane retains its intent and defers new work to the DLQ backstop (resume-first rule as specified; US3 closes it). A public-testnet run needs user-funded keys — the controlled chain covers the full pipeline including broadcast, mining, and receipts. Gate 2 re-verified 42/42 after the fix; suites 208; wasm clippy 0*

**Checkpoint**: complete relay on the new platform; merge as its own PR.

---

## Phase 5: User Story 3 — Time-driven behavior without resident processes (P3)

**Goal**: alarms fire the hold ladder, receipt checks, TTLs, reconcile — within
declared tolerances.

**Independent Test**: quickstart Gate 4 tolerance assertions under emulation.

- [x] T019 [US3] LaneDO delayed inbox + alarm in `lane_do.rs`: `DeferOperation` arm stores payload + post-increment attempt + due (core schedule values), packs the alarm earliest-of(delayed due, reconcile-while-intent); alarm handler re-drives due items through the same batch entry (idempotent re-derivation from storage per R5). *As built: due entries re-drive via `execute_batch` (docker `reconcile_delayed_user_operations` scoped to the lane, `DELAYED_REDRIVE_BATCH` = docker's 100); claim fencing = a re-park during the batch rewrites the entry, invalidating the pass's (attempts, due) snapshot — exactly the docker Lua's claim-invalidated no-op; a transient failure climbs the same core hold ladder in place (docker `retry_delayed_user_operation` HINCRBY + schedule); retention = max(`VELA_RELAY_EXECUTOR_ATTEMPT_TTL_SECS`, 14 d) sliding from `updated_ms` (docker PEXPIRE refresh). Ladder measured live at 5/10/21 s (≤1 s deviation); a parked op auto-executed once its nonce became current (Gate 4)*
- [x] T020 [US3] RecordDO receipt/TTL alarms in `record_do.rs`: receipt fetch → core receipt rules → lifecycle transition via `patch`; reschedule at the same interval values; TTL cleanup never-early; LaneDO reconcile alarm applies `audit_bundle_replay` + resume/mark/clear exactly as the shell composite. *As built, one deliberate deviation from the task's letter, faithful to docker's actual architecture: receipts are BUNDLE-scoped (docker's reconciler works per prepared intent, never per record — `next_receipt_check_at_ms` has no active consumer on either shell), so the receipt flow lives in the LaneDO reconcile alarm: resume → `eth_getTransactionReceipt` → core `receipt_succeeded`/`user_operation_events` → per-member `receipt_patch` through RecordDO `Patch` (docker's byte-identical field set: status/transactionHash/admitted/receipt/blockHash/blockNumber/event; Included iff the member's event succeeded, Rejected for members without a successful event, Failed on a reverted receipt) → clear intent + bundle index. Armed on SavePreparedBundle (covers the save-to-broadcast crash window) and on MarkBundleSubmitted; cadence = `VELA_RELAY_EXECUTOR_RECEIPT_POLL_SECS` (the DO is the lane's only prober, so the alarm IS the docker claim throttle). RecordDO alarms stay TTL-only, never-early (unchanged). Live: submitted → included in 2–3 s; `eth_getUserOperationReceipt` serves the real on-chain event; the Gate-3 lane wedge is closed*
- [x] T021 [US3] Gate 4: park at attempt k → redelivery within max(30 s, 10%) tolerance; reconcile + receipt alarm observations under `wrangler dev`; document measured tolerances in `contracts/platform-bindings.md`'s table. *Measured on the anvil rig (quickstart 'Gate 4 as run'): ladder re-drives at +5/+10/+21 s vs the 5/10/20 s schedule (≤1 s deviation); full lifecycle queued→submitted (T+6–8 s)→included (T+9–10 s); park→auto-execute arc closed at T+15–17 s with zero external traffic. Tolerances table updated with measured columns. Rig lesson recorded: a stale prepared intent from a wiped chain correctly refuses to clear without terminal proof — local `.wrangler/state` must be reset together with the rig chain. Gate 2 re-verified 42/42; suites 208; wasm clippy 0*

**Checkpoint**: no resident processes anywhere; merge as its own PR.

---

## Phase 6: User Story 4 — Scale and latency (P4)

**Goal**: SC-004/SC-007 evidenced on a deployed environment.

**Independent Test**: quickstart Gate 5.

- [ ] T022 [US4] Load harness (k6 or fetch-based) in `specs/002-cf-worker-shell/load/`: sustained ≥1,000 submits/s + ≥10,000 reads/s from three regions, 30 min, p95 targets; then per-chain isolation experiment (SC-007); tune queue batch size/consumer concurrency from results; record numbers in quickstart. *Harness BUILT and smoke-verified (specs/002-cf-worker-shell/load/): load.js encodes SC-004 as k6 scenarios+thresholds (constant-arrival submits/reads, p95 500/200 ms, failure rate <0.1%, per-region nonce salting, accepted-only counting via the JS port of the payment-calldata builder — smoke vs wrangler dev: 603/603 checks, 101/101 submissions accepted, zero refusals) and isolation.js encodes SC-007 (baseline vs saturated runs, chain-tagged p95 comparison — smoke 257/257). The RUN remains user-gated: a real deployed Workers environment (Paid account + `wrangler deploy` per docs/cloudflare.md, executor disabled, throwaway secret) + three regional load generators; README.md in load/ carries the exact commands, posture rules, and the result-record table*

**Checkpoint**: scale evidence recorded; merge as its own PR.

---

## Phase 7: User Story 5 — Coexistence, ops parity, governance (P5)

**Goal**: two deployments, one repo, operational parity, explicit ownership.

**Independent Test**: Gate 0 on the final merge + Gate 6 ownership review.

- [x] T023 [P] [US5] `vela-relay-cf/src/arms/telegram.rs` + diagnostics: alert delivery with the core-decided gating, structured logs carrying the historical field names (EmitDiagnostic arm parity with the docker shell's). *As built: the suppression RULES (fingerprint normalization — numbers → #, ≥10-hex-digit literals → <hex> — bounded single-line reason, frozen message text) were promoted to the NEW core `alert` module (4 tests moved shell→core; docker alert.rs delegates); the suppression SLOT lives in each chain's TreasuryDO (`ClaimAlert` = SET NX PX cooldown, token-guarded `ReleaseAlert` on delivery failure — strongly consistent per chain, unlike KV); transport = fetch + 5 s Delay race; config TELEGRAM_BOT_TOKEN (secret) + TELEGRAM_CHAT_ID paired-or-neither + VELA_RELAY_TELEGRAM_ALERT_COOLDOWN_SECS (30 min default) — docker names/rules verbatim. Unconfigured = silent no-op (docker parity; the T014 placeholder log removed). EmitDiagnostic arm parity was already complete since T014*
- [x] T024 [P] [US5] `docs/cloudflare.md`: deploy workflow (wrangler secrets, vars, Paid-plan requirement, EXECUTION_CHAINS semantics incl. the shared-key rule, lane-width provisioning note per R11, Gate 6 checklist); README architecture section gains the three-member picture. *Done: full provision→secrets→vars→deploy→verify workflow, the (chain, key set) ownership rule, narrowing-width drain warning, executor RPC resolution order, Gate 6 checklist, operational notes (JSON-text DO boundaries, Delay-raced fetches, per-chain alert dedup, wasm size); README now describes the three-member workspace with both shells*
- [x] T025 [US5] Constitution PATCH amendment PR: Principle I's illustrative shell list + Architectural Constraints name the second shell (`vela-relay-cf` wiring Cloudflare primitives); Sync Impact Report + version bump per governance. *Done: 1.0.2 → 1.0.3; Principle I now says 'a Shell wires Core decisions to real infrastructure; the repository has two' with both shells' primitive lists; Architectural Constraints gained the second-shell bullet (wasm-only member, bindings-contract-in-same-PR rule, (chain, key set) ownership) and the shell-owned/toolchain bullets name both shells' mechanisms; Sync Impact Report records the rationale*
- [x] T026 [US5] Finalize FR-012 equivalence notes: `specs/002-cf-worker-shell/equivalence-notes/transport.md` (queue ack granularity vs offset rule, structural lease answers, alarm tolerances, treasury lock mapping) + platform-bindings.md as-built pass. *Done: transport.md written covering ack-vs-offset mapping, structural lane lease + never-produced Interrupted, the treasury lock + renewal-on-touch, alarms-for-loops with measured tolerances, alert suppression, and the no-auto-deploy simulation delta; platform-bindings.md needed no further pass — it was updated as-built in the same PR as every transport change (the FR-012 rule working as designed)*

**Checkpoint**: feature complete; final merge PR.

---

## Phase 8: Polish & Cross-Cutting

- [x] T027 Full gate pass: Gates 0–4 re-run, SC-003 rule-duplication audit recorded (grep set from quickstart Gate 1), suite counts recorded; `specs/002-cf-worker-shell/checklists/requirements.md` re-verified; memory of measured wasm size + battery result in quickstart. *Recorded in quickstart 'Full gate pass record': Gate 0 green (fmt / clippy 5-baseline / docker 79 + core 129), Gate 1 clean (wasm check + clippy 0, core tree IO-free, SC-003 grep finds no local rule definitions), Gate 2 final 42/42, Gates 3–4 as-run records, Gate 5 deferred with T022, Gate 6 checklist in docs. Final wasm 2,422,916 B raw / 804,691 B gz (>12× Paid-limit headroom); checklist 16/16*

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
