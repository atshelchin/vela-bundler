# Feature Specification: Cloudflare Worker Shell (second deployment target)

**Feature Branch**: `002-cf-worker-shell`

**Created**: 2026-08-28

**Status**: Draft

**Input**: User description: "同一套代码添加一个新的部署端：使用 Rust Cloudflare Workers（wasm），把 Iggy/Redis 替换为 Cloudflare 原生原语（Queues/KV/Durable Objects 等）的架构，借助 Cloudflare 实现无限水平扩容、支撑数百万用户访问。现有 docker compose 部署端必须原样保留（同一仓库双 shell）。新架构要清晰、性能高、准确；业务规则和状态机必须 100% 复用 vela-relay-core（crux core），不 fork 任何决策逻辑；对外 JSON-RPC API 面与现有部署端一致。"

## User Scenarios & Testing *(mandatory)*

The "users" of this feature are (a) the operator, who gains a second, elastically
scaling deployment target for the same relay service; (b) the maintainers, who must
keep exactly one executable definition of every business rule while two shells
consume it; and (c) API consumers, who see the identical JSON-RPC contract
regardless of which deployment serves them.

### User Story 1 - Edge intake and reads (Priority: P1)

As an operator, I can deploy the relay's full external surface — all eight JSON-RPC
methods and the operational GET endpoints — to the edge platform, so that
submissions and status reads are served close to users worldwide. An accepted
operation is durably recorded and durably queued before it is acknowledged, exactly
as today; reads reflect the durable record. Execution may be left disabled in this
deployment, mirroring today's producer-only mode.

**Why this priority**: the intake/read surface is where "millions of users" arrive;
it is independently valuable (a globally distributed front door with durable
queuing), it exercises the admission program end-to-end on the new platform, and it
carries no on-chain risk. It is the MVP.

**Independent Test**: run the existing replay battery against the new deployment
and the docker deployment; every deterministic response is identical. Submit a
valid operation to the new deployment; its status is readable as `queued` and the
envelope is durably queued. Kill/restart platform instances between the durable
write and the acknowledgment; the crash-window behavior matches today's (record
retained, retry-safe, idempotent re-submission).

**Acceptance Scenarios**:

1. **Given** the new deployment with execution disabled, **When** the deterministic
   request battery (validation refusals, unknown-method/malformed envelopes,
   status/byHash/receipt lookups, supported entry points) runs against both
   deployments, **Then** every response body is byte-identical.
2. **Given** a valid in-band operation, **When** it is submitted to the new
   deployment, **Then** the acknowledgment, stored record shape, and subsequent
   status/byHash reads are identical to the docker deployment's, and a duplicate
   re-submission returns the same hash idempotently.
3. **Given** the queue is unavailable after the durable record was created,
   **When** admission runs, **Then** the settled outcome and stored state match
   today's crash-window behavior (record retained, no acknowledgment of a
   non-queued operation).

---

### User Story 2 - Edge execution with identical dispositions (Priority: P2)

As a maintainer, I need queued operations on the new platform to be executed by the
same core programs — admission recovery, dedupe, simulation interpretation, nonce
triage, settlement/hold/reject, funding, signing, broadcast classification, durable
outcome recording — so that every disposition (advance, retry, hold, reject,
dead-letter) is decided by the single existing rule set, with the platform supplying
only transport and storage.

**Why this priority**: this is the money path. It depends on Story 1's intake and
on transport/storage semantics being proven; it converts the new deployment from a
front door into a complete relay.

**Independent Test**: scripted fault injection on the new platform — duplicate
delivery, reordered delivery, storage loss mid-batch, concurrent consumer
scale-out — settles every scenario to the same disposition the core's Driver tests
pin; a soak test produces zero same-nonce-different-bytes broadcasts.

**Acceptance Scenarios**:

1. **Given** a healthy queued batch, **When** the new deployment executes it,
   **Then** the on-chain transaction content, stored lifecycle transitions, and
   diagnostics are those the core program dictates, and the operation reaches
   `submitted`/`included` exactly as on the docker deployment.
