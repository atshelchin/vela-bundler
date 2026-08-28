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
            let response =
                rpc_dispatch(chain_id, user_rpc_url.as_deref(), &env, &config, &body).await;
            json_response(&response)
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
        // Read-side methods that consult chain RPCs land with the second US1
        // change set (tasks T009/T011 remain open until they do).
        RpcMethod::GetUserOperationGasPrice => {
            RpcResponse::error(request.id, RpcError::gas_price_unavailable())
        }
        RpcMethod::EstimateUserOperationGas => {
            RpcResponse::error(request.id, RpcError::estimation_unavailable())
        }
        RpcMethod::GetInBandGasQuote => {
            RpcResponse::error(request.id, RpcError::in_band_gas_quote_unavailable())
        }
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
