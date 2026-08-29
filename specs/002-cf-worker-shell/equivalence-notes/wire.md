# Equivalence Note — T004–T006: the core `wire` module

For the PR description (spec 002 FR-012; docker-side change is FR-003
additive-only + behavior-preserving).

## Old → New

| Old (docker shell) | New location |
|---|---|
| `src/app/rpc/types.rs` — the entire envelope vocabulary: `RpcRequest`/`RpcResponse`/`RpcError` (+ every error constructor and its frozen code/message/data), `RpcMethod` parse/as_str, all eight params types, all response-shape structs (`UserOperationGasEstimate`, `UserOperationByHash`, `UserOperationReceipt`, `Log`, `TransactionReceipt`, `UserOperationGasPrice`, `InBandGasQuote*`, `GasPriceTier`, `UserOperationStatus`), `Estimatable*`, `StateOverride*` | Moved verbatim to `vela_relay_core::wire`; `types.rs` is a one-line re-export shim, so every handler and test keeps its historical paths. Zero call-site changes outside `mod.rs` |
| `src/app/rpc/mod.rs` inline envelope handling: body parse → parse-error response (null id), `jsonrpc != "2.0"` refusal (echoing the id), `validate_call` + `parse_params` + `validate_empty_params` | `wire::parse_envelope` and `wire::validate_call` — byte-identical flows; `mod.rs` now calls them and renders the returned responses unchanged |
| (new guarantee) | Six byte-pinning tests in `wire::tests` whose golden vectors are production bytes captured by the 001 replay battery (result envelope, method-not-found, parse error, version refusal, invalid-params, -32500 rejection) |

## Adjustments from the tasks plan

- The admission `render` fn stays in the shell (T005 note): it emits tracing
  (a shell concern); the bytes it produces are already `wire` types, so parity
  is preserved without moving the logging.

## Verification

- Full suite green: shell 101 + core 105 (99 + 6 new wire pins); fmt clean;
  clippy = baseline (one `result_large_err` expectation added with rationale,
  matching the repository's `#[expect]` convention).
- Replay battery re-run (2026-08-29) against the delegated build vs the
  pre-change baseline: byte-identical on every surface except `/version`'s
  build-sha field (environmental — the baseline binary embedded a stale
  build.rs sha; declared delta class in `contracts/deployment-parity.md`).
