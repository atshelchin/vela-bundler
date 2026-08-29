//! The fetch surface: operational GETs + `POST /{chain_id}` JSON-RPC.
//! Envelope parsing, validation, and rendering come from
//! `vela_relay_core::wire` — the same bytes as the docker shell.

use serde_json::{Value, json};
use vela_relay_core::admission::SUPPORTED_ENTRY_POINTS;
use vela_relay_core::wire::{
    self, GetUserOperationByHashParams, GetUserOperationReceiptParams,
    GetUserOperationStatusParams, RpcError, RpcMethod, RpcResponse, UserOperationByHash,
    UserOperationStatus, UserOperationStatusKind,
};
use worker::{Env, Request, Response, Result};

use crate::{
    admission,
    arms::rpc::USER_RPC_URL_HEADER,
    config::CfConfig,
    proto::{RecordCommand, RecordReply},
};

pub async fn handle(mut req: Request, env: Env) -> Result<Response> {
    let path = req.path();
    let method = req.method();

    match (method, path.as_str()) {
        (worker::Method::Get, "/") => {
            Response::from_json(&json!({"name": "vela-relay", "status": "ok"}))
        }
        (worker::Method::Get, "/health") | (worker::Method::Get, "/api/health") => {
            // Same shape as the docker shell; the runtime field truthfully
            // names this deployment's runtime (declared delta,
            // contracts/deployment-parity.md).
            let response = Response::from_json(
                &json!({"service": "vela-relay", "runtime": "workerd", "status": "ok"}),
            )?;
            let headers = response.headers().clone();
            headers.set("cache-control", "no-cache, no-store, must-revalidate")?;
            Ok(response.with_headers(headers))
        }
        (worker::Method::Get, "/healthz") => Ok(Response::empty()?.with_status(204)),
        (worker::Method::Get, "/readyz") => readiness(&env),
        (worker::Method::Get, "/version") => Response::from_json(&json!({
            "name": "vela-relay",
            "version": env!("CARGO_PKG_VERSION"),
            "build": option_env!("VELA_RELAY_BUILD").unwrap_or("dev"),
        })),
        (worker::Method::Post, _) => {
            let Some(chain_id) = parse_chain_path(&path) else {
                return Response::error("not found", 404);
            };
            let config = match CfConfig::from_env(&env) {
                Ok(config) => config,
                Err(error) => {
                    worker::console_error!("configuration error: {error}");
                    return Response::error("configuration error", 500);
                }
            };
            let user_rpc_url = req.headers().get(USER_RPC_URL_HEADER).ok().flatten();
            let body = req.bytes().await?;
            let mut rpc_domain = None;
            let response = rpc_dispatch(
                chain_id,
                user_rpc_url.as_deref(),
                &env,
                &config,
                &body,
                &mut rpc_domain,
            )
            .await;
            let http_response = json_response(&response)?;
            if let Some(domain) = rpc_domain {
                let headers = http_response.headers().clone();
                if headers.set("x-vela-rpc-domain", &domain).is_err() {
                    worker::console_warn!("could not add RPC domain response header");
                }
                return Ok(http_response.with_headers(headers));
            }
            Ok(http_response)
        }
        _ => Response::error("not found", 404),
    }
}

