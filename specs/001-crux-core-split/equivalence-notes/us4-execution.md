# Equivalence Note — US4: Executor batch pipeline through the core

For the PR description (spec FR-011). Covers T025–T033 + the T035 deletions.

## Old → New (decision by decision)

| Old behavior (main) | New location |
|---|---|
| `handle_lane_batch` triage (`engine.rs:376-593`): mixed-batch guard, chain-support gate, asset resolution, missing-record dead-letter/restore/reload, durable skip, `admission_action` recover/dead-letter, `candidate_from_record` validation, per-arm result writing | `vela_relay_core::execution::drive_batch` — the same sequence as core decisions over `CheckChainSupported`/`LoadChainAssets`/`LoadRecords`/`DeadLetterRouted`/`RestoreQueued`/`ReloadRecord`/`MarkAdmitted`/`MarkRejected` operations. All per-item failure strings byte-identical. `admission_action`, `queued_operation_from_routed`, `candidate_from_record`, `queue_record_matches` moved as pure fns with their validation strings |
| Dedupe by hash, one-nonce-per-sender, `max_bundle_operations` truncation | Pure, in-program, byte-identical |
| Lease: acquire → heartbeat → execute → release; renew checkpoints inside the pipeline | `AcquireLaneLease` starts a shell heartbeat task; `EnsureLaneLease` checkpoints sit at exactly the old `ensure_lease` call sites; release in `BatchShell::finish`. **Declared divergence 1**: the old heartbeat raced the pipeline and aborted it on renewal failure; the new heartbeat task logs and stops, and the loss is caught at the next `EnsureLaneLease` checkpoint (the same checkpoints the old code also had). The lease still fences every store write that matters |
| `execute_with_lane_lease` (`engine.rs:786-1243`): prepared-intent resume, EntryPoint uniformity, per-op simulation verdict handling, nonce-mismatch resolution, bundle simulation with 1-op fallback, settlement gate, hold, funding precedence + USD cap, sign/persist/broadcast/mark, save-race resume, indexed-count guard | The core program `execute_with_lane_lease`, branch-for-branch; the 18-step Driver walk pins the operation order, and failure-injection tests pin store-outage, lease-unavailable, durable-skip, and the in-budget hold |
| `resolve_nonce_mismatches` (results-slice mutation) | `resolve_nonce_mismatch_items` — same probes, same defer/reject rules, same strings/logs; outcomes returned as data. Composite operation (T034 decomposes) |
| `execute_tempo_bundle` (results-slice mutation) | `execute_tempo_bundle_composite` — same fee-token gate, pathUSD settlement gate, funding, sign, save-race, broadcast, mark; outcomes returned as data. Composite (T034) |
| `native_top_up_cap` (market price → USD cap, static fallback + logs) | The fail-open policy is now IN THE CORE: `FetchMarketPrice` failure → static `top_up_max_wei`; success → `native_amount_for_usd_cap`. The debug/warn cap logs are dropped (observability-only) |
| `ensure_relayer_funded` precedence check (`relayer_balance >= required_prefund → Ready`) | Lifted into the core program (funding operation is only requested when the balance is short); the treasury lease + probe + sign + broadcast remain the composite `EnsureRelayerFunded` |
| Deferral diagnostics + Telegram policy (`record_executor_deferred` with embedded `should_notify_executor_deferred`) | The policy moved to the core: `RecordDeferred` (store write) and `NotifyIssue` (Telegram) are separate operations and the core decides when to emit each; `should_notify_executor_deferred` and its test now live in the core |
| Wire/store vocabulary: `RoutedUserOperation`, `QueuedUserOperation`, `StoredUserOperation`, `PreparedBundleIntent`, `StoredUserOperation::rpc_status` | Moved to `vela_relay_core::task` (serde shapes frozen) behind path-stable re-exports; `rpc_status` became a shell free function (the RPC response struct stays transport-side) |
| Delayed-inbox reprocessing (`process_delayed_lane`) | Unchanged — it feeds the same `handle_lane_batch`, which now drives the core program |

## Declared divergences

1. **Heartbeat abort semantics** (above): renewal failure no longer aborts the
   pipeline mid-await; the next checkpoint refuses instead. Store writes remain
   lease-fenced at the same points as before.
2. **Log placement/fields**: tracing stays in the shell, emitted from the
   executor arms. Messages and fields are preserved where the arm has the data
   (rejections, admissions, holds, nonce resolutions, submissions); a few logs
   lost fields the shell no longer computes (the hold info log no longer carries
   `paid`/`required` — they remain inside the recorded diagnostic reason — and
   the USD-cap debug/warn logs are gone). No stored or user-visible string
   changed.
3. **`resolve` engine failure**: a crux resolve error yields
   "could not resolve execution effect" per item (new, previously impossible);
   a program that never settles yields "lane batch never settled".

## Test accounting

180 → 185: engine's `admission_action`/`should_notify` tests moved to the core
(+0), five new Driver tests (+5). Shell 110 / core 75; engine.rs shrank to
~3,100 lines and no longer contains business decisions on the batch path.