2. **Given** the same envelope delivered twice (at-least-once transport), **When**
   both deliveries are processed, **Then** the second is resolved by the existing
   idempotency rules (durable-status skip / prepared-intent resume) and no second
   nonce is consumed.
3. **Given** two execution contexts racing for the same chain and lane, **When**
   both attempt to proceed, **Then** the platform's serialization guarantees that
   exactly one executes while the other reaches the existing "lane owned by
   another worker" disposition — under all scale-out settings.
4. **Given** an infrastructure failure mid-pipeline (storage or chain read),
   **When** the program runs, **Then** the batch settles to retry/defer with the
   existing reason strings, never to a half-recorded outcome.

---

### User Story 3 - Time-driven behavior without resident processes (Priority: P3)

As a maintainer, I need every time-driven behavior — the hold ladder's deferral
schedule, delayed-inbox redelivery, receipt confirmation polling, prepared-bundle
reconciliation, funding retries — to fire on the new platform within a declared
tolerance of the core-defined schedule, even though the platform has no long-lived
processes.

**Why this priority**: correctness of holds and reconciliation is business policy
already encoded in the core; the platform must honor the schedule or held
operations would starve or fire early.

**Independent Test**: park an operation via the hold ladder at attempt *k*; observe
redelivery within the declared tolerance of the core's schedule value; observe a
prepared-bundle reconcile pass and a receipt check occurring within their declared
intervals with no resident process.

**Acceptance Scenarios**:

1. **Given** an operation held at attempt *k*, **When** the schedule elapses,
   **Then** it is redelivered within the declared tolerance and the next decision
   uses the post-increment attempt exactly as today.
2. **Given** a prepared bundle whose worker died after broadcast, **When** the
   reconciliation schedule fires, **Then** the bundle is resumed/marked/cleared by
   the same core audit rules.

---

### User Story 4 - Scale and latency at the edge (Priority: P4)

As the operator, I need the intake/read surface to absorb very large, bursty,
globally distributed traffic without pre-provisioning, so the service can serve
millions of users; execution throughput scales with the number of chains × lanes,
which is the protocol-inherent unit of parallelism.

**Why this priority**: scale is the feature's raison d'être, but it is only
meaningful once Stories 1–3 exist to scale.

**Independent Test**: a load test from at least three regions sustains the target
submission and read rates with the declared latency percentiles, with zero operator
scaling actions and no cross-tenant interference between chains/lanes.

**Acceptance Scenarios**:

1. **Given** sustained global load at the target rate, **When** the intake layer
   serves it, **Then** latency percentiles stay within target and no request is
   dropped for capacity reasons.
2. **Given** one chain's execution is saturated or degraded, **When** other chains
   receive traffic, **Then** their intake and execution latency are unaffected
   (per-chain/lane isolation).

---

### User Story 5 - Two deployments, one repository, safe coexistence (Priority: P5)

As the operator and maintainers, we need both deployment targets to live in one
repository with the docker compose deployment unchanged, a deployment workflow for
each, operational parity (alerts, diagnostics, dead-letters, config/secrets), and a
structural guarantee that the same chain with the same relayer keys is never
executed by two deployments concurrently.

**Why this priority**: coexistence and operational safety close the feature; they
depend on everything above existing.

**Independent Test**: the existing deployment's full gate suite passes unchanged on
the feature branch; a configuration review shows each chain/key set owned by
exactly one execution deployment; alert and diagnostic surfaces on the new
deployment carry the same policy decisions (same gating, same reason strings).

**Acceptance Scenarios**:

1. **Given** the feature branch, **When** the existing gates run (fmt, clippy,
   full test suite, replay battery against the docker deployment), **Then** all
   pass with the docker deployment's behavior unchanged.
2. **Given** both deployments configured, **When** their execution ownership is
   inspected, **Then** no (chain, relayer key set) pair is enabled for execution in
   more than one deployment, and this is enforced by configuration structure, not
   convention.
3. **Given** an executor deferral or rejection on the new platform, **When** the
   operator-facing surfaces are compared, **Then** the alerting policy (what pages,
   what stays quiet) and the recorded diagnostic strings match the existing
   deployment's.

