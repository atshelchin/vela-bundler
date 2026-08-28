# Phase 1 Data Model: vela-relay-core core vocabulary

All types below live in the `vela-relay-core` crate. Names are normative for the
implementation; field lists capture business content, not exact Rust syntax.
Source-of-truth references point at the pre-refactor code the behavior is lifted
from (Constitution VI: byte-identical migration).

## 1. Shared vocabulary (`task.rs`)

### UserOperationStatus
`Queued | NotSubmitted | Submitted | Rejected | Included | Failed`
(wire names unchanged: `queued`, `not_submitted`, `submitted`, `rejected`,
`included`, `failed`; the API-only `not_found` remains a response concept, not a
stored state). Today: `src/app/rpc/types.rs` (`UserOperationStatusKind`).

### StoredUserOperation
The durable record: user operation hash (id), chain id, entry point, sender, nonce,
packed operation payload, status, `admitted` flag, transaction hash (once
submitted), executor diagnostics (last stage / error / attempt timestamp),
timestamps. Field names and JSON shape are frozen (`camelCase` keys as stored
today). Today: `src/app/user_operation_store.rs` record shape.

### RoutedUserOperation
A queue envelope: operation hash + chain id + lane index (+ payload as consumed).
Today: `src/worker/consumer.rs::parse_routed_operation`.

## 2. Lifecycle (`lifecycle.rs`) — Story 1

### Transition table (THE single executable definition)
Lifted byte-for-byte from `PATCH_RECORD_SCRIPT`
(`src/app/user_operation_store.rs:45-49`):

| From | Allowed to |
|---|---|
| `queued` | `not_submitted`, `submitted`, `rejected`, `failed` |
| `not_submitted` | `submitted`, `rejected`, `failed` |
| `submitted` | `rejected`, `included`, `failed` |
| `rejected` / `included` / `failed` | (terminal — no transitions) |

Rules preserved exactly:
- A patch whose `status` equals the current status (or carries no status) is always
  a legal write (field merge, no transition check).
- A patch against a missing record is a no-op reported as not-applied.
- Terminal statuses accept same-status field merges but no transitions
  (`is_durable_status` today at `engine.rs:3400` becomes
  `UserOperationStatus::is_terminal`).

### Bundle-submission transition (second embedded copy, also Story 1 scope)
`MARK_BUNDLE_SUBMITTED_SCRIPT` (`user_operation_store.rs:63-98`) embeds its own
policy; it is re-expressed through the same core table plus these preserved rules:
- Eligible: same chain AND status ∈ {`queued`, `not_submitted`} → becomes
  `submitted` with the bundle's transaction hash, `admitted = true`.
- Idempotent re-index: same chain AND already `submitted` with the *same*
  transaction hash → indexed again without mutation.
- Chain comparison uses the decimal-string chain id with the documented
  fail-closed legacy fallback (comment at `:70-72` preserved).

### PatchDecision (core output consumed by the shell store adapter)
`Apply { merged_fields } | RefuseIllegalTransition | RecordMissing` — the shell's
reduced script performs a guarded write: "apply only if stored status still equals
the status this decision was computed against" (optimistic-concurrency guard, no
policy).

## 3. Hold ladder (`hold.rs`) — Story 2

### HoldLadder (policy values injected from validated config)
- `base_delay_ms = 5_000` (`DELAYED_RETRY_BASE_MS`, `user_operation_store.rs:33`)
- `max_delay_ms = 300_000` (`DELAYED_RETRY_MAX_MS`, `:34`)
- `max_attempts = settlement_hold_max_attempts` (default 12, ≈35 min of holds)

### Schedule function (lifted from Lua `:207-217` / `:283-293`)
`delay(attempt) = min(base × 2^(attempt−1), max)` computed by the same
doubling loop semantics: attempt 1 → 5 s, 2 → 10 s, 3 → 20 s, 4 → 40 s, 5 → 80 s,
6 → 160 s, 7+ → 300 s. The core exports the schedule as a lookup table
(`retry_delay_schedule_ms()`); the scripts index it by the post-increment
attempt count and anchor `due = TIME + delay` server-side so the writer and the
claim reader share one clock (research.md R9, adjusted).

### HoldDecision
`Defer { attempt, due_at_ms } | RejectBudgetExhausted { reason }` — reason string
byte-identical to today's (`settlement_hold_reason` / `settlement_rejection_reason`,
`engine.rs:3498/3505`). Guard preserved: `attempt > max_attempts ⇒ reject`
(`engine.rs:1846`).

## 4. Settlement (`settlement.rs`) — Stories 3 & 6

### ReimbursementLeg / BatchReimbursement (Story 6: the single parser)
Decoded from Safe `executeUserOp → MultiSend(delegatecall)` calldata: payer, asset
(native or allowlisted stable), amount, recipient. One implementation absorbing
both `worker/executor/settlement.rs:172` (`Address`/`U256`) and
`app/rpc/handlers/in_band_settlement.rs:22` (String/`u128`), with thin
representation adapters where the HTTP layer needs strings. One
`TRUSTED_MULTISEND` constant.

