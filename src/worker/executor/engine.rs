use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fmt::{Display, Formatter},
    future::Future,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy::primitives::{Address, Bytes, U256};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::{sync::Mutex, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{
        ClaimedDelayedUserOperation, DelayedUserOperation, PreparedBundleIntent,
        PreparedFundingIntent, USER_OPERATION_QUEUE_RETENTION, UserOperationStatusStore,
        rpc::types::UserOperationStatusKind,
    },
    utils::{
        config::ExecutorConfig,
        market::binance_usdt_price,
        rpc as chain_directory, tempo,
        vault::{derive_pool_relayer_secret_key, derive_treasury_secret_key},
    },
    worker::consumer::{
        MalformedUserOperation, MalformedUserOperationHandlerFuture, RoutedUserOperation,
        UserOperationBatchResults, UserOperationHandler, UserOperationHandlerError,
        UserOperationHandlerFuture,
    },
};

use super::{
    abi::get_nonce_calldata,
    alert::TelegramAlertNotifier,
    deployment::SimulationContractDeployer,
    receipt::{receipt_succeeded, user_operation_events},
    rpc::{BroadcastOutcome, RpcBatchCall, RpcError, TrustedRpcClient},
    settlement::{ChainAssetConfig, SettlementLog, StablecoinConfig},
    simulation::{SimulationVerdict, simulate_bundle, simulate_individually},
    transaction::{
        TempoTransactionPlan, TransactionPlan, sign_eip1559, sign_tempo, signer_address,
    },
};

const BROADCAST_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const RECEIPT_RECONCILE_FAILURE_DELAY: Duration = Duration::from_secs(1);
const DELAYED_CLAIM_BATCH_SIZE: usize = 100;
const DELAYED_CLAIM_TTL_MIN: Duration = Duration::from_secs(2 * 60);
const BINANCE_PRICE_TTL: Duration = Duration::from_secs(60);
const ERC20_DECIMALS_SELECTOR: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];

static LEASE_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct ExecutorEngine {
    config: Arc<ExecutorConfig>,
    rpc: TrustedRpcClient,
    store: UserOperationStatusStore,
    relayer_keys: Arc<[k256::SecretKey]>,
    relayer_addresses: Arc<[Address]>,
    treasury_key: Arc<k256::SecretKey>,
    treasury_address: Address,
    simulation_deployer: SimulationContractDeployer,
    directory_chain_assets: Arc<Mutex<HashMap<u64, ResolvedChainAssets>>>,
    market_http: Client,
    market_prices: Arc<Mutex<HashMap<String, CachedMarketPrice>>>,
    broadcast_seen: Arc<Mutex<HashMap<String, Instant>>>,
    telegram_notifier: Option<TelegramAlertNotifier>,
}

#[derive(Clone)]
struct ResolvedChainAssets {
    assets: ChainAssetConfig,
    native_symbol: String,
}

#[derive(Clone, Debug)]
struct CachedMarketPrice {
    expires_at: Instant,
    price: U256,
}

#[derive(Clone, Copy, Debug, Default)]
struct BundleReplayAudit {
    active: usize,
    awaiting_submission: usize,
    terminal: usize,
    expired: usize,
}

#[derive(Clone, Debug)]
struct TransactionContext {
    estimated_gas: U256,
    /// Kept alongside the cap so a repriced bundle can still tell whether its lower cap clears
    /// the inclusion floor.
    base_fee_per_gas: u128,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    nonce: u64,
    relayer_balance: U256,
}

