# Contract: External API (FROZEN)

The refactor declares **zero** changes to any externally observable surface
(spec FR-003). This file enumerates the frozen surfaces; it is a checklist for
equivalence review, not a redesign.

## HTTP endpoints (unchanged)

- `GET /`, `/health`, `/api/health`, `/healthz`, `/readyz`, `/version`
- `GET /v1/account/{chain}/{safe}` — balances + EntryPoint nonces
- `GET /v1/treasury`, `/v1/treasury/{chain}`

`/readyz` semantics unchanged: all four job names (including the three placeholder
jobs) still gate readiness — their removal is explicitly out of scope.

## JSON-RPC 2.0 over `POST /{chain_id}` (unchanged)

All eight methods keep identical params validation, result shapes, error codes,
and reason strings:

1. `eth_sendUserOperation`
2. `eth_estimateUserOperationGas`
3. `eth_supportedEntryPoints`
4. `pimlico_getUserOperationStatus`
5. `eth_getUserOperationByHash`
6. `eth_getUserOperationReceipt`
7. `pimlico_getUserOperationGasPrice`
8. `vela_getInBandGasQuote`

Response header `x-vela-rpc-domain` unchanged. Status vocabulary unchanged:
`not_found | queued | not_submitted | submitted | rejected | included | failed`.

## Durable data shapes (unchanged)

- Redis record JSON (camelCase field names), key naming, TTL classes.
- Iggy topology (`chain-{id}` / default streams), envelope payloads, consumer
  group semantics, offset-advance rule.
- Prepared-intent shapes (bundle / funding / deployment), dead-letter records,
  executor diagnostics.

## On-chain behavior (unchanged)

- `handleOps` calldata construction, userOpHash computation, EIP-1559 and Tempo
  0x76 transaction encoding/signing, nonce management, relayer lane routing
  (HKDF salt `vela-bundler-dedicated-eoa-v1`, pool width 10), funding targets and
  caps, receipt-confirmation depth.

## Operational surfaces (unchanged)

- All `VELA_RELAY_*` / secret env variables and their validation rules.
- Telegram alert texts and dedup fingerprints.
- Log event fields relied on by operators (best-effort: no deliberate renames).
- `deploy_simulations` binary behavior.

## Equivalence obligations

Every migration PR touching admission, settlement, funding, or broadcast carries an
equivalence note (spec FR-011) mapping old path → new core decision and accounting
for every reason string. SC-006 (replay comparison) is the operator-level
verification of this contract.