### SettlementInputs (all injected — Constitution II)
Quoted fee (`2×baseFee + tip`), base fee, tip, markup bps, inclusion-floor bps
(floor = 1.5× base fee today), per-op gas allocation, per-payer balances as fetched,
optional market USD price (pre-fetched by the shell), hold attempt count.

### SettlementVerdict
`Accept { fee_per_gas } | Reprice { affordable_fee_per_gas } |
Hold { ops } | Reject { ops: Vec<(hash, reason)> }`
Math lifted unchanged from `settlement.rs` (`affordable_fee_per_gas:109`,
`inclusion_floor_fee_per_gas:123`, `evaluate_batch:269`, `evaluate_one:324`,
`native_to_usd_stable_ceil:396`); orchestration lifted from
`settle_at_affordable_fee` (`engine.rs:1898`) with its in-place fee mutation
replaced by the returned verdict.

## 5. Execution (`execution.rs`) — Story 4

### ExecutionApp (per-lane, per-batch one-shot core)
- **Event**: `Start { envelope_ids, chain_context, now_ms, policy } |
  Settled(BatchVerdict)`
- **Model**: `Option<BatchVerdict>`
- **BatchVerdict**: `Advance | Retry { reason }` — mapped by the shell onto the
  contiguous-durable-prefix offset rule (`consumer.rs::batch_result`) unchanged.

### Program step vocabulary (internal decisions, all pure)
- `AdmissionAction`: `Execute | Recover | DeadLetter` (today `engine.rs:3392`).
- `SimulationVerdict` interpretation (today `simulation.rs`): per-op pass/fail and
  nonce-mismatch classification.
- `BroadcastDisposition` (`broadcast.rs`): `Accepted | Ambiguous | Rejected` ×
  hash-match × `nonce_too_low` × stale-nonce decision table (today
  `broadcast_bundle_intent`, `engine.rs:2243`).
- `FundingPlan` (`funding.rs`): target = `max(prefund×5, float_target, float_min)`
  capped by `native_top_up_cap`; `treasury_affordable_top_up` (today
  `engine.rs:3485/2888-2907`); Tempo pathUSD variant preserved.
- `ResumeDisposition` for prepared-intent recovery (today
  `BundleResumeDisposition`) with the correlation guard: a resumed intent must
  match the stored compare-hash.
- `ReceiptOutcome` (in `bundle.rs`, today `receipt.rs`): confirmed / reverted per
  the `receipt_succeeded` rule (`engine.rs:2539-2571`).

### Prepared intents (durable, stored by shell; shapes frozen)
`PreparedBundleIntent`, `PreparedFundingIntent`,
`PreparedSimulationDeploymentIntent` (today `user_operation_store.rs:407/421/434`).

## 6. Admission (`admission.rs`) — Story 5

### AdmissionApp (per-request one-shot core)
- **Event**: `Submit { request, new_task_id, now_ms, chain_id, entry_point,
  settlement_recipient } | Settled(AdmissionOutcome)`
- **Model**: `Option<AdmissionOutcome>`
- **AdmissionOutcome** (rendered to today's exact HTTP responses by the shell):
  accepted (hash ack), duplicate-identical (idempotent ack), duplicate-conflicting
  (refusal), invalid (per-check reason strings from today's validators),
  queue-unavailable-after-record (today's crash-window response), store-unavailable.
- Preserved pure pieces: `existing_admission_action`
  (`send_user_operation.rs:185`), `admission_fingerprint` (`:225`), the in-band
  zero-fee validations. Two-phase order frozen: durable record (SETNX) → queue
  append → admitted mark; no un-admit operation exists (Constitution III).

## 7. Signing & bundle math (`signing.rs`, `bundle.rs`)

Deterministic transforms migrated as-is with secrets injected per call:
EIP-1559 + Tempo 0x76 signing (`transaction.rs`), HKDF derivation and lane routing
(`vault.rs` — salt and golden vectors unchanged), ABI packing / userOpHash
(`abi.rs`), per-op gas allocation (`cost.rs`), gas-price math
(`price_from_fee_history`, `median_priority_fee`, `scale`).

## 8. Relationships

```
AdmissionApp ──creates──▶ StoredUserOperation (status=queued) ──envelope──▶ Iggy
ExecutionApp ──loads────▶ StoredUserOperation ──governed by──▶ lifecycle table
ExecutionApp ──uses─────▶ settlement verdict ──may──▶ HoldDecision (delayed inbox)
ExecutionApp ──uses─────▶ FundingPlan / BroadcastDisposition / ReceiptOutcome
ExecutionApp ──settles──▶ BatchVerdict ──drives──▶ shell offset commit
lifecycle table ◀─consulted by─ every status write (store adapter, single path)
```

## 9. Validation rules carried into the core

- Zero `maxFeePerGas`/`maxPriorityFeePerGas` required (in-band fee model).
- Reimbursement must target the settlement recipient with an allowlisted asset.
- Markup ≥ 10000 bps, inclusion floor > 10000 bps, float target ≥ float min,
  receipt confirmations ≥ 2, pool width = 10 — validated at config parse (shell)
  and consumed by the core as trusted policy data.
