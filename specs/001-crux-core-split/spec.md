# Feature Specification: Crux Core/Shell Split (vela-relay-core extraction)

**Feature Branch**: `001-crux-core-split`

**Created**: 2026-08-28

**Status**: Draft

**Input**: User description: "学习 p256-index 中 crux shell/core 模型来重构 vela-relay：抽出纯业务决策核心（命名为 vela-relay-core），现有服务转为执行 IO 的 shell，采用 GitHub Spec Kit 流程完成整个重构。"

## User Scenarios & Testing *(mandatory)*

The "users" of this feature are (a) the engineers who maintain and review the relay,
and (b) the operator who runs it in production and depends on it behaving exactly as
before. End users of the relay's API see no change at all.

### User Story 1 - One authoritative lifecycle state machine (Priority: P1)

As a maintainer, I need the user-operation lifecycle transition rules
(`queued → not_submitted → submitted → included/failed/rejected` and their guards) to
exist in exactly one authoritative, executable place in the decision core, so that a
rule change cannot silently drift between the production data-store script and its
hand-maintained test mirror.

**Why this priority**: today the canonical transition table lives inside a data-store
script string while the only executable model of it is a test-only mirror; drift
between the two would corrupt lifecycle state on the money path. This is the highest
risk-removed-per-effort step and it unblocks nothing else — it can merge alone.

**Independent Test**: delete or alter one transition in the core's table and observe
the corresponding core test fail; run the full suite with no infrastructure and see
every legal/illegal transition pinned; confirm the data-store script no longer
encodes any transition decision.

**Acceptance Scenarios**:

1. **Given** an operation in a terminal state (`included`, `failed`, `rejected`),
   **When** any component attempts a further status change, **Then** the change is
   refused by the same core rule that the tests exercise, and the stored record is
   unchanged.
2. **Given** the transition table in the core, **When** the data-store script is
   inspected, **Then** it contains only mechanical guarded writes (compare-and-set,
   ownership checks) and no transition policy.
3. **Given** the pre-refactor behavior, **When** the same sequence of lifecycle
   updates is applied post-refactor, **Then** every accepted/refused outcome and
   every stored field is identical.

---

### User Story 2 - Unified, testable fee-hold policy (Priority: P2)

As a maintainer, I need the in-band fee hold policy — retry backoff schedule, hold
attempt budget, and the hold/reject cutoff — expressed as one pure decision in the
core, so the whole policy can be read and tested as a unit instead of being split
across two languages and a network hop.

**Why this priority**: the hold budget lives in the executor while the backoff
schedule lives in a data-store script; recent production work (hold-budget tuning)
required reasoning across both. Second-highest drift risk after Story 1.

**Independent Test**: core tests enumerate the full hold ladder (attempt 1 → N,
backoff values, cutoff at the budget) and the rejection reason once the budget is
exhausted, with no infrastructure.

**Acceptance Scenarios**:

1. **Given** an operation whose payer cannot afford the current market fee,
   **When** the hold decision runs at attempt *k* within budget, **Then** the core
   yields the same deferral delay the pre-refactor system would have scheduled.
2. **Given** the hold attempt budget is exhausted, **When** the next decision runs,
   **Then** the core yields a rejection with the identical reason string used today.

---

### User Story 3 - Settlement decision as a pure verdict (Priority: P3)

As a maintainer, I need the in-band settlement evaluation — accept at quoted fee,
reprice down to an affordable fee above the inclusion floor, hold, or reject — to be
a single pure decision that consumes pre-fetched market data, so the most
economically sensitive rule in the service is exhaustively testable.

**Why this priority**: this is the business's revenue/safety core (markup,
inclusion floor, cross-subsidy prevention). The math is already mostly pure; what
remains is extracting the decision from the surrounding IO so it can be pinned.

**Independent Test**: table-driven core tests cover accept/reprice/hold/reject
including boundary fees (exactly at the inclusion floor, exactly affordable),
missing market price, and multi-operation batches with mixed outcomes.

**Acceptance Scenarios**:

1. **Given** a batch where every payer covers the quoted fee, **When** the decision
   runs, **Then** the verdict is acceptance at the quoted fee, unchanged from today.
2. **Given** a payer short of quote but clearing the inclusion floor, **When** the
   decision runs, **Then** the verdict reprices to the same affordable fee the
   pre-refactor code computes.
3. **Given** a payer below the inclusion floor, **When** the decision runs, **Then**
   the verdict is hold (within budget) or reject (budget exhausted) with identical
   reason strings.

