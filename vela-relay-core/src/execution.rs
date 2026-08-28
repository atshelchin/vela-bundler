//! The per-lane batch execution program.
//!
//! One `Core<ExecutionApp>` drives one consumed lane batch from routed
//! envelopes to a per-operation resolution vector. Every business decision —
//! triage, deduplication, simulation-verdict handling, the settlement gate,
//! funding readiness, the sign/persist/broadcast/mark sequence — is made
//! here; the shell executes the requested operations against Redis, the
//! chain, and Telegram and reports what happened as data.
//!
//! Three nested pipelines are still modeled as composite operations executed
//! by the shell (`ResumeBundleIntent`, `ResolveNonceMismatches`,
//! `EnsureRelayerFunded`, plus the whole Tempo bundle tail and
//! `BroadcastBundle`): their internal decisions migrate here in a follow-up
//! (tasks T034); the operations' *outcomes* already flow through this
//! program's decisions.

use std::collections::HashSet;

use alloy::primitives::{Address, B256, U256};
use crux_core::{App, Command, macros::effect};

use crate::{
    abi::{PackedOperation, handle_ops_calldata, user_operation_hash},
    cost::allocate_bundle_gas,
    funding::{NATIVE_TOP_UP_USD_CAP, native_amount_for_usd_cap},
    hold::{HoldDecision, decide_hold},
    settlement::{
        ChainAssetConfig, FeeContext, SettlementDecision, SettlementLog, decide_settlement,
        has_stablecoin_payment, settlement_rejection_reason, verify_stable_transfer_logs,
    },
    task::{
        PreparedBundleIntent, QueuedUserOperation, RoutedUserOperation, StoredUserOperation,
        UserOperation,
    },
    vault::relayer_index_for_sender,
};

/// Validated policy values the shell injects per batch (from its config).
#[derive(Clone, Debug)]
pub struct ExecutionPolicy {
    pub pool_width: usize,
    pub max_bundle_operations: usize,
    pub gas_buffer_bps: u64,
    pub fixed_gas_buffer: u64,
    pub settlement_inclusion_floor_bps: u64,
    pub settlement_hold_max_attempts: u32,
    /// Static fallback per-transfer top-up cap in wei, used when no market
    /// price is available.
    pub top_up_max_wei: u128,
    /// Whether this chain settles through Tempo's pathUSD `0x76` path.
    pub is_tempo: bool,
    /// The settlement recipient / treasury address (derived by the shell).
    pub treasury: Address,
}

/// Everything the program needs to know about one lane batch at start.
#[derive(Debug)]
pub struct StartBatch {
    pub operations: Vec<RoutedUserOperation>,
    pub policy: ExecutionPolicy,
    /// Shell-generated lane lease token (nondeterministic input).
    pub lease_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemResolution {
    /// A durable outcome was reached; the consumer offset may pass this item.
    Durable,
    /// No durable outcome; the message must be redelivered.
    Failed { reason: String },
}

/// Chain asset metadata as resolved by the shell (directory or Tempo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedChainAssets {
    pub assets: ChainAssetConfig,
    pub native_symbol: String,
}

/// Mirror of the shell's per-operation simulation verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationSimVerdict {
    Success,
    NonceMismatch,
    Rejected {
        reason: String,
    },
    /// Waiting for automatic simulation-contract deployment.
    Pending {
        reason: String,
    },
    Transient {
        reason: String,
    },
}

/// Mirror of the shell's whole-bundle simulation verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleSimVerdict {
    Success(BundleSimulationData),
    NonceMismatch,
    Rejected { reason: String },
    Pending { reason: String },
    Transient { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSimulationData {
    pub gas_used: U256,
    /// `actual_gas_used` per surviving operation, in bundle order.
    pub operation_gas_used: Vec<U256>,
    /// Logs from the exact final handleOps simulation (settlement evidence).
    pub logs: Vec<SettlementLog>,
}

/// Fee/nonce/balance context for the outer transaction, fetched by the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionContext {
    pub estimated_gas: U256,
    pub base_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub nonce: u64,
    pub relayer_balance: U256,
}

/// The signed outer transaction as produced by the shell's keystore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedBundle {
    pub raw_transaction_hex: String,
    pub transaction_hash: String,
    pub nonce: u64,
}

/// The plan the shell signs; keys never cross into the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSignRequest {
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub entry_point: Address,
    pub calldata: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub enum ExecutionOperation {
    // --- triage ---
    CheckChainSupported,
    LoadChainAssets,
    LoadRecords {
        hashes: Vec<String>,
    },
    DeadLetterRouted {
        index: usize,
        reason: String,
    },
    RestoreQueued {
        index: usize,
        queued: QueuedUserOperation,
    },
    ReloadRecord {
        hash: String,
    },
    MarkAdmitted {
        hash: String,
    },
    MarkRejected {
        hash: String,
    },
    MarkRejectedWithReason {
        hash: String,
        stage: &'static str,
        reason: String,
    },
    /// Park item `index` in the durable delayed inbox (post-increment attempt
    /// count comes back; the hold budget is judged here).
    DeferOperation {
        index: usize,
    },
    // --- diagnostics (best-effort writes; Telegram policy decided here) ---
    RecordDeferred {
        hash: String,
        stage: &'static str,
        reason: String,
    },
    NotifyIssue {
        hash: String,
        stage: &'static str,
        reason: String,
    },
    // --- leases ---
    AcquireLaneLease,
    EnsureLaneLease,
    // --- prepared intent ---
    LoadPreparedBundle,
    /// Composite: audit replay, clear or rebroadcast as today (T034 splits it).
    ResumeBundleIntent {
        intent: PreparedBundleIntent,
    },
    // --- simulation ---
    SimulateIndividually {
        operations: Vec<(B256, PackedOperation)>,
    },
    /// Composite: batch getNonce probe + defer/reject per mismatch (T034).
    ResolveNonceMismatches {
        items: Vec<NonceMismatchItem>,
    },
    SimulateBundle {
        operations: Vec<(B256, PackedOperation)>,
    },
    // --- outer transaction ---
    FetchTransactionContext {
        entry_point: Address,
        calldata: Vec<u8>,
    },
    FetchMarketPrice,
    /// Composite: treasury lease + probe + sign + broadcast of a top-up (T034).
    EnsureRelayerFunded {
        relayer_balance: U256,
        required_prefund: U256,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        top_up_max: U256,
    },
    SignBundle {
        request: BundleSignRequest,
    },
    SavePreparedBundle {
        intent: PreparedBundleIntent,
    },
    /// Composite: recently-confirmed cache, broadcast, observability probes
    /// (its judgement already lives in `crate::broadcast`; T034 sequences it).
    BroadcastBundle {
        intent: PreparedBundleIntent,
    },
    MarkBundleSubmitted {
        transaction_hash: String,
        hashes: Vec<String>,
    },
    /// Composite: the whole Tempo `0x76` tail (context, settlement gate,
    /// funding, sign, broadcast) exactly as today.
    ExecuteTempoBundle {
        entry_point: Address,
        survivors: Vec<usize>,
        simulation: BundleSimulationData,
    },
}

