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
| Logs (create-failed warn, read-failed warn, conflict error, already-exists info, retry-append info, queue-preserved warn, finalize error, accepted info) | Emitted from the executor arms / the outcome renderer with identical messages and fields (the accepted info log's `entry_point` field is no longer emitted — the renderer logs sender/hash/settlement as before minus that one field) |

## Declared divergences

1. **Accepted-log field**: the success log no longer carries `entry_point`
   (observability-only; the response and stored record are unchanged).
2. **Crux resolve failure**: an engine-level resolve error renders as
   store-unavailable (previously impossible).

## Test accounting

185 → 191: 9 shell tests moved to the core (7 admission helpers + 2 parser
tests, now against `string_reimbursement`), 6 new Driver walks. Shell 101 /
core 90; the core suite completes in ≈0.1 s.
