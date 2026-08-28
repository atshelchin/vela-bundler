# Equivalence Note — US4: Executor batch pipeline through the core

For the PR description (spec FR-011). Covers T025–T033 + the T035 deletions.

## Old → New (decision by decision)

| Old behavior (main) | New location |
|---|---|
| `handle_lane_batch` triage (`engine.rs:376-593`): mixed-batch guard, chain-support gate, asset resolution, missing-record dead-letter/restore/reload, durable skip, `admission_action` recover/dead-letter, `candidate_from_record` validation, per-arm result writing | `vela_relay_core::execution::drive_batch` — the same sequence as core decisions over `CheckChainSupported`/`LoadChainAssets`/`LoadRecords`/`DeadLetterRouted`/`RestoreQueued`/`ReloadRecord`/`MarkAdmitted`/`MarkRejected` operations. All per-item failure strings byte-identical. `admission_action`, `queued_operation_from_routed`, `candidate_from_record`, `queue_record_matches` moved as pure fns with their validation strings |
| Dedupe by hash, one-nonce-per-sender, `max_bundle_operations` truncation | Pure, in-program, byte-identical |
| Lease: acquire → heartbeat → execute → release; renew checkpoints inside the pipeline | `AcquireLaneLease` starts a shell heartbeat task; `EnsureLaneLease` checkpoints sit at exactly the old `ensure_lease` call sites; release in `BatchShell::finish`. A failed renewal (lease gone or store error) trips a `LeaseInterrupt`: the driver abandons the in-flight operation mid-await (biased `select!`, as the old wrapper did) and answers every further non-bookkeeping operation with `ExecutionOutcome::Interrupted`, which the core converts into the transient `Err` channel — the batch settles through the same "execution" deferral, reason strings included, as the old future-drop. Post-audit: this restored the old abort promptness; the interrupt fires at operation boundaries, which are exactly the program's await points |
| `execute_with_lane_lease` (`engine.rs:786-1243`): prepared-intent resume, EntryPoint uniformity, per-op simulation verdict handling, nonce-mismatch resolution, bundle simulation with 1-op fallback, settlement gate, hold, funding precedence + USD cap, sign/persist/broadcast/mark, save-race resume, indexed-count guard | The core program `execute_with_lane_lease`, branch-for-branch; the 18-step Driver walk pins the operation order, and failure-injection tests pin store-outage, lease-unavailable, durable-skip, and the in-budget hold |
| `resolve_nonce_mismatches` (results-slice mutation) | `resolve_nonce_mismatch_items` — same probes, same defer/reject rules, same strings/logs; outcomes returned as data. Composite operation (T034 decomposes) |
| `execute_tempo_bundle` (results-slice mutation) | `execute_tempo_bundle_composite` — same fee-token gate, pathUSD settlement gate, funding, sign, save-race, broadcast, mark; outcomes returned as data. Composite (T034) |
| `native_top_up_cap` (market price → USD cap, static fallback + logs) | The fail-open policy is now IN THE CORE: `FetchMarketPrice` failure → static `top_up_max_wei` (silent, as before); success → `native_amount_for_usd_cap`, and the historical debug ("using USD-denominated relayer top-up cap") and warn ("could not convert … using static cap") lines are emitted via `EmitDiagnostic` |
| `ensure_relayer_funded` precedence check (`relayer_balance >= required_prefund → Ready`) | Lifted into the core program (funding operation is only requested when the balance is short); the treasury lease + probe + sign + broadcast remain the composite `EnsureRelayerFunded` |
| Deferral diagnostics + Telegram policy (`record_executor_deferred` with embedded `should_notify_executor_deferred`) | The policy moved to the core: `RecordDeferred` (store write) and `NotifyIssue` (Telegram) are separate operations and the core decides when to emit each; `should_notify_executor_deferred` keeps the old ALLOWLIST form (`rpc\|assets\|simulation\|bundle_simulation\|broadcast\|execution`) — in particular `in_band_settlement_hold` stays silent. (The first core landing had flipped this to a denylist, silently paging Telegram on every in-budget hold; the 2026-08-28 audit caught it and the allowlist was restored, pinned by the extended unit test and the hold Driver walk) |
| Wire/store vocabulary: `RoutedUserOperation`, `QueuedUserOperation`, `StoredUserOperation`, `PreparedBundleIntent`, `StoredUserOperation::rpc_status` | Moved to `vela_relay_core::task` (serde shapes frozen) behind path-stable re-exports; `rpc_status` became a shell free function (the RPC response struct stays transport-side) |
| Delayed-inbox reprocessing (`process_delayed_lane`) | Unchanged — it feeds the same `handle_lane_batch`, which now drives the core program |

## Declared divergences (post-audit revision, 2026-08-28)

1. **Heartbeat abort granularity**: the old wrapper cancelled the pipeline at
   any await point; the new interrupt cancels the in-flight operation and
   refuses every subsequent one at operation boundaries — the program's only
   await points — so the practical window is identical (≤ ttl/3 + one
   operation). Bookkeeping that the old engine performed after the abort
   (deferral diagnostics, Telegram, the "UserOperation lane execution
   deferred" warn, treasury-lease release) still runs; a diagnostics loop the
   old abort could cut mid-way now completes its writes — strictly more
   complete records, same stored strings.
2. **Log placement**: tracing stays in the shell. Lines whose data the arm
   has are emitted from the arms; lines whose data exists only inside core
   decisions (simulation waits/unavailability, bundle-simulation verdicts,
   floor-unfundable, reprice, hold-budget-exhausted, the field-rich
   settlement/Tempo rejections, USD-cap, "UserOperation lane execution
   deferred") are emitted through the `EmitDiagnostic` operation with the old
   engine's byte-identical messages, levels, and field names. The hold info
   log ("holding UserOperation until the market fits …") keeps its arm-side
   form: `paid`/`required` live inside the recorded reason string.
3. **`resolve` engine failure**: a crux resolve error yields
   "could not resolve execution effect" per item (new, previously impossible);
   a program that never settles yields "lane batch never settled".
4. **Store failure while persisting a simulation rejection** defers the whole
   lane batch (old `map_err(store_item_error)?` semantics, restored by the
   audit — the first landing failed only that item and let the rest
   broadcast).
5. **Store error during treasury-lease acquire** is batch-fatal through the
   transient channel (old semantics, restored by the audit — the first
   landing folded it into "another funder owns the lease" and deferred
   silently under the "funding" stage).

## Test accounting

180 → 185: engine's `admission_action`/`should_notify` tests moved to the core
(+0), five new Driver tests (+5). Shell 110 / core 75; engine.rs shrank to
~3,100 lines and no longer contains business decisions on the batch path.
