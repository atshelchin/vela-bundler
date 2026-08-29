//! LaneDO — one Durable Object per (chain, lane): the execution unit
//! (data-model §1). Drives the core's `ExecutionApp` per delivered batch; the
//! DO's single-threaded input gate IS the lane lease, so `AcquireLaneLease`/
//! `EnsureLaneLease` are answered structurally and the docker shell's
//! heartbeat/interrupt machinery has no counterpart here (declared in
//! contracts/platform-bindings.md).
//!
//! Storage layout: `intent` → `PreparedBundleIntent`; `bundle:{txhash}` →
//! member-hash index; `seen:{txhash}` → broadcast-seen timestamp;
//! `delayed:{hash}` → parked operation (payload + attempts + due). The single
//! alarm serves the delayed inbox (earliest due; US3 adds reconcile packing).

use vela_relay_core::execution::{
    self as core_execution, ExecutionOperation as Op, ExecutionOutcome as Out,
};
use vela_relay_core::task::{PreparedBundleIntent, RoutedUserOperation, truncate_diagnostic};
use worker::{
    Date, DurableObject, Env, Request, Response, Result, State, durable_object, wasm_bindgen,
};

use crate::{
    config::CfConfig,
    proto::{ItemResolutionWire, LaneCommand, LaneReply, RecordCommand, RecordReply},
};

const INTENT_KEY: &str = "intent";
const BROADCAST_RETRY_INTERVAL_MS: u64 = 30_000;

#[derive(serde::Serialize, serde::Deserialize)]
struct DelayedEntry {
    routed: RoutedUserOperation,
    attempts: u32,
    due_ms: u64,
    created_ms: u64,
}

#[durable_object]
pub struct LaneDo {
    state: State,
    env: Env,
}

impl DurableObject for LaneDo {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let command: LaneCommand = req.json().await?;
        match command {
            LaneCommand::ExecuteBatch { operations } => {
                let resolutions = self.execute_batch(operations).await;
                Response::from_json(&LaneReply::Resolutions {
                    resolutions: resolutions
                        .into_iter()
                        .map(ItemResolutionWire::from)
                        .collect(),
                })
            }
        }
    }

    async fn alarm(&self) -> Result<Response> {
        // US3 (T019): re-drive due delayed operations through the same batch
        // entry. Until then the alarm only reschedules for the earliest due.
        self.schedule_delayed_alarm().await?;
        Response::empty()
    }
}

impl LaneDo {
    /// The driver loop — the docker `handle_lane_batch` without the lease
    /// machinery (the DO is the lease).
    async fn execute_batch(
        &self,
        operations: Vec<RoutedUserOperation>,
    ) -> Vec<core_execution::ItemResolution> {
        use core_execution::ItemResolution;
        let failure = |reason: &str, len: usize| -> Vec<ItemResolution> {
            (0..len)
                .map(|_| ItemResolution::Failed {
                    reason: reason.to_owned(),
                })
                .collect()
        };

        if operations.is_empty() {
            return Vec::new();
        }
        let count = operations.len();
        let chain_id = operations[0].chain_id;
        let lane = operations[0].lane;
        let config = match CfConfig::from_env(&self.env) {
            Ok(config) => config,
            Err(error) => {
                worker::console_error!("configuration error: {error}");
                return failure("executor configuration is invalid", count);
            }
        };
        let policy = match config.execution_policy(chain_id, lane) {
            Ok(policy) => policy,
            Err(error) => {
                worker::console_error!("execution policy error: {error}");
                return failure("executor configuration is invalid", count);
            }
        };

        let core: crux_core::Core<core_execution::ExecutionApp> = crux_core::Core::new();
        let mut effects: std::collections::VecDeque<core_execution::ExecutionEffect> = core
            .process_event(core_execution::ExecutionEvent::Start(Box::new(
                core_execution::StartBatch {
                    operations: operations.clone(),
                    policy,
                    // The DO's identity is the lease; the token is bookkeeping.
                    lease_token: format!("lane:{chain_id}:{lane}"),
                },
            )))
            .into_iter()
            .collect();
        while let Some(core_execution::ExecutionEffect::Work(mut request)) = effects.pop_front() {
            let outcome = self
                .execute(&config, chain_id, lane, &operations, &request.operation)
                .await;
            match core.resolve(&mut request, outcome) {
                Ok(next) => effects.extend(next),
                Err(_) => return failure("could not resolve execution effect", count),
            }
        }

        match core.view().outcome {
            Some(resolutions) => resolutions,
            None => failure("lane batch never settled", count),
        }
    }