#[derive(Clone, Debug)]
struct TempoTransactionContext {
    base_fee_atto: U256,
    nonce: u64,
    relayer_path_usd_balance: U256,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FundingReadiness {
    Ready,
    Pending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BundleBroadcastDisposition {
    Confirmed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BundleResumeDisposition {
    Confirmed,
    Unknown,
    Cleared,
}

#[derive(Debug)]
pub(crate) struct ExecutorBuildError(String);

impl Display for ExecutorBuildError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ExecutorBuildError {}

#[derive(Debug)]
struct ExecutorItemError(String);

impl Display for ExecutorItemError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ExecutorItemError {}

impl ExecutorEngine {
    pub(crate) fn new(
        config: ExecutorConfig,
        store: UserOperationStatusStore,
    ) -> Result<Self, ExecutorBuildError> {
        let rpc = TrustedRpcClient::new(&config)
            .map_err(|error| ExecutorBuildError(error.to_string()))?;
        let market_http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(config.rpc_timeout)
            .build()
            .map_err(|error| {
                ExecutorBuildError(format!("could not create market data client: {error}"))
            })?;
        let telegram_notifier = TelegramAlertNotifier::new(&config.telegram_alerts, store.clone())
            .map_err(|error| {
                ExecutorBuildError(format!("could not create Telegram alert client: {error}"))
            })?;
        let mut relayer_keys = Vec::with_capacity(config.pool_width);
        let mut relayer_addresses = Vec::with_capacity(config.pool_width);
        for index in 0..config.pool_width {
            let key = derive_pool_relayer_secret_key(config.operator_secret.expose(), index)
                .map_err(|error| ExecutorBuildError(error.to_string()))?;
            relayer_addresses.push(signer_address(&key));
            relayer_keys.push(key);
        }
        let treasury_key = Arc::new(
            derive_treasury_secret_key(config.operator_secret.expose())
                .map_err(|error| ExecutorBuildError(error.to_string()))?,
        );
        let treasury_address = signer_address(&treasury_key);
        let simulation_deployer = SimulationContractDeployer::new(
            rpc.clone(),
            store.clone(),
            treasury_key.clone(),
            treasury_address,
            config.lease_ttl,
            config.treasury_floor_wei,
        );
        Ok(Self {
            config: Arc::new(config),
            rpc,
            store,
            relayer_keys: relayer_keys.into(),
            relayer_addresses: relayer_addresses.into(),
            treasury_key,
            treasury_address,
            simulation_deployer,
            directory_chain_assets: Arc::new(Mutex::new(HashMap::new())),
            market_http,
            market_prices: Arc::new(Mutex::new(HashMap::new())),
            broadcast_seen: Arc::new(Mutex::new(HashMap::new())),
            telegram_notifier,
        })
    }

    pub(crate) fn treasury_address(&self) -> Address {
        self.treasury_address
    }

    pub(crate) async fn run_reconciler(&self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(self.config.receipt_poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => {
                    let mut failed = false;
                    if let Err(error) = self.reconcile_prepared_bundles().await {
                        failed = true;
                        tracing::warn!(%error, "prepared bundle reconciliation failed");
                    }
                    if let Err(error) = self.reconcile_delayed_user_operations().await {
                        failed = true;
                        tracing::warn!(%error, "delayed UserOperation reconciliation failed");
                    }
                    if failed {
                        tokio::select! {
                            _ = shutdown.cancelled() => return,
                            _ = tokio::time::sleep(RECEIPT_RECONCILE_FAILURE_DELAY) => {}
                        }
                    }
                }
            }
        }
    }

    async fn reconcile_delayed_user_operations(&self) -> Result<(), ExecutorItemError> {
        let token = unique_token("delayed");
        let claims = self
            .store
            .claim_due_user_operations(
                &token,
                DELAYED_CLAIM_BATCH_SIZE,
                self.config.lease_ttl.max(DELAYED_CLAIM_TTL_MIN),
            )
            .await
            .map_err(store_item_error)?;
        if claims.is_empty() {
            return Ok(());
        }

        let mut by_lane = BTreeMap::<(u64, u8), Vec<ClaimedDelayedUserOperation>>::new();
        for claim in claims {
            by_lane
                .entry((claim.operation.chain_id, claim.operation.lane))
                .or_default()
                .push(claim);
        }

        let mut tasks = JoinSet::new();
        for ((chain_id, lane), claims) in by_lane {
            let engine = self.clone();
            let token = token.clone();
            tasks.spawn(async move {
                let result = engine.process_delayed_lane(&token, claims).await;
                (chain_id, lane, result)
            });
        }

        let mut first_error = None;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok((_, _, Ok(()))) => {}
                Ok((chain_id, lane, Err(error))) => {
                    tracing::warn!(chain_id, lane, %error, "delayed lane processing failed");
                    first_error.get_or_insert(error);
                }
                Err(error) => {
                    tracing::error!(?error, "delayed lane task panicked");
                    first_error.get_or_insert_with(|| {
                        ExecutorItemError("delayed lane task panicked".into())
                    });
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn process_delayed_lane(
        &self,
        token: &str,
        claims: Vec<ClaimedDelayedUserOperation>,
    ) -> Result<(), ExecutorItemError> {
        let operations = claims
            .iter()
            .map(|claim| routed_operation_from_delayed(&claim.operation))
            .collect::<Vec<_>>();
        let outcomes = self.handle_lane_batch(operations).await;
        if outcomes.len() != claims.len() {
            return Err(ExecutorItemError(
                "delayed executor returned a misaligned result vector".into(),
            ));
        }

        let mut first_error = None;
        for (claim, outcome) in claims.into_iter().zip(outcomes) {
            let result = match outcome {
                Ok(()) => self
                    .store
                    .complete_delayed_user_operation(&claim.identifier, token)
                    .await
                    .map(|_| ()),
                Err(_) => self
                    .store
                    .retry_delayed_user_operation(
                        &claim.operation,
                        token,
                        self.delayed_payload_ttl(),
                    )
                    .await
                    .map(|_| ()),
            };
            if let Err(error) = result {
                tracing::warn!(
                    chain_id = claim.operation.chain_id,
                    lane = claim.operation.lane,
                    user_operation_hash = %claim.operation.user_operation_hash,
                    %error,
                    "could not finalize delayed UserOperation claim"
                );
                first_error.get_or_insert_with(|| store_item_error(error));
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Executes one consumed lane batch by driving the core's
    /// `ExecutionApp` program: `Core::new()` per batch, one operation in
    /// flight at a time, per-item resolutions mapped back onto the
    /// `UserOperationHandler` results contract.
    async fn handle_lane_batch(
        &self,
        operations: Vec<RoutedUserOperation>,
    ) -> UserOperationBatchResults {
        if operations.is_empty() {
            return Vec::new();
        }
        let chain_id = operations[0].chain_id;
        let lane = operations[0].lane;
        let policy = core_execution::ExecutionPolicy {
            pool_width: self.config.pool_width,
            max_bundle_operations: self.config.max_bundle_operations,
            gas_buffer_bps: self.config.gas_buffer_bps,
            fixed_gas_buffer: self.config.fixed_gas_buffer,
            settlement_inclusion_floor_bps: self.config.settlement_inclusion_floor_bps,
            settlement_hold_max_attempts: self.config.settlement_hold_max_attempts,
            top_up_max_wei: self.config.top_up_max_wei,
            is_tempo: tempo::is_tempo_chain(chain_id),
            treasury: self.treasury_address,
        };
        let lease_token = unique_token("lane");
        let mut shell = BatchShell {
            engine: self,
            operations: &operations,
            chain_id,
            lane,
            lease_scope: format!("executor:{chain_id}:{lane}"),
            lease_token: lease_token.clone(),
            assets: None,
            heartbeat: None,
            lease_acquired: false,
        };

        let core: crux_core::Core<core_execution::ExecutionApp> = crux_core::Core::new();
        let mut effects: VecDeque<core_execution::ExecutionEffect> = core
            .process_event(core_execution::ExecutionEvent::Start(Box::new(
                core_execution::StartBatch {
                    operations: operations.clone(),
                    policy,
                    lease_token,
                },
            )))
            .into_iter()
            .collect();
        while let Some(core_execution::ExecutionEffect::Work(mut request)) = effects.pop_front() {
            let outcome = shell.execute(&request.operation).await;
            match core.resolve(&mut request, outcome) {
                Ok(next) => effects.extend(next),
                Err(_) => {
                    shell.finish().await;
                    return failure_results(operations.len(), "could not resolve execution effect");
                }
            }
        }
        shell.finish().await;

        match core.view().outcome {
            Some(resolutions) => resolutions
                .into_iter()
                .map(|resolution| match resolution {
                    core_execution::ItemResolution::Durable => Ok(()),
                    core_execution::ItemResolution::Failed { reason } => {
                        Err(Box::new(ExecutorItemError(reason)) as UserOperationHandlerError)
                    }
                })
                .collect(),
            None => failure_results(operations.len(), "lane batch never settled"),
        }
    }

    /// Best-effort retry diagnostic (store write only; the Telegram decision
    /// lives in the core's program).
    async fn record_deferred_diagnostic(
        &self,
        chain_id: u64,
        user_operation_hash: &str,
        stage: &str,
        reason: &str,
    ) {
        let _ = chain_id;
        if let Err(error) = self
            .store
            .record_executor_deferred(user_operation_hash, stage, reason)
            .await
        {
            tracing::warn!(
                user_operation_hash,
                stage,
                %error,
                "could not persist executor retry diagnostic"
            );
        }
    }

    fn notify_executor_issue(
        &self,
        chain_id: u64,
        stage: &str,
        user_operation_hash: &str,
        reason: &str,
    ) {
        let Some(notifier) = self.telegram_notifier.clone() else {
            return;
        };
        let stage = stage.to_owned();
        let user_operation_hash = user_operation_hash.to_owned();
        let reason = reason.to_owned();
        tokio::spawn(async move {
            notifier
                .notify_executor_issue(chain_id, &stage, &user_operation_hash, &reason)
                .await;
        });
    }

    async fn chain_assets_for(
        &self,
        chain_id: u64,
    ) -> Result<ResolvedChainAssets, ExecutorItemError> {
        if let Some(assets) = self
            .directory_chain_assets
            .lock()
            .await
            .get(&chain_id)
            .cloned()
        {
            return Ok(assets);
        }

        let assets = self.directory_usd_stable_assets(chain_id).await?;
        self.directory_chain_assets
            .lock()
            .await
            .insert(chain_id, assets.clone());
        Ok(assets)
    }

    fn tempo_chain_assets(&self) -> ResolvedChainAssets {
        ResolvedChainAssets {
            assets: ChainAssetConfig {
                native_decimals: tempo::PATH_USD_DECIMALS,
                settlement_markup_bps: self.config.settlement_markup_bps,
                stablecoins: BTreeMap::from([(
                    tempo::PATH_USD,
                    StablecoinConfig {
                        symbol: tempo::PATH_USD_SYMBOL.into(),
                        decimals: tempo::PATH_USD_DECIMALS,
                    },
                )]),
            },
            native_symbol: tempo::PATH_USD_SYMBOL.into(),
        }
    }

    async fn directory_usd_stable_assets(
        &self,
        chain_id: u64,
    ) -> Result<ResolvedChainAssets, ExecutorItemError> {
        let metadata = chain_directory::payment_assets(chain_id)
            .await
            .map_err(|_| {
                ExecutorItemError("could not load payment assets from chain directory".into())
            })?;
        let mut stablecoins = metadata
            .stablecoins
            .into_iter()
            .filter_map(|stablecoin| {
                Address::from_str(&stablecoin.contract)
                    .ok()
                    .map(|address| (address, stablecoin.symbol, stablecoin.decimals))
            })
            .collect::<Vec<_>>();
        let missing_decimals = stablecoins
            .iter()
            .enumerate()
            .filter_map(|(index, (address, _, decimals))| {
                decimals.is_none().then_some((index, *address))
            })
            .collect::<Vec<_>>();
        if !missing_decimals.is_empty() {
            let calls = missing_decimals
                .iter()
                .map(|(_, address)| RpcBatchCall {
                    method: "eth_call",
                    params: json!([{
                        "to": address.to_string(),
                        "data": format!("0x{}", hex::encode(ERC20_DECIMALS_SELECTOR)),
                    }, "latest"]),
                })
                .collect::<Vec<_>>();
            let responses = self
                .rpc
                .batch(chain_id, &calls)
                .await
                .map_err(rpc_item_error)?;
            for (response_index, (stable_index, _)) in missing_decimals.into_iter().enumerate() {
                let decimals = response_abi_u256(&responses, response_index, "ERC-20 decimals")
                    .ok()
                    .and_then(|value| u32::try_from(value).ok())
                    .filter(|decimals| *decimals <= 38);
                stablecoins[stable_index].2 = decimals;
            }
        }

        let stablecoins = stablecoins
            .into_iter()
            .filter_map(|(address, symbol, decimals)| {
                let decimals = decimals?;
                Some((address, StablecoinConfig { symbol, decimals }))
            })
            .collect::<BTreeMap<_, _>>();
        Ok(ResolvedChainAssets {
            assets: ChainAssetConfig {
                native_decimals: metadata.native.decimals,
                settlement_markup_bps: self.config.settlement_markup_bps,
                stablecoins,
            },
            native_symbol: metadata.native.symbol,
        })
    }

    async fn tempo_transaction_context(
        &self,
        chain_id: u64,
        relayer: Address,
    ) -> Result<TempoTransactionContext, ExecutorItemError> {
        let calls = [
            RpcBatchCall {
                method: "eth_getBlockByNumber",
                params: json!(["latest", false]),
            },
            RpcBatchCall {
                method: "eth_gasPrice",
                params: json!([]),
            },
            RpcBatchCall {
                method: "eth_getTransactionCount",
                params: json!([relayer.to_string(), "pending"]),
            },
            RpcBatchCall {
                method: "eth_call",
                params: json!([{
                    "to": tempo::PATH_USD.to_string(),
                    "data": format!("0x{}", hex::encode(tempo::path_usd_balance_calldata(relayer))),
                }, "latest"]),
            },
        ];
        let responses = self
            .rpc
            .batch(chain_id, &calls)
            .await
            .map_err(rpc_item_error)?;
        let base_fee_atto = response_value(&responses, 0, "Tempo latest block")?
            .get("baseFeePerGas")
            .and_then(Value::as_str)
            .and_then(parse_quantity)
            .or_else(|| response_quantity_optional(&responses, 1))
            .unwrap_or_else(|| U256::from(tempo::TEMPO_BASE_FEE_ATTO));
        let nonce = u64::try_from(response_quantity(&responses, 2, "Tempo relayer nonce")?)
            .map_err(|_| ExecutorItemError("Tempo relayer nonce exceeds uint64".into()))?;
        let relayer_path_usd_balance = response_abi_u256(&responses, 3, "Tempo pathUSD balance")?;
        Ok(TempoTransactionContext {
            base_fee_atto,
            nonce,
            relayer_path_usd_balance,
        })
    }

    fn delayed_payload_ttl(&self) -> Duration {
        self.config.attempt_ttl.max(USER_OPERATION_QUEUE_RETENTION)
    }

    async fn ensure_lease(&self, scope: &str, token: &str) -> Result<(), ExecutorItemError> {
        match self
            .store
            .renew_lease(scope, token, self.config.lease_ttl)
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(ExecutorItemError("executor lease was lost".into())),
            Err(error) => Err(store_item_error(error)),
        }
    }

    async fn run_with_lease_heartbeat<T, F>(
        &self,
        scope: &str,
        token: &str,
        future: F,
    ) -> Result<T, ExecutorItemError>
    where
        F: Future<Output = Result<T, ExecutorItemError>>,
    {
        let period = (self.config.lease_ttl / 3).max(Duration::from_millis(1));
        let start = tokio::time::Instant::now() + period;
        let mut heartbeat = tokio::time::interval_at(start, period);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(future);

        loop {
            tokio::select! {
                biased;
                _ = heartbeat.tick() => {
                    self.ensure_lease(scope, token).await?;
                }
                result = &mut future => return result,
            }
        }
    }

    async fn dead_letter_routed(
        &self,
        operation: &RoutedUserOperation,
        reason: &str,
    ) -> Result<(), ExecutorItemError> {
        let payload = serde_json::to_vec(&json!({
            "schemaVersion": operation.schema_version,
            "userOperationHash": operation.user_operation_hash,
            "chainId": operation.chain_id,
            "entryPoint": operation.entry_point,
            "userOperation": operation.user_operation,
        }))
        .map_err(|_| ExecutorItemError("could not serialize dead-letter payload".into()))?;
        self.store
            .save_malformed_dead_letter(
                operation.chain_id,
                operation.partition_id,
                operation.offset,
                &payload,
                reason,
                Some(&operation.user_operation_hash),
                self.config.attempt_ttl,
            )
            .await
            .map_err(store_item_error)?;
        Ok(())
    }

    async fn transaction_context(
        &self,
        chain_id: u64,
        relayer: Address,
        entry_point: Address,
        calldata: &Bytes,
    ) -> Result<TransactionContext, ExecutorItemError> {
        let transaction = json!({
            "from": relayer.to_string(),
            "to": entry_point.to_string(),
            "data": format!("0x{}", hex::encode(calldata)),
        });
        let calls = [
            RpcBatchCall {
                method: "eth_estimateGas",
                params: json!([transaction]),
            },
            RpcBatchCall {
                method: "eth_getBlockByNumber",
                params: json!(["latest", false]),
            },
            RpcBatchCall {
                method: "eth_maxPriorityFeePerGas",
                params: json!([]),
            },
            RpcBatchCall {
                method: "eth_getTransactionCount",
                params: json!([relayer.to_string(), "pending"]),
            },
            RpcBatchCall {
                method: "eth_getBalance",
                params: json!([relayer.to_string(), "pending"]),
            },
        ];
        let responses = self
            .rpc
            .batch(chain_id, &calls)
            .await
            .map_err(rpc_item_error)?;
        let estimated_gas = response_quantity(&responses, 0, "eth_estimateGas")?;
        let block = response_value(&responses, 1, "eth_getBlockByNumber")?;
        let base_fee = block
            .get("baseFeePerGas")
            .and_then(Value::as_str)
            .and_then(parse_quantity)
            .ok_or_else(|| ExecutorItemError("latest block has no EIP-1559 base fee".into()))?;
        let tip = match response_quantity_optional(&responses, 2) {
            Some(tip) => tip,
            None => {
                let gas_price = self
                    .rpc
                    .call(chain_id, "eth_gasPrice", json!([]))
                    .await
                    .map_err(rpc_item_error)?
                    .as_str()
                    .and_then(parse_quantity)
                    .ok_or_else(|| {
                        ExecutorItemError("eth_gasPrice returned an invalid quantity".into())
                    })?;
                vela_relay_core::gas_math::tip_from_legacy_gas_price(gas_price, base_fee)
                    .ok_or_else(|| {
                        ExecutorItemError("gas price is below the latest base fee".into())
                    })?
            }
        };
        let base_fee = u128::try_from(base_fee)
            .map_err(|_| ExecutorItemError("base fee exceeds uint128".into()))?;
        let tip = u128::try_from(tip)
            .map_err(|_| ExecutorItemError("priority fee exceeds uint128".into()))?;
        let max_fee_per_gas = vela_relay_core::gas_math::quoted_outer_fee(base_fee, tip)
            .ok_or_else(|| ExecutorItemError("EIP-1559 fee overflow".into()))?;
        let nonce = u64::try_from(response_quantity(&responses, 3, "eth_getTransactionCount")?)
            .map_err(|_| ExecutorItemError("relayer nonce exceeds uint64".into()))?;
        let relayer_balance = response_quantity(&responses, 4, "eth_getBalance")?;

        Ok(TransactionContext {
            estimated_gas,
            base_fee_per_gas: base_fee,
            max_fee_per_gas,
            max_priority_fee_per_gas: tip,
            nonce,
            relayer_balance,
        })
    }

    async fn market_usd_price(
        &self,
        _chain_id: u64,
        symbol: &str,
    ) -> Result<U256, ExecutorItemError> {
        // The Gnosis xDAI peg is decided in the core program
        // (`settlement::pegged_native_usd_price`); this executor is only asked
        // for genuinely market-priced chains.
        let symbol = symbol.trim().to_ascii_uppercase();
        if symbol.is_empty() || !symbol.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(ExecutorItemError(
                "native currency symbol is invalid".into(),
            ));
        }
        let now = Instant::now();
        if let Some(cached) = self.market_prices.lock().await.get(&symbol).cloned()
            && cached.expires_at > now
        {
            return Ok(cached.price);
        }
        let raw_price = binance_usdt_price(&self.market_http, &symbol)
            .await
            .ok_or_else(|| ExecutorItemError("Binance native USD price request failed".into()))?;
        let price = parse_market_usd_price(&raw_price)
            .ok_or_else(|| ExecutorItemError("Binance native USD price is invalid".into()))?;
        self.market_prices.lock().await.insert(
            symbol,
            CachedMarketPrice {
                expires_at: now + BINANCE_PRICE_TTL,
                price,
            },
        );
        Ok(price)
    }

    async fn resume_bundle_intent(
        &self,
        intent: &PreparedBundleIntent,
    ) -> Result<BundleResumeDisposition, ExecutorItemError> {
        let audit = self.audit_bundle_replay(intent).await?;
        if audit.active == 0 && audit.terminal != 0 {
            self.clear_obsolete_bundle_intent(intent, audit).await?;
            return Ok(BundleResumeDisposition::Cleared);
        }
        if audit.terminal != 0 || audit.expired != 0 {
            tracing::warn!(
                chain_id = intent.chain_id,
                lane = intent.lane,
                transaction_hash = %intent.transaction_hash,
                active_members = audit.active,
                terminal_members = audit.terminal,
                expired_members = audit.expired,
                "replaying prepared bundle after auditing unavailable members"
            );
        }
        match self.broadcast_bundle_intent(intent).await? {
            BundleBroadcastDisposition::Unknown => {
                return Ok(BundleResumeDisposition::Unknown);
            }
            BundleBroadcastDisposition::Confirmed => {}
        }
        if audit.active == 0 {
            // Lifecycle records expire sooner than the signed outbox. Without a terminal record
            // or receipt there is no proof that this transaction is safe to forget, so retain it
            // and keep reconciling the relayer nonce.
            return Ok(BundleResumeDisposition::Confirmed);
        }
        let indexed = self
            .store
            .mark_bundle_submitted(
                intent.chain_id,
                &intent.transaction_hash,
                &intent.user_operation_hashes,
            )
            .await
            .map_err(store_item_error)?;
        if indexed != audit.active {
            // Records retain a shorter TTL than prepared outbox entries. A member can expire or
            // reach a terminal state between the preflight audit and this atomic transition.
            // Re-audit before deciding that the lane is corrupt; exact submitted membership is
            // also safe when an earlier recovery attempt already completed the transition.
            let after = self.audit_bundle_replay(intent).await?;
            if after.active == 0 {
                if after.terminal != 0 {
                    self.clear_obsolete_bundle_intent(intent, after).await?;
                    return Ok(BundleResumeDisposition::Cleared);
                }
                return Ok(BundleResumeDisposition::Confirmed);
            }
            if after.awaiting_submission != 0 {
                return Err(ExecutorItemError(
                    "prepared bundle has live members that could not enter submitted state".into(),
                ));
            }
        }
        Ok(BundleResumeDisposition::Confirmed)
    }

    async fn audit_bundle_replay(
        &self,
        intent: &PreparedBundleIntent,
    ) -> Result<BundleReplayAudit, ExecutorItemError> {
        let records = self
            .store
            .get_many(&intent.user_operation_hashes)
            .await
            .map_err(store_item_error)?;
        if records.len() != intent.user_operation_hashes.len() {
            return Err(ExecutorItemError(
                "Redis returned incomplete prepared bundle membership".into(),
            ));
        }

        let mut audit = BundleReplayAudit::default();
        for (hash, record) in intent.user_operation_hashes.iter().zip(records) {
            let Some(record) = record else {
                audit.expired += 1;
                continue;
            };
            if record.chain_id != intent.chain_id
                || !record.entry_point.eq_ignore_ascii_case(&intent.entry_point)
            {
                return Err(ExecutorItemError(format!(
                    "prepared bundle member {hash} no longer matches its chain and EntryPoint"
                )));
            }

            match record.status {
                UserOperationStatusKind::Queued | UserOperationStatusKind::NotSubmitted => {
                    if !record.admitted {
                        return Err(ExecutorItemError(format!(
                            "prepared bundle member {hash} is no longer admitted"
                        )));
                    }
                    audit.active += 1;
                    audit.awaiting_submission += 1;
                }
                UserOperationStatusKind::Submitted => {
                    if !record
                        .transaction_hash
                        .as_ref()
                        .is_some_and(|transaction_hash| {
                            transaction_hash.eq_ignore_ascii_case(&intent.transaction_hash)
                        })
                    {
                        return Err(ExecutorItemError(format!(
                            "prepared bundle member {hash} belongs to another transaction"
                        )));
                    }
                    audit.active += 1;
                }
                UserOperationStatusKind::Rejected
                | UserOperationStatusKind::Included
                | UserOperationStatusKind::Failed => audit.terminal += 1,
                UserOperationStatusKind::NotFound => {
                    return Err(ExecutorItemError(format!(
                        "prepared bundle member {hash} has an invalid stored status"
                    )));
                }
            }
        }
        Ok(audit)
    }

    async fn clear_obsolete_bundle_intent(
        &self,
        intent: &PreparedBundleIntent,
        audit: BundleReplayAudit,
    ) -> Result<(), ExecutorItemError> {
        if audit.terminal == 0 {
            return Err(ExecutorItemError(
                "refusing to clear an unproven prepared bundle".into(),
            ));
        }
        self.store
            .clear_prepared_bundle_intent(intent.chain_id, intent.lane, &intent.transaction_hash)
            .await
            .map_err(store_item_error)?;
        self.broadcast_seen
            .lock()
            .await
            .remove(&intent.transaction_hash);
        tracing::warn!(
            chain_id = intent.chain_id,
            lane = intent.lane,
            transaction_hash = %intent.transaction_hash,
            terminal_members = audit.terminal,
            expired_members = audit.expired,
            "cleared prepared bundle with no live lifecycle members"
        );
        Ok(())
    }

    /// Broadcasts the exact durable bytes. An ambiguous send is not mempool admission: the
    /// expected transaction hash must be observable before callers may persist `submitted`.
    async fn broadcast_bundle_intent(
        &self,
        intent: &PreparedBundleIntent,
    ) -> Result<BundleBroadcastDisposition, ExecutorItemError> {
        let raw = validate_raw_transaction(&intent.raw_transaction, &intent.transaction_hash)?;
        if self
            .recently_confirmed_broadcast(&intent.transaction_hash)
            .await
        {
            return Ok(BundleBroadcastDisposition::Confirmed);
        }
        let outcome = match self
            .rpc
            .broadcast_raw_transaction(intent.chain_id, &raw)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                return Err(rpc_item_error(error));
            }
        };
        match outcome {
            BroadcastOutcome::Accepted(hash)
                if hash.eq_ignore_ascii_case(&intent.transaction_hash) =>
            {
                self.remember_confirmed_broadcast(&intent.transaction_hash)
                    .await;
                Ok(BundleBroadcastDisposition::Confirmed)
            }
            BroadcastOutcome::Accepted(_) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                Err(ExecutorItemError(
                    "RPC returned a transaction hash different from the signed bytes".into(),
                ))
            }
            BroadcastOutcome::Ambiguous(reason) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                if self
                    .transaction_is_known(intent.chain_id, &intent.transaction_hash)
                    .await
                {
                    self.remember_confirmed_broadcast(&intent.transaction_hash)
                        .await;
                    Ok(BundleBroadcastDisposition::Confirmed)
                } else if nonce_too_low(&reason) && self.bundle_nonce_is_stale(intent).await {
                    self.clear_stale_bundle_intent(intent, &reason).await?;
                    Ok(BundleBroadcastDisposition::Unknown)
                } else {
                    tracing::warn!(
                        chain_id = intent.chain_id,
                        lane = intent.lane,
                        transaction_hash = %intent.transaction_hash,
                        reason,
                        "ambiguous handleOps broadcast is not yet observable"
                    );
                    Ok(BundleBroadcastDisposition::Unknown)
                }
            }
            BroadcastOutcome::Rejected(reason) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                if self
                    .transaction_is_known(intent.chain_id, &intent.transaction_hash)
                    .await
                {
                    self.remember_confirmed_broadcast(&intent.transaction_hash)
                        .await;
                    return Ok(BundleBroadcastDisposition::Confirmed);
                }
                if nonce_too_low(&reason) && self.bundle_nonce_is_stale(intent).await {
                    self.clear_stale_bundle_intent(intent, &reason).await?;
                    return Ok(BundleBroadcastDisposition::Unknown);
                }
                tracing::warn!(
                    chain_id = intent.chain_id,
                    lane = intent.lane,
                    transaction_hash = %intent.transaction_hash,
                    reason,
                    "rejected broadcast is unproven; retaining exact handleOps outbox"
                );
                Ok(BundleBroadcastDisposition::Unknown)
            }
        }
    }

