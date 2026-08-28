//! Chain-directory metadata over fetch, cached in KV (caches only — FR-006).
//! Mirrors the docker shell's `utils::rpc` metadata path: same directory URL,
//! same retry cadence, same filtering; transport itself is shell-owned.

use serde::Deserialize;
use serde_json::Value;
use worker::{Delay, Env, Fetch, Headers, Method, Request, RequestInit};

const RPC_LIST_URL: &str = "https://ethereum-data.awesometools.dev/chains/eip155-";
const METADATA_REQUEST_ATTEMPTS: usize = 3;
const METADATA_CACHE_TTL_SECS: u64 = 60 * 60;
const KV_BINDING: &str = "CACHE";

#[derive(Clone, Deserialize)]
pub struct ChainMetadata {
    #[serde(default)]
    pub rpc: Vec<String>,
    #[serde(default)]
    pub stables: Vec<StablecoinMetadata>,
    #[serde(rename = "nativeCurrency")]
    pub native_currency: Option<NativeCurrencyMetadata>,
}

#[derive(Clone, Deserialize)]
pub struct StablecoinMetadata {
    pub contract: String,
}

#[derive(Clone, Deserialize)]
pub struct NativeCurrencyMetadata {
    pub decimals: u32,
}

pub struct SettlementAssets {
    pub native_decimals: u32,
    pub stablecoins: Vec<String>,
}

/// The admission arm's `LoadSettlementAssets` source (docker:
/// `rpc::settlement_assets`): native decimals + allowlisted stablecoin
/// contracts from the controlled chain directory.
pub async fn settlement_assets(env: &Env, chain_id: u64) -> Result<SettlementAssets, ()> {
    let metadata = chain_metadata(env, chain_id).await.map_err(|error| {
        worker::console_warn!("could not fetch chain metadata for in-band payments: {error}");
    })?;
    let native = metadata.native_currency.ok_or_else(|| {
        worker::console_warn!("chain {chain_id} metadata does not declare a native currency");
    })?;

    Ok(SettlementAssets {
        native_decimals: native.decimals,
        stablecoins: metadata
            .stables
            .into_iter()
            .filter(|stable| is_hex_address(&stable.contract))
            .map(|stable| stable.contract)
            .collect(),
    })
}

pub async fn fallback_rpc_urls(env: &Env, chain_id: u64) -> Result<Vec<String>, String> {
    let metadata = chain_metadata(env, chain_id).await?;
    Ok(metadata
        .rpc
        .into_iter()
        .filter(|url| is_plain_http_url(url))
        .collect())
}

async fn chain_metadata(env: &Env, chain_id: u64) -> Result<ChainMetadata, String> {
    let cache_key = format!("chainmeta:{chain_id}");
    if let Ok(kv) = env.kv(KV_BINDING)
        && let Ok(Some(cached)) = kv.get(&cache_key).text().await
        && let Ok(metadata) = serde_json::from_str::<ChainMetadata>(&cached)
    {
        return Ok(metadata);
    }

    let url = format!("{RPC_LIST_URL}{chain_id}.json");
    let mut last_error = None;
    for attempt in 1..=METADATA_REQUEST_ATTEMPTS {
        match fetch_json(&url).await {
            Ok(body) => {
                if let Ok(metadata) = serde_json::from_str::<ChainMetadata>(&body) {
                    if let Ok(kv) = env.kv(KV_BINDING) {
                        let _ = kv
                            .put(&cache_key, body)
                            .and_then(|put| Ok(put.expiration_ttl(METADATA_CACHE_TTL_SECS)))
                            .map(|put| put.execute());
                    }
                    return Ok(metadata);
                }
                last_error = Some("metadata body is not valid chain metadata".to_owned());
            }
            Err(error) => last_error = Some(error),
        }
        if attempt < METADATA_REQUEST_ATTEMPTS {
            Delay::from(std::time::Duration::from_millis(100 * attempt as u64)).await;
        }
    }

    Err(format!(
        "metadata request failed after {METADATA_REQUEST_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "unknown error".into())
    ))
}

async fn fetch_json(url: &str) -> Result<String, String> {
    let headers = Headers::new();
    headers
        .set("accept", "application/json")
        .map_err(|error| error.to_string())?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let request = Request::new_with_init(url, &init).map_err(|error| error.to_string())?;
    let mut response = Fetch::Request(request)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status_code() >= 400 {
        return Err(format!(
            "metadata request returned {}",
            response.status_code()
        ));
    }
    response.text().await.map_err(|error| error.to_string())
}

pub fn is_hex_address(address: &str) -> bool {
    address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_plain_http_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && !url.contains("${")
}

/// Fetch a JSON-RPC result `Value` from one upstream. Ok(None) = this
/// upstream answered with an error/invalid envelope (try the next); Err =
/// transport failure (try the next).
pub async fn json_rpc(url: &str, method: &str, params: &Value) -> Result<Option<Value>, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let headers = Headers::new();
    headers
        .set("content-type", "application/json")
        .map_err(|error| error.to_string())?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(
            &body.to_string(),
        )));
    let request = Request::new_with_init(url, &init).map_err(|error| error.to_string())?;
    let mut response = Fetch::Request(request)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let value: Value = response.json().await.map_err(|error| error.to_string())?;
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Ok(None);
    }
    match value.get("result") {
        Some(result) if !result.is_null() => Ok(Some(result.clone())),
        _ => Ok(None),
    }
}