---

### User Story 4 - Executor batch pipeline testable end-to-end (Priority: P4)

As a maintainer, I need the per-lane batch pipeline (admission recovery, dedupe,
simulation interpretation, nonce-mismatch resolution, gas allocation, settlement,
funding readiness, signing plan, broadcast-outcome classification, durable outcome
recording) driven as a core program whose every step is a decision answered by the
shell, so the currently untested ~2,000-line execution path gains
infrastructure-free coverage.

**Why this priority**: largest engineering effort and depends on Stories 1–3 being
in place; delivers the bulk of the testability payoff.

**Independent Test**: a scripted-driver core test walks a full batch from envelope
ids to a batch verdict (advance/retry), asserting each requested operation in order;
failure-injection variants (store down, chain read failed, broadcast ambiguous,
lease lost) settle to the same retry/advance/dead-letter outcomes as today.

**Acceptance Scenarios**:

1. **Given** a healthy batch, **When** the core program runs against scripted shell
   responses, **Then** the sequence of requested operations matches the
   pre-refactor execution order and the batch settles as advance.
2. **Given** a transient infrastructure failure at any step, **When** the program
   runs, **Then** the batch settles as retry without advancing the consumer offset,
   matching today's contiguous-durable-prefix rule.
3. **Given** an ambiguous broadcast outcome (transaction possibly landed), **When**
   the program runs, **Then** the core requests the same verification steps and
   reaches the same disposition as the pre-refactor classifier.

---

### User Story 5 - Admission protocol testable (Priority: P5)

As a maintainer, I need the two-phase admission protocol (durable record creation,
queue append, admitted mark, idempotent-retry fingerprinting, crash-window
recovery) expressed as a core program, so its crash-consistency reasoning is pinned
by tests instead of prose comments.

**Why this priority**: well-understood and lower-risk than the executor, but its
main path currently has no test; benefits from the vocabulary established by
Stories 1 and 4.

**Independent Test**: core tests cover first submission, duplicate submission with
matching/mismatching fingerprint, queue-unavailable-after-record crash window, and
each response (status code class and body) is identical to today's.

**Acceptance Scenarios**:

1. **Given** a valid new user operation, **When** admission runs, **Then** the core
   requests record creation, queue append, and admitted-mark in that order and
   settles with the same acknowledgement returned today.
2. **Given** the queue is unavailable after the record was created, **When**
   admission runs, **Then** the settled outcome and stored state match today's
   crash-window behavior (record retained, retry-safe).

---

### User Story 6 - One reimbursement interpretation (Priority: P6)

As a maintainer, I need in-band reimbursement calldata interpretation to exist as a
single core function used by both the quoting/validation path and the execution
path, so the two copies that exist today cannot diverge in what they accept.

**Why this priority**: closes the remaining known duplication; small, mechanical,
best done after both call sites already speak the core vocabulary.

**Independent Test**: the existing tests of both former copies all pass against the
single function; a dependency audit shows one implementation remains.

**Acceptance Scenarios**:

1. **Given** any calldata accepted or rejected by either pre-refactor parser,
   **When** the unified function evaluates it, **Then** the accept/reject outcome
   and extracted amounts are identical.

---

### Edge Cases

- Crash windows: process dies between durable-record creation and queue append, or
  between queue append and admitted-mark — recovery behavior must be identical.
- Infrastructure loss mid-batch: store or chain endpoints become unavailable between
  steps of an executing batch — the batch must settle to retry without offset
  advance, never to a half-recorded outcome.
- Lease loss mid-pipeline: another process takes over a lane — the losing side must
  reach the same abandon/retry outcome as today.
- Stale market data: price source unavailable — quoting and top-up capping must
  degrade exactly as today (static caps, fail-closed where currently fail-closed).
- Ambiguous broadcast: transaction submission times out but may have landed — the
  resume/reconcile disposition must be unchanged.
- Terminal-state races: a late executor result arriving after an operation reached a
  terminal state must be refused by the single transition table.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: All business decision rules (admission, lifecycle transitions,
  settlement, hold, funding, broadcast classification, dead-letter policy) MUST
  reside in a dedicated decision-core component (`vela-relay-core`) that has no
  infrastructure dependencies and performs no I/O.
- **FR-002**: Every state transition table, retry budget, and backoff schedule MUST
  have exactly one executable definition, located in the decision core; data-store
  scripts MUST be reduced to mechanical guarded writes.
