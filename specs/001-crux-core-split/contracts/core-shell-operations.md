# Contract: Core ↔ Shell Operation Vocabularies

The internal contract between `vela-relay-core` (decisions) and `vela-relay` (IO).
Each app defines one `Operation` enum (`type Output = …Result`) wrapped in a
single-variant `#[effect] Work(Operation)`. Conventions (research.md R2/R3):

- Shell executors NEVER return `Err` to the core; every `Result` type enumerates
  infrastructure failure as data variants.
- Programs are strictly sequential: at most one operation in flight per core
  (asserted by the Driver test harness).
- The vocabulary is deliberately incomplete: operations the system must never
  perform (e.g. un-admitting a durable admission) do not exist.
- All nondeterminism (time, ids, chain context, policy values, market prices)
  enters via the start Event; operations below carry only business parameters.

## AdmissionOperation (`admission.rs`) — Story 5

| Operation | Output variants (failure included) |
|---|---|
| `FindByHash { hash }` | `TaskFound { record: Option<StoredUserOperation> }`, `StoreUnavailable` |
| `CreateQueuedRecord { record }` | `Created`, `AlreadyExists { existing }`, `StoreUnavailable` |
| `Enqueue { envelope }` | `Enqueued`, `QueueUnavailable` |
| `MarkAdmitted { hash }` | `Persisted`, `StoreUnavailable` |

Preserved policies: duplicate resolution via `admission_fingerprint`;
`QueueUnavailable` after `Created` keeps the record (crash-window behavior
unchanged; no delete operation exists).

## ExecutionOperation (`execution.rs`) — Story 4

| Group | Operations | Output variants |
|---|---|---|
| Load | `LoadOperations { hashes }` | `Loaded { records }`, `StoreUnavailable` |
| Lease | `AcquireLaneLease { scope }`, `ReleaseLaneLease` | `LeaseAcquired { ok }`, `Released`, `StoreUnavailable` |
| Chain read | `FetchTransactionContext`, `FetchAccountNonces { senders }`, `FetchReceipts { tx_hashes }`, `FetchTransactionByHash { hash }` | `Context { … }`, `Nonces { … }`, `Receipts { … }`, `TxKnown { bool }`, `ChainReadFailed` |
| Simulate | `SimulateOperations { ops }`, `SimulateBundle { ops }` | `Simulated { verdicts }`, `ChainReadFailed` |
| Market | `FetchMarketPrice { symbol }` | `Price { value }`, `PriceUnavailable` |
| Funding | `FetchTreasuryContext`, `SignFundingTransaction { plan }`, `SavePreparedFunding { intent }`, `ClearPreparedFunding { compare }` | `Context`, `Signed { raw }`, `Persisted`, `CompareMismatch`, `StoreUnavailable` |
| Sign | `SignBundle { plan, lane }` | `Signed { raw, tx_hash }` (shell injects key custody; signing math is core) |
| Persist | `SavePreparedBundle { intent }`, `ClearPreparedBundle { compare }`, `ListPreparedBundles` | `Persisted`, `CompareMismatch`, `Intents { … }`, `StoreUnavailable` |
| Broadcast | `BroadcastRawTransaction { raw }` | `Broadcast(TxOutcome)` where `TxOutcome = Confirmed | Reverted | ReceiptUncertain { error } | SendFailed { error } | NoncePoolUnavailable` |
| Outcome | `MarkBundleSubmitted { … }`, `MarkIncluded { … }`, `MarkFailed { hash, kind, reason }`, `RecordTransientFailure { hash, reason }`, `SaveDeadLetter { … }`, `RecordDiagnostic { … }` | `Persisted`, `RefusedByLifecycle`, `StoreUnavailable` |
| Hold | `DeferOperation { hash, attempt, due_at_ms }`, `CompleteDelayed { hash }`, `RetryDelayed { hash }` | `Deferred { attempts }`, `Done`, `ClaimLost`, `StoreUnavailable` |
| Alert | `ClaimAlertSlot { fingerprint }`, `SendAlert { text }` | `Claimed { bool }`, `Delivered { ok }` |

Settled outcome: `BatchVerdict::Advance | Retry { reason }` — the shell maps it
onto the contiguous-durable-prefix offset rule unchanged.

## Lifecycle contract (Story 1)

Every status write flows through one core decision:
`decide_patch(current_record, patch) → Apply { merged } | RefuseIllegalTransition |
RecordMissing`. The shell's reduced Redis script performs only an
optimistic-concurrency guard: apply iff the stored status still equals the status
the decision was computed against; otherwise report conflict and the caller
re-reads and re-decides. No transition table exists outside the core.

## Hold contract (Story 2)

`decide_hold(attempt, now_ms, ladder) → Defer { due_at_ms } | RejectBudgetExhausted`.
The store persists the given absolute due time; Redis `TIME` is no longer a
decision input (claim-lease bookkeeping may still use server time — that is
infrastructure, not policy).

## Frozen invariants

- Operation/Result serde shapes are internal (no cross-process serialization), but
  reason strings inside them are user-visible and byte-frozen.
- The executor `match` in the shell is flat: one arm per Operation, each folding
  its infrastructure errors into the declared failure variants — no policy in arms.
- Lease heartbeating remains a shell driver (`run_with_lease_heartbeat`) racing the
  program; losing the lease surfaces to the core as an operation result, and the
  core decides abandon/retry exactly as today.