/// Same envelope flow as the docker shell's `rpc::handle`, over wire fns.
async fn rpc_dispatch(
    chain_id: u64,
    user_rpc_url: Option<&str>,
    env: &Env,
    config: &CfConfig,
    body: &[u8],
    rpc_domain: &mut Option<String>,
) -> RpcResponse<Value> {
    let request = match wire::parse_envelope(body) {
        Ok(request) => request,
        Err(error_response) => return error_response,
    };

    let method = match wire::validate_call(&request.method, request.params.clone()) {
        Ok(method) => method,
        Err(error) => return RpcResponse::error(request.id, error),
    };

    worker::console_log!(
        "bundler RPC request received: chain_id={chain_id} method={}",
        method.as_str()
    );

    match method {
        RpcMethod::SupportedEntryPoints => RpcResponse::result(
            request.id,
            Value::Array(
                SUPPORTED_ENTRY_POINTS
                    .iter()
                    .map(|entry_point| Value::String((*entry_point).to_owned()))
                    .collect(),
            ),
        ),
        RpcMethod::GetUserOperationStatus => {
            let params: GetUserOperationStatusParams = match serde_json::from_value(request.params)
            {
                Ok(params) => params,
                Err(error) => {
                    return RpcResponse::error(
                        request.id,
                        RpcError::invalid_params(error.to_string()),
                    );
                }
            };
            let GetUserOperationStatusParams([hash]) = params;
            match load_record(env, chain_id, &hash).await {
                Ok(Some(record)) => result_value(request.id, wire::rpc_status(&record)),
                Ok(None) => result_value(
                    request.id,
                    UserOperationStatus {
                        status: UserOperationStatusKind::NotFound,
                        transaction_hash: None,
                        last_executor_stage: None,
                        last_executor_error: None,
                        last_executor_attempt_at_ms: None,
                    },
                ),
                Err(error) => RpcResponse::error(request.id, error),
            }
        }
        RpcMethod::GetUserOperationByHash => {
            let params: GetUserOperationByHashParams = match serde_json::from_value(request.params)
            {
                Ok(params) => params,
                Err(error) => {
                    return RpcResponse::error(
                        request.id,
                        RpcError::invalid_params(error.to_string()),
                    );
                }
            };
            let GetUserOperationByHashParams([hash]) = params;
            match load_record(env, chain_id, &hash).await {
                Ok(Some(record)) => result_value(
                    request.id,
                    UserOperationByHash {
                        user_operation: record.user_operation,
                        entry_point: record.entry_point,
                        block_number: record.block_number,
                        block_hash: record.block_hash,
                        transaction_hash: record.transaction_hash,
                    },
                ),
                Ok(None) => RpcResponse::result(request.id, Value::Null),
                Err(error) => RpcResponse::error(request.id, error),
            }
        }
        RpcMethod::GetUserOperationReceipt => {
            let params: GetUserOperationReceiptParams = match serde_json::from_value(request.params)
            {
                Ok(params) => params,
                Err(error) => {
                    return RpcResponse::error(
                        request.id,
                        RpcError::invalid_params(error.to_string()),
                    );
                }
            };
            let GetUserOperationReceiptParams([hash]) = params;
            match load_record(env, chain_id, &hash).await {
                Ok(Some(record)) => RpcResponse::result(
                    request.id,
                    wire::receipt_response(&hash, &record).unwrap_or(Value::Null),
                ),
                Ok(None) => RpcResponse::result(request.id, Value::Null),
                Err(error) => RpcResponse::error(request.id, error),
            }
        }
        RpcMethod::SendUserOperation => {
            let params = match serde_json::from_value(request.params) {
                Ok(params) => params,
                Err(error) => {
                    return RpcResponse::error(
                        request.id,
                        RpcError::invalid_params(error.to_string()),
                    );
                }
            };
            admission::handle(request.id, chain_id, user_rpc_url, env, config, params).await
        }
        RpcMethod::GetUserOperationGasPrice => {
            match crate::arms::gas_price::user_operation_gas_prices(
                config,
                env,
                chain_id,
                user_rpc_url,
            )
            .await
            {
                Ok(quote) => {
                    *rpc_domain = Some(quote.rpc_domain);
                    result_value(request.id, gas_price_result(quote.tiers))
                }
                Err(error) => {
                    worker::console_warn!(
                        "could not estimate user operation gas prices: {error:?}"
                    );
                    RpcResponse::error(request.id, gas_price_error(error))
                }
            }
        }
        RpcMethod::GetInBandGasQuote => {
            let params: vela_relay_core::wire::GetInBandGasQuoteParams =
                match serde_json::from_value(request.params) {
                    Ok(params) => params,
                    Err(error) => {
                        return RpcResponse::error(
                            request.id,
                            RpcError::invalid_params(error.to_string()),
                        );
                    }
                };
            match crate::arms::quote::handle(
                config,
                env,
                chain_id,
                user_rpc_url,
                params.safe_address(),
            )
            .await
            {
                Ok((quotes, domain)) => {
                    *rpc_domain = Some(domain);
                    result_value(request.id, quotes)
                }
                Err(error) => RpcResponse::error(request.id, error),
            }
        }
        RpcMethod::EstimateUserOperationGas => {
            let params: vela_relay_core::wire::EstimateUserOperationGasParams =
                match serde_json::from_value(request.params) {
                    Ok(params) => params,
                    Err(error) => {
                        return RpcResponse::error(
                            request.id,
                            RpcError::invalid_params(error.to_string()),
                        );
                    }
                };
            match estimate_gas(config, env, chain_id, user_rpc_url, params).await {
                Ok((estimate, domain)) => {
                    *rpc_domain = Some(domain);
                    result_value(request.id, estimate)
                }
                Err(error) => RpcResponse::error(request.id, error),
            }
        }
    }
}