    async fn transaction_is_known(&self, chain_id: u64, expected_hash: &str) -> bool {
        match self
            .rpc
            .call(chain_id, "eth_getTransactionByHash", json!([expected_hash]))
            .await
        {
            Ok(Value::Object(transaction)) => transaction
                .get("hash")
                .and_then(Value::as_str)
                .is_some_and(|hash| hash.eq_ignore_ascii_case(expected_hash)),
            Ok(_) => false,
            Err(error) => {
                tracing::warn!(
                    chain_id,
                    transaction_hash = expected_hash,
                    %error,
                    "could not confirm ambiguous transaction broadcast"
                );
                false
            }
        }
    }

    /// A nonce error alone is not sufficient to discard an exact outbox: it could merely be a
    /// pending transaction observed by a different node. Only a higher *latest* nonce proves the
    /// signed bytes can never be included, so clearing then lets queued UserOperations rebuild
    /// against the next nonce without risking a duplicate handleOps submission.
    async fn bundle_nonce_is_stale(&self, intent: &PreparedBundleIntent) -> bool {
        let Some(relayer) = self.relayer_addresses.get(intent.lane as usize) else {
            return false;
        };
        match self
            .rpc
            .call(
                intent.chain_id,
                "eth_getTransactionCount",
                json!([relayer.to_string(), "latest"]),
            )
            .await
            .ok()
            .and_then(|value| value.as_str().and_then(parse_quantity))
            .and_then(|nonce| u64::try_from(nonce).ok())
        {
            Some(latest_nonce) => latest_nonce > intent.nonce,
            None => false,
        }
    }

