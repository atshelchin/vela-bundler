# In-band fees: the settlement rule, end to end

Vela Relay charges no separate fee. Instead every UserOperation declares **zero
EntryPoint fees** (`maxFeePerGas = maxPriorityFeePerGas = 0`) and must embed, in
its own calldata, a trusted Safe MultiSend transfer that reimburses the relay's
settlement recipient for the gas the relay will spend. This is the *in-band*
reimbursement. This document is the authoritative statement of the rule the
relay enforces, and the guidance a client must follow to price a payment that
survives to inclusion.

All of the logic below lives in `vela-relay-core` (I/O-free, replayable) and is
byte-identical across the docker and Cloudflare deployments — `settlement.rs`
(evaluation, repricing, USD conversion), `cost.rs` (gas allocation), `gas_math.rs`
(quote tiers), `quote.rs` (the wallet-facing quote).

## 1. What the relay REQUIRES (the hard rule)

For each operation, evaluated independently (surplus on one op never subsidizes
another in the same bundle):

```
required = max( markup × gas_native_cost ,  floor )
```

- **`markup`** — default **14000 bps = 1.4×** (`VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS`,
  hard lower bound 1.0×). The relay recovers 1.4× the gas it spends.
- **`gas_native_cost`** = the operation's allocated gas × the **quoted per-gas
  fee**, and that fee is `max_fee_per_gas = 2 × base_fee + tip`
  (`gas_math::quoted_outer_fee`). The `2×` is **inclusion headroom, not cost** —
  the chain only ever charges `base_fee + tip`; the extra base-fee multiple lets
  the outer transaction survive a rising base fee without a re-sign.
- **`floor`** — a dust guard: `0.00001` native coin, or `0.01` of a stablecoin
  (≈ **1 cent**). This is NOT the price; it only bites when `1.4 × gas` rounds
  below it (near-zero-gas ops). Both the quote layer and the settlement layer
  compute it through the same `minimum_amount(decimals, fraction)` with the same
  constants, so they can never disagree.

Gas is split across a bundle by `cost::allocate_bundle_gas`: each op pays its own
simulated gas plus an even share of the outer overhead + buffer; the per-op
allocations sum to the bundle total exactly (no wei lost or double-charged), and
every op is guaranteed ≥ its own direct gas (no free-riding).

Every rounding step in the chain (`mul_div_ceil` markup, `native_to_usd_stable_ceil`
USD conversion, Binance price parse, Tempo cost) rounds **toward the relay**, and
every multiply/scale is `checked_` and fails closed on overflow. The relay can
never round in the payer's favor or wrap silently.

## 2. Repricing — the safety valve that makes a fixed client payment work

A client signs its payment at quote time; the base fee at *inclusion* time may be
higher. The relay does not simply reject a payment that falls short of the nominal
`required` — it first tries to **reprice the outer transaction down** to a fee the
payment CAN cover, because the `2×base` quote was headroom, not cost
(`settlement::decide_settlement`):

1. Evaluate at the quoted fee. If every op is fully paid → **KeepQuote** (submit
   as quoted).
2. If an op is short (but the payment parsed and went to the right recipient — a
   *shortfall*, not a malformed/misdirected payment), compute the **affordable
   fee** the weakest payer funds: `affordable = quoted_fee × (paid / required)`.
3. If `affordable` is at least the **inclusion floor**
   (`inclusion_floor_bps × base_fee + tip`, default **1.5×base + tip**) and below
   the quoted fee → **Reprice** to `affordable` and submit. Repricing preserves
   the full markup (reimbursement still covers `markup × gas × new_fee`, and the
   chain can never charge more than `new_fee`).
4. If `affordable` is below the inclusion floor → **FloorUnfundable**: reject.
   This is a clean rejection, never a loss — the relay never signs an outer
   transaction it would lose money on.

Because the floor uses `max(cost, dust_floor)` on the stablecoin path, a payment
below the *dust* floor can never be repriced into acceptance (the requirement is
pinned at the floor at every fee) — a case now pinned by
`a_stablecoin_below_the_floor_cannot_be_repriced_into_acceptance`.

## 3. What a CLIENT should pay (and why it must exceed the relay minimum)

A client that pays *exactly* the relay's instantaneous requirement is doomed: the
base fee at inclusion is almost always higher than at quote time, so the signed
payment falls short and the op is rejected. **A client must over-pay at quote
time to absorb the quote→inclusion gas drift.**

