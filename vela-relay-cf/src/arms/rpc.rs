//! Chain JSON-RPC over fetch with the docker shell's failover ORDER
//! (user-header URL → Alchemy → directory fallbacks). Failover/cooldown are
//! shell-owned transport policy (Constitution, Shell-owned concerns).

use serde_json::{Value, json};
use worker::Env;

use super::market;
use crate::config::CfConfig;

/// The same request header the docker shell honors for caller-supplied RPCs.
pub const USER_RPC_URL_HEADER: &str = "x-vela-rpc-url";

pub async fn call(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    method: &str,
    params: Value,
) -> Result<Value, ()> {
    if let Some(url) = user_rpc_url.map(str::trim).filter(|url| is_rpc_url(url)) {
        if let Ok(Some(result)) = market::json_rpc(url, method, &params).await {
            return Ok(result);
        }
    } else if user_rpc_url.is_some() {
        worker::console_warn!("ignored invalid user RPC URL header");
    }

    if let Some(api_key) = &config.alchemy_api_key
        && let Some(url) = vela_relay_core::alchemy::rpc_url(chain_id, api_key)
        && let Ok(Some(result)) = market::json_rpc(&url, method, &params).await
    {
        return Ok(result);
    }

    let fallback_urls = match market::fallback_rpc_urls(env, chain_id).await {
        Ok(urls) => urls,
        Err(error) => {
            worker::console_warn!("could not fetch fallback RPC URLs: {error}");
            return Err(());
        }
    };
    for url in fallback_urls {
        if let Ok(Some(result)) = market::json_rpc(&url, method, &params).await {
            return Ok(result);
        }
    }
    Err(())
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
    let value = result.as_str().ok_or(())?;
    let value = value.strip_prefix("0x").ok_or(())?;
    let decimals = u32::from_str_radix(value, 16).map_err(|_| ())?;
    (decimals <= 38).then_some(decimals).ok_or(())
}

fn is_rpc_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && !url.contains("${")
}