#[derive(Debug, PartialEq)]
pub struct NonceMismatchItem {
    pub index: usize,
    pub hash: String,
    pub sender: Address,
    pub nonce: U256,
}

#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "Shell results are constructed once and consumed immediately in-process; boxing every record-bearing variant would add noise at each decision site."
)]
pub enum ExecutionOutcome {
    Supported {
        supported: bool,
    },
    Assets {
        resolved: ResolvedChainAssets,
    },
    AssetsUnavailable {
        reason: String,
    },
    Records {
        records: Vec<Option<StoredUserOperation>>,
    },
    Record {
        record: Option<StoredUserOperation>,
    },
    Persisted {
        persisted: bool,
    },
    Marked {
        marked: bool,
    },
    Deferred {
        attempt: u32,
    },
    Done,
    LeaseAcquired {
        acquired: bool,
    },
    LeaseHeld {
        held: bool,
    },
    Intent {
        intent: Option<PreparedBundleIntent>,
    },
    Resumed {
        known_outcome: bool,
    },
    OperationVerdicts {
        verdicts: Vec<OperationSimVerdict>,
    },
    MismatchResolutions {
        resolutions: Vec<(usize, ItemResolution)>,
    },
    BundleVerdict {
        verdict: BundleSimVerdict,
    },
    Context {
        context: TransactionContext,
    },
    Price {
        price: Option<U256>,
    },
    Funding {
        ready: bool,
    },
    Signed {
        signed: SignedBundle,
    },
    Saved {
        saved: bool,
    },
    Broadcast {
        confirmed: bool,
    },
    Indexed {
        indexed: usize,
    },
    TempoOutcome {
        resolutions: Vec<(usize, ItemResolution)>,
    },
    /// Any infrastructure failure, folded to its display text; the program
    /// decides what it means at each step.
    Failed {
        message: String,
    },
}

impl crux_core::capability::Operation for ExecutionOperation {
    type Output = ExecutionOutcome;
}

#[effect]
pub enum ExecutionEffect {
    Work(ExecutionOperation),
}

#[derive(Debug)]
pub enum ExecutionEvent {
    Start(Box<StartBatch>),
    Settled(Vec<ItemResolution>),
}

#[derive(Default)]
pub struct ExecutionModel {
    outcome: Option<Vec<ItemResolution>>,
}

pub struct ExecutionViewModel {
    pub outcome: Option<Vec<ItemResolution>>,
}

#[derive(Default)]
pub struct ExecutionApp;

impl App for ExecutionApp {
    type Event = ExecutionEvent;
    type Model = ExecutionModel;
    type ViewModel = ExecutionViewModel;
    type Effect = ExecutionEffect;

    fn update(
        &self,
        event: Self::Event,
        model: &mut Self::Model,
    ) -> Command<Self::Effect, Self::Event> {
        match event {
            ExecutionEvent::Start(start) => Command::new(|ctx| async move {
                let outcome = drive_batch(&ctx, *start).await;
                ctx.send_event(ExecutionEvent::Settled(outcome));
            }),
            ExecutionEvent::Settled(outcome) => {
                model.outcome = Some(outcome);
                Command::done()
            }
        }
    }

    fn view(&self, model: &Self::Model) -> Self::ViewModel {
        ExecutionViewModel {
            outcome: model.outcome.clone(),
        }
    }
}

type Ctx = crux_core::command::CommandContext<ExecutionEffect, ExecutionEvent>;

async fn request(ctx: &Ctx, operation: ExecutionOperation) -> ExecutionOutcome {
    ctx.request_from_shell(operation).await
}

/// Whether an executor deferral is operator-actionable. A lease held by
/// another worker and a freshly submitted funding/deployment transaction are
/// expected hand-offs; Telegram is reserved for work that is actually blocked.
pub fn should_notify_executor_deferred(stage: &str) -> bool {
    !matches!(stage, "lease" | "funding" | "simulation_deployment")
}

/// Validation of one routed envelope for durable-payload restoration.
/// (Formerly the shell's `queued_operation_from_routed`.)
pub fn queued_operation_from_routed(
    routed: &RoutedUserOperation,
    pool_width: usize,
) -> Result<QueuedUserOperation, &'static str> {
    let operation = serde_json::from_value::<UserOperation>(routed.user_operation.clone())
        .map_err(|_| "queue UserOperation payload is not canonical v0.7 JSON")?;
    if serde_json::to_value(&operation).ok().as_ref() != Some(&routed.user_operation) {
        return Err("queue UserOperation payload is not canonical JSON");
    }
    let entry_point = parse_address(&routed.entry_point, "queue EntryPoint address is invalid")?;
    let hash = parse_hash(
        &routed.user_operation_hash,
        "queue UserOperation hash is invalid",
    )?;
    let packed =
        PackedOperation::try_from(&operation).map_err(|_| "could not pack queue UserOperation")?;
    if packed.has_eip7702_authorization {
        return Err("EIP-7702 UserOperations are not enabled in the executor");
    }
    if !packed
        .sender
        .to_string()
        .eq_ignore_ascii_case(&routed.sender)
    {
        return Err("queue sender does not match UserOperation sender");
    }
    if relayer_index_for_sender(&routed.sender, pool_width) != routed.lane as usize {
        return Err("sender route does not match relayer lane");
    }
    if user_operation_hash(&packed, entry_point, routed.chain_id) != hash {
        return Err("queue UserOperation hash does not match immutable payload");
    }
    Ok(QueuedUserOperation {
        user_operation_hash: routed.user_operation_hash.to_ascii_lowercase(),
        chain_id: routed.chain_id,
        entry_point: routed.entry_point.clone(),
        user_operation: operation,
    })
}

/// A record admitted for execution, packed and cross-checked against its
/// envelope. (Formerly the shell's `candidate_from_record`.)
pub struct Candidate {
    pub result_index: usize,
    pub hash: B256,
    pub hash_string: String,
    pub entry_point: Address,
    pub packed: PackedOperation,
}

