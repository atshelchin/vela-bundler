# Contract: Platform Bindings (Operation → Cloudflare primitive)

The core's operation vocabularies are FROZEN (they are the crux seam both
shells share). This contract maps every operation to its CF-shell executor and
names the guarantee that answers it. It is the skeleton of the FR-012
equivalence notes: any cell that changes during implementation must update this
file in the same PR.

Legend: RDO = RecordDO, LDO = LaneDO, TDO = TreasuryDO (see data-model.md).

## AdmissionOperation (fetch handler drives `AdmissionApp`)

| Operation | Executor | Guarantee source |
|---|---|---|
| `LoadSettlementAssets` | metadata service fetch + KV cache | same upstream + fail direction as docker (cache only) |
| `FetchTokenDecimals` | chain RPC fetch (failover) | same upstream policy; shell-owned transport |
| `CreateQueued` | RDO create-if-absent | DO serial = SETNX |
| `LoadExisting` | RDO read | read-your-write |
| `Enqueue` | Queues producer send — the envelope travels as its exact JSON text (the v8 structured clone of a serde map arrives as a JS Map, unreadable as an object; the consumer parses the string) | durable at-least-once; `QueueUnavailable` on send failure (crash-window semantics preserved) |
| `MarkAdmitted` | RDO write | DO serial |

## ExecutionOperation (LaneDO drives `ExecutionApp`)

| Operation | Executor | Guarantee source |
|---|---|---|
| `CheckChainSupported` | trusted-RPC resolution (env override → Alchemy → directory), dynamic per chain; optional `EXECUTION_CHAINS` restriction for shared-key topologies only | docker-parity dynamic chains (R10) |
| `LoadChainAssets` | metadata fetch + KV cache | cache only |
| `LoadRecords` / `ReloadRecord` | RDO reads (one subrequest per record) | read-your-write |
| `DeadLetterRouted` | dead-letter queue send + RDO diagnostic | at-least-once |
| `RestoreQueued` / `MarkAdmitted` / `MarkRejected` / `MarkRejectedWithReason` | RDO writes | DO serial + core decisions |
| `DeferOperation` | LDO delayed-inbox write + alarm (post-increment attempt returned) | DO serial; schedule values core-owned |
| `RecordDeferred` / `NotifyIssue` / `EmitDiagnostic` | RDO diagnostic write / Telegram fetch / structured log | same gating policy (core-decided) |
| `AcquireLaneLease` / `EnsureLaneLease` | constant `true` | LaneDO single-threading (structurally stronger; interrupt path unreachable — declared) |
| `LoadPreparedBundle` / `SavePreparedBundle` / `ClearStaleIntent` | LDO storage (put-if-absent / guarded clear) | DO serial |
| `ResumeBundleIntent` | LDO composite (same sequencing as docker shell) | DO serial + core rules |
| `SimulateIndividually` / `SimulateBundle` / `FetchAccountNonces` | chain RPC fetches (failover), same three-tier order (`eth_simulateV1` → deployed Pimlico `eth_call` → `debug_traceCall`); this shell does NOT auto-deploy the Pimlico pair (see Explicitly absent) | shell-owned transport; same interpretation rules (core `simulation`) |
| `FetchTransactionContext` / `FetchTempoContext` / `FetchTreasuryContext` / `FetchTempoTreasuryContext` | chain RPC batch fetches | same call sets as docker |
| `FetchMarketPrice` | Binance fetch + KV cache | same fail-open/closed directions (core-decided) |
| `AcquireTreasuryLease` / `EnsureTreasuryLease` / `ReleaseTreasuryLease` | TDO lock (token + deadline) | DO serial + explicit lock; store error → `Failed` (batch-fatal, as docker) |
| `LoadPreparedFunding` / `SaveFundingIntent` / `ClearFundingIntent` | TDO storage | DO serial |
| `SignTreasuryTransfer` / `SignTreasuryPathUsd` / `SignBundle` / `SignTempoBundle` | core signing fns + secret bindings | keys never enter core; same signing math |
| `AcquireReceiptProbe` | TDO short lock | DO serial |
| `FetchTransactionReceipt` | chain RPC fetch | same policy |
| `RecordTreasuryShortfall` / `RecordPartialTopUp` / `RecordFundingSubmitted` / `RecordUnprovenFunding` / `NoteFundingReceipt` | TDO/RDO writes + logs | same texts |
| `CheckBroadcastSeen` / `RememberBroadcast` / `ForgetBroadcast` | LDO cache | cache (30 s), loss harmless |
| `BroadcastRaw` / `ProbeTransactionKnown` / `ProbeStaleNonce` | chain RPC fetches | same classification rules (core `broadcast`) |
| `RecordUnprovenBroadcast` | LDO/RDO diagnostic | same texts |
| `MarkBundleSubmitted` | per-member RDO guarded writes + LDO index | core `decide_bundle_submission` per member; indexed-count guard unchanged |

## Time-driven behaviors

| Behavior | docker mechanism | CF mechanism | Tolerance (declared) |
|---|---|---|---|
| Hold-ladder redelivery | Redis zset + claim readers | LDO alarm at min(due) | fire within max(30 s, 10%) of due |
| Receipt confirmation checks | shell loop + record field | RDO alarm | same interval values; same tolerance class |
| Prepared-bundle reconcile | shell timer | LDO alarm while intent exists | same interval values |
| Record TTL expiry | Redis TTL | RDO alarm cleanup | ± one alarm granule; never early |
| Funding retry cadence | in-batch + redelivery | queue retry delay + TDO state | core-owned values |

## Explicitly absent on this shell

- The lease-interrupt channel (`ExecutionOutcome::Interrupted`) is never
  produced: mutual exclusion is structural. The vocabulary stays shared and
  unchanged; the docker shell continues to use it.
- No operation exists to delete an admitted record (Constitution III — the
  absent-operation guarantee carries over verbatim).
- Automatic Pimlico simulation-contract deployment (docker
  `SimulationContractDeployer`) has no counterpart: deployment needs treasury
  signing plus a receipt wait, which belongs to the docker shell. The CREATE2
  addresses are a pure function of the shared treasury (core
  `simulation::pimlico_contracts_for_treasury`), so a pair deployed by the
  docker shell is found `Ready` here. A `Missing` pair falls through to
  `debug_traceCall`; the `SimulationVerdict::Pending` verdict and the
  deployment-wait diagnostics are never produced on this shell (chains lacking
  all three tiers defer on the core's hold ladder, unchanged).