    async fn clear_stale_bundle_intent(
        &self,
        intent: &PreparedBundleIntent,
        reason: &str,
    ) -> Result<(), ExecutorItemError> {
        let cleared = self
            .store
            .clear_prepared_bundle_intent(intent.chain_id, intent.lane, &intent.transaction_hash)
            .await
            .map_err(store_item_error)?;
        // The consumer and the recovery loop can observe the same intent concurrently. Only the
        // caller that atomically removed it should emit the recovery log and clear its local
        // broadcast cache.
        if !cleared {
            return Ok(());
        }
        self.broadcast_seen
            .lock()
            .await
            .remove(&intent.transaction_hash);
        tracing::warn!(
            chain_id = intent.chain_id,
            lane = intent.lane,
            relayer = %self.relayer_addresses[intent.lane as usize],
            stale_nonce = intent.nonce,
            transaction_hash = %intent.transaction_hash,
            reason,
            "discarded a prepared handleOps transaction whose nonce is already mined; queued operations will be rebuilt"
        );
        Ok(())
    }

    async fn recently_confirmed_broadcast(&self, transaction_hash: &str) -> bool {
        let now = Instant::now();
        let mut confirmed = self.broadcast_seen.lock().await;
        confirmed.retain(|_, at| now.saturating_duration_since(*at) < BROADCAST_RETRY_INTERVAL);
        confirmed.contains_key(transaction_hash)
    }

    async fn remember_confirmed_broadcast(&self, transaction_hash: &str) {
        self.broadcast_seen
            .lock()
            .await
            .insert(transaction_hash.to_owned(), Instant::now());
    }

