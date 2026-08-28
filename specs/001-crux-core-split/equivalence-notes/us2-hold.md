# Equivalence Note — US2: Unified fee-hold policy

For the PR description (spec FR-011). Also covers the Phase-2 tail (T005/T006
pure relocations), which is move-only.

## Old → New

| Old behavior (main) | New location |
|---|---|
| Backoff doubling loop in `SAVE_DELAYED_OPERATION_SCRIPT` (`user_operation_store.rs:211-217`) and `RETRY_DELAYED_OPERATION_SCRIPT` (`:287-293`), constants `DELAYED_RETRY_BASE_MS`/`DELAYED_RETRY_MAX_MS` (`:33-34`) | `vela-relay-core/src/hold.rs::retry_delay_ms` (identical loop semantics) + `retry_delay_schedule_ms()` (the table `[5s,10s,20s,40s,80s,160s,300s]`). The shell appends the table to both script calls (`append_retry_schedule`); the scripts do `delay = ARGV[5 + min(attempts, slots)]` — a lookup, no arithmetic. Pinned by `ladder_doubles_from_five_seconds_to_a_five_minute_cap` and `schedule_table_matches_the_ladder_and_ends_at_the_cap` (attempts 1→20) |
| Due time `= Redis TIME + delay` computed in-script | Unchanged mechanically. The clock anchor deliberately stays Redis `TIME`: the claim reader (`CLAIM_DELAYED_OPERATIONS_SCRIPT`) also reads `TIME`, so writer and reader share one clock exactly as before. Only the delay *value* moved |
| Hold budget guard `attempt > settlement_hold_max_attempts` (`engine.rs:1846`) with the over-budget path returning `false` → rejection | `vela_relay_core::hold::decide_hold(attempt, max, paid, required) → Hold{reason} | RejectBudgetExhausted`; engine matches on it in `hold_for_affordable_market`. Same post-deferral ordering: the deferral is recorded first, then judged — an over-budget hold still leaves one scheduled entry behind (comment preserved) |
| `settlement_hold_reason` (`engine.rs:3492`) | `vela-relay-core/src/settlement.rs::settlement_hold_reason` — byte-identical format string, pinned by `hold_reason_reports_progress_against_the_budget` and the `decide_hold` boundary test (attempt 12/12 holds, 13/12 rejects) |
| `settlement_rejection_reason` (`engine.rs:3499`) + its byte-pin test | `vela-relay-core/src/settlement.rs::settlement_rejection_reason`; the test `explains_an_insufficient_in_band_reimbursement` moved along, plus a new `distinguishes_rejection_causes` covering the two non-shortfall arms |
| Store-error during deferral → warn + `Ok(false)` (reject) | Unchanged (shell-side, ahead of the decision) |
| `RETRY_DELAYED_OPERATION_SCRIPT` canonical-payload guard at `ARGV[6]` | Same guard at `ARGV[4]` (argument positions shifted when base/max were replaced by the table; script-text test updated). Token/identifier/TTL guards untouched |

## Phase-2 tail relocations (move-only, no behavior)

- `src/utils/vault.rs` → `vela-relay-core/src/vault.rs` (salt, golden vectors,
  routing all byte-identical; 7 tests moved; the two secret-key derivations are
  now `pub` to the workspace-internal shell — neither crate is published).
  `src/bin/deploy_simulations.rs` uses the crate instead of a `#[path]` include.
- `src/utils/tempo.rs`, `src/utils/alchemy.rs` → core (2 tests moved); shell
  keeps path-stable re-export shims.
- Gas-price arithmetic (`GasPrice`/`GasPriceError`/`GasPricePolicy`/`FeeHistory`,
  `price_from_fee_history`, `tiers`, `scale`, `median_priority_fee`,
  `parse_quantity`, `legacy_price_from_result`, fallback tip rule) →
  `vela-relay-core/src/gas_math.rs` (5 tests moved; `GasPriceManager` keeps
  polling/caching/failover and calls the core math). All shell import paths
  preserved via re-exports.

## Test accounting

166 → 171: 6 tests moved shell→core net-zero; 5 genuinely new (2 hold ladder,
1 budget boundary, 2 settlement-reason). Shell suite modifications are limited
to the moved tests and the script-text asserts that now pin "no policy in Lua".

## Declared corner-case divergences

None beyond US1's. The over-budget deferral ordering, store-error fallback, and
all reason strings are unchanged; clippy warnings dropped 9 → 7 because two
pre-existing dead-code warnings disappeared with the vault relocation.
