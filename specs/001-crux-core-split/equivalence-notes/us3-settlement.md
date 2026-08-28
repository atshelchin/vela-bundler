# Equivalence Note — US3: Settlement decision as a pure verdict

For the PR description (spec FR-011).

## Old → New

| Old behavior (main) | New location |
|---|---|
| `worker/executor/settlement.rs` (evaluation, repricing math, log verification; 17 tests) | Moved wholesale into `vela-relay-core/src/settlement.rs` with `pub(crate)` → `pub` (workspace-internal); the worker module is a re-export shim, every `super::settlement::…` path unchanged. Zero logic edits |
| `worker/executor/cost.rs` (`allocate_bundle_gas`, `native_cost`; 2 tests) | Moved to `vela-relay-core/src/cost.rs` (pulled forward from US4's T025 because the settlement verdict recomputes per-op costs at the repriced fee); worker shim keeps `allocate_bundle_gas` for the engine, `native_cost` is now consumed only inside the core |
| `settle_at_affordable_fee` decision flow (`engine.rs:1900-1984`): evaluate at quote → early-outs (all accepted / uncurable rejection / no affordable fee / no floor) → floor gate → re-evaluate at affordable → keep-original-if-not-all-accepted → mutate `context.max_fee_per_gas` | `vela_relay_core::settlement::decide_settlement(recipient, chain_assets, call_datas, allocations, native_usd_price, FeeContext) -> KeepQuote | FloorUnfundable{affordable, floor} | Reprice{fee_per_gas, evaluation}`. Branch-for-branch identical, pinned by the six verdict tests (accept, reprice incl. exact repriced fee, floor-unfundable with exact affordable/floor values, uncurable-rejection batch, cost overflow, stablecoin detection). The in-place fee mutation became the `Reprice` arm applied by the shell |
| `evaluate_settlement` (`engine.rs:1986-2015`): builds inputs, conditionally fetches the Binance price, calls `evaluate_batch` | Deleted. The shell pre-fetches the price ONCE before the decision (`has_stablecoin_payment` on the signed calldata, then `market_usd_price`), and the core evaluates both fee points with that one price. Equivalent: the predicate reads only calldata (identical for both evaluations), and the old second call hit the same TTL cache within microseconds |
| `has_stablecoin_payment` (`engine.rs:3152`) | Moved to core with a calldata-slice signature (it never read the cost field); pinned by `stablecoin_payment_detection_reads_the_signed_calldata_only` |
| Error strings: `"bundle native cost overflow"` / `SettlementError` Display via `to_string()` | `SettlementDecisionError` Display produces the identical strings (`CostOverflow` arm byte-frozen, `Evaluation` arm forwards the inner Display); shell still wraps in `ExecutorItemError(error.to_string())` |
| Log lines: "in-band reimbursement cannot fund an includable outer fee" (fields `chain_id, quoted_fee, affordable, floor, base_fee`) and "repriced the outer transaction to the signed in-band budget" (fields `chain_id, quoted_fee, repriced_fee, base_fee, tip`) | Emitted with identical messages and field names. *History*: at the US3 landing the shell emitted them while applying the verdict; the US4 driver swap (T033) silently dropped both — caught by the 2026-08-28 audit — and they now flow through the core's `EmitDiagnostic` operation at the `FloorUnfundable` / `Reprice` decision arms |
| Market-price path (`market_usd_price`): Gnosis pegged shortcut, symbol validation, TTL cache, Binance fetch | Unchanged, stays in the shell (IO); now invoked at most once per settle instead of up to twice (both invocations previously returned the same cached value) |

## Declared corner-case divergences

1. **Price-cache expiry between the two evaluations**: previously the repriced
   evaluation could in principle re-fetch a *newer* price if the TTL expired in
   the microseconds between calls; now both fee points use one price. The old
   behavior was an unintended race, not a rule; the new behavior is the
   deterministic reading of the same decision.
2. None otherwise: branch order, early-outs, floor comparison
   (`affordable < floor || affordable >= quoted`), keep-original-evaluation on
   failed reprice, and all strings/log fields are preserved.

## Test accounting

171 → 177: 19 tests moved shell→core net-zero (17 settlement + 2 cost);
6 genuinely new verdict-table tests. Shell 126 / core 51.
