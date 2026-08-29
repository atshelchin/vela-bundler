# Equivalence Note — transport mapping (FR-012, final)

How each docker transport guarantee is reproduced (or structurally replaced)
on the Cloudflare shell. Companion to
[contracts/platform-bindings.md](../contracts/platform-bindings.md) (the
per-Operation table with declared deltas and measured tolerances); this note
explains the WHY of each mapping. Every business decision consumed along
these paths lives in `vela-relay-core` and is byte-shared — nothing below is
a rule, only a carrier for rules.

## Queue: ack granularity vs the offset rule

Iggy delivers ordered, offset-committed batches: the docker consumer commits
an offset only when every item at or before it is durably resolved, so a
crash replays the whole tail. Cloudflare Queues are unordered at-least-once
with PER-MESSAGE ack/retry. The mapping:

- `ItemResolution::Durable` → `message.ack()` — exactly the states in which
  the docker shell would have allowed the offset past this item (terminal
  record state, parked in the durable delayed inbox, or submitted);
- `ItemResolution::Failed` → `message.retry()` — the docker "leave on the
  stream for redelivery" tail, with the queue's backoff instead of Iggy's
  poll cadence, bounded by `max_retries` → DLQ (observed at 101 attempts in
  Gate 3);
- a malformed envelope (fails `schemaVersion`/shape checks before the core
  sees it) → DLQ send + ack, the docker dead-letter path.

Ordering loss is acceptable by design: nonce triage and record idempotency
already tolerate reorder and redelivery (core rules; Gate 3 proved a
six-op same-sender burst serializes to exactly one outer transaction). The
envelope travels as its exact JSON text — a v8 structured clone turns serde
maps into JS `Map`s (unreadable) and degrades u128 to floats.

## Lane lease: structural answers

The docker lane lease (Redis `SET NX PX` + heartbeat task + interrupt
channel) exists to serialize one lane across MANY worker processes. A
Durable Object is single-threaded by contract, so the LaneDO's input gate IS
the mutual exclusion: `AcquireLaneLease`/`EnsureLaneLease` answer constant
`true`, and `ExecutionOutcome::Interrupted` is never produced on this shell
(the vocabulary stays shared; the docker shell keeps using it). This is a
strictly stronger guarantee, declared in the bindings contract rather than
emulated with weaker machinery.

## Treasury lock: the one real lock

The treasury is contended by every lane of a chain, so unlike the lane
lease it stays an explicit lock — in the chain's TreasuryDO with the docker
store's exact semantics (acquire = `SET NX PX`, renew/release token-guarded,
expiry judged on read). The docker background heartbeat (renew every ttl/3)
is replaced by renewal-on-touch: every TreasuryDO request from the current
holder piggybacks an extension. Funding outbox put-if-absent and
hash-guarded clear reproduce the Redis Lua guards; the receipt-probe
throttle is an expiring slot exactly like the docker per-interval receipt
lease. The whole TreasuryDO boundary speaks JSON text (Gate 3 found u128
`amount_wei` degrading to a float through JsValue paths).

## Time-driven behaviors: alarms for loops

The docker reconciler is an interval task (`receipt_poll_interval`, 3 s)
whose body does two jobs; on this shell the same body hangs off each
LaneDO's single packed alarm (earliest-of):

- **prepared-intent reconcile**: resume → `eth_getTransactionReceipt` →
  core receipt rules → per-member `receipt_patch` (docker's byte-identical
  field set) → clear intent. Armed on `SavePreparedBundle` (covering the
  save-to-broadcast crash window) and on `MarkBundleSubmitted`. Receipts are
  bundle-scoped exactly as docker's reconciler (never per record);
- **delayed re-drive**: due entries re-enter through the same batch entry;
  docker's claim fencing is reproduced by snapshot invalidation (a re-park
  during the batch rewrites the entry); transient failures climb the same
  core hold ladder in place; retention `max(attempt_ttl, 14 d)` sliding.

Measured under Gate 4: ladder re-drives at +5/+10/+21 s against the 5/10/20 s
schedule (≤1 s deviation, inside the declared max(30 s, 10%)); submitted →
`included` 2–3 s after mining. RecordDO alarms remain TTL-only, never early.

## Alerts

Suppression rules (fingerprint normalization, bounded reason, message text)
moved to core `alert`; both shells consume them. The suppression slot is
Redis `SET NX PX` on docker and the chain's TreasuryDO on this shell — same
claim/release-on-delivery-failure protocol, strongly consistent per chain.

## Simulation tiers

Same three-tier order over the shell's own transport. One declared absence:
this shell does not auto-deploy the Pimlico simulation pair (deployment
needs treasury signing + a receipt wait — the docker treasury's job); the
CREATE2 addresses are a pure function of the shared treasury (core
`simulation::pimlico_contracts_for_treasury`), so docker-deployed pairs are
found `Ready` here, and a `Missing` pair falls through to `debug_traceCall`.
`SimulationVerdict::Pending` and the deployment-wait diagnostics are never
produced on this shell.