    async fn reconcile_prepared_bundles(&self) -> Result<(), ExecutorItemError> {
        let intents = self
            .store
            .list_prepared_bundle_intents()
            .await
            .map_err(store_item_error)?;
        let mut claimed_by_chain = BTreeMap::<u64, Vec<PreparedBundleIntent>>::new();
        for intent in intents {
            let disposition = match self.resume_bundle_intent(&intent).await {
                Ok(disposition) => disposition,
                Err(error) => {
                    tracing::warn!(
                        chain_id = intent.chain_id,
                        lane = intent.lane,
                        transaction_hash = %intent.transaction_hash,
                        %error,
                        "could not resume prepared bundle"
                    );
                    continue;
                }
            };
            if disposition != BundleResumeDisposition::Confirmed {
                continue;
            }
            let still_exists = match self
                .store
                .get_prepared_bundle_intent(intent.chain_id, intent.lane)
                .await
            {
                Ok(intent) => intent.is_some(),
                Err(error) => {
                    tracing::warn!(
                        chain_id = intent.chain_id,
                        lane = intent.lane,
                        transaction_hash = %intent.transaction_hash,
                        %error,
                        "could not reload prepared bundle"
                    );
                    continue;
                }
            };
            if !still_exists {
                continue;
            }
            let claimed = match self
                .store
                .acquire_lease(
                    &format!("receipt:{}:{}", intent.chain_id, intent.transaction_hash),
                    &unique_token("receipt"),
                    self.config.receipt_poll_interval,
                )
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    tracing::warn!(
                        chain_id = intent.chain_id,
                        lane = intent.lane,
                        transaction_hash = %intent.transaction_hash,
                        %error,
                        "could not claim prepared bundle receipt check"
                    );
                    continue;
                }
            };
            if claimed {
                claimed_by_chain
                    .entry(intent.chain_id)
                    .or_default()
                    .push(intent);
            }
        }

        for (chain_id, intents) in claimed_by_chain {
            let calls = intents
                .iter()
                .map(|intent| RpcBatchCall {
                    method: "eth_getTransactionReceipt",
                    params: json!([intent.transaction_hash]),
                })
                .collect::<Vec<_>>();
            let receipts = match self.rpc.batch(chain_id, &calls).await {
                Ok(receipts) => receipts,
                Err(error) => {
                    tracing::warn!(chain_id, %error, "bundle receipt batch RPC failed");
                    continue;
                }
            };
            for (intent, receipt) in intents.into_iter().zip(receipts) {
                let receipt = match receipt {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        tracing::warn!(
                            chain_id,
                            lane = intent.lane,
                            transaction_hash = %intent.transaction_hash,
                            %error,
                            "bundle receipt item RPC failed"
                        );
                        continue;
                    }
                };
                if receipt.is_null() {
                    continue;
                }
                let persisted = match receipt_succeeded(&receipt) {
                    Some(false) => {
                        self.store
                            .mark_bundle_failed(chain_id, &intent.transaction_hash, receipt.clone())
                            .await
                    }
                    Some(true) => {
                        let entry_point = match Address::from_str(&intent.entry_point) {
                            Ok(entry_point) => entry_point,
                            Err(_) => {
                                tracing::warn!(
                                    chain_id,
                                    lane = intent.lane,
                                    transaction_hash = %intent.transaction_hash,
                                    "prepared bundle EntryPoint is invalid"
                                );
                                continue;
                            }
                        };
                        let events = user_operation_events(
                            &receipt,
                            entry_point,
                            &intent.user_operation_hashes,
                        );
                        self.store
                            .mark_bundle_confirmed(
                                chain_id,
                                &intent.transaction_hash,
                                receipt,
                                &events,
                            )
                            .await
                    }
                    None => {
                        tracing::warn!(
                            chain_id,
                            lane = intent.lane,
                            transaction_hash = %intent.transaction_hash,
                            "bundle receipt has an invalid status"
                        );
                        continue;
                    }
                };
                if let Err(error) = persisted {
                    tracing::warn!(
                        chain_id,
                        lane = intent.lane,
                        transaction_hash = %intent.transaction_hash,
                        %error,
                        "could not persist reconciled bundle receipt"
                    );
                    continue;
                }
                if let Err(error) = self
                    .store
                    .clear_prepared_bundle_intent(chain_id, intent.lane, &intent.transaction_hash)
                    .await
                {
                    tracing::warn!(
                        chain_id,
                        lane = intent.lane,
                        transaction_hash = %intent.transaction_hash,
                        %error,
                        "could not clear reconciled prepared bundle"
                    );
                    continue;
                }
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                tracing::info!(
                    chain_id,
                    lane = intent.lane,
                    transaction_hash = %intent.transaction_hash,
                    "reconciled handleOps transaction receipt"
                );
            }
        }
        Ok(())
    }

    async fn ensure_tempo_relayer_funded(
        &self,
        chain_id: u64,
        relayer: Address,
        relayer_balance: U256,
        required_prefund: U256,
        max_fee_per_gas: u128,
    ) -> Result<FundingReadiness, ExecutorItemError> {
        let minimum = required_prefund.max(U256::from(tempo::TEMPO_FLOAT_MIN));
        if relayer_balance >= minimum {
            return Ok(FundingReadiness::Ready);
        }

        let scope = format!("treasury:{chain_id}");
        let token = unique_token("tempo-treasury");
        if !self
            .store
            .acquire_lease(&scope, &token, self.config.lease_ttl)
            .await
            .map_err(store_item_error)?
        {
            return Ok(FundingReadiness::Pending);
        }
        let result = self
            .run_with_lease_heartbeat(
                &scope,
                &token,
                self.ensure_tempo_relayer_funded_locked(
                    chain_id,
                    relayer,
                    relayer_balance,
                    required_prefund,
                    max_fee_per_gas,
                    &scope,
                    &token,
                ),
            )
            .await;
        if let Err(error) = self.store.release_lease(&scope, &token).await {
            tracing::warn!(chain_id, %error, "could not release Tempo treasury nonce lease");
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn ensure_tempo_relayer_funded_locked(
        &self,
        chain_id: u64,
        relayer: Address,
        relayer_balance: U256,
        required_prefund: U256,
        max_fee_per_gas: u128,
        lease_scope: &str,
        lease_token: &str,
    ) -> Result<FundingReadiness, ExecutorItemError> {
        if let Some(intent) = self
            .store
            .get_prepared_funding_intent(chain_id)
            .await
            .map_err(store_item_error)?
        {
            self.resume_funding_intent(&intent).await?;
            return Ok(FundingReadiness::Pending);
        }

        let Some(amount) =
            vela_relay_core::funding::plan_tempo_top_up(relayer_balance, required_prefund)
                .map_err(|_| ExecutorItemError("Tempo relayer funding amount underflow".into()))?
        else {
            return Ok(FundingReadiness::Ready);
        };
        let amount_u128 = u128::try_from(amount)
            .map_err(|_| ExecutorItemError("Tempo relayer top-up exceeds uint128".into()))?;
        let transfer_calldata = tempo::path_usd_transfer_calldata(relayer, amount);

        let calls = [
            RpcBatchCall {
                method: "eth_getTransactionCount",
                params: json!([self.treasury_address.to_string(), "pending"]),
            },
            RpcBatchCall {
                method: "eth_call",
                params: json!([{
                    "to": tempo::PATH_USD.to_string(),
                    "data": format!("0x{}", hex::encode(tempo::path_usd_balance_calldata(self.treasury_address))),
                }, "latest"]),
            },
            RpcBatchCall {
                method: "eth_estimateGas",
                params: json!([{
                    "from": self.treasury_address.to_string(),
                    "to": tempo::PATH_USD.to_string(),
                    "data": format!("0x{}", hex::encode(&transfer_calldata)),
                    "feeToken": tempo::PATH_USD.to_string(),
                }, "latest"]),
            },
        ];
        let responses = self
            .rpc
            .batch(chain_id, &calls)
            .await
            .map_err(rpc_item_error)?;
        let nonce = u64::try_from(response_quantity(&responses, 0, "Tempo treasury nonce")?)
            .map_err(|_| ExecutorItemError("Tempo treasury nonce exceeds uint64".into()))?;
        let treasury_balance = response_abi_u256(&responses, 1, "Tempo treasury pathUSD balance")?;
        let estimated_gas = u64::try_from(response_quantity(
            &responses,
            2,
            "Tempo pathUSD top-up eth_estimateGas",
        )?)
        .map_err(|_| {
            ExecutorItemError("Tempo pathUSD top-up gas estimate exceeds uint64".into())
        })?;
        let top_up_gas_limit = tempo::buffered_top_up_gas_limit(estimated_gas)
            .ok_or_else(|| ExecutorItemError("Tempo pathUSD top-up gas buffer overflow".into()))?;
        let top_up_gas_cost =
            tempo_cost_in_path_usd(U256::from(top_up_gas_limit), U256::from(max_fee_per_gas))?;
        let required_treasury = amount
            .checked_add(top_up_gas_cost)
            .and_then(|value| value.checked_add(U256::from(tempo::TEMPO_TREASURY_FLOOR)))
            .ok_or_else(|| {
                ExecutorItemError("Tempo treasury balance requirement overflow".into())
            })?;
        if treasury_balance < required_treasury {
            tracing::warn!(
                chain_id,
                treasury_path_usd_balance = %treasury_balance,
                required_path_usd = %required_treasury,
                top_up_path_usd = %amount,
                top_up_gas_limit,
                top_up_gas_path_usd = %top_up_gas_cost,
                reserve_path_usd = tempo::TEMPO_TREASURY_FLOOR,
                "Tempo treasury cannot fund the pending relayer top-up"
            );
            return Err(ExecutorItemError(
                "Tempo treasury pathUSD is below top-up amount, gas, and reserve floor".into(),
            ));
        }

        self.ensure_lease(lease_scope, lease_token).await?;
        let signed = sign_tempo(
            &self.treasury_key,
            TempoTransactionPlan {
                chain_id,
                nonce,
                gas_limit: top_up_gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas: 0,
                fee_token: tempo::PATH_USD,
                to: tempo::PATH_USD,
                input: transfer_calldata,
            },
        )
        .map_err(|error| ExecutorItemError(error.to_string()))?;
        let intent = PreparedFundingIntent {
            chain_id,
            relayer: relayer.to_string(),
            amount_wei: amount_u128,
            raw_transaction: format!("0x{}", hex::encode(&signed.raw_transaction)),
            transaction_hash: signed.transaction_hash,
            nonce: signed.nonce,
        };
        self.ensure_lease(lease_scope, lease_token).await?;
        if !self
            .store
            .save_prepared_funding_intent(&intent)
            .await
            .map_err(store_item_error)?
        {
            if let Some(existing) = self
                .store
                .get_prepared_funding_intent(chain_id)
                .await
                .map_err(store_item_error)?
            {
                self.resume_funding_intent(&existing).await?;
                return Ok(FundingReadiness::Pending);
            }
            return Err(ExecutorItemError(
                "another Tempo treasury relayer top-up is pending".into(),
            ));
        }
        self.broadcast_funding_intent(&intent).await?;
        tracing::info!(
            chain_id,
            relayer = %relayer,
            amount_path_usd = amount_u128,
            transaction_hash = %intent.transaction_hash,
            "submitted Tempo treasury pathUSD relayer top-up"
        );
        Ok(FundingReadiness::Pending)
    }

    #[allow(clippy::too_many_arguments)]
    async fn ensure_relayer_funded(
        &self,
        chain_id: u64,
        relayer: Address,
        relayer_balance: U256,
        required_prefund: U256,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        top_up_max: U256,
    ) -> Result<FundingReadiness, ExecutorItemError> {
        // The current bundle takes precedence over filling the relayer float. A relayer that can
        // already cover this handleOps must never be held back merely because it has not reached
        // the preferred float target yet.
        if relayer_balance >= required_prefund {
            return Ok(FundingReadiness::Ready);
        }

        let scope = format!("treasury:{chain_id}");
        let token = unique_token("treasury");
        if !self
            .store
            .acquire_lease(&scope, &token, self.config.lease_ttl)
            .await
            .map_err(store_item_error)?
        {
            return Ok(FundingReadiness::Pending);
        }
        let result = self
            .run_with_lease_heartbeat(
                &scope,
                &token,
                self.ensure_relayer_funded_locked(
                    chain_id,
                    relayer,
                    relayer_balance,
                    required_prefund,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    top_up_max,
                    &scope,
                    &token,
                ),
            )
            .await;
        if let Err(error) = self.store.release_lease(&scope, &token).await {
            tracing::warn!(chain_id, %error, "could not release treasury nonce lease");
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn ensure_relayer_funded_locked(
        &self,
        chain_id: u64,
        relayer: Address,
        relayer_balance: U256,
        required_prefund: U256,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        top_up_max: U256,
        lease_scope: &str,
        lease_token: &str,
    ) -> Result<FundingReadiness, ExecutorItemError> {
        if let Some(intent) = self
            .store
            .get_prepared_funding_intent(chain_id)
            .await
            .map_err(store_item_error)?
        {
            self.resume_funding_intent(&intent).await?;
            return Ok(FundingReadiness::Pending);
        }

        let plan = plan_native_top_up(
            relayer_balance,
            required_prefund,
            self.config.relayer_float_cost_multiplier,
            self.config.relayer_float_target_wei,
            self.config.relayer_float_min_wei,
            top_up_max,
        )
        .map_err(|error| ExecutorItemError(error.to_string()))?;
        let (capped_amount, deficit) = (plan.amount_capped, plan.deficit);

        let calls = [
            RpcBatchCall {
                method: "eth_getTransactionCount",
                params: json!([self.treasury_address.to_string(), "pending"]),
            },
            RpcBatchCall {
                method: "eth_getBalance",
                params: json!([self.treasury_address.to_string(), "pending"]),
            },
        ];
        let responses = self
            .rpc
            .batch(chain_id, &calls)
            .await
            .map_err(rpc_item_error)?;
        let nonce = u64::try_from(response_quantity(
            &responses,
            0,
            "treasury eth_getTransactionCount",
        )?)
        .map_err(|_| ExecutorItemError("treasury nonce exceeds uint64".into()))?;
        let treasury_balance = response_quantity(&responses, 1, "treasury eth_getBalance")?;
        let protected_treasury =
            native_top_up_reserve(max_fee_per_gas, self.config.treasury_floor_wei)
                .map_err(|error| ExecutorItemError(error.to_string()))?;
        let top_up_gas_cost = protected_treasury - U256::from(self.config.treasury_floor_wei);
        // If the treasury can satisfy this bundle but not the preferred float, make a partial
        // top-up. The next bundle will replenish the float when more treasury funds arrive.
        let Some(amount) = treasury_affordable_top_up(
            capped_amount,
            deficit,
            treasury_balance,
            protected_treasury,
        ) else {
            let required_treasury = deficit
                .checked_add(protected_treasury)
                .ok_or_else(|| ExecutorItemError("treasury balance requirement overflow".into()))?;
            tracing::warn!(
                chain_id,
                treasury_native_balance = %treasury_balance,
                required_native_balance = %required_treasury,
                requested_top_up_native_amount = %capped_amount,
                minimum_top_up_native_amount = %deficit,
                top_up_gas_cost = %top_up_gas_cost,
                reserve_native_amount = self.config.treasury_floor_wei,
                "treasury cannot fund the current UserOperation relayer prefund"
            );
            return Err(ExecutorItemError(
                "treasury balance cannot cover the current UserOperation prefund, top-up gas, and reserve floor".into(),
            ));
        };
        if amount < capped_amount {
            tracing::info!(
                chain_id,
                requested_top_up_native_amount = %capped_amount,
                submitted_top_up_native_amount = %amount,
                minimum_top_up_native_amount = %deficit,
                "treasury funding the current UserOperation with a partial relayer float top-up"
            );
        }
        let amount_u128 = u128::try_from(amount)
            .map_err(|_| ExecutorItemError("top-up amount exceeds uint128".into()))?;

        self.ensure_lease(lease_scope, lease_token).await?;
        let signed = sign_eip1559(
            &self.treasury_key,
            TransactionPlan {
                chain_id,
                nonce,
                gas_limit: TOP_UP_GAS_LIMIT,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                to: relayer,
                value: amount,
                input: Bytes::new(),
            },
        )
        .map_err(|error| ExecutorItemError(error.to_string()))?;
        let intent = PreparedFundingIntent {
            chain_id,
            relayer: relayer.to_string(),
            amount_wei: amount_u128,
            raw_transaction: format!("0x{}", hex::encode(&signed.raw_transaction)),
            transaction_hash: signed.transaction_hash,
            nonce: signed.nonce,
        };
        self.ensure_lease(lease_scope, lease_token).await?;
        if !self
            .store
            .save_prepared_funding_intent(&intent)
            .await
            .map_err(store_item_error)?
        {
            if let Some(existing) = self
                .store
                .get_prepared_funding_intent(chain_id)
                .await
                .map_err(store_item_error)?
            {
                self.resume_funding_intent(&existing).await?;
                return Ok(FundingReadiness::Pending);
            }
            return Err(ExecutorItemError(
                "another treasury relayer top-up is pending".into(),
            ));
        }
        self.broadcast_funding_intent(&intent).await?;
        tracing::info!(
            chain_id,
            relayer = %relayer,
            amount_wei = amount_u128,
            transaction_hash = %intent.transaction_hash,
            "submitted treasury relayer gas top-up"
        );
        Ok(FundingReadiness::Pending)
    }

    async fn resume_funding_intent(
        &self,
        intent: &PreparedFundingIntent,
    ) -> Result<(), ExecutorItemError> {
        self.broadcast_funding_intent(intent).await?;
        let claimed = self
            .store
            .acquire_lease(
                &format!("receipt:{}:{}", intent.chain_id, intent.transaction_hash),
                &unique_token("funding-receipt"),
                self.config.receipt_poll_interval,
            )
            .await
            .map_err(store_item_error)?;
        if !claimed {
            return Ok(());
        }
        let receipt = self
            .rpc
            .call(
                intent.chain_id,
                "eth_getTransactionReceipt",
                json!([intent.transaction_hash]),
            )
            .await
            .map_err(rpc_item_error)?;
        if receipt.is_null() {
            return Ok(());
        }
        let Some(success) = receipt_succeeded(&receipt) else {
            return Err(ExecutorItemError(
                "funding transaction receipt has invalid status".into(),
            ));
        };
        self.store
            .clear_prepared_funding_intent(intent.chain_id, &intent.transaction_hash)
            .await
            .map_err(store_item_error)?;
        self.broadcast_seen
            .lock()
            .await
            .remove(&intent.transaction_hash);
        if !success {
            tracing::error!(
                chain_id = intent.chain_id,
                relayer = %intent.relayer,
                amount_wei = intent.amount_wei,
                transaction_hash = %intent.transaction_hash,
                "treasury relayer top-up transaction reverted"
            );
            return Err(ExecutorItemError(format!(
                "treasury relayer top-up transaction reverted: {}",
                intent.transaction_hash
            )));
        }
        tracing::info!(
            chain_id = intent.chain_id,
            relayer = %intent.relayer,
            amount_wei = intent.amount_wei,
            transaction_hash = %intent.transaction_hash,
            "treasury relayer gas top-up included"
        );
        Ok(())
    }

    async fn broadcast_funding_intent(
        &self,
        intent: &PreparedFundingIntent,
    ) -> Result<(), ExecutorItemError> {
        let raw = validate_raw_transaction(&intent.raw_transaction, &intent.transaction_hash)?;
        if self
            .recently_confirmed_broadcast(&intent.transaction_hash)
            .await
        {
            return Ok(());
        }
        let outcome = match self
            .rpc
            .broadcast_raw_transaction(intent.chain_id, &raw)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                return Err(rpc_item_error(error));
            }
        };
        match outcome {
            BroadcastOutcome::Accepted(hash)
                if hash.eq_ignore_ascii_case(&intent.transaction_hash) =>
            {
                self.remember_confirmed_broadcast(&intent.transaction_hash)
                    .await;
                Ok(())
            }
            BroadcastOutcome::Accepted(_) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                Err(ExecutorItemError(
                    "RPC returned a different funding transaction hash".into(),
                ))
            }
            BroadcastOutcome::Ambiguous(reason) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                tracing::debug!(
                    chain_id = intent.chain_id,
                    transaction_hash = %intent.transaction_hash,
                    reason,
                    "funding broadcast is ambiguous; retaining exact outbox"
                );
                Ok(())
            }
            BroadcastOutcome::Rejected(reason) => {
                self.broadcast_seen
                    .lock()
                    .await
                    .remove(&intent.transaction_hash);
                if self
                    .transaction_is_known(intent.chain_id, &intent.transaction_hash)
                    .await
                {
                    self.remember_confirmed_broadcast(&intent.transaction_hash)
                        .await;
                } else {
                    tracing::warn!(
                        chain_id = intent.chain_id,
                        transaction_hash = %intent.transaction_hash,
                        reason,
                        "rejected broadcast is unproven; retaining exact funding outbox"
                    );
                }
                Ok(())
            }
        }
    }
}