The vela-wallet client does this with a flat **3× markup on the network gas
basis** (`INBAND_MARKUP = 3`), against the relay's quoted `networkFeePerGas`
(where the relay's own quote applies `base_fee_multiplier = 120` → ~`1.2×base`),
and pays the **same floors** (`0.00001` native / `$0.01` stable). It ignores the
relay's `requiredAmount` field entirely and self-computes 3×. The signed amount
is what the confirm screen displayed — it is **not** re-priced just before submit
(a 30 s quote TTL is advisory, not enforced), so the whole buffer must live in
that 3×.

### Headroom, worked out

Two bases differ: the client prices against ~`1.2×base` (quote time), the relay
settles against `2×base'` (inclusion time). Netting the client's 3× against the
relay's 1.4×, with the default 1.5×base inclusion floor and tip ≈ 0:

| Base-fee rise, quote → inclusion | Outcome |
|---|---|
| up to **~1.29×** (+29%) | fully paid at the quoted fee → **KeepQuote**, submitted |
| **~1.29× to ~1.71×** (+29% … +71%) | short of nominal, but **repriced** down to a fundable fee → submitted |
| above **~1.71×** (+71%) | `affordable` drops below the inclusion floor → **FloorUnfundable**, cleanly rejected (retry with a fresh quote) |

So the effective tolerance is roughly a **+70% base-fee spike** between signing
and inclusion — comfortable for normal conditions, and any larger spike fails
safe (a rejected send, never an under-charge or a loss to the relay).

The exact numbers move with the tip, the client's gas padding (it pads limits
×1.5), and which quote tier the client prices against; the shape (direct-accept
band → reprice band → clean-reject) is fixed by the rule.

### Integration caveats worth knowing

- **The two fee bases are not identical.** The client prices against the relay's
  quoted network fee (~1.2×base); the relay settles against `2×base'`. The 3×
  client markup and the repricing valve cover the gap, but a client that lowered
  its markup toward the relay's 1.4× would lose almost all drift tolerance.
- **The floors are exactly equal**, so at the dust floor there is zero headroom:
  a near-zero-gas op whose gas rises enough to lift `1.4×gas` above the floor,
  while the client is still pinned at the floor, is a shortfall. Rare, and it
  fails safe.
- **No client-side re-price before submit** (confirm-UI flow): the drift budget
  is entirely the 3× buffer. A user sitting on the confirm screen past the 30 s
  TTL spends that budget on think-time.

## 4. Stablecoin payments

A client may reimburse in an allowlisted stablecoin instead of the native coin.
The relay converts its native `required` into stablecoin units via the asset's
USD price (`native_to_usd_stable_ceil`, 8-dp fixed-point price, ceil rounding),
floors it at `$0.01`, and — critically — **verifies the on-chain Transfer event**
from the final bundle simulation actually paid the settlement recipient: the log
must come from the allowlisted token, carry the `Transfer(address,address,uint256)`
signature, and show `sender → recipient` for the claimed amount. A transfer to a
third party, from the wrong token, or with the wrong shape credits nothing
(pinned by `verify_stable_transfer_logs_enforces_every_field_of_the_transfer_event`).

Native transfers have no standard log and are instead covered by the successful
final bundle simulation.

## 5. Tempo (pathUSD gas)

Tempo chains have no native gas coin; the relay prices gas directly in pathUSD
(attodollar-denominated), applies the same 1.4× in-band markup with the same
`$0.01` floor (`marked_tempo_cost`), and signs the outer transaction with Tempo's
`0x76` envelope paying fees in pathUSD. The client mirrors this with a separate
Tempo model (2× margin plus an explicit gas/split cushion annotated "must match
vela-relay", added after a real sub-floor deploy rejection).

## 6. Summary

- The relay requires `max(1.4 × gas × (2×base+tip), floor)`, recovers 1.4× its
  gas, and rounds every step in its own favor with fail-closed overflow.
- Repricing turns the `2×base` headroom into a live safety valve: a short-but-
  honest payment is repriced down to a fundable fee rather than rejected, down to
  the 1.5×base inclusion floor.
- A client must pay above the relay minimum to survive gas drift; vela-wallet
  pays a flat 3× the network basis, giving roughly a +70% base-fee-spike
  tolerance before a clean, loss-free rejection.
- Stablecoin reimbursements are verified against the real on-chain Transfer
  event; a misdirected or wrong-token transfer is never credited.