    async fn execute(
        &self,
        config: &CfConfig,
        chain_id: u64,
        lane: u8,
        batch: &[RoutedUserOperation],
        operation: &Op,
    ) -> Out {
        match operation {
            // --- chain support & assets ---
            Op::CheckChainSupported => Out::Supported {
                supported: self.chain_supported(config, chain_id).await,
            },
            Op::LoadChainAssets => self.load_chain_assets(config, chain_id).await,
            // --- record store (RecordDO subrequests) ---
            Op::LoadRecords { hashes } => {
                let mut records = Vec::with_capacity(hashes.len());
                for hash in hashes {
                    match self.record(chain_id, hash, &RecordCommand::Get).await {
                        Ok(RecordReply::Record { record }) => {
                            records.push(record.map(|record| *record));
                        }
                        _ => {
                            return Out::Failed {
                                message: "could not read UserOperation records".into(),
                            };
                        }
                    }
                }
                Out::Records { records }
            }
            Op::ReloadRecord { hash } => {
                match self.record(chain_id, hash, &RecordCommand::Get).await {
                    Ok(RecordReply::Record { record }) => Out::Record {
                        record: record.map(|record| *record),
                    },
                    _ => Out::Failed {
                        message: "could not read UserOperation record".into(),
                    },
                }
            }
            Op::RestoreQueued { index: _, queued } => {
                match self
                    .record(
                        chain_id,
                        &queued.user_operation_hash.clone(),
                        &RecordCommand::RestoreQueued {
                            operation: queued.clone(),
                        },
                    )
                    .await
                {
                    Ok(RecordReply::Created { .. }) => Out::Done,
                    _ => Out::Failed {
                        message: "could not restore queued UserOperation".into(),
                    },
                }
            }
            Op::MarkAdmitted { hash } => {
                match self
                    .record(chain_id, hash, &RecordCommand::MarkAdmitted)
                    .await
                {
                    Ok(RecordReply::Marked { marked }) => Out::Marked { marked },
                    _ => Out::Failed {
                        message: "could not recover UserOperation admission".into(),
                    },
                }
            }
            Op::MarkRejected { hash, cause } => {
                self.log_rejection_cause(chain_id, hash, cause);
                let patch = serde_json::json!({ "status": "rejected", "admitted": true });
                match self
                    .record(chain_id, hash, &RecordCommand::Patch { patch })
                    .await
                {
                    Ok(RecordReply::Patched { .. }) => Out::Done,
                    _ => {
                        if let core_execution::RejectionCause::StaleNonce { .. } = cause {
                            worker::console_warn!(
                                "could not persist stale nonce rejection: {hash}"
                            );
                        }
                        Out::Failed {
                            message: "could not persist UserOperation rejection".into(),
                        }
                    }
                }
            }
            Op::MarkRejectedWithReason {
                hash,
                stage,
                reason,
            } => {
                let patch = serde_json::json!({
                    "status": "rejected",
                    "admitted": true,
                    "lastExecutorStage": truncate_diagnostic(stage, 64),
                    "lastExecutorError": truncate_diagnostic(reason, 512),
                    "lastExecutorAttemptAtMs": Date::now().as_millis(),
                });
                match self
                    .record(chain_id, hash, &RecordCommand::Patch { patch })
                    .await
                {
                    Ok(RecordReply::Patched { .. }) => Out::Done,
                    _ => Out::Failed {
                        message: "could not persist UserOperation rejection".into(),
                    },
                }
            }
            Op::RecordDeferred {
                hash,
                stage,
                reason,
            } => {
                let patch = serde_json::json!({
                    "lastExecutorStage": truncate_diagnostic(stage, 64),
                    "lastExecutorError": truncate_diagnostic(reason, 512),
                    "lastExecutorAttemptAtMs": Date::now().as_millis(),
                });
                if self
                    .record(chain_id, hash, &RecordCommand::Patch { patch })
                    .await
                    .is_err()
                {
                    worker::console_warn!(
                        "could not persist executor retry diagnostic: {hash} stage={stage}"
                    );
                }
                Out::Done
            }
            Op::NotifyIssue {
                hash,
                stage,
                reason,
            } => {
                // Telegram delivery lands with T023; the gating decision is
                // the core's and is preserved in the log line meanwhile.
                worker::console_error!(
                    "executor issue: chain_id={chain_id} stage={stage} user_operation_hash={hash} reason={reason}"
                );
                Out::Done
            }
            Op::EmitDiagnostic { diagnostic } => {
                self.emit_diagnostic(chain_id, lane, diagnostic);
                Out::Done
            }
            Op::DeadLetterRouted { index, reason } => self.dead_letter(batch, *index, reason).await,
            // --- the lease IS the DO ---
            Op::AcquireLaneLease => Out::LeaseAcquired { acquired: true },
            Op::EnsureLaneLease => Out::LeaseHeld { held: true },
            // --- prepared intent ---
            Op::LoadPreparedBundle => Out::Intent {
                intent: self.intent().await,
            },
            Op::SavePreparedBundle { intent } => {
                if self.intent().await.is_some() {
                    Out::Saved { saved: false }
                } else {
                    match self.state.storage().put(INTENT_KEY, intent).await {
                        Ok(()) => Out::Saved { saved: true },
                        Err(_) => Out::Failed {
                            message: "could not persist prepared bundle intent".into(),
                        },
                    }
                }
            }
            Op::ClearStaleIntent { intent, reason } => {
                worker::console_warn!(
                    "clearing stale prepared bundle intent: chain_id={chain_id} lane={lane} transaction_hash={} reason={reason}",
                    intent.transaction_hash
                );
                match self.clear_intent_if_matches(&intent.transaction_hash).await {
                    Ok(()) => Out::Done,
                    Err(_) => Out::Failed {
                        message: "could not clear stale prepared bundle intent".into(),
                    },
                }
            }
            // --- broadcast-seen cache (loss harmless) ---
            Op::CheckBroadcastSeen { transaction_hash } => Out::Seen {
                seen: self.broadcast_seen(transaction_hash).await,
            },
            Op::RememberBroadcast { transaction_hash } => {
                let _ = self
                    .state
                    .storage()
                    .put(&format!("seen:{transaction_hash}"), Date::now().as_millis())
                    .await;
                Out::Done
            }
            Op::ForgetBroadcast { transaction_hash } => {
                let _ = self
                    .state
                    .storage()
                    .delete(&format!("seen:{transaction_hash}"))
                    .await;
                Out::Done
            }
            // --- delayed inbox (the hold ladder's durable parking) ---
            Op::DeferOperation { index, cause } => {
                self.defer_operation(chain_id, batch, *index, cause).await
            }
            // --- bundle submitted (per-member RecordDO + lane index) ---
            Op::MarkBundleSubmitted { intent, gas_limit } => {
                self.mark_bundle_submitted(chain_id, lane, intent, *gas_limit)
                    .await
            }
            // --- chain IO: lands with T015; unreachable while
            // `CheckChainSupported` gates execution off (see chain_supported) ---
            Op::ResumeBundleIntent { .. }
            | Op::SimulateIndividually { .. }
            | Op::FetchAccountNonces { .. }
            | Op::SimulateBundle { .. }
            | Op::FetchTransactionContext { .. }
            | Op::FetchMarketPrice
            | Op::SignBundle { .. }
            | Op::BroadcastRaw { .. }
            | Op::ProbeTransactionKnown { .. }
            | Op::ProbeStaleNonce { .. }
            | Op::RecordUnprovenBroadcast { .. }
            | Op::FetchTempoContext
            | Op::SignTempoBundle { .. }
            | Op::AcquireTreasuryLease
            | Op::EnsureTreasuryLease
            | Op::ReleaseTreasuryLease
            | Op::LoadPreparedFunding
            | Op::SaveFundingIntent { .. }
            | Op::ClearFundingIntent { .. }
            | Op::FetchTreasuryContext
            | Op::FetchTempoTreasuryContext { .. }
            | Op::SignTreasuryTransfer { .. }
            | Op::SignTreasuryPathUsd { .. }
            | Op::AcquireReceiptProbe { .. }
            | Op::FetchTransactionReceipt { .. }
            | Op::RecordTreasuryShortfall { .. }
            | Op::RecordTempoTreasuryShortfall { .. }
            | Op::RecordPartialTopUp { .. }
            | Op::RecordFundingSubmitted { .. }
            | Op::RecordUnprovenFunding { .. }
            | Op::NoteFundingReceipt { .. } => Out::Failed {
                message: "chain transport lands with T015".into(),
            },
        }
    }

