//! Chain JSON-RPC over fetch with the docker shell's failover ORDER
//! (user-header URL → Alchemy → directory fallbacks). Failover/cooldown are
//! shell-owned transport policy (Constitution, Shell-owned concerns).

use serde_json::{Value, json};
use worker::Env;

use super::market;
use crate::config::CfConfig;

/// The same request header the docker shell honors for caller-supplied RPCs.
pub const USER_RPC_URL_HEADER: &str = "x-vela-rpc-url";

/// Mirror of the docker shell's `RpcCallResult` surface consumed here: the
/// JSON-RPC result plus the answering host (for `x-vela-rpc-domain`).
pub struct CallResult {
    pub value: Value,
    pub domain: String,
}

pub async fn call(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    method: &str,
    params: Value,
) -> Result<CallResult, ()> {
    if let Some(url) = user_rpc_url.map(str::trim).filter(|url| is_rpc_url(url)) {
        if let Ok(Some(value)) = market::json_rpc(url, method, &params).await {
            return Ok(CallResult {
                value,
                domain: rpc_domain(url),
            });
        }
    } else if user_rpc_url.is_some() {
        worker::console_warn!("ignored invalid user RPC URL header");
    }

    if let Some(api_key) = &config.alchemy_api_key
        && let Some(url) = vela_relay_core::alchemy::rpc_url(chain_id, api_key)
        && let Ok(Some(value)) = market::json_rpc(&url, method, &params).await
    {
        return Ok(CallResult {
            domain: rpc_domain(&url),
            value,
        });
    }

    let fallback_urls = match market::fallback_rpc_urls(env, chain_id).await {
        Ok(urls) => urls,
        Err(error) => {
            worker::console_warn!("could not fetch fallback RPC URLs: {error}");
            return Err(());
        }
    };
    for url in fallback_urls {
        if let Ok(Some(value)) = market::json_rpc(&url, method, &params).await {
            return Ok(CallResult {
                domain: rpc_domain(&url),
                value,
            });
        }
    }
    Err(())
}

fn rpc_domain(value: &str) -> String {
    worker::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unknown>".into())
}

/// Simulation failover with the docker semantics: an upstream JSON-RPC error
/// classified as a contract revert (core `estimate::is_execution_revert`)
/// stops the failover and surfaces; anything else tries the next source.
pub async fn call_simulation(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    method: &str,
    params: Value,
) -> Result<CallResult, vela_relay_core::estimate::SimulationCallError> {
    use vela_relay_core::estimate::{SimulationCallError, is_execution_revert};

    let mut sources: Vec<String> = Vec::new();
    if let Some(url) = user_rpc_url.map(str::trim).filter(|url| is_rpc_url(url)) {
        sources.push(url.to_owned());
    } else if user_rpc_url.is_some() {
        worker::console_warn!("ignored invalid user RPC URL header");
    }
    if let Some(api_key) = &config.alchemy_api_key
        && let Some(url) = vela_relay_core::alchemy::rpc_url(chain_id, api_key)
    {
        sources.push(url);
    }
    if let Ok(fallback_urls) = market::fallback_rpc_urls(env, chain_id).await {
        sources.extend(fallback_urls);
    }

    for url in sources {
        match market::json_rpc_simulation(&url, method, &params).await {
            Ok(market::SimulationReply::Result(value)) => {
                return Ok(CallResult {
                    domain: rpc_domain(&url),
                    value,
                });
            }
            Ok(market::SimulationReply::UpstreamError(revert)) => {
                if is_execution_revert(&revert) {
                    return Err(SimulationCallError::Reverted(revert));
                }
                // Non-revert JSON-RPC error (rate limit, missing state
                // override support, …): try the next source.
            }
            Err(_) => {}
        }
    }
    Err(SimulationCallError::Unavailable)
}

/// `decimals()` via `eth_call`, exactly as the docker arm: ≤ 38 accepted.
pub async fn erc20_decimals(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    token: &str,
) -> Result<u32, ()> {
    let result = call(
        config,
        env,
        chain_id,
        user_rpc_url,
        "eth_call",
        json!([
            { "to": token, "data": "0x313ce567" },
            "latest",
        ]),
    )
    .await?;
    let value = result.value.as_str().ok_or(())?;
    let value = value.strip_prefix("0x").ok_or(())?;
    let decimals = u32::from_str_radix(value, 16).map_err(|_| ())?;
    (decimals <= 38).then_some(decimals).ok_or(())
}

fn is_rpc_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && !url.contains("${")
}
