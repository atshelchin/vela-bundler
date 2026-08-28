# Equivalence Note — US5: Admission protocol through the core

For the PR description (spec FR-011).

## Old → New

| Old behavior (main) | New location |
|---|---|
| `accept` (`send_user_operation.rs:32-176`): EntryPoint gate → structural validation → in-band validation → store/queue availability → SETNX create → duplicate triage → Iggy append → admitted mark | `vela_relay_core::admission::drive_admission`, step-for-step, over `LoadSettlementAssets`/`FetchTokenDecimals`/`CreateQueued`/`LoadExisting`/`Enqueue`/`MarkAdmitted` operations. Six Driver walks pin: the create→enqueue→admit order, duplicate-admitted idempotency, conflict refusal, the queue-outage crash window (record retained, no further operations — the vocabulary has no delete), underpayment rejection, and zero-fee refusal before any operation |
| `supported_entry_points::is_supported` + the static list | `admission::SUPPORTED_ENTRY_POINTS`/`entry_point_is_supported` (core); the RPC method handler re-exports the list |
| `PreparedUserOperation` (structural validation + EntryPoint v0.7 userOpHash) with `RpcError::invalid_params(message)` refusals | Moved verbatim to the core with `String` messages; the shell maps `Invalid{message}` → `invalid_params(message)`. The golden-vector hash test moved along |
| `existing_admission_action` + `admission_fingerprint` (+ their 3 tests) | Moved verbatim (tests included) |
| `validate_in_band_submission`: zero-fee rule, Tempo pathUSD gate + 0.01 minimum, directory assets + native/stable minimums with per-token decimals | In the core program; the directory/RPC reads are operations; all refusal strings byte-identical. The Tempo canonicalization comment and test preserved |
| HTTP-side reimbursement parsing for validation (`in_band_settlement::parse_reimbursement` adapter) | Deleted — the core's `admission::string_reimbursement` (same saturating/lowercase semantics over the single parser) is used inside the program; the adapter's two parser tests moved to it. `in_band_settlement.rs` now keeps only minimum-amount adapters and hex/address string utilities for the quote/estimate handlers |
| Resource availability ordering: store then queue checked AFTER validation, BEFORE the durable write | Preserved: the shell's `CreateQueued` arm reports a missing store as `StoreFailed` and a missing queue as `QueueUnavailable` before creating the record; the core settles those before any write |
| Logs (create-failed warn, read-failed warn, conflict error, already-exists info, retry-append info, queue-preserved warn, finalize error, accepted info) | Emitted from the executor arms / the outcome renderer with identical messages and fields. The accepted info log's `entry_point` field — dropped in the first landing — was restored by the 2026-08-28 audit: `AdmissionOutcome::Accepted` now echoes the request's `entryPoint` verbatim and the renderer logs the full historical field set (`chain_id`, `entry_point`, `sender`, `user_operation_hash`, `settlement`) |

## Declared divergences (post-audit revision, 2026-08-28)

1. ~~Accepted-log field~~ — restored (see the logs row above); no admission
   log divergence remains.
2. **Crux resolve failure**: an engine-level resolve error renders as
   store-unavailable (previously impossible).
3. **Test note**: `rejects_native_prefund_fee_fields` moved to the core with
   its error-code assertions reduced to the message string (core errors are
   plain `String`s); the untouched shell E2E test
   `rejects_native_prefund_user_operations_before_any_upstream_call` still
   pins `-32602` and the exact message through the router, so the wire
   contract stays pinned end-to-end.

## Test accounting

185 → 191: 9 shell tests moved to the core (7 admission helpers + 2 parser
tests, now against `string_reimbursement`), 6 new Driver walks. Shell 101 /
core 90; the core suite completes in ≈0.1 s.
