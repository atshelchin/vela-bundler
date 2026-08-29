//! Shell driver for `eth_estimateUserOperationGas`. Planning, decoding, and
//! every gas rule live in `vela_relay_core::estimate` (spec 002); this
//! handler performs the two simulation RPC calls and logs.

use axum::http::HeaderValue;
use serde_json::Value;
use vela_relay_core::estimate::{self, CallGasSource, SimulationCallError, SimulationRevert};

use crate::{
    app::rpc::types::{EstimateUserOperationGasParams, RpcError, RpcResponse},
    utils::rpc::{self, RpcRevert, RpcSimulationError},
};

pub async fn handle(
    id: Value,
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    params: EstimateUserOperationGasParams,
) -> (RpcResponse<Value>, Option<String>) {
    let EstimateUserOperationGasParams(user_operation, entry_point, state_overrides) = params;
    let result = estimate(
        chain_id,
        user_rpc_url,
        user_operation,
        entry_point,
        state_overrides,
    )
    .await;

    match result {
        Ok((estimate, rpc_domain)) => (
            RpcResponse::result(
                id,
                serde_json::to_value(estimate).expect("gas estimate response must serialize"),
            ),
            Some(rpc_domain),
        ),
        Err(error) => (RpcResponse::error(id, error), None),
    }
}

async fn estimate(
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    user_operation: vela_relay_core::wire::EstimatableUserOperation,
    entry_point: String,
    state_overrides: Option<vela_relay_core::wire::StateOverrideSet>,
) -> Result<(vela_relay_core::wire::UserOperationGasEstimate, String), RpcError> {
    let plan = estimate::plan(
        chain_id,
        user_operation,
        &entry_point,
        state_overrides.as_ref(),
    )?;

    let validation = rpc::call_simulation(
        chain_id,
        user_rpc_url,
        "eth_call",
        plan.validation_params().clone(),
    )
    .await
    .map_err(|error| estimate::simulation_error(call_error(error)))?;

    let call_gas = match plan.execution_params() {
        None => CallGasSource::NotNeeded,
        Some(params) => {
            match rpc::call_simulation(chain_id, user_rpc_url, "eth_estimateGas", params.clone())
                .await
            {
                Ok(result) => CallGasSource::Estimated(result.value),
                Err(RpcSimulationError::Reverted(error)) => CallGasSource::Reverted(revert(error)),
                Err(RpcSimulationError::Unavailable) => CallGasSource::Unavailable,
            }
        }
    };

    let outcome = estimate::finish(&plan, &validation.value, call_gas)?;
    if let Some(fallback) = outcome.fallback_call_gas {
        tracing::warn!(
            chain_id,
            fallback_call_gas_limit = fallback,
            "could not estimate UserOperation call gas; returning the conservative fallback"
        );
    }
    Ok((outcome.estimate, validation.domain))
}

fn call_error(error: RpcSimulationError) -> SimulationCallError {
    match error {
        RpcSimulationError::Reverted(error) => SimulationCallError::Reverted(revert(error)),
        RpcSimulationError::Unavailable => SimulationCallError::Unavailable,
    }
}

fn revert(error: RpcRevert) -> SimulationRevert {
    SimulationRevert {
        code: error.code,
        message: error.message,
        data: error.data,
    }
}