- **FR-003**: The refactor MUST be behavior-preserving: every externally observable
  response (HTTP status, JSON shape, reason string, error message), every stored
  record shape, every threshold and timing budget remains identical unless this spec
  explicitly declares a change. This spec declares none.
- **FR-004**: Every nondeterministic input consumed by a decision (time, generated
  identifiers, chain context, market prices) MUST be supplied to the core by the
  shell; the core MUST NOT observe clocks, randomness, or environment.
- **FR-005**: Infrastructure failures MUST reach the core as ordinary result data,
  and the core alone MUST decide their meaning (retry, hold, reject, fail-open,
  fail-closed), preserving today's per-case policy.
- **FR-006**: Each decision path in the core MUST be covered by tests that run with
  no external infrastructure and no async runtime, including scripted full-pipeline
  walks for admission and batch execution.
- **FR-007**: The consumer's offset-advance rule (only the contiguous durable prefix
  advances) MUST be preserved and its decision expressed in the core.
- **FR-008**: Session management, reconnection, lease heartbeats, connection pools,
  task supervision, and process lifecycle MUST remain in the shell.
- **FR-009**: Reimbursement calldata interpretation MUST exist as exactly one
  implementation used by all call sites.
- **FR-010**: Each user story MUST land as an independently mergeable change with
  the full pre-existing test suite green at every merge point.
- **FR-011**: Migration changes touching admission, settlement, funding, or
  broadcast MUST include a written equivalence note mapping old code path to new
  core decision, including the destination of every reason string.

### Key Entities

- **Decision core (`vela-relay-core`)**: the business vocabulary and every decision
  rule; consumes events and result data, produces requested operations and settled
  outcomes; infrastructure-free.
- **Shell (`vela-relay`)**: transport, storage, queue, chain access, key custody,
  alerting, scheduling; executes the core's requested operations and reports results
  back as data.
- **User operation lifecycle**: the states an accepted operation moves through and
  the single transition table governing them.
- **Batch program**: one consumed lane batch driven from envelope ids to an
  advance/retry verdict.
- **Admission program**: one submitted operation driven from validation to a settled
  acknowledgement or refusal.
- **Settlement verdict**: the accept / reprice / hold / reject decision for a batch's
  in-band fees.
- **Hold ladder**: the attempt-indexed deferral schedule and budget for operations
  awaiting an affordable market.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of the enumerated money-path decisions (lifecycle transitions,
  hold ladder, settlement verdict, broadcast classification, funding affordability,
  admission protocol, batch pipeline) are exercised by tests that require no
  external infrastructure and complete in under 10 seconds total.
- **SC-002**: The complete pre-existing test suite (160 tests) passes unmodified at
  every merge point; any intentional test change is traceable to a spec-declared
  behavior change (of which there are none).
- **SC-003**: A dependency audit of the decision core shows zero
  network/storage/runtime dependencies.
- **SC-004**: Exactly one executable definition exists for the lifecycle transition
  table, the hold backoff schedule, and reimbursement interpretation (today: 2, 2,
  and 2 respectively); the test-only transition mirror is deleted.
- **SC-005**: The five currently untested executor main-path behaviors (lane batch
  handling, lease-scoped execution, settlement-at-affordable-fee, funding top-up,
  bundle reconciliation) each have scripted-driver coverage including at least one
  failure-injection scenario.
- **SC-006**: An operator replaying production-shaped traffic against old and new
  builds observes byte-identical API responses and identical stored lifecycle
  records.

## Assumptions

- The `p256-index` per-unit-of-work paradigm is the template: short-lived decision
  programs per request/batch; long-lived operational state stays in the shell.
- The decision core is a separate crate named `vela-relay-core` (user-approved name);
  the repository becomes a two-member workspace. The workspace conversion itself is
  structural, not behavioral, and is in scope.
- The engine version and paradigm follow the reference projects (`crux_core` 0.19,
  custom operation vocabulary, no stock capability crates, no foreign-language
  shells or type generation).
- Read-side endpoints (status, receipts, quotes) may migrate opportunistically; they
  are lower priority than the six stories and may be deferred to a follow-up spec.
- The three placeholder background jobs continue to exist and continue to gate
  readiness exactly as today; removing them is a deliberate behavior change deferred
  to a separate future spec.
- Operational tooling (deployment binary, dashboards, env configuration) is
  unchanged; configuration parsing stays in the shell while validated policy values
  are passed into the core as data.
- No API consumers are notified because no observable contract changes.
