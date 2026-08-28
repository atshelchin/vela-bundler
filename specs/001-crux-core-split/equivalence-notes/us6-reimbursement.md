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
evaluation (the authoritative money path), changing only the RPC validation's
answer on two pathological inputs:

1. **Amount words above `u128::MAX`**: the old HTTP parser saturated per leg
   and could report `u128::MAX` "paid" (validation would accept); the executor
   parsed exactly and rejected on overflow. Now both reject (adapter reads the
   core's overflow as "nothing paid"), so an operation that would previously
   pass RPC validation only to be rejected by the executor is refused up front.
2. **Zero-amount stablecoin legs**: the old HTTP parser created a `{token: 0}`
   entry (which could trip mixed-asset/minimum checks); the executor ignored
   zero-amount legs. Now both ignore them.

Both inputs are economically meaningless (no real reimbursement can exceed
`u128` wei; a zero transfer pays nothing); in the old system they produced an
RPC-vs-executor disagreement, which is precisely the defect FR-009 exists to
eliminate.

## Test accounting

177 total unchanged: shell 126 / core 51. The three HTTP tests continue to pass
against the unified parser.