pub fn candidate_from_record(
    result_index: usize,
    routed: &RoutedUserOperation,
    record: &StoredUserOperation,
    pool_width: usize,
) -> Result<Candidate, &'static str> {
    let hash = parse_hash(
        &routed.user_operation_hash,
        "queue UserOperation hash is invalid",
    )?;
    let entry_point = parse_address(&routed.entry_point, "queue EntryPoint address is invalid")?;
    if relayer_index_for_sender(&routed.sender, pool_width) != routed.lane as usize {
        return Err("sender route does not match relayer lane");
    }
    let packed = PackedOperation::try_from(&record.user_operation)
        .map_err(|_| "could not pack queued UserOperation")?;
    if packed.has_eip7702_authorization {
        return Err("EIP-7702 UserOperations are not enabled in the executor");
    }
    if !packed
        .sender
        .to_string()
        .eq_ignore_ascii_case(&routed.sender)
    {
        return Err("queue sender does not match UserOperation sender");
    }
    if user_operation_hash(&packed, entry_point, routed.chain_id) != hash {
        return Err("queue UserOperation hash does not match immutable payload");
    }
    Ok(Candidate {
        result_index,
        hash,
        hash_string: routed.user_operation_hash.to_ascii_lowercase(),
        entry_point,
        packed,
    })
}