impl UserOperationHandler for ExecutorEngine {
    fn handle_batch(&self, operations: Vec<RoutedUserOperation>) -> UserOperationHandlerFuture<'_> {
        Box::pin(async move { self.handle_lane_batch(operations).await })
    }

    fn handle_malformed(
        &self,
        operation: MalformedUserOperation,
    ) -> MalformedUserOperationHandlerFuture<'_> {
        Box::pin(async move {
            self.store
                .save_malformed_dead_letter(
                    operation.chain_id,
                    operation.partition_id,
                    operation.offset,
                    &operation.payload,
                    &operation.error,
                    operation.user_operation_hash.as_deref(),
                    self.config.attempt_ttl,
                )
                .await
                .map(|_| ())
                .map_err(|error| Box::new(error) as UserOperationHandlerError)
        })
    }
}

/// The shell side of one lane-batch program: executes the core's requested
/// operations against real infrastructure. Failures fold into result data —
/// the core decides what they mean.
struct BatchShell<'a> {
    engine: &'a ExecutorEngine,
    operations: &'a [RoutedUserOperation],
    chain_id: u64,
    lane: u8,
    lease_scope: String,
    lease_token: String,
    assets: Option<ResolvedChainAssets>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
    lease_acquired: bool,
}

impl BatchShell<'_> {
    async fn execute(
        &mut self,
        operation: &core_execution::ExecutionOperation,
    ) -> core_execution::ExecutionOutcome {
        use core_execution::{ExecutionOperation as Op, ExecutionOutcome as Out};
        let engine = self.engine;
        let chain_id = self.chain_id;
        match operation {
            Op::CheckChainSupported => {
                let supported = engine.rpc.supports_chain(chain_id).await;
                if !supported {
                    tracing::warn!(
                        chain_id,
                        "Iggy stream discovered without a trusted executor RPC"
                    );
                }
                Out::Supported { supported }
            }
            Op::LoadChainAssets => {
                let resolved = if tempo::is_tempo_chain(chain_id) {
                    Ok(engine.tempo_chain_assets())
                } else {
                    engine.chain_assets_for(chain_id).await
                };
                match resolved {
                    Ok(resolved) => {
                        self.assets = Some(resolved.clone());
                        Out::Assets {
                            resolved: core_execution::ResolvedChainAssets {
                                assets: resolved.assets,
                                native_symbol: resolved.native_symbol,
                            },
                        }
                    }
                    Err(error) => {
                        tracing::warn!(chain_id, %error, "Iggy stream has no usable executor asset policy");
                        Out::AssetsUnavailable {
                            reason: error.to_string(),
                        }
                    }
                }
            }
            Op::LoadRecords { hashes } => match engine.store.get_many(hashes).await {
                Ok(records) => Out::Records { records },
                Err(error) => Out::Failed {
                    message: error.to_string(),
                },
            },
            Op::DeadLetterRouted { index, reason } => Out::Persisted {
                persisted: engine
                    .dead_letter_routed(&self.operations[*index], reason)
                    .await
                    .is_ok(),
            },
            Op::RestoreQueued { queued, .. } => {
                match engine
                    .store
                    .restore_queued_from_durable_payload(queued.clone())
                    .await
                {
                    Ok(_) => Out::Done,
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::ReloadRecord { hash } => match engine.store.get(hash).await {
                Ok(record) => {
                    if record.is_some() {
                        tracing::info!(
                            chain_id,
                            user_operation_hash = %hash,
                            "rebuilt expired UserOperation status from durable queue payload"
                        );
                    }
                    Out::Record { record }
                }
                Err(error) => Out::Failed {
                    message: error.to_string(),
                },
            },
            Op::MarkAdmitted { hash } => match engine.store.mark_admitted(hash).await {
                Ok(marked) => {
                    if marked {
                        tracing::warn!(
                            chain_id,
                            user_operation_hash = %hash,
                            "recovered Redis admission after Iggy producer crash window"
                        );
                    }
                    Out::Marked { marked }
                }
                Err(error) => Out::Failed {
                    message: error.to_string(),
                },
            },
            Op::MarkRejected { hash, cause } => match engine.store.mark_rejected(hash).await {
                Ok(_) => {
                    match cause {
                        core_execution::RejectionCause::InvalidQueuedPayload { reason } => {
                            tracing::warn!(
                                chain_id,
                                user_operation_hash = %hash,
                                reason,
                                "rejected invalid queued UserOperation"
                            );
                        }
                        core_execution::RejectionCause::SimulationRejected { reason } => {
                            tracing::warn!(
                                chain_id,
                                user_operation_hash = %hash,
                                reason,
                                "single-operation simulation rejected UserOperation"
                            );
                        }
                        core_execution::RejectionCause::StaleNonce {
                            user_nonce,
                            onchain_nonce,
                        } => {
                            tracing::warn!(
                                chain_id,
                                user_operation_hash = %hash,
                                user_nonce = %user_nonce,
                                onchain_nonce = %onchain_nonce,
                                "stale account nonce rejected UserOperation"
                            );
                        }
                        core_execution::RejectionCause::UnsupportedTempoFeeToken { fee_token } => {
                            tracing::warn!(
                                chain_id,
                                user_operation_hash = %hash,
                                fee_token = ?fee_token,
                                "Tempo UserOperation requested an unsupported fee token"
                            );
                        }
                    }
                    Out::Done
                }
                Err(error) => {
                    if let core_execution::RejectionCause::StaleNonce { .. } = cause {
                        tracing::warn!(
                            chain_id,
                            user_operation_hash = %hash,
                            %error,
                            "could not persist stale nonce rejection"
                        );
                    }
                    Out::Failed {
                        message: error.to_string(),
                    }
                }
            },
            Op::MarkRejectedWithReason {
                hash,
                stage,
                reason,
            } => {
                match engine
                    .store
                    .mark_rejected_with_executor_reason(hash, stage, reason)
                    .await
                {
                    Ok(_) => {
                        tracing::warn!(
                            chain_id,
                            user_operation_hash = %hash,
                            stage,
                            reason,
                            "in-band settlement rejected UserOperation"
                        );
                        Out::Done
                    }
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::DeferOperation { index, cause } => {
                let delayed = delayed_operation_from_routed(&self.operations[*index]);
                match engine
                    .store
                    .defer_user_operation(&delayed, engine.delayed_payload_ttl())
                    .await
                {
                    Ok(attempt) => {
                        match cause {
                            core_execution::DeferCause::AffordableMarketHold => {
                                tracing::info!(
                                    chain_id,
                                    user_operation_hash = %delayed.user_operation_hash,
                                    attempt,
                                    "holding UserOperation until the market fits its signed reimbursement"
                                );
                            }
                            core_execution::DeferCause::FutureNonce {
                                user_nonce,
                                onchain_nonce,
                            } => {
                                tracing::info!(
                                    chain_id,
                                    user_operation_hash = %delayed.user_operation_hash,
                                    user_nonce = %user_nonce,
                                    onchain_nonce = %onchain_nonce,
                                    attempt,
                                    "future account nonce moved to durable delayed inbox"
                                );
                            }
                        }
                        Out::Deferred { attempt }
                    }
                    Err(error) => {
                        match cause {
                            core_execution::DeferCause::AffordableMarketHold => {
                                tracing::warn!(
                                    chain_id,
                                    user_operation_hash = %delayed.user_operation_hash,
                                    %error,
                                    "could not hold UserOperation for a cheaper market"
                                );
                            }
                            core_execution::DeferCause::FutureNonce { .. } => {
                                tracing::warn!(
                                    chain_id,
                                    user_operation_hash = %delayed.user_operation_hash,
                                    %error,
                                    "could not persist future nonce in delayed inbox"
                                );
                            }
                        }
                        Out::Failed {
                            message: error.to_string(),
                        }
                    }
                }
            }
            Op::RecordDeferred {
                hash,
                stage,
                reason,
            } => {
                engine
                    .record_deferred_diagnostic(chain_id, hash, stage, reason)
                    .await;
                Out::Done
            }
            Op::NotifyIssue {
                hash,
                stage,
                reason,
            } => {
                engine.notify_executor_issue(chain_id, stage, hash, reason);
                Out::Done
            }
            Op::AcquireLaneLease => {
                let acquired = engine
                    .store
                    .acquire_lease(
                        &self.lease_scope,
                        &self.lease_token,
                        engine.config.lease_ttl,
                    )
                    .await
                    .unwrap_or(false);
                if acquired {
                    self.lease_acquired = true;
                    self.start_heartbeat();
                }
                Out::LeaseAcquired { acquired }
            }
            Op::EnsureLaneLease => {
                match engine
                    .store
                    .renew_lease(
                        &self.lease_scope,
                        &self.lease_token,
                        engine.config.lease_ttl,
                    )
                    .await
                {
                    Ok(held) => Out::LeaseHeld { held },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::LoadPreparedBundle => {
                match engine
                    .store
                    .get_prepared_bundle_intent(chain_id, self.lane)
                    .await
                {
                    Ok(intent) => Out::Intent { intent },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::ResumeBundleIntent { intent } => match engine.resume_bundle_intent(intent).await {
                Ok(disposition) => Out::Resumed {
                    known_outcome: disposition != BundleResumeDisposition::Unknown,
                },
                Err(error) => Out::Failed {
                    message: error.to_string(),
                },
            },
            Op::SimulateIndividually {
                entry_point,
                operations,
            } => {
                let packed = operations
                    .iter()
                    .map(|(_, packed)| packed.clone())
                    .collect::<Vec<_>>();
                let hashes = operations.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
                let verdicts = simulate_individually(
                    &engine.rpc,
                    chain_id,
                    *entry_point,
                    engine.relayer_addresses[self.lane as usize],
                    engine.treasury_address,
                    &engine.simulation_deployer,
                    &packed,
                    &hashes,
                )
                .await;
                Out::OperationVerdicts {
                    verdicts: verdicts
                        .into_iter()
                        .map(|verdict| match verdict {
                            SimulationVerdict::Success(_) => {
                                core_execution::OperationSimVerdict::Success
                            }
                            SimulationVerdict::NonceMismatch => {
                                core_execution::OperationSimVerdict::NonceMismatch
                            }
                            SimulationVerdict::Rejected(reason) => {
                                core_execution::OperationSimVerdict::Rejected {
                                    reason: reason.to_string(),
                                }
                            }
                            SimulationVerdict::Pending(reason) => {
                                core_execution::OperationSimVerdict::Pending {
                                    reason: reason.to_string(),
                                }
                            }
                            SimulationVerdict::Transient(reason) => {
                                core_execution::OperationSimVerdict::Transient {
                                    reason: reason.to_string(),
                                }
                            }
                        })
                        .collect(),
                }
            }
            Op::FetchAccountNonces {
                entry_point,
                probes,
            } => {
                let calls = probes
                    .iter()
                    .map(|(sender, nonce)| RpcBatchCall {
                        method: "eth_call",
                        params: json!([{
                            "to": entry_point.to_string(),
                            "data": format!(
                                "0x{}",
                                hex::encode(get_nonce_calldata(*sender, *nonce))
                            ),
                        }, "latest"]),
                    })
                    .collect::<Vec<_>>();
                match engine.rpc.batch(chain_id, &calls).await {
                    Ok(responses) => Out::AccountNonces {
                        nonces: (0..probes.len())
                            .map(|index| {
                                response_abi_u256(&responses, index, "EntryPoint getNonce")
                                    .map_err(|error| {
                                        tracing::warn!(
                                            chain_id,
                                            %error,
                                            "could not decode EntryPoint account nonce"
                                        );
                                    })
                                    .ok()
                            })
                            .collect(),
                    },
                    Err(error) => {
                        tracing::warn!(
                            chain_id,
                            count = probes.len(),
                            %error,
                            "could not resolve account nonce mismatches"
                        );
                        Out::Failed {
                            message: error.to_string(),
                        }
                    }
                }
            }
            Op::SimulateBundle {
                entry_point,
                operations,
            } => {
                let packed = operations
                    .iter()
                    .map(|(_, packed)| packed.clone())
                    .collect::<Vec<_>>();
                let hashes = operations.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
                let verdict = simulate_bundle(
                    &engine.rpc,
                    chain_id,
                    *entry_point,
                    engine.relayer_addresses[self.lane as usize],
                    engine.treasury_address,
                    &engine.simulation_deployer,
                    &packed,
                    &hashes,
                )
                .await;
                Out::BundleVerdict {
                    verdict: match verdict {
                        SimulationVerdict::Success(simulation) => {
                            core_execution::BundleSimVerdict::Success(
                                core_execution::BundleSimulationData {
                                    gas_used: simulation.gas_used,
                                    operation_gas_used: simulation
                                        .events
                                        .iter()
                                        .map(|event| event.actual_gas_used)
                                        .collect(),
                                    logs: simulation
                                        .logs
                                        .iter()
                                        .map(|log| SettlementLog {
                                            address: log.address,
                                            topics: log.topics.clone(),
                                            data: log.data.clone(),
                                        })
                                        .collect(),
                                },
                            )
                        }
                        SimulationVerdict::NonceMismatch => {
                            core_execution::BundleSimVerdict::NonceMismatch
                        }
                        SimulationVerdict::Rejected(reason) => {
                            core_execution::BundleSimVerdict::Rejected {
                                reason: reason.to_string(),
                            }
                        }
                        SimulationVerdict::Pending(reason) => {
                            core_execution::BundleSimVerdict::Pending {
                                reason: reason.to_string(),
                            }
                        }
                        SimulationVerdict::Transient(reason) => {
                            core_execution::BundleSimVerdict::Transient {
                                reason: reason.to_string(),
                            }
                        }
                    },
                }
            }
            Op::FetchTransactionContext {
                entry_point,
                calldata,
            } => {
                let calldata: Bytes = calldata.clone().into();
                match engine
                    .transaction_context(
                        chain_id,
                        engine.relayer_addresses[self.lane as usize],
                        *entry_point,
                        &calldata,
                    )
                    .await
                {
                    Ok(context) => Out::Context {
                        context: core_execution::TransactionContext {
                            estimated_gas: context.estimated_gas,
                            base_fee_per_gas: context.base_fee_per_gas,
                            max_fee_per_gas: context.max_fee_per_gas,
                            max_priority_fee_per_gas: context.max_priority_fee_per_gas,
                            nonce: context.nonce,
                            relayer_balance: context.relayer_balance,
                        },
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::FetchMarketPrice => {
                let Some(assets) = &self.assets else {
                    return Out::Failed {
                        message: "chain assets were not resolved".into(),
                    };
                };
                match engine
                    .market_usd_price(chain_id, &assets.native_symbol)
                    .await
                {
                    Ok(price) => Out::Price { price },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::EnsureRelayerFunded {
                relayer_balance,
                required_prefund,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                top_up_max,
            } => {
                match engine
                    .ensure_relayer_funded(
                        chain_id,
                        engine.relayer_addresses[self.lane as usize],
                        *relayer_balance,
                        *required_prefund,
                        *max_fee_per_gas,
                        *max_priority_fee_per_gas,
                        *top_up_max,
                    )
                    .await
                {
                    Ok(readiness) => Out::Funding {
                        ready: readiness == FundingReadiness::Ready,
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::SignBundle { request } => {
                match sign_eip1559(
                    &engine.relayer_keys[self.lane as usize],
                    TransactionPlan {
                        chain_id,
                        nonce: request.nonce,
                        gas_limit: request.gas_limit,
                        max_fee_per_gas: request.max_fee_per_gas,
                        max_priority_fee_per_gas: request.max_priority_fee_per_gas,
                        to: request.entry_point,
                        value: U256::ZERO,
                        input: request.calldata.clone().into(),
                    },
                ) {
                    Ok(signed) => Out::Signed {
                        signed: core_execution::SignedBundle {
                            raw_transaction_hex: format!(
                                "0x{}",
                                hex::encode(&signed.raw_transaction)
                            ),
                            transaction_hash: signed.transaction_hash,
                            nonce: signed.nonce,
                        },
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::SavePreparedBundle { intent } => {
                match engine.store.save_prepared_bundle_intent(intent).await {
                    Ok(saved) => Out::Saved { saved },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::CheckBroadcastSeen { transaction_hash } => Out::Seen {
                seen: engine.recently_confirmed_broadcast(transaction_hash).await,
            },
            Op::BroadcastRaw {
                raw_transaction,
                transaction_hash: _,
            } => match engine
                .rpc
                .broadcast_raw_transaction(chain_id, raw_transaction)
                .await
            {
                Ok(outcome) => Out::Sent {
                    reply: match outcome {
                        BroadcastOutcome::Accepted(hash) => {
                            core_execution::BroadcastReply::Accepted {
                                transaction_hash: hash,
                            }
                        }
                        BroadcastOutcome::Ambiguous(reason) => {
                            core_execution::BroadcastReply::Ambiguous { reason }
                        }
                        BroadcastOutcome::Rejected(reason) => {
                            core_execution::BroadcastReply::Rejected { reason }
                        }
                    },
                },
                Err(error) => Out::Failed {
                    message: error.to_string(),
                },
            },
            Op::RememberBroadcast { transaction_hash } => {
                engine.remember_confirmed_broadcast(transaction_hash).await;
                Out::Done
            }
            Op::ForgetBroadcast { transaction_hash } => {
                engine.broadcast_seen.lock().await.remove(transaction_hash);
                Out::Done
            }
            Op::ProbeTransactionKnown { transaction_hash } => Out::Known {
                known: engine
                    .transaction_is_known(chain_id, transaction_hash)
                    .await,
            },
            Op::ProbeStaleNonce { intent } => Out::Stale {
                stale: engine.bundle_nonce_is_stale(intent).await,
            },
            Op::ClearStaleIntent { intent, reason } => {
                match engine.clear_stale_bundle_intent(intent, reason).await {
                    Ok(()) => Out::Done,
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::RecordUnprovenBroadcast {
                transaction_hash,
                ambiguous,
                reason,
            } => {
                if *ambiguous {
                    tracing::warn!(
                        chain_id,
                        lane = self.lane,
                        transaction_hash = %transaction_hash,
                        reason,
                        "ambiguous handleOps broadcast is not yet observable"
                    );
                } else {
                    tracing::warn!(
                        chain_id,
                        lane = self.lane,
                        transaction_hash = %transaction_hash,
                        reason,
                        "rejected broadcast is unproven; retaining exact handleOps outbox"
                    );
                }
                Out::Done
            }
            Op::MarkBundleSubmitted { intent, gas_limit } => {
                match engine
                    .store
                    .mark_bundle_submitted(
                        chain_id,
                        &intent.transaction_hash,
                        &intent.user_operation_hashes,
                    )
                    .await
                {
                    Ok(indexed) => {
                        if indexed == intent.user_operation_hashes.len() {
                            if intent.raw_transaction.starts_with("0x76") {
                                tracing::info!(
                                    chain_id,
                                    lane = self.lane,
                                    relayer = %engine.relayer_addresses[self.lane as usize],
                                    transaction_hash = %intent.transaction_hash,
                                    nonce = intent.nonce,
                                    operations = intent.user_operation_hashes.len(),
                                    gas_limit,
                                    fee_token = %tempo::PATH_USD,
                                    "submitted Tempo 0x76 handleOps transaction"
                                );
                            } else {
                                tracing::info!(
                                    chain_id,
                                    lane = self.lane,
                                    relayer = %engine.relayer_addresses[self.lane as usize],
                                    transaction_hash = %intent.transaction_hash,
                                    nonce = intent.nonce,
                                    operations = intent.user_operation_hashes.len(),
                                    gas_limit,
                                    "submitted handleOps transaction"
                                );
                            }
                        }
                        Out::Indexed { indexed }
                    }
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::FetchTempoContext => {
                match engine
                    .tempo_transaction_context(
                        chain_id,
                        engine.relayer_addresses[self.lane as usize],
                    )
                    .await
                {
                    Ok(context) => Out::TempoContext {
                        base_fee_atto: context.base_fee_atto,
                        nonce: context.nonce,
                        relayer_path_usd_balance: context.relayer_path_usd_balance,
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::SignTempoBundle { request } => {
                match sign_tempo(
                    &engine.relayer_keys[self.lane as usize],
                    TempoTransactionPlan {
                        chain_id,
                        nonce: request.nonce,
                        gas_limit: request.gas_limit,
                        max_fee_per_gas: request.max_fee_per_gas,
                        max_priority_fee_per_gas: 0,
                        fee_token: tempo::PATH_USD,
                        to: request.entry_point,
                        input: request.calldata.clone().into(),
                    },
                ) {
                    Ok(signed) => Out::Signed {
                        signed: core_execution::SignedBundle {
                            raw_transaction_hex: format!(
                                "0x{}",
                                hex::encode(&signed.raw_transaction)
                            ),
                            transaction_hash: signed.transaction_hash,
                            nonce: signed.nonce,
                        },
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::EnsureTempoRelayerFunded {
                relayer_path_usd_balance,
                required_prefund,
                outer_max_fee,
            } => {
                match engine
                    .ensure_tempo_relayer_funded(
                        chain_id,
                        engine.relayer_addresses[self.lane as usize],
                        *relayer_path_usd_balance,
                        *required_prefund,
                        *outer_max_fee,
                    )
                    .await
                {
                    Ok(readiness) => Out::Funding {
                        ready: readiness == FundingReadiness::Ready,
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
        }
    }

    fn start_heartbeat(&mut self) {
        let engine = self.engine.clone();
        let scope = self.lease_scope.clone();
        let token = self.lease_token.clone();
        let ttl = engine.config.lease_ttl;
        self.heartbeat = Some(tokio::spawn(async move {
            let period = (ttl / 3).max(Duration::from_millis(1));
            let start = tokio::time::Instant::now() + period;
            let mut heartbeat = tokio::time::interval_at(start, period);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                heartbeat.tick().await;
                if let Err(error) = engine.ensure_lease(&scope, &token).await {
                    tracing::warn!(%error, "lane lease heartbeat stopped");
                    return;
                }
            }
        }));
    }

    async fn finish(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        if self.lease_acquired
            && let Err(error) = self
                .engine
                .store
                .release_lease(&self.lease_scope, &self.lease_token)
                .await
        {
            tracing::warn!(
                chain_id = self.chain_id,
                lane = self.lane,
                %error,
                "could not release relayer lane lease"
            );
        }
    }
}

/// Converts Tempo's `attodollars/gas` price to micro-pathUSD, always rounding up so the relay
/// never accepts an in-band reimbursement below the cost it is about to front.
fn tempo_cost_in_path_usd(gas: U256, price_atto: U256) -> Result<U256, ExecutorItemError> {
    tempo::tempo_cost_in_path_usd(gas, price_atto)
        .ok_or_else(|| ExecutorItemError("Tempo pathUSD cost overflow".into()))
}

fn validate_raw_transaction(
    raw_transaction: &str,
    transaction_hash: &str,
) -> Result<Vec<u8>, ExecutorItemError> {
    vela_relay_core::broadcast::validate_raw_transaction(raw_transaction, transaction_hash)
        .map_err(|error| ExecutorItemError(error.to_string()))
}

fn delayed_operation_from_routed(routed: &RoutedUserOperation) -> DelayedUserOperation {
    DelayedUserOperation {
        schema_version: routed.schema_version,
        user_operation_hash: routed.user_operation_hash.to_ascii_lowercase(),
        chain_id: routed.chain_id,
        entry_point: routed.entry_point.clone(),
        user_operation: routed.user_operation.clone(),
        sender: routed.sender.clone(),
        lane: routed.lane,
        stream: routed.stream.clone(),
        partition_id: routed.partition_id,
        offset: routed.offset,
    }
}

fn routed_operation_from_delayed(operation: &DelayedUserOperation) -> RoutedUserOperation {
    RoutedUserOperation {
        schema_version: operation.schema_version,
        user_operation_hash: operation.user_operation_hash.clone(),
        chain_id: operation.chain_id,
        entry_point: operation.entry_point.clone(),
        user_operation: operation.user_operation.clone(),
        sender: operation.sender.clone(),
        lane: operation.lane,
        stream: operation.stream.clone(),
        partition_id: operation.partition_id,
        offset: operation.offset,
    }
}

fn item_error(message: &str) -> Result<(), UserOperationHandlerError> {
    Err(Box::new(ExecutorItemError(message.into())))
}

fn store_item_error(error: impl Display) -> ExecutorItemError {
    ExecutorItemError(error.to_string())
}

fn rpc_item_error(error: RpcError) -> ExecutorItemError {
    ExecutorItemError(error.to_string())
}

fn response_value<'a>(
    responses: &'a [Result<Value, RpcError>],
    index: usize,
    method: &str,
) -> Result<&'a Value, ExecutorItemError> {
    match responses.get(index) {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(ExecutorItemError(format!("{method} failed: {error}"))),
        None => Err(ExecutorItemError(format!(
            "{method} is missing from the RPC batch response"
        ))),
    }
}

fn response_quantity(
    responses: &[Result<Value, RpcError>],
    index: usize,
    method: &str,
) -> Result<U256, ExecutorItemError> {
    response_value(responses, index, method)?
        .as_str()
        .and_then(parse_quantity)
        .ok_or_else(|| ExecutorItemError(format!("{method} returned an invalid quantity")))
}

fn response_abi_u256(
    responses: &[Result<Value, RpcError>],
    index: usize,
    method: &str,
) -> Result<U256, ExecutorItemError> {
    let bytes = response_value(responses, index, method)?
        .as_str()
        .and_then(parse_hex_bytes)
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| ExecutorItemError(format!("{method} returned invalid ABI data")))?;
    Ok(U256::from_be_slice(&bytes))
}

fn response_quantity_optional(responses: &[Result<Value, RpcError>], index: usize) -> Option<U256> {
    responses
        .get(index)
        .and_then(|response| response.as_ref().ok())
        .and_then(Value::as_str)
        .and_then(parse_quantity)
}

fn parse_quantity(value: &str) -> Option<U256> {
    let digits = value.strip_prefix("0x")?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    U256::from_str_radix(digits, 16).ok()
}

// Settlement reason strings live in `vela_relay_core::settlement`; the hold
// ladder and budget in `vela_relay_core::hold`; broadcast judgement and
// funding policy in their core modules.
use vela_relay_core::broadcast::{nonce_too_low, parse_hex_bytes};
use vela_relay_core::execution as core_execution;
use vela_relay_core::funding::{
    TOP_UP_GAS_LIMIT, native_top_up_reserve, plan_native_top_up, treasury_affordable_top_up,
};
use vela_relay_core::settlement::parse_market_usd_price;

fn failure_results(count: usize, message: &str) -> UserOperationBatchResults {
    (0..count).map(|_| item_error(message)).collect()
}

fn unique_token(prefix: &str) -> String {
    let counter = LEASE_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}:{}:{timestamp}:{counter}", std::process::id())
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use serde_json::json;

    use super::{ExecutorItemError, RpcError, parse_quantity, response_quantity};

    #[test]
    fn parses_canonical_rpc_quantities_only() {
        assert_eq!(parse_quantity("0x0"), Some(U256::ZERO));
        assert_eq!(parse_quantity("0x2a"), Some(U256::from(42u8)));
        assert_eq!(parse_quantity("0xABC"), Some(U256::from(0xabcu16)));
        for invalid in ["", "0x", "2a", "0x00", "0xgg"] {
            assert_eq!(parse_quantity(invalid), None, "{invalid}");
        }
    }

    // Money-math and broadcast-judgement tests moved to `vela_relay_core`
    // (settlement, funding, broadcast) with their functions.

    #[test]
    fn batch_quantity_helper_distinguishes_errors_and_invalid_values() {
        let responses = vec![
            Ok(json!("0x2a")),
            Err(RpcError::Unavailable),
            Ok(json!(null)),
        ];
        assert_eq!(
            response_quantity(&responses, 0, "eth_test").unwrap(),
            U256::from(42u8)
        );
        assert!(response_quantity(&responses, 1, "eth_test").is_err());
        assert!(response_quantity(&responses, 2, "eth_test").is_err());
        assert!(response_quantity(&responses, 3, "eth_test").is_err());
    }

    #[test]
    fn executor_item_error_is_sendable() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ExecutorItemError>();
    }
}