    /// Dynamic-chain gate (research.md R10): the optional allowlist first,
    /// then trusted-RPC availability. Until T015 wires the trusted transport,
    /// execution stays gated off and every batch defers with the frozen
    /// "chain has no trusted executor RPC" reason (safe staging).
    async fn chain_supported(&self, config: &CfConfig, chain_id: u64) -> bool {
        if !config.execution_chains.is_empty() && !config.execution_chains.contains(&chain_id) {
            return false;
        }
        false // T015: trusted-RPC resolution
    }

    async fn load_chain_assets(&self, config: &CfConfig, chain_id: u64) -> Out {
        if vela_relay_core::tempo::is_tempo_chain(chain_id) {
            return Out::Assets {
                resolved: tempo_chain_assets(config),
            };
        }
        match crate::arms::market::payment_metadata(&self.env, chain_id).await {
            Ok(metadata) => {
                let Some(native) = metadata.native_currency else {
                    return Out::AssetsUnavailable {
                        reason: "could not load payment assets from chain directory".into(),
                    };
                };
                let mut stablecoins = std::collections::BTreeMap::new();
                for stable in metadata.stables {
                    let Ok(address) = stable.contract.parse::<alloy::primitives::Address>() else {
                        continue;
                    };
                    // T015 resolves missing decimals via the trusted RPC batch
                    // exactly as the docker shell; until execution is enabled
                    // this path is unreachable.
                    let Some(decimals) = stable.decimals.filter(|decimals| *decimals <= 38) else {
                        continue;
                    };
                    stablecoins.insert(
                        address,
                        vela_relay_core::settlement::StablecoinConfig {
                            symbol: stable.symbol.clone(),
                            decimals,
                        },
                    );
                }
                Out::Assets {
                    resolved: core_execution::ResolvedChainAssets {
                        assets: vela_relay_core::settlement::ChainAssetConfig {
                            native_decimals: native.decimals,
                            settlement_markup_bps: config.settlement_markup_bps,
                            stablecoins,
                        },
                        native_symbol: native.symbol,
                    },
                }
            }
            Err(_) => Out::AssetsUnavailable {
                reason: "could not load payment assets from chain directory".into(),
            },
        }
    }