/// The same thin driver as the docker handler: plan → two simulation calls →
/// finish; every rule lives in `vela_relay_core::estimate`.
async fn estimate_gas(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    params: vela_relay_core::wire::EstimateUserOperationGasParams,
) -> std::result::Result<(vela_relay_core::wire::UserOperationGasEstimate, String), RpcError> {
    use vela_relay_core::estimate::{self, CallGasSource, SimulationCallError};

    let vela_relay_core::wire::EstimateUserOperationGasParams(
        user_operation,
        entry_point,
        state_overrides,
    ) = params;
    let plan = estimate::plan(
        chain_id,
        user_operation,
        &entry_point,
        state_overrides.as_ref(),
    )?;

    let validation = crate::arms::rpc::call_simulation(
        config,
        env,
        chain_id,
        user_rpc_url,
        "eth_call",
        plan.validation_params().clone(),
    )
    .await
    .map_err(estimate::simulation_error)?;

    let call_gas = match plan.execution_params() {
        None => CallGasSource::NotNeeded,
        Some(params) => {
            match crate::arms::rpc::call_simulation(
                config,
                env,
                chain_id,
                user_rpc_url,
                "eth_estimateGas",
                params.clone(),
            )
            .await
            {
                Ok(result) => CallGasSource::Estimated(result.value),
                Err(SimulationCallError::Reverted(error)) => CallGasSource::Reverted(error),
                Err(SimulationCallError::Unavailable) => CallGasSource::Unavailable,
            }
        }
    };

    let outcome = estimate::finish(&plan, &validation.value, call_gas)?;
    if let Some(fallback) = outcome.fallback_call_gas {
        worker::console_warn!(
            "could not estimate UserOperation call gas; returning the conservative fallback: chain_id={chain_id} fallback_call_gas_limit={fallback}"
        );
    }
    Ok((outcome.estimate, validation.domain))
}

/// The docker handler's tier → wire conversion, byte-for-byte.
fn gas_price_result(
    tiers: vela_relay_core::gas_math::GasPriceTiers,
) -> vela_relay_core::wire::UserOperationGasPrice {
    fn tier(price: vela_relay_core::gas_math::GasPrice) -> vela_relay_core::wire::GasPriceTier {
        vela_relay_core::wire::GasPriceTier {
            max_fee_per_gas: format!("0x{:x}", price.max_fee_per_gas),
            max_priority_fee_per_gas: format!("0x{:x}", price.max_priority_fee_per_gas),
        }
    }
    vela_relay_core::wire::UserOperationGasPrice {
        slow: tier(tiers.slow),
        standard: tier(tiers.standard),
        fast: tier(tiers.fast),
    }
}

fn gas_price_error(error: vela_relay_core::gas_math::GasPriceError) -> RpcError {
    match error {
        vela_relay_core::gas_math::GasPriceError::ResponseDeadlineExceeded => {
            RpcError::gas_price_timeout()
        }
        _ => RpcError::gas_price_unavailable(),
    }
}

async fn load_record(
    env: &Env,
    chain_id: u64,
    hash: &str,
) -> std::result::Result<Option<vela_relay_core::task::StoredUserOperation>, RpcError> {
    match admission::record_command(env, chain_id, hash, &RecordCommand::Get).await {
        Ok(RecordReply::Record { record }) => Ok(record.map(|record| *record)),
        _ => Err(RpcError::user_operation_status_store_unavailable()),
    }
}

fn result_value<T: serde::Serialize>(id: Value, value: T) -> RpcResponse<Value> {
    match serde_json::to_value(value) {
        Ok(value) => RpcResponse::result(id, value),
        Err(_) => RpcResponse::error(id, RpcError::backend_unavailable()),
    }
}

fn readiness(env: &Env) -> Result<Response> {
    let bindings_ok = env.durable_object("RECORDS").is_ok()
        && env.kv("CACHE").is_ok()
        && env.queue("OPS_QUEUE").is_ok();
    if bindings_ok {
        Ok(Response::empty()?.with_status(204))
    } else {
        Response::error("bindings unavailable", 503)
    }
}

fn parse_chain_path(path: &str) -> Option<u64> {
    path.strip_prefix('/')?.parse::<u64>().ok()
}

fn json_response(response: &RpcResponse<Value>) -> Result<Response> {
    Response::from_json(response)
}
