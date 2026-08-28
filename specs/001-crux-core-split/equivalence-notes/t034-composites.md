# Equivalence Note — T034: Composite decomposition (three batches)

Covers commits `9a249bd` (batch 1), `efc452b` (2a), `c735f6a` (2b), and the
audit-rule batch (2c). Behavior-preserving; divergences listed at the end.

## Batch 1 — broadcast, nonce triage, peg, fee quotes

| Old | New |
|---|---|
| `broadcast_bundle_intent` inside the batch pipeline (validate, seen-cache, send, observability/stale probes, cache bookkeeping, retained-outbox warns) | The core `broadcast_bundle` sequence over mechanical ops; judgement via `crate::broadcast::resolve_unproven_broadcast`; identical error/warn texts (the shell's composite survives only for resume paths) |
| `resolve_nonce_mismatch_items` | `FetchAccountNonces` probe + per-item core decisions; `DeferOperation`/`MarkRejected` carry `FutureNonce`/`StaleNonce` causes the shell logs verbatim |
| Gnosis peg inside `market_usd_price` | `settlement::pegged_native_usd_price` — the program never requests a market price for a pegged chain (settlement or top-up cap) |
| `2×base+tip` and `gasPrice−baseFee` inline math | `gas_math::{quoted_outer_fee, tip_from_legacy_gas_price}` with tests; identical error texts at the shell call sites |

## Batch 2a — the Tempo `0x76` tail

`execute_tempo_bundle` is an in-core continuation mirroring the old composite
branch-for-branch: fee-token gate (`RejectionCause::UnsupportedTempoFeeToken`),
allocation with `TEMPO_COST_BUFFER_GAS`, the pathUSD settlement gate over the
single parser + `marked_tempo_cost`, `tempo_handle_ops_gas_limit` and
`tempo_outer_max_fee` (moved to `tempo.rs`), the lifted funding-sufficiency
floor (`max(prefund, TEMPO_FLOAT_MIN)`), sign/save-race, the decomposed
broadcast, and the Tempo-specific mark log picked by the `0x76` raw prefix.
An 18-step Driver walk pins the tail (gas 110_203 / fee 30e9 / threshold
10_000 agreeing with the migrated rules).

## Batch 2b — treasury funding

| Old | New |
|---|---|
| `ensure_relayer_funded(+locked)` under `run_with_lease_heartbeat` | `ensure_native_funding` / `native_funding_locked` in the core; treasury lease as `Acquire`/`Ensure`/`Release` ops with a shell heartbeat task and a **batch-end backstop release** (a transiently erroring program can no longer leak the treasury lease — strictly safer than before) |
| shortfall/partial/submitted warn+info logs with amounts | `RecordTreasuryShortfall`/`RecordPartialTopUp`/`RecordFundingSubmitted` ops logged verbatim by the shell |
| `resume_funding_intent` (rebroadcast + receipt-probe claim + clear/revert) | `resume_funding` in the core over `AcquireReceiptProbe`/`FetchTransactionReceipt`/`ClearFundingIntent`/`NoteFundingReceipt`; `receipt_succeeded` was already a core rule |
| `broadcast_funding_intent` | `broadcast_funding` in the core — same shape as the bundle broadcast minus the stale-nonce path (the treasury nonce is serialized by its lease); ambiguous → debug + retain, rejected → known-probe else warn, identical texts |
| Tempo funding twins | `ensure_tempo_funding` with the raw gas estimate fetched by the shell and the buffer applied in-core |

A 20-step underfunded-relayer Driver walk pins the native sequence.

## Batch 2c — bundle replay audit

`audit_bundle_replay` (member classification into active / awaiting /
terminal / expired plus the four integrity refusals, byte-frozen) is a pure
core rule consumed by the shell's thin `audit_bundle_replay` method; a matrix
test pins every class and refusal.

## Deliberate residue

`ResumeBundleIntent` stays a composite operation and `reconcile_prepared_bundles`
stays a shell timer loop: every rule they apply (audit classification, receipt
success, broadcast judgement, `mark_bundle_submitted` lifecycle policy) already
lives in the core; what remains is IO orchestration.

## Declared divergences

1. Treasury-lease heartbeat: task-based like the lane lease (batch 1 note);
   plus the new backstop release at batch end (safety improvement).
2. Tempo settlement rejection warn now uses the generic
   "in-band settlement rejected UserOperation" text (previously
   "Tempo pathUSD in-band settlement rejected UserOperation") — the stored
   reason string and response are unchanged.
3. Funding/receipt log placement moved to operation arms; messages and fields
   preserved.