    async fn record(
        &self,
        chain_id: u64,
        hash: &str,
        command: &RecordCommand,
    ) -> std::result::Result<RecordReply, ()> {
        crate::admission::record_command(&self.env, chain_id, hash, command).await
    }

    async fn intent(&self) -> Option<PreparedBundleIntent> {
        self.state.storage().get(INTENT_KEY).await.ok().flatten()
    }

    async fn clear_intent_if_matches(&self, transaction_hash: &str) -> Result<()> {
        if let Some(intent) = self.intent().await
            && intent
                .transaction_hash
                .eq_ignore_ascii_case(transaction_hash)
        {
            self.state.storage().delete(INTENT_KEY).await?;
        }
        Ok(())
    }

    async fn broadcast_seen(&self, transaction_hash: &str) -> bool {
        let seen_at: Option<u64> = self
            .state
            .storage()
            .get(&format!("seen:{transaction_hash}"))
            .await
            .ok()
            .flatten();
        seen_at.is_some_and(|seen_at| {
            Date::now().as_millis().saturating_sub(seen_at) < BROADCAST_RETRY_INTERVAL_MS
        })
    }

    async fn defer_operation(
        &self,
        chain_id: u64,
        batch: &[RoutedUserOperation],
        index: usize,
        cause: &core_execution::DeferCause,
    ) -> Out {
        let Some(routed) = batch.get(index) else {
            return Out::Failed {
                message: "could not persist deferred UserOperation".into(),
            };
        };
        let key = format!("delayed:{}", routed.user_operation_hash);
        let existing: Option<DelayedEntry> = self.state.storage().get(&key).await.ok().flatten();
        let attempts = existing.map(|entry| entry.attempts).unwrap_or(0) + 1;
        let now = Date::now().as_millis();
        let due_ms = now + vela_relay_core::hold::retry_delay_ms(attempts);
        let entry = DelayedEntry {
            routed: routed.clone(),
            attempts,
            due_ms,
            created_ms: now,
        };
        if self.state.storage().put(&key, &entry).await.is_err() {
            self.log_defer_failure(chain_id, &routed.user_operation_hash, cause);
            return Out::Failed {
                message: "could not persist deferred UserOperation".into(),
            };
        }
        let mut index_list: Vec<String> = self
            .state
            .storage()
            .get("delayed_index")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        if !index_list.contains(&routed.user_operation_hash) {
            index_list.push(routed.user_operation_hash.clone());
            let _ = self.state.storage().put("delayed_index", &index_list).await;
        }
        let _ = self.schedule_delayed_alarm().await;
        self.log_defer_success(chain_id, &routed.user_operation_hash, attempts, cause);
        Out::Deferred { attempt: attempts }
    }

