//! Env/secrets → validated policy values, injected into the core as data
//! (Constitution II). Bounds mirror the docker parser where they apply; the
//! Iggy fixed-routing width pin deliberately does NOT (research.md R11).

use vela_relay_core::vault;
use worker::Env;

#[derive(Clone)]
#[expect(
    dead_code,
    reason = "relayer_count/executor_enabled/execution_chains are consumed by the US2 queue consumer and LaneDO; parsed now so configuration errors surface before execution lands."
)]
pub struct CfConfig {
    pub settlement_recipient: Option<String>,
    pub relayer_count: usize,
    pub executor_enabled: bool,
    /// Empty = dynamic directory-driven chains (research.md R10).
    pub execution_chains: Vec<u64>,
    pub alchemy_api_key: Option<String>,
}

impl CfConfig {
    pub fn from_env(env: &Env) -> Result<Self, String> {
        let relayer_count = match var(env, "VELA_RELAY_RELAYER_COUNT") {
            Some(value) => value
                .parse::<usize>()
                .map_err(|error| format!("invalid VELA_RELAY_RELAYER_COUNT: {error}"))?,
            None => vault::RELAYER_ROUTING_WIDTH,
        };
        if relayer_count == 0 || relayer_count > vault::RELAYER_POOL_SIZE {
            return Err(format!(
                "VELA_RELAY_RELAYER_COUNT must be 1..={}",
                vault::RELAYER_POOL_SIZE
            ));
        }

        let executor_enabled = match var(env, "VELA_RELAY_EXECUTOR_ENABLED") {
            Some(value) => parse_bool("VELA_RELAY_EXECUTOR_ENABLED", &value)?,
            None => true,
        };

        let operator_secret = secret(env, "OPERATOR_SECRET");
        if executor_enabled && operator_secret.is_none() {
            return Err(
                "OPERATOR_SECRET is required when the UserOperation consumer is enabled".into(),
            );
        }

        // Same rule as the docker parser: an explicitly configured recipient
        // must match the address derived from the operator secret.
        let configured_recipient = var(env, "VELA_RELAY_SETTLEMENT_RECIPIENT");
        let settlement_recipient = match (&operator_secret, configured_recipient) {
            (Some(secret_value), configured) => {
                let derived = vault::derive_address(secret_value)
                    .map_err(|error| format!("invalid OPERATOR_SECRET: {error}"))?;
                if let Some(configured) = configured
                    && !configured.eq_ignore_ascii_case(&derived)
                {
                    return Err(
                        "VELA_RELAY_SETTLEMENT_RECIPIENT does not match the address derived from OPERATOR_SECRET"
                            .into(),
                    );
                }
                Some(derived)
            }
            (None, configured) => configured,
        };

        let execution_chains = match var(env, "VELA_RELAY_EXECUTION_CHAINS") {
            None => Vec::new(),
            Some(value) if value.trim().is_empty() => Vec::new(),
            Some(value) => value
                .split(',')
                .map(|chain| {
                    chain
                        .trim()
                        .parse::<u64>()
                        .map_err(|error| format!("invalid VELA_RELAY_EXECUTION_CHAINS: {error}"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        };

        Ok(Self {
            settlement_recipient,
            relayer_count,
            executor_enabled,
            execution_chains,
            alchemy_api_key: secret(env, "ALCHEMY_API_KEY").filter(|key| !key.trim().is_empty()),
        })
    }
}

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|value| value.to_string())
}

fn secret(env: &Env, name: &str) -> Option<String> {
    env.secret(name).ok().map(|value| value.to_string())
}

fn parse_bool(name: &str, value: &str) -> Result<bool, String> {
    // Same vocabulary as the docker parser.
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(format!("invalid {name}; expected true or false")),
    }
}
