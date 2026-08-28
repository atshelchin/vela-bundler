# Data Model: Cloudflare Worker Shell

State classes, their platform mapping, and the object catalog. The business
vocabulary itself (records, intents, envelopes, statuses) is unchanged — it is
`vela_relay_core::task` — this document maps where each class LIVES on the new
platform and which mechanism supplies its guard semantics.

## 1. Durable Object catalog

Three DO classes — SQLite-backed (`new_sqlite_classes`, the current platform
default) — with deterministic ids (`idFromName`): no registries, no discovery.
Inter-object protocol is fetch-based with serde JSON payloads (workers-rs has
no typed DO RPC); each DO packs its schedules into the platform's single alarm
as earliest-of, and every alarm action re-derives from stored state via core
rules, so at-least-once alarm firing is idempotent by construction.

### RecordDO — one per user operation

- **Id**: `{chain_id}:{user_operation_hash}` (lowercase hash — the store's key
  discipline today).
- **Owns**: the `StoredUserOperation` JSON (camelCase shape FROZEN per
  `001/contracts/external-api.md`), its TTL deadline, and the receipt-check
  schedule for `submitted` records.
- **Guard semantics**: the DO is single-threaded — `create_queued`
  (put-if-absent = today's SETNX), `patch` (read → `lifecycle::decide_patch` →
  write), `mark_admitted`, `mark_bundle_member_submitted` (read →
  `lifecycle::decide_bundle_submission` → write) run the same core decisions the
  Redis Lua guards apply today, with strict serializability for free.
- **Alarm**: earliest of (a) TTL expiry → delete storage (the 3600 s record TTL
  class), (b) next receipt check for `submitted` records → fetch receipt via
  chain RPC, apply the core receipt rules, reschedule or finalize.
- **Read path**: status/byHash/receipt handlers fetch this DO directly (strong
  read-your-write; FR-006).

### LaneDO — one per (chain, lane): the execution unit

- **Id**: `{chain_id}:{lane}` where lane = `vault::relayer_index_for_sender`
  (pure core fn, pool width 10 — unchanged).
- **Owns**: the prepared bundle intent (create-only), the broadcast-seen cache
  (30 s), the delayed inbox (payload + attempt counter + due time per parked
  operation), the bundle→members index, and the reconcile schedule.
- **Runs**: the `ExecutionApp` driver loop — one `Core::new()` per delivered
  batch (per-unit-of-work, Constitution). Executor arms live here: chain RPC
  failover fetches, signing (secrets from env bindings), record reads/writes
  (subrequests to RecordDOs), treasury calls (subrequests to TreasuryDO),
  Telegram, queue-independent bookkeeping.
- **Serialization**: the DO's single-threaded input gate IS the lane lease.
  `AcquireLaneLease`/`EnsureLaneLease` are answered `true` structurally;
  the lease-interrupt path is unreachable on this shell (declared in the
  platform-bindings contract — the guarantee is strictly stronger, not weaker).
- **Alarm**: earliest of (a) delayed-inbox min(due) → re-drive due items through
  the same batch entry (docker's delayed path likewise re-enters
  `handle_lane_batch`), (b) reconcile pass while a prepared intent exists.

### TreasuryDO — one per chain

- **Id**: `{chain_id}`.
- **Owns**: the funding lock (holder token + deadline — the `treasury:{chain}`
  lease semantics, kept as REAL lock state because the core program holds it
  across several operations), the `PreparedFundingIntent`, and the treasury
  nonce serialization point.
- **Guard semantics**: single-threaded DO + explicit lock state = today's lease
  with race-free acquire/ensure/release; expiry via deadline check (+ alarm
  cleanup), preserving the batch-end backstop-release property.

## 2. Queue topology

- **One queue** (`vela-relay-ops`) carries the admission→execution envelopes;
  one dead-letter queue. The envelope JSON is byte-identical to today's Iggy
  envelope (`schemaVersion: 1`, `userOperationHash`, `chainId`, `entryPoint`,
  `userOperation`, `sender`).
- **Producer**: the admission fetch handler (the `Enqueue` operation's arm).
- **Consumer**: the queue handler groups a delivered batch by
  (chainId, `relayer_index_for_sender`) — pure core routing — and forwards each
  group to its LaneDO, then acks/retries per message from the returned
  `ItemResolution`s.
- **Ack mapping** (the offset-rule translation): `Durable` → ack;
  `Failed` → retry (platform redelivery with backoff). Durability still gates
  acknowledgment — the at-least-once guarantee is identical; granularity is
  per-message instead of contiguous-prefix, which only removes redundant
  redelivery of already-durable items (declared in the equivalence note).
- **Ordering**: none assumed. Redelivery and reorder are absorbed by the
  existing rules: durable-status skip, dedupe by hash, one-nonce-per-sender,
  nonce triage (future → delayed inbox, stale → reject).

## 3. State-class mapping table

| State class | docker primitive | CF primitive | Guard/consistency |
|---|---|---|---|
| Lifecycle record | Redis string + Lua CAS | RecordDO storage | DO serial + core decisions |
| Record TTL (3600 s class) | Redis TTL | RecordDO alarm cleanup | tolerance declared in bindings contract |
| Bundle→members index | Redis set (TTL'd) | LaneDO storage | lane-scoped, written at mark-submitted |
| Lane mutual exclusion | Redis lease + heartbeat + interrupt | LaneDO single-threading | structural; stronger than lease |
| Prepared bundle intent | Redis create-only Lua | LaneDO storage put-if-absent | DO serial |
| Broadcast-seen (30 s) | Redis short-TTL keys | LaneDO memory+storage | cache — loss harmless |
| Delayed inbox (hold ladder) | Redis zset + claim tokens + server TIME | LaneDO storage + alarm | one clock source per lane (DO time); schedule values stay core-owned |
| Treasury funding lock | Redis lease | TreasuryDO lock state | DO serial + explicit lock |
| Prepared funding intent | Redis create-only | TreasuryDO storage | DO serial |
| Receipt-check schedule | record field + shell loop | RecordDO alarm | tolerance declared |
| Reconcile schedule | shell timer loop | LaneDO alarm (armed while intent exists) | tolerance declared |
| Gas/market/metadata caches | in-process TTL caches | KV (+ isolate memory) | eventual OK — caches only (FR-006) |
| Alert dedup fingerprints | in-process cooldown map | isolate memory (best-effort KV) | cache — loss = extra alert |
| Admission dedupe/fingerprint | Redis SETNX + record read | RecordDO create-if-absent + read | DO serial |

## 4. Wire module (additive core change)

To make byte-parity structural rather than aspirational, the JSON-RPC
envelope — request parsing for the eight methods, error codes/messages
(`-32602 invalid params`, `-32500`, method-not-found, malformed-envelope), and
response rendering (outcome → body JSON) — moves into a pure, additive
`vela_relay_core::wire` module consumed by BOTH shells. The docker shell
delegates to it behavior-preservingly (its tests and the replay battery pin the
bytes); the CF shell gets the same bytes by construction. Transport itself
(axum vs workers-rs, body limits, header plumbing) stays per-shell
(constitution: shell-owned). This is the FR-003 "additive-only with pinning
tests" path.

## 5. Configuration surface

Same validated policy values as the docker shell (markup bps, floors, budgets,
pool width, hold attempts, top-up caps), parsed from Worker env vars/secrets in
the CF shell's config module and injected as data (`ExecutionPolicy`, admission
policy) — Constitution II. Execution ownership (FR-010): a per-deployment
`EXECUTION_CHAINS` allowlist; the deploy workflow's review step asserts global
disjointness across deployments for any shared key material.