    async fn mark_bundle_submitted(
        &self,
        chain_id: u64,
        _lane: u8,
        intent: &PreparedBundleIntent,
        _gas_limit: u64,
    ) -> Out {
        let mut indexed = 0usize;
        let mut members: Vec<String> = Vec::new();
        for hash in &intent.user_operation_hashes {
            match self
                .record(
                    chain_id,
                    hash,
                    &RecordCommand::MarkBundleMemberSubmitted {
                        bundle_chain_id: intent.chain_id,
                        transaction_hash: intent.transaction_hash.clone(),
                    },
                )
                .await
            {
                Ok(RecordReply::Indexed { indexed: true }) => {
                    indexed += 1;
                    members.push(hash.clone());
                }
                Ok(_) => {}
                Err(()) => {
                    return Out::Failed {
                        message: "could not mark bundle members submitted".into(),
                    };
                }
            }
        }
        let _ = self
            .state
            .storage()
            .put(&format!("bundle:{}", intent.transaction_hash), &members)
            .await;
        Out::Indexed { indexed }
    }

    async fn dead_letter(&self, batch: &[RoutedUserOperation], index: usize, reason: &str) -> Out {
        let Some(routed) = batch.get(index) else {
            return Out::Failed {
                message: "could not dead-letter queue message".into(),
            };
        };
        let Ok(queue) = self.env.queue("DLQ_QUEUE") else {
            return Out::Failed {
                message: "could not dead-letter queue message".into(),
            };
        };
        let payload = serde_json::json!({
            "reason": reason,
            "chainId": routed.chain_id,
            "userOperationHash": routed.user_operation_hash,
            "envelope": routed.user_operation,
        });
        match queue.send(&payload).await {
            Ok(()) => {
                worker::console_error!(
                    "dead-lettered queue message: chain_id={} user_operation_hash={} reason={reason}",
                    routed.chain_id,
                    routed.user_operation_hash
                );
                Out::Persisted { persisted: true }
            }
            Err(_) => Out::Failed {
                message: "could not dead-letter queue message".into(),
            },
        }
    }

