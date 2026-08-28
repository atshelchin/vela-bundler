# Equivalence Note — US1: One authoritative lifecycle state machine

For the PR description (spec FR-011). Maps every behavior of the old Lua policy
to its new location; anything not listed is byte-identical by inspection.

## Old → New

| Old behavior (main) | New location |
|---|---|
| `PATCH_RECORD_SCRIPT` `allowed` table (`user_operation_store.rs:45-49`) | `vela-relay-core/src/lifecycle.rs::transition_is_allowed` — identical matrix, pinned by `status_transition_matrix_is_monotonic` (moved from the shell's test module, plus a `NotFound` refusal case) |
| Same-status / status-less patches always merge | `decide_patch` returns `Apply` for `requested == None` or `requested == current` — pinned by `same_status_patches_are_always_field_merges` |
| Illegal transition → script returns 0 → `patch()` → `Ok(false)` | `decide_patch` → `RefuseIllegalTransition` → `patch()` → `Ok(false)` (no Redis write issued at all) |
| Missing record → 0 → `Ok(false)` | Shell GET sees `None` → `Ok(false)` |
| Merge + `SET … KEEPTTL` atomicity | Reduced script: guarded merge (`record['status'] ~= ARGV[2]` → `-1`), merge + `SET … KEEPTTL` unchanged; on `-1` the shell re-reads and re-decides (≤ 4 rounds). Statuses move monotonically, so contention converges; exhaustion returns a store error (previously unreachable: single atomic script) |
| `MARK_BUNDLE_SUBMITTED_SCRIPT` eligibility: same chain AND `queued`/`not_submitted` → `submitted` + tx hash + `admitted=true` | `lifecycle::decide_bundle_submission` → `Transition`; applied by the reduced script guarded on the observed status |
| Idempotent re-index: `submitted` + same tx hash | `decide_bundle_submission` → `IndexOnly`; script re-checks `record['transactionHash'] == ARGV[1]` before `SADD` |
| Chain compare: `chainIdText` decimal string; legacy fallback `tostring(record['chainId'])`, deliberately fail-closed for values Lua cannot render canonically | `decide_bundle_submission`: text compare when `chainIdText` present; legacy numeric compare gated by `< 10^14` (Lua `%.14g` exact-render bound). Non-integer legacy `chainId` values parse to no `u64` → 0 → mismatch → skip, matching Lua's fail-closed render mismatch |
| Return value: count of indexed members | Count of applied `Transition` + `IndexOnly` members accumulated across guard-retry rounds |
| `is_durable_status` (`engine.rs:3400`): `submitted|rejected|included|failed` | `UserOperationStatus::is_durable()` (engine delegates). Distinct from `is_terminal()` (`rejected|included|failed`), both pinned by `terminal_and_durable_predicates_stay_distinct` |
| `#[cfg(test)] transition_is_allowed` mirror + `patch_lua_has_the_same_terminal_guards` script-text test | Deleted. The mirror IS now the production rule; the script-text test is replaced by `patch_lua_is_a_mechanical_guarded_merge`, which asserts the script contains **no** policy |

## Declared corner-case divergences (all practically unreachable)

1. **Stored record with missing/unparsable `status`** (no writer produces one):
   status-changing patch → `Ok(false)` (unchanged); field-only patch → store
   error instead of blind merge (old Lua merged). Chosen deliberately: merging
   into a record we cannot judge is unsafe.
2. **Guard-retry exhaustion** (4 rounds of concurrent status flips in the
   sub-millisecond window): store error instead of the old atomic outcome.
3. **Read-then-apply window in `mark_bundle_submitted`**: between MGET and the
   guarded apply, a record that changed status is re-decided (equivalent); a
   `submitted` record whose tx hash changed while status stayed `submitted` is
   skipped by the hash guard — no current writer can produce that interleaving.
4. **Legacy chain ids ≥ 10^14 with exact `%.14g` renders**: Lua would have
   matched a value like `1e14` whose render round-trips; Rust fails closed for
   the whole ≥ 10^14 legacy range. No such chain id exists; text-carrying
   records (all records written since `chainIdText` was introduced) are exact
   at any magnitude.

## Perf note

`patch()` gains one GET round trip; `mark_bundle_submitted` gains one MGET.
Both paths are dominated by chain RPC latency and run on the worker, not the
request path.