---

### Edge Cases

- At-least-once transport: duplicate and reordered deliveries of the same envelope;
  the existing idempotency and nonce-triage rules must absorb both without new
  decision logic.
- Crash windows: instance terminated between durable record and queue append, or
  between queue append and admitted-mark; recovery behavior must match today's.
- Storage degradation mid-batch: failures surface to the core as result data and
  settle to retry/defer with existing reason strings; never a half-recorded outcome.
- Consumer scale-out: the platform may run many execution contexts; per-(chain,
  lane) mutual exclusion must hold at every concurrency setting, including during
  deploys and regional failover.
- Eventual consistency: any storage surface that is not read-your-write consistent
  must not host lifecycle records, leases, or intents (caches only).
- Platform execution limits (request duration, subrequest counts, payload sizes):
  batch sizing and pipeline structure must respect them without changing business
  rules; an oversized batch is truncated by the existing core rule.
- Cold starts on the money path: first-request latency must not violate the
  declared latency targets; execution scheduling must not rely on instance warmth.
- Clock discipline: the core never reads clocks; every timestamp the new shell
  injects must come from the platform's time source with the same units and
  semantics the docker shell supplies today.
- Dual-deployment interference: simultaneous execution of the same chain+keys by
  both deployments (nonce collisions, double funding) must be structurally
  impossible via configuration ownership.
- Price/metadata source outages: fail-open/fail-closed directions are core
  decisions and must reach the core as data exactly as today.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The repository MUST gain a second deployment target (an edge/serverless
  shell) that consumes every business decision exclusively from `vela-relay-core`;
  no transition table, schedule, budget, parser, threshold, or reason string may be
  reimplemented, forked, or translated in the new shell.
- **FR-002**: The new deployment MUST expose the identical external contract: all
  eight JSON-RPC methods, the operational GET endpoints, identical params
  validation, result shapes, error codes, and reason strings. The deterministic
  replay battery MUST produce byte-identical response bodies across deployments.
- **FR-003**: The existing docker compose deployment MUST remain behaviorally
  unchanged: its full gate suite stays green, its code paths keep their semantics,
  and any change to `vela-relay-core` for this feature MUST be additive-only with
  its own pinning tests (no semantic edit to an existing decision).
- **FR-004**: Admission on the new platform MUST preserve two-phase durability: an
  acknowledgment is returned only after the operation is durably recorded and
  durably queued, with today's crash-window and idempotent-retry semantics.
- **FR-005**: Execution on the new platform MUST preserve: per-(chain, lane) mutual
  exclusion under all concurrency settings; at-least-once redelivery tolerance via
  the existing idempotency rules; prepared-intent persistence and resume such that
  a same-nonce-different-bytes double broadcast is impossible; treasury funding
  serialized per chain.
- **FR-006**: Lifecycle records, leases/ownership, prepared intents, and the
  delayed inbox MUST reside on storage with read-your-write consistency and
  guarded-write (compare-and-set-equivalent) semantics; eventually-consistent
  storage MAY host only caches whose loss is harmless.
- **FR-007**: Every time-driven behavior MUST fire within a declared tolerance of
  the core-defined schedule without resident processes; the tolerance MUST be
  stated per behavior in the plan and verified by test.
- **FR-008**: Operational surfaces MUST carry the same policy: alert gating
  (what notifies, what stays silent), diagnostic record content, dead-letter
  handling, and deferral reason strings match the existing deployment.
- **FR-009**: Configuration and secrets MUST inject validated policy values into
  the core as data (constitution Principle II); raw secrets never cross into the
  core; each deployment target has a documented configuration/secret workflow.
- **FR-010**: Deployment topology MUST make it structurally impossible for two
  deployments to execute the same (chain, relayer key set) concurrently; execution
  ownership is explicit configuration, reviewed as part of the deploy workflow.
- **FR-011**: The intake/read surface MUST scale horizontally without shared
  per-request bottlenecks; per-chain/lane isolation MUST prevent one chain's
  saturation from degrading others.