    async fn schedule_delayed_alarm(&self) -> Result<()> {
        let index_list: Vec<String> = self
            .state
            .storage()
            .get("delayed_index")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut earliest: Option<u64> = None;
        for hash in &index_list {
            let entry: Option<DelayedEntry> = self
                .state
                .storage()
                .get(&format!("delayed:{hash}"))
                .await
                .ok()
                .flatten();
            if let Some(entry) = entry {
                earliest = Some(earliest.map_or(entry.due_ms, |due| due.min(entry.due_ms)));
            }
        }
        if let Some(due_ms) = earliest {
            let now = Date::now().as_millis();
            let delay = due_ms.saturating_sub(now).max(1);
            self.state
                .storage()
                .set_alarm(std::time::Duration::from_millis(delay))
                .await?;
        }
        Ok(())
    }

    fn log_rejection_cause(
        &self,
        chain_id: u64,
        hash: &str,
        cause: &core_execution::RejectionCause,
    ) {
        match cause {
            core_execution::RejectionCause::InvalidQueuedPayload { reason } => {
                worker::console_warn!(
                    "rejected invalid queued UserOperation: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
                );
            }
            core_execution::RejectionCause::SimulationRejected { reason } => {
                worker::console_warn!(
                    "single-operation simulation rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
                );
            }
            core_execution::RejectionCause::StaleNonce {
                user_nonce,
                onchain_nonce,
            } => {
                worker::console_warn!(
                    "stale account nonce rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} user_nonce={user_nonce} onchain_nonce={onchain_nonce}"
                );
            }
            core_execution::RejectionCause::UnsupportedTempoFeeToken { fee_token } => {
                worker::console_warn!(
                    "Tempo UserOperation requested an unsupported fee token: chain_id={chain_id} user_operation_hash={hash} fee_token={fee_token:?}"
                );
            }
        }
    }

    fn log_defer_success(
        &self,
        chain_id: u64,
        hash: &str,
        attempt: u32,
        cause: &core_execution::DeferCause,
    ) {
        match cause {
            core_execution::DeferCause::AffordableMarketHold => {
                worker::console_log!(
                    "holding UserOperation until the market fits its signed reimbursement: chain_id={chain_id} user_operation_hash={hash} attempt={attempt}"
                );
            }
            core_execution::DeferCause::FutureNonce {
                user_nonce,
                onchain_nonce,
            } => {
                worker::console_log!(
                    "future account nonce moved to durable delayed inbox: chain_id={chain_id} user_operation_hash={hash} user_nonce={user_nonce} onchain_nonce={onchain_nonce} attempt={attempt}"
                );
            }
        }
    }

    fn log_defer_failure(&self, chain_id: u64, hash: &str, cause: &core_execution::DeferCause) {
        match cause {
            core_execution::DeferCause::AffordableMarketHold => {
                worker::console_warn!(
                    "could not hold UserOperation for a cheaper market: chain_id={chain_id} user_operation_hash={hash}"
                );
            }
            core_execution::DeferCause::FutureNonce { .. } => {
                worker::console_warn!(
                    "could not persist future nonce in delayed inbox: chain_id={chain_id} user_operation_hash={hash}"
                );
            }
        }
    }