/// Whether the queue envelope still matches the admitted record.
pub fn queue_record_matches(routed: &RoutedUserOperation, record: &StoredUserOperation) -> bool {
    record.chain_id == routed.chain_id
        && record.entry_point.eq_ignore_ascii_case(&routed.entry_point)
        && serde_json::to_value(&record.user_operation).ok().as_ref()
            == Some(&routed.user_operation)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionAction {
    Execute,
    Recover,
    DeadLetter,
}

pub fn admission_action(admitted: bool, envelope_matches: bool) -> AdmissionAction {
    match (admitted, envelope_matches) {
        (_, false) => AdmissionAction::DeadLetter,
        (false, true) => AdmissionAction::Recover,
        (true, true) => AdmissionAction::Execute,
    }
}

fn parse_address(value: &str, error: &'static str) -> Result<Address, &'static str> {
    value.parse::<Address>().map_err(|_| error)
}

fn parse_hash(value: &str, error: &'static str) -> Result<B256, &'static str> {
    value.parse::<B256>().map_err(|_| error)
}

struct Results {
    slots: Vec<Option<ItemResolution>>,
}

impl Results {
    fn new(len: usize) -> Self {
        Self {
            slots: vec![None; len],
        }
    }

    fn durable(&mut self, index: usize) {
        self.slots[index] = Some(ItemResolution::Durable);
    }

    fn failed(&mut self, index: usize, reason: impl Into<String>) {
        self.slots[index] = Some(ItemResolution::Failed {
            reason: reason.into(),
        });
    }

    fn is_settled(&self, index: usize) -> bool {
        self.slots[index].is_some()
    }

    fn finish(self, default_reason: &str) -> Vec<ItemResolution> {
        self.slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or(ItemResolution::Failed {
                    reason: default_reason.to_owned(),
                })
            })
            .collect()
    }

    fn all_failed(len: usize, reason: &str) -> Vec<ItemResolution> {
        (0..len)
            .map(|_| ItemResolution::Failed {
                reason: reason.to_owned(),
            })
            .collect()
    }
}

/// Best-effort deferral diagnostic plus the Telegram policy. Mirrors the
/// shell's old `record_executor_deferred`.
async fn record_deferred(ctx: &Ctx, hash: &str, stage: &'static str, reason: &str) {
    let _ = request(
        ctx,
        ExecutionOperation::RecordDeferred {
            hash: hash.to_owned(),
            stage,
            reason: reason.to_owned(),
        },
    )
    .await;
    if should_notify_executor_deferred(stage) {
        let _ = request(
            ctx,
            ExecutionOperation::NotifyIssue {
                hash: hash.to_owned(),
                stage,
                reason: reason.to_owned(),
            },
        )
        .await;
    }
}

/// Deferral diagnostics for every unresolved routed operation. Mirrors
/// `record_routed_deferred`.
async fn record_routed_deferred(
    ctx: &Ctx,
    operations: &[RoutedUserOperation],
    results: Option<&Results>,
    stage: &'static str,
    reason: &str,
) {
    for (index, operation) in operations.iter().enumerate() {
        if results.is_some_and(|results| results.is_settled(index)) {
            continue;
        }
        record_deferred(ctx, &operation.user_operation_hash, stage, reason).await;
    }
}

async fn record_candidates_deferred(
    ctx: &Ctx,
    candidates: &[Candidate],
    stage: &'static str,
    reason: &str,
) {
    for candidate in candidates {
        record_deferred(ctx, &candidate.hash_string, stage, reason).await;
    }
}

const DEFERRED_FINISH: &str = "UserOperation execution was deferred";
const NO_OUTCOME_FINISH: &str = "no durable executor outcome";

/// The whole lane-batch program. Sequential: at most one operation is ever in
/// flight (the Driver tests assert this invariant).
async fn drive_batch(ctx: &Ctx, start: StartBatch) -> Vec<ItemResolution> {
    let StartBatch {
        operations, policy, ..
    } = &start;
    if operations.is_empty() {
        return Vec::new();
    }
    let chain_id = operations[0].chain_id;
    let lane = operations[0].lane;
    if operations
        .iter()
        .any(|operation| operation.chain_id != chain_id || operation.lane != lane)
    {
        return Results::all_failed(
            operations.len(),
            "consumer returned a mixed chain/lane batch",
        );
    }

    match request(ctx, ExecutionOperation::CheckChainSupported).await {
        ExecutionOutcome::Supported { supported: true } => {}
        _ => {
            let reason = "chain has no trusted executor RPC";
            record_routed_deferred(ctx, operations, None, "rpc", reason).await;
            return Results::all_failed(operations.len(), reason);
        }
    }
    let chain_assets = match request(ctx, ExecutionOperation::LoadChainAssets).await {
        ExecutionOutcome::Assets { resolved } => resolved,
        ExecutionOutcome::AssetsUnavailable { reason }
        | ExecutionOutcome::Failed { message: reason } => {
            record_routed_deferred(ctx, operations, None, "assets", &reason).await;
            return Results::all_failed(operations.len(), &reason);
        }
        _ => return Results::all_failed(operations.len(), "unexpected shell response"),
    };

    let hashes = operations
        .iter()
        .map(|operation| operation.user_operation_hash.clone())
        .collect::<Vec<_>>();
    let records = match request(ctx, ExecutionOperation::LoadRecords { hashes }).await {
        ExecutionOutcome::Records { records } => records,
        ExecutionOutcome::Failed { message } => {
            return Results::all_failed(operations.len(), &message);
        }
        _ => return Results::all_failed(operations.len(), "unexpected shell response"),
    };

    let mut results = Results::new(operations.len());
    let mut candidates = Vec::new();
    for (index, (routed, record)) in operations.iter().zip(records).enumerate() {
        let mut record = match record {
            Some(record) => record,
            None => {
                let queued = match queued_operation_from_routed(routed, policy.pool_width) {
                    Ok(queued) => queued,
                    Err(reason) => {
                        match request(
                            ctx,
                            ExecutionOperation::DeadLetterRouted {
                                index,
                                reason: reason.to_owned(),
                            },
                        )
                        .await
                        {
                            ExecutionOutcome::Persisted { persisted: true } => {
                                results.durable(index);
                            }
                            _ => {
                                results.failed(index, "could not persist invalid queue message");
                            }
                        }
                        continue;
                    }
                };
                match request(ctx, ExecutionOperation::RestoreQueued { index, queued }).await {
                    ExecutionOutcome::Done => {}
                    ExecutionOutcome::Failed { message } => {
                        results.failed(index, &message);
                        continue;
                    }
                    _ => {
                        results.failed(index, "unexpected shell response");
                        continue;
                    }
                }
                match request(
                    ctx,
                    ExecutionOperation::ReloadRecord {
                        hash: routed.user_operation_hash.clone(),
                    },
                )
                .await
                {
                    ExecutionOutcome::Record {
                        record: Some(record),
                    } => record,
                    ExecutionOutcome::Record { record: None } => {
                        results.failed(
                            index,
                            "rebuilt UserOperation status disappeared before execution",
                        );
                        continue;
                    }
                    ExecutionOutcome::Failed { message } => {
                        results.failed(index, &message);
                        continue;
                    }
                    _ => {
                        results.failed(index, "unexpected shell response");
                        continue;
                    }
                }
            }
        };
        if record.status.is_durable() {
            results.durable(index);
            continue;
        }
        match admission_action(record.admitted, queue_record_matches(routed, &record)) {
            AdmissionAction::DeadLetter => {
                match request(
                    ctx,
                    ExecutionOperation::DeadLetterRouted {
                        index,
                        reason: "Iggy envelope does not match Redis admission".to_owned(),
                    },
                )
                .await
                {
                    ExecutionOutcome::Persisted { persisted: true } => results.durable(index),
                    _ => results.failed(index, "could not persist mismatched queue message"),
                }
                continue;
            }
            AdmissionAction::Recover => {
                match request(
                    ctx,
                    ExecutionOperation::MarkAdmitted {
                        hash: routed.user_operation_hash.clone(),
                    },
                )
                .await
                {
                    ExecutionOutcome::Marked { marked: true } => record.admitted = true,
                    ExecutionOutcome::Marked { marked: false } => {
                        results.failed(index, "could not recover expired UserOperation admission");
                        continue;
                    }
                    ExecutionOutcome::Failed { message } => {
                        results.failed(index, &message);
                        continue;
                    }
                    _ => {
                        results.failed(index, "unexpected shell response");
                        continue;
                    }
                }
            }
            AdmissionAction::Execute => {}
        }
        match candidate_from_record(index, routed, &record, policy.pool_width) {
            Ok(candidate) => candidates.push(candidate),
            Err(_reason) => {
                match request(
                    ctx,
                    ExecutionOperation::MarkRejected {
                        hash: routed.user_operation_hash.clone(),
                    },
                )
                .await
                {
                    ExecutionOutcome::Failed { message } => results.failed(index, &message),
                    _ => results.durable(index),
                }
            }
        }
    }
    if candidates.is_empty() {
        return results.finish(NO_OUTCOME_FINISH);
    }

    // Never put two nonces from the same sender into one outer transaction.
    let mut unique_hashes = HashSet::new();
    candidates.retain(|candidate| unique_hashes.insert(candidate.hash));
    let mut senders = HashSet::new();
    candidates.retain(|candidate| {
        senders.insert(candidate.packed.sender) && candidate.result_index < operations.len()
    });
    candidates.truncate(policy.max_bundle_operations);

    match request(ctx, ExecutionOperation::AcquireLaneLease).await {
        ExecutionOutcome::LeaseAcquired { acquired: true } => {}
        _ => {
            record_routed_deferred(
                ctx,
                operations,
                Some(&results),
                "lease",
                "relayer lane is currently owned by another worker",
            )
            .await;
            return results.finish("relayer lane is owned by another worker");
        }
    }

    match execute_with_lane_lease(ctx, &start, chain_assets, candidates, &mut results).await {
        Ok(()) => results.finish(DEFERRED_FINISH),
        Err(reason) => {
            record_routed_deferred(ctx, &start.operations, Some(&results), "execution", &reason)
                .await;
            results.finish(DEFERRED_FINISH)
        }
    }
}

/// The leased execution pipeline. `Err(reason)` is the transient-deferral
/// channel (the old `ExecutorItemError` path), not a failure of the program.
async fn execute_with_lane_lease(
    ctx: &Ctx,
    start: &StartBatch,
    chain_assets: ResolvedChainAssets,
    mut candidates: Vec<Candidate>,
    results: &mut Results,
) -> Result<(), String> {
    let policy = &start.policy;

    match request(ctx, ExecutionOperation::LoadPreparedBundle).await {
        ExecutionOutcome::Intent { intent: None } => {}
        ExecutionOutcome::Intent {
            intent: Some(intent),
        } => {
            let known = match request(
                ctx,
                ExecutionOperation::ResumeBundleIntent {
                    intent: intent.clone(),
                },
            )
            .await
            {
                ExecutionOutcome::Resumed { known_outcome } => known_outcome,
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            };
            if known {
                for candidate in candidates {
                    if intent
                        .user_operation_hashes
                        .iter()
                        .any(|hash| hash.eq_ignore_ascii_case(&candidate.hash_string))
                    {
                        results.durable(candidate.result_index);
                    }
                }
            }
            return Ok(());
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }

    let entry_point = candidates[0].entry_point;
    if candidates
        .iter()
        .any(|candidate| candidate.entry_point != entry_point)
    {
        return Err("one lane batch contains multiple EntryPoints".to_owned());
    }

    let verdicts = match request(
        ctx,
        ExecutionOperation::SimulateIndividually {
            operations: candidates
                .iter()
                .map(|candidate| (candidate.hash, candidate.packed.clone()))
                .collect(),
        },
    )
    .await
    {
        ExecutionOutcome::OperationVerdicts { verdicts } => verdicts,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    if verdicts.len() != candidates.len() {
        return Err("simulation verdicts do not match candidates".to_owned());
    }

    let mut survivors = Vec::new();
    let mut nonce_mismatches = Vec::new();
    for (candidate, verdict) in candidates.drain(..).zip(verdicts) {
        match verdict {
            OperationSimVerdict::Success => survivors.push(candidate),
            OperationSimVerdict::NonceMismatch => nonce_mismatches.push(candidate),
            OperationSimVerdict::Rejected { reason } => {
                if let ExecutionOutcome::Failed { message } = request(
                    ctx,
                    ExecutionOperation::MarkRejected {
                        hash: candidate.hash_string.clone(),
                    },
                )
                .await
                {
                    results.failed(candidate.result_index, &message);
                    continue;
                }
                let _ = request(
                    ctx,
                    ExecutionOperation::NotifyIssue {
                        hash: candidate.hash_string.clone(),
                        stage: "simulation",
                        reason: reason.clone(),
                    },
                )
                .await;
                results.durable(candidate.result_index);
            }
            OperationSimVerdict::Pending { reason } => {
                record_deferred(
                    ctx,
                    &candidate.hash_string,
                    "simulation_deployment",
                    &reason,
                )
                .await;
            }
            OperationSimVerdict::Transient { reason } => {
                record_deferred(ctx, &candidate.hash_string, "simulation", &reason).await;
            }
        }
    }
    if !nonce_mismatches.is_empty() {
        let items = nonce_mismatches
            .iter()
            .map(|candidate| NonceMismatchItem {
                index: candidate.result_index,
                hash: candidate.hash_string.clone(),
                sender: candidate.packed.sender,
                nonce: candidate.packed.packed.nonce,
            })
            .collect();
        match request(ctx, ExecutionOperation::ResolveNonceMismatches { items }).await {
            ExecutionOutcome::MismatchResolutions { resolutions } => {
                for (index, resolution) in resolutions {
                    match resolution {
                        ItemResolution::Durable => results.durable(index),
                        ItemResolution::Failed { reason } => results.failed(index, reason),
                    }
                }
            }
            _ => {
                for candidate in &nonce_mismatches {
                    results.failed(
                        candidate.result_index,
                        "account nonce lookup is temporarily unavailable",
                    );
                }
            }
        }
    }
    if survivors.is_empty() {
        return Ok(());
    }
    ensure_lane_lease(ctx).await?;

    // If a multi-op bundle has a state interaction that does not exist in
    // isolated simulation, fall back to the first op. Later ops stay queued
    // instead of poisoning the whole handleOps transaction.
    let mut bundle_verdict = simulate_bundle(ctx, &survivors).await?;
    if matches!(
        bundle_verdict,
        BundleSimVerdict::Rejected { .. } | BundleSimVerdict::NonceMismatch
    ) && survivors.len() > 1
    {
        survivors.truncate(1);
        bundle_verdict = simulate_bundle(ctx, &survivors).await?;
    }
    let bundle_simulation = match bundle_verdict {
        BundleSimVerdict::Success(simulation) => simulation,
        BundleSimVerdict::Rejected { reason } => {
            record_candidates_deferred(ctx, &survivors, "bundle_simulation", &reason).await;
            return Ok(());
        }
        BundleSimVerdict::NonceMismatch => {
            record_candidates_deferred(
                ctx,
                &survivors,
                "bundle_simulation",
                "final handleOps simulation reported an account nonce mismatch",
            )
            .await;
            return Ok(());
        }
        BundleSimVerdict::Pending { reason } => {
            record_candidates_deferred(ctx, &survivors, "simulation_deployment", &reason).await;
            return Ok(());
        }
        BundleSimVerdict::Transient { reason } => return Err(reason),
    };
    ensure_lane_lease(ctx).await?;

    if policy.is_tempo {
        let resolutions = match request(
            ctx,
            ExecutionOperation::ExecuteTempoBundle {
                entry_point,
                survivors: survivors
                    .iter()
                    .map(|candidate| candidate.result_index)
                    .collect(),
                simulation: bundle_simulation,
            },
        )
        .await
        {
            ExecutionOutcome::TempoOutcome { resolutions } => resolutions,
            ExecutionOutcome::Failed { message } => return Err(message),
            _ => return Err("unexpected shell response".to_owned()),
        };
        for (index, resolution) in resolutions {
            match resolution {
                ItemResolution::Durable => results.durable(index),
                ItemResolution::Failed { reason } => results.failed(index, reason),
            }
        }
        return Ok(());
    }

    let calldata = handle_ops_calldata(
        &survivors
            .iter()
            .map(|candidate| candidate.packed.packed.clone())
            .collect::<Vec<_>>(),
        start_treasury(start),
    );
    let mut context = match request(
        ctx,
        ExecutionOperation::FetchTransactionContext {
            entry_point,
            calldata: calldata.to_vec(),
        },
    )
    .await
    {
        ExecutionOutcome::Context { context } => context,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let allocations = allocate_bundle_gas(
        bundle_simulation.gas_used,
        context.estimated_gas,
        &bundle_simulation.operation_gas_used,
        policy.gas_buffer_bps,
        policy.fixed_gas_buffer,
    )
    .ok_or_else(|| "bundle gas allocation overflow".to_owned())?;

    // --- settlement gate (US3 verdict + US2 hold, composed here) ---
    let call_datas = survivors
        .iter()
        .map(|candidate| candidate.packed.call_data.as_ref())
        .collect::<Vec<&[u8]>>();
    let treasury = start_treasury(start);
    let native_usd_price = if has_stablecoin_payment(treasury, &chain_assets.assets, &call_datas) {
        match request(ctx, ExecutionOperation::FetchMarketPrice).await {
            ExecutionOutcome::Price { price: Some(price) } => Some(price),
            ExecutionOutcome::Price { price: None } | ExecutionOutcome::Failed { .. } => {
                return Err("Binance native USD price request failed".to_owned());
            }
            _ => return Err("unexpected shell response".to_owned()),
        }
    } else {
        None
    };
    let fees = FeeContext {
        quoted_fee_per_gas: context.max_fee_per_gas,
        base_fee_per_gas: context.base_fee_per_gas,
        max_priority_fee_per_gas: context.max_priority_fee_per_gas,
        inclusion_floor_bps: policy.settlement_inclusion_floor_bps,
    };
    let settlement = match decide_settlement(
        treasury,
        &chain_assets.assets,
        &call_datas,
        &allocations,
        native_usd_price,
        &fees,
    )
    .map_err(|error| error.to_string())?
    {
        SettlementDecision::KeepQuote { evaluation } => evaluation,
        SettlementDecision::FloorUnfundable { evaluation, .. } => evaluation,
        SettlementDecision::Reprice {
            fee_per_gas,
            evaluation,
        } => {
            context.max_fee_per_gas = fee_per_gas;
            evaluation
        }
    };

    let mut rejected_any = false;
    for (candidate, evaluation) in survivors.iter().zip(&settlement.operations) {
        let stable_logs_valid = verify_stable_transfer_logs(
            &evaluation.reimbursement,
            candidate.packed.sender,
            treasury,
            &bundle_simulation.logs,
        );
        if evaluation.accepted() && stable_logs_valid {
            continue;
        }
        if evaluation.is_shortfall() && stable_logs_valid {
            // Hold: park in the delayed inbox, judge the budget on the
            // post-increment attempt.
            if let ExecutionOutcome::Deferred { attempt } = request(
                ctx,
                ExecutionOperation::DeferOperation {
                    index: candidate.result_index,
                },
            )
            .await
            {
                match decide_hold(
                    attempt,
                    policy.settlement_hold_max_attempts,
                    evaluation.paid_amount,
                    evaluation.required_amount,
                ) {
                    HoldDecision::Hold { reason } => {
                        record_deferred(
                            ctx,
                            &candidate.hash_string,
                            "in_band_settlement_hold",
                            &reason,
                        )
                        .await;
                        results.durable(candidate.result_index);
                        rejected_any = true;
                        continue;
                    }
                    HoldDecision::RejectBudgetExhausted => {}
                }
            }
        }
        let reason = settlement_rejection_reason(
            evaluation.paid_amount,
            evaluation.required_amount,
            stable_logs_valid,
        );
        if let ExecutionOutcome::Failed { message } = request(
            ctx,
            ExecutionOperation::MarkRejectedWithReason {
                hash: candidate.hash_string.clone(),
                stage: "in_band_settlement",
                reason: reason.clone(),
            },
        )
        .await
        {
            return Err(message);
        }
        results.durable(candidate.result_index);
        rejected_any = true;
        let _ = request(
            ctx,
            ExecutionOperation::NotifyIssue {
                hash: candidate.hash_string.clone(),
                stage: "in_band_settlement",
                reason,
            },
        )
        .await;
    }
    if rejected_any {
        // Reassemble on the next queue delivery so the cost allocation and
        // aggregate estimate never include a rejected payer.
        return Ok(());
    }

    let gas_limit = allocations
        .iter()
        .try_fold(U256::ZERO, |sum, gas| sum.checked_add(*gas))
        .ok_or_else(|| "bundle gas limit overflow".to_owned())?;
    let gas_limit =
        u64::try_from(gas_limit).map_err(|_| "bundle gas limit exceeds uint64".to_owned())?;
    let prefund = U256::from(gas_limit)
        .checked_mul(U256::from(context.max_fee_per_gas))
        .ok_or_else(|| "bundle prefund overflow".to_owned())?;

    // Per-transfer top-up cap: USD-denominated when a market price exists,
    // otherwise the static wei cap (fail-open on price unavailability).
    let top_up_max = match request(ctx, ExecutionOperation::FetchMarketPrice).await {
        ExecutionOutcome::Price { price: Some(price) } => native_amount_for_usd_cap(
            chain_assets.assets.native_decimals,
            price,
            NATIVE_TOP_UP_USD_CAP,
        )
        .unwrap_or(U256::from(policy.top_up_max_wei)),
        _ => U256::from(policy.top_up_max_wei),
    };

    // The current bundle takes precedence over filling the relayer float.
    if context.relayer_balance < prefund {
        match request(
            ctx,
            ExecutionOperation::EnsureRelayerFunded {
                relayer_balance: context.relayer_balance,
                required_prefund: prefund,
                max_fee_per_gas: context.max_fee_per_gas,
                max_priority_fee_per_gas: context.max_priority_fee_per_gas,
                top_up_max,
            },
        )
        .await
        {
            ExecutionOutcome::Funding { ready: true } => {}
            ExecutionOutcome::Funding { ready: false } => {
                record_candidates_deferred(
                    ctx,
                    &survivors,
                    "funding",
                    "waiting for relayer funding transaction confirmation",
                )
                .await;
                return Ok(());
            }
            ExecutionOutcome::Failed { message } => return Err(message),
            _ => return Err("unexpected shell response".to_owned()),
        }
    }
    ensure_lane_lease(ctx).await?;

    let signed = match request(
        ctx,
        ExecutionOperation::SignBundle {
            request: BundleSignRequest {
                nonce: context.nonce,
                gas_limit,
                max_fee_per_gas: context.max_fee_per_gas,
                max_priority_fee_per_gas: context.max_priority_fee_per_gas,
                entry_point,
                calldata: calldata.to_vec(),
            },
        },
    )
    .await
    {
        ExecutionOutcome::Signed { signed } => signed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let intent = PreparedBundleIntent {
        chain_id: start.operations[0].chain_id,
        lane: start.operations[0].lane,
        entry_point: entry_point.to_string(),
        raw_transaction: signed.raw_transaction_hex.clone(),
        transaction_hash: signed.transaction_hash.clone(),
        nonce: signed.nonce,
        user_operation_hashes: survivors
            .iter()
            .map(|candidate| candidate.hash_string.clone())
            .collect(),
    };
    ensure_lane_lease(ctx).await?;
    match request(
        ctx,
        ExecutionOperation::SavePreparedBundle {
            intent: intent.clone(),
        },
    )
    .await
    {
        ExecutionOutcome::Saved { saved: true } => {}
        ExecutionOutcome::Saved { saved: false } => {
            // Raced: another writer holds an intent; resume that one instead.
            let existing = match request(ctx, ExecutionOperation::LoadPreparedBundle).await {
                ExecutionOutcome::Intent {
                    intent: Some(existing),
                } => existing,
                ExecutionOutcome::Intent { intent: None } => {
                    return Err("prepared bundle raced and disappeared".to_owned());
                }
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            };
            match request(
                ctx,
                ExecutionOperation::ResumeBundleIntent { intent: existing },
            )
            .await
            {
                ExecutionOutcome::Resumed { .. } => {}
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            }
            return Ok(());
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    match request(
        ctx,
        ExecutionOperation::BroadcastBundle {
            intent: intent.clone(),
        },
    )
    .await
    {
        ExecutionOutcome::Broadcast { confirmed: true } => {}
        ExecutionOutcome::Broadcast { confirmed: false } => {
            record_candidates_deferred(
                ctx,
                &survivors,
                "broadcast",
                "signed handleOps transaction awaits broadcast confirmation",
            )
            .await;
            return Ok(());
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    let indexed = match request(
        ctx,
        ExecutionOperation::MarkBundleSubmitted {
            transaction_hash: intent.transaction_hash.clone(),
            hashes: intent.user_operation_hashes.clone(),
        },
    )
    .await
    {
        ExecutionOutcome::Indexed { indexed } => indexed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    if indexed != intent.user_operation_hashes.len() {
        return Err("not every signed UserOperation entered submitted state".to_owned());
    }
    for candidate in survivors {
        results.durable(candidate.result_index);
    }
    Ok(())
}

async fn ensure_lane_lease(ctx: &Ctx) -> Result<(), String> {
    match request(ctx, ExecutionOperation::EnsureLaneLease).await {
        ExecutionOutcome::LeaseHeld { held: true } => Ok(()),
        ExecutionOutcome::LeaseHeld { held: false } => Err("executor lease was lost".to_owned()),
        ExecutionOutcome::Failed { message } => Err(message),
        _ => Err("unexpected shell response".to_owned()),
    }
}

async fn simulate_bundle(ctx: &Ctx, survivors: &[Candidate]) -> Result<BundleSimVerdict, String> {
    match request(
        ctx,
        ExecutionOperation::SimulateBundle {
            operations: survivors
                .iter()
                .map(|candidate| (candidate.hash, candidate.packed.clone()))
                .collect(),
        },
    )
    .await
    {
        ExecutionOutcome::BundleVerdict { verdict } => Ok(verdict),
        ExecutionOutcome::Failed { message } => Err(message),
        _ => Err("unexpected shell response".to_owned()),
    }
}

fn start_treasury(start: &StartBatch) -> Address {
    start.policy.treasury
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use alloy::primitives::{Address, U256, address};
    use crux_core::{Core, Request};
    use serde_json::Value;

    use super::{
        BundleSimVerdict, BundleSimulationData, ExecutionApp, ExecutionEvent, ExecutionOperation,
        ExecutionOutcome, ExecutionPolicy, ItemResolution, OperationSimVerdict,
        ResolvedChainAssets, SignedBundle, StartBatch, TransactionContext,
    };
    use crate::{
        abi::{PackedOperation, user_operation_hash},
        settlement::ChainAssetConfig,
        task::{
            RoutedUserOperation, StoredUserOperation, UserOperation, UserOperationStatus,
            UserOperationV0_7,
        },
        vault::relayer_index_for_sender,
    };

    const TREASURY: Address = address!("1111111111111111111111111111111111111111");
    const ENTRY_POINT: &str = "0x2222222222222222222222222222222222222222";
    const SENDER: &str = "0x00000000000000000000000000000000000000aa";
    const CHAIN_ID: u64 = 42_161;

    /// Scripts the shell side of the conversation, asserting the strictly
    /// sequential invariant: at most one operation is ever in flight.
    struct Driver {
        core: Core<ExecutionApp>,
        queue: VecDeque<Request<ExecutionOperation>>,
    }

    impl Driver {
        fn start(batch: StartBatch) -> Self {
            let core: Core<ExecutionApp> = Core::new();
            let effects = core.process_event(ExecutionEvent::Start(Box::new(batch)));
            let mut driver = Self {
                core,
                queue: VecDeque::new(),
            };
            driver.absorb(effects);
            driver
        }

        fn absorb(&mut self, effects: Vec<super::ExecutionEffect>) {
            for effect in effects {
                let super::ExecutionEffect::Work(request) = effect;
                self.queue.push_back(request);
            }
            assert!(
                self.queue.len() <= 1,
                "the batch program must be strictly sequential"
            );
        }

        fn step(&mut self, expected: ExecutionOperation, outcome: ExecutionOutcome) {
            let mut request = self
                .queue
                .pop_front()
                .unwrap_or_else(|| panic!("no operation in flight; expected {expected:?}"));
            assert_eq!(request.operation, expected);
            let effects = self
                .core
                .resolve(&mut request, outcome)
                .expect("resolve must succeed");
            self.absorb(effects);
        }

        fn assert_settled(&self, expected: &[ItemResolution]) {
            assert!(
                self.queue.is_empty(),
                "no operation may remain in flight, found {:?}",
                self.queue.front().map(|request| &request.operation)
            );
            assert_eq!(
                self.core.view().outcome.as_deref(),
                Some(expected),
                "batch must settle with the expected resolutions"
            );
        }
    }

    fn policy() -> ExecutionPolicy {
        ExecutionPolicy {
            pool_width: 10,
            max_bundle_operations: 4,
            gas_buffer_bps: 0,
            fixed_gas_buffer: 0,
            settlement_inclusion_floor_bps: 15_000,
            settlement_hold_max_attempts: 12,
            top_up_max_wei: 1_000_000,
            is_tempo: false,
            treasury: TREASURY,
        }
    }

    fn assets() -> ResolvedChainAssets {
        ResolvedChainAssets {
            assets: ChainAssetConfig {
                native_decimals: 5,
                settlement_markup_bps: 14_000,
                stablecoins: BTreeMap::new(),
            },
            native_symbol: "ETH".into(),
        }
    }

    // --- calldata fixture: Safe executeUserOp -> MultiSend(delegatecall) ---

    fn word_u128(value: u128) -> Vec<u8> {
        let mut word = vec![0; 16];
        word.extend(value.to_be_bytes());
        word
    }

    fn native_payment_calldata(amount: u128) -> Vec<u8> {
        let mut packed = Vec::new();
        packed.push(0); // CALL
        packed.extend(TREASURY.as_slice());
        packed.extend(word_u128(amount));
        packed.extend(word_u128(0));

        let mut multisend = vec![0x8d, 0x80, 0xff, 0x0a];
        multisend.extend(word_u128(32));
        multisend.extend(word_u128(packed.len() as u128));
        multisend.extend(packed);
        let padding = (32 - multisend.len() % 32) % 32;
        multisend.resize(multisend.len() + padding, 0);

        let mut call_data = vec![0x7b, 0xb3, 0x74, 0x28];
        let mut trusted = vec![0u8; 12];
        trusted.extend(address!("38869bf66a61cf6bdb996a6ae40d5853fd43b526").as_slice());
        call_data.extend(trusted);
        call_data.extend(word_u128(0));
        call_data.extend(word_u128(128));
        call_data.extend(word_u128(1));
        call_data.extend(word_u128(multisend.len() as u128));
        call_data.extend(multisend);
        call_data
    }

    fn user_op(paid: u128) -> UserOperationV0_7 {
        UserOperationV0_7 {
            sender: SENDER.into(),
            nonce: "0x0".into(),
            factory: None,
            factory_data: None,
            call_data: format!("0x{}", hex::encode(native_payment_calldata(paid))),
            call_gas_limit: "0x64".into(),
            verification_gas_limit: "0x64".into(),
            pre_verification_gas: "0x0".into(),
            max_fee_per_gas: "0x0".into(),
            max_priority_fee_per_gas: "0x0".into(),
            paymaster: None,
            paymaster_verification_gas_limit: None,
            paymaster_post_op_gas_limit: None,
            paymaster_data: None,
            signature: "0x".into(),
            eip7702_auth: None,
            fee_token: None,
        }
    }

    struct Fixture {
        routed: RoutedUserOperation,
        record: StoredUserOperation,
        hash_string: String,
        packed: PackedOperation,
    }

    fn fixture(paid: u128) -> Fixture {
        let operation = UserOperation::V0_7(Box::new(user_op(paid)));
        let packed = PackedOperation::try_from(&operation).expect("fixture packs");
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let hash = user_operation_hash(&packed, entry_point, CHAIN_ID);
        let hash_string = hash.to_string().to_ascii_lowercase();
        let lane = relayer_index_for_sender(SENDER, 10) as u8;
        let value: Value = serde_json::to_value(&operation).unwrap();
        let routed = RoutedUserOperation {
            schema_version: 1,
            user_operation_hash: hash_string.clone(),
            chain_id: CHAIN_ID,
            entry_point: ENTRY_POINT.into(),
            user_operation: value,
            sender: SENDER.into(),
            lane,
            stream: "chain-42161".into(),
            partition_id: 1,
            offset: 7,
        };
        let record = StoredUserOperation {
            status: UserOperationStatus::Queued,
            transaction_hash: None,
            chain_id: CHAIN_ID,
            chain_id_text: CHAIN_ID.to_string(),
            entry_point: ENTRY_POINT.into(),
            user_operation: operation,
            admitted: true,
            next_receipt_check_at_ms: 0,
            block_hash: None,
            block_number: None,
            receipt: None,
            event: None,
            last_executor_stage: None,
            last_executor_error: None,
            last_executor_attempt_at_ms: None,
        };
        Fixture {
            routed,
            record,
            hash_string,
            packed,
        }
    }

    fn start(operations: Vec<RoutedUserOperation>) -> StartBatch {
        StartBatch {
            operations,
            policy: policy(),
            lease_token: "lane-token-1".into(),
        }
    }

    fn sim_data() -> BundleSimulationData {
        BundleSimulationData {
            gas_used: U256::from(100u64),
            operation_gas_used: vec![U256::from(100u64)],
            logs: Vec::new(),
        }
    }

    fn context() -> TransactionContext {
        TransactionContext {
            estimated_gas: U256::from(100u64),
            base_fee_per_gas: 1,
            max_fee_per_gas: 2,
            max_priority_fee_per_gas: 0,
            nonce: 7,
            relayer_balance: U256::from(10_000u64),
        }
    }

    #[test]
    fn a_fully_funded_batch_walks_the_pipeline_to_submission() {
        // allocation 100 gas at the quoted fee 2 → cost 200; markup 1.4 →
        // required 280; the calldata pays exactly 280.
        let fixture = fixture(280);
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));

        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver.step(
            ExecutionOperation::SimulateIndividually {
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata: calldata.clone(),
            },
            ExecutionOutcome::Context { context: context() },
        );
        driver.step(
            ExecutionOperation::FetchMarketPrice,
            ExecutionOutcome::Price { price: None },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SignBundle {
                request: super::BundleSignRequest {
                    nonce: 7,
                    gas_limit: 100,
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 0,
                    entry_point,
                    calldata,
                },
            },
            ExecutionOutcome::Signed {
                signed: SignedBundle {
                    raw_transaction_hex: "0x02aabb".into(),
                    transaction_hash: "0xdeadbeef".into(),
                    nonce: 7,
                },
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let intent = crate::task::PreparedBundleIntent {
            chain_id: CHAIN_ID,
            lane: fixture.routed.lane,
            entry_point: entry_point.to_string(),
            raw_transaction: "0x02aabb".into(),
            transaction_hash: "0xdeadbeef".into(),
            nonce: 7,
            user_operation_hashes: vec![fixture.hash_string.clone()],
        };
        driver.step(
            ExecutionOperation::SavePreparedBundle {
                intent: intent.clone(),
            },
            ExecutionOutcome::Saved { saved: true },
        );
        driver.step(
            ExecutionOperation::BroadcastBundle {
                intent: intent.clone(),
            },
            ExecutionOutcome::Broadcast { confirmed: true },
        );
        driver.step(
            ExecutionOperation::MarkBundleSubmitted {
                transaction_hash: "0xdeadbeef".into(),
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Indexed { indexed: 1 },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn a_store_outage_fails_every_item_without_touching_the_lane() {
        let fixture = fixture(280);
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Failed {
                message: "Redis command timed out".into(),
            },
        );
        driver.assert_settled(&[ItemResolution::Failed {
            reason: "Redis command timed out".into(),
        }]);
    }

    #[test]
    fn a_lost_lane_lease_defers_with_a_diagnostic_for_every_unresolved_item() {
        let fixture = fixture(280);
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: false },
        );
        // The "lease" stage is an expected hand-off: diagnostic, no Telegram.
        driver.step(
            ExecutionOperation::RecordDeferred {
                hash: fixture.hash_string.clone(),
                stage: "lease",
                reason: "relayer lane is currently owned by another worker".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Failed {
            reason: "relayer lane is owned by another worker".into(),
        }]);
    }

    #[test]
    fn an_already_durable_record_advances_without_execution() {
        let mut fixture = fixture(280);
        fixture.record.status = UserOperationStatus::Included;
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn an_unaffordable_market_holds_the_operation_and_notifies() {
        // Paying 1 against a requirement of 280 is a shortfall: parked in the
        // delayed inbox, attempt 3 of 12 stays within budget.
        let fixture = fixture(1);
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver.step(
            ExecutionOperation::SimulateIndividually {
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata,
            },
            ExecutionOutcome::Context { context: context() },
        );
        driver.step(
            ExecutionOperation::DeferOperation { index: 0 },
            ExecutionOutcome::Deferred { attempt: 3 },
        );
        driver.step(
            ExecutionOperation::RecordDeferred {
                hash: fixture.hash_string.clone(),
                stage: "in_band_settlement_hold",
                reason: "waiting for network fees to fit the signed in-band reimbursement: \
                         paid=1, required=280, shortfall=279, attempt=3/12"
                    .into(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::NotifyIssue {
                hash: fixture.hash_string.clone(),
                stage: "in_band_settlement_hold",
                reason: "waiting for network fees to fit the signed in-band reimbursement: \
                         paid=1, required=280, shortfall=279, attempt=3/12"
                    .into(),
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }
}