- **FR-012**: Each user story MUST land as an independently mergeable change with
  the full pre-existing suite green at every merge point; money-path transport
  mappings (old primitive → new primitive) require an equivalence note documenting
  where each guarantee (durability, ordering tolerance, mutual exclusion, TTL)
  now lives.

### Key Entities

- **Deployment target**: a complete, independently operable instantiation of the
  relay (shell + infrastructure bindings); this feature adds the second.
- **Edge intake**: the globally distributed request-handling surface; stateless per
  request, unbounded horizontal scale.
- **Execution unit**: the (chain, lane) pair — the protocol-inherent serialization
  unit; owns its nonce sequence and batch pipeline.
- **Durable queue**: the at-least-once envelope transport between admission and
  execution on the new platform.
- **Lifecycle store**: the strongly consistent home of operation records, leases,
  prepared intents, and the delayed inbox on the new platform.
- **Schedule source**: the platform mechanism that fires time-driven behaviors
  (hold redelivery, receipt checks, reconciliation) without resident processes.
- **Execution ownership**: the explicit configuration mapping each (chain, relayer
  key set) to exactly one deployment.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: The deterministic replay battery produces byte-identical response
  bodies between the docker deployment and the new deployment for 100% of
  deterministic surfaces.
- **SC-002**: The pre-existing test suite (200 tests) passes unmodified at every
  merge point; the docker deployment's replay battery output is unchanged from its
  pre-feature baseline.
- **SC-003**: A rule-duplication audit finds every business rule defined exactly
  once: no transition table, backoff schedule, retry budget, reimbursement parser,
  or reason string exists outside the core crate.
- **SC-004**: Load: the new deployment sustains ≥ 1,000 accepted submissions per
  second aggregate and ≥ 10,000 status reads per second from three continents for
  30 minutes with p95 submission acknowledgment < 500 ms, p95 read < 200 ms, zero
  capacity-caused failures, and zero operator scaling actions — headroom consistent
  with millions of daily users.
- **SC-005**: Fault-injection on the new platform (duplicate delivery, reordering,
  storage loss mid-batch, consumer scale-out races, instance kill during
  admission's crash windows) settles 100% of scenarios to the dispositions the
  core tests pin, with zero same-nonce-different-bytes broadcasts in a soak run.
- **SC-006**: Time-driven behaviors fire within their declared tolerances in 99% of
  observations over a 24-hour window (hold redeliveries, receipt checks,
  reconciliation passes).
- **SC-007**: Per-chain isolation: with one chain artificially saturated, other
  chains' p95 intake and read latencies degrade by less than 10%.

## Assumptions

- The target platform is Cloudflare's developer platform (user decision); the
  specific storage/queue/scheduling primitives are a plan-phase decision driven by
  the consistency and serialization requirements above (FR-005/FR-006/FR-007) —
  the user named Queues/KV/Durable Objects as candidates.
- Feasibility was pre-verified on 2026-08-28: `vela-relay-core` compiles unchanged
  to the platform's wasm target (only a consumer-side randomness-backend feature
  flag is needed in the new shell crate); both crux programs and all decision
  functions are available there.
- The new deployment is a parallel, independent environment: its own queue,
  storage, and relayer/treasury key material; no data migration from the existing
  deployment's Redis/Iggy is in scope.
- Execution ownership is partitioned by configuration (FR-010); serving the same
  chain's intake from both deployments simultaneously is out of scope for this
  feature (a routing/migration strategy would be a separate spec).
- The docker deployment remains the reference implementation; where platform
  constraints force a divergence in *operational* behavior (e.g., scheduling
  granularity), the divergence must be declared in an equivalence note with its
  tolerance — business decisions never diverge.
- Read-side endpoints that consult chain RPCs (estimation, gas price) continue to
  use the same upstream sources and failover policy as today, invoked from the new
  platform's outbound HTTP.
- The constitution's Core/Shell principles apply to the new shell verbatim; a
  constitution amendment naming the second shell is expected during planning
  (PATCH-level, no principle change).