    fn emit_diagnostic(
        &self,
        chain_id: u64,
        lane: u8,
        diagnostic: &core_execution::ExecutionDiagnostic,
    ) {
        use core_execution::ExecutionDiagnostic as Diagnostic;
        match diagnostic {
            Diagnostic::SimulationDeploymentWait { hash, reason } => worker::console_log!(
                "single-operation simulation is waiting for automatic contract deployment: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
            ),
            Diagnostic::SimulationUnavailable { hash, reason } => worker::console_warn!(
                "single-operation simulation unavailable: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
            ),
            Diagnostic::BundleSimulationRejected { reason } => worker::console_warn!(
                "final handleOps simulation rejected bundle: chain_id={chain_id} lane={lane} reason={reason}"
            ),
            Diagnostic::BundleSimulationNonceMismatch => worker::console_warn!(
                "final handleOps simulation reported an account nonce mismatch: chain_id={chain_id} lane={lane}"
            ),
            Diagnostic::BundleSimulationDeploymentWait { reason } => worker::console_log!(
                "final handleOps simulation is waiting for automatic contract deployment: chain_id={chain_id} lane={lane} reason={reason}"
            ),
            Diagnostic::FloorUnfundable {
                quoted_fee,
                affordable,
                floor,
                base_fee,
            } => worker::console_log!(
                "in-band reimbursement cannot fund an includable outer fee: chain_id={chain_id} quoted_fee={quoted_fee} affordable={affordable} floor={floor} base_fee={base_fee}"
            ),
            Diagnostic::Repriced {
                quoted_fee,
                repriced_fee,
                base_fee,
                tip,
            } => worker::console_log!(
                "repriced the outer transaction to the signed in-band budget: chain_id={chain_id} quoted_fee={quoted_fee} repriced_fee={repriced_fee} base_fee={base_fee} tip={tip}"
            ),
            Diagnostic::HoldBudgetExhausted {
                hash,
                attempt,
                paid,
                required,
            } => worker::console_warn!(
                "in-band reimbursement stayed unaffordable for the whole hold budget: chain_id={chain_id} user_operation_hash={hash} attempt={attempt} paid={paid} required={required}"
            ),
            Diagnostic::SettlementRejected {
                hash,
                payment_asset,
                paid,
                required,
                stable_logs_valid,
            } => worker::console_warn!(
                "in-band settlement rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} payment_asset={payment_asset:?} paid={paid} required={required} stable_logs_valid={stable_logs_valid}"
            ),
            Diagnostic::TempoSettlementRejected {
                hash,
                paid,
                required,
                stable_logs_valid,
            } => worker::console_warn!(
                "Tempo pathUSD in-band settlement rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} paid={paid} required={required} stable_logs_valid={stable_logs_valid}"
            ),
            Diagnostic::TopUpCapUsd { native_units } => worker::console_log!(
                "using USD-denominated relayer top-up cap: chain_id={chain_id} native_units={native_units}"
            ),
            Diagnostic::TopUpCapUnconvertible => worker::console_warn!(
                "could not convert USD relayer top-up cap to native units; using static cap: chain_id={chain_id}"
            ),
            Diagnostic::ExecutionDeferred { reason } => worker::console_warn!(
                "UserOperation lane execution deferred: chain_id={chain_id} lane={lane} error={reason}"
            ),
        }
    }
}

fn tempo_chain_assets(config: &CfConfig) -> core_execution::ResolvedChainAssets {
    core_execution::ResolvedChainAssets {
        assets: vela_relay_core::settlement::ChainAssetConfig {
            native_decimals: vela_relay_core::tempo::PATH_USD_DECIMALS,
            settlement_markup_bps: config.settlement_markup_bps,
            stablecoins: std::collections::BTreeMap::from([(
                vela_relay_core::tempo::PATH_USD,
                vela_relay_core::settlement::StablecoinConfig {
                    symbol: vela_relay_core::tempo::PATH_USD_SYMBOL.into(),
                    decimals: vela_relay_core::tempo::PATH_USD_DECIMALS,
                },
            )]),
        },
        native_symbol: vela_relay_core::tempo::PATH_USD_SYMBOL.into(),
    }
}
