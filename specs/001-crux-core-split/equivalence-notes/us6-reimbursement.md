# Equivalence Note — US6: One reimbursement interpretation

For the PR description (spec FR-011).

## Old → New

| Old behavior (main) | New location |
|---|---|
| HTTP-side parser `app/rpc/handlers/in_band_settlement.rs` (String/`u128`, own MultiSend decoder, own selectors + `TRUSTED_MULTISEND`) | The module is now a string-facing **adapter** over `vela_relay_core::settlement::parse_reimbursement`: it converts hex strings → bytes/`Address`, calls the single core parser, and converts `U256` amounts back to `u128` (saturating) and token addresses back to lowercase hex keys. The production `TRUSTED_MULTISEND`, selectors, and MultiSend decoding exist only in the core (the string constant remaining in the shell is a `#[cfg(test)]` fixture for encoding test calldata) |
| `minimum_native_amount` / `minimum_stablecoin_amount` (local `pow10` rules) | Thin adapters over the core's `minimum_amount(decimals, MIN_*_FRACTION_DECIMALS)` (made `pub`); `None` conditions match: decimals below the floor, or a result that exceeds `u128` |
| `is_tempo_chain` (third copy of the chain-id list) | Re-export of `vela_relay_core::tempo::is_tempo_chain` |
| `parse_address` / `decode_hex` string helpers | Kept in the shell unchanged — transport-layer string handling, not business duplication |
| HTTP parser tests (3) | Retained as adapter tests, now exercising the adapter → core path |

## Resolved divergences (the two old parsers already disagreed; the executor's semantics win)

The spec's acceptance ("identical outcome to either pre-refactor parser") is
satisfiable only where the two old parsers agreed — which is every well-formed
input. Where they disagreed, unification resolves toward the executor's
evaluation (the authoritative money path). *This section was corrected by the
2026-08-28 audit: the first draft named the wrong input classes.* The RPC
validation's answer changes on exactly two pathological classes:

1. **Reimbursement legs whose exact sum overflows `U256`** (e.g. two native
   value words of `2^255` to the recipient): the core's `checked_add` reports
   `ArithmeticOverflow`, which the string adapter reads as "nothing paid", so
   the RPC now refuses with the minimum-reimbursement rejection. The old HTTP
   parser saturated each leg to `u128::MAX` and accepted; the old executor
   later stored an arithmetic-overflow rejection — an accept-then-reject the
   unified parser refuses up front. (A *single* word above `u128::MAX` never
   disagreed and still validates: the adapter saturates the exact `U256` back
   to `u128::MAX`, and one 32-byte word cannot overflow `U256` in the
   executor.)
2. **Zero-amount stablecoin legs**: the old HTTP parser created a `{token: 0}`
   entry; the executor ignored zero legs (and main's RPC validation had no
   mixed-asset check for it to trip). The observable class: a zero-amount leg
   of allowlisted token B alongside sufficient payment in token A, where B's
   `decimals()` lookup fails or returns < 2 and B sorts before A — the old
   validation refused with `estimation_unavailable`; the unified path never
   surfaces the zero leg and accepts. With healthy lookups the only change is
   one fewer `erc20_decimals` eth_call.

Both classes are economically meaningless (no real reimbursement sums past
`U256`; a zero transfer pays nothing); in the old system they produced an
RPC-vs-executor disagreement, which is precisely the defect FR-009 exists to
eliminate.

## Post-US5 state

US5's admission program absorbed the string-facing adapter: it now lives as
`vela_relay_core::admission::string_reimbursement` (with the two former HTTP
parser tests exercising it there), and the quoting path's
`in_band_settlement.rs` module keeps only transport-side helpers over the same
single core parser.

## Test accounting

177 total unchanged: shell 126 / core 51. The three HTTP tests continue to pass
against the unified parser.
