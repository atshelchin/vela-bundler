//! Env/secrets → validated policy values, injected into the core as data
//! (Constitution II). Bounds mirror the docker parser where they apply; the
//! Iggy fixed-routing width pin deliberately does NOT (research.md R11).

use vela_relay_core::vault;
use worker::Env;

#[derive(Clone)]
pub struct CfConfig {
    pub settlement_recipient: Option<String>,
    pub relayer_count: usize,
    pub executor_enabled: bool,
    /// Empty = dynamic directory-driven chains (research.md R10).
    pub execution_chains: Vec<u64>,
    pub alchemy_api_key: Option<String>,
    pub operator_secret: Option<String>,
    // Executor policy values — same names, defaults, and bounds as the docker
    // parser; injected into the core as data.
    pub max_bundle_operations: usize,
    pub gas_buffer_bps: u64,
    pub fixed_gas_buffer: u64,
    pub settlement_markup_bps: u64,
    pub settlement_inclusion_floor_bps: u64,
    pub settlement_hold_max_attempts: u32,
    pub relayer_float_cost_multiplier: u64,
    pub relayer_float_target_wei: u128,
    pub relayer_float_min_wei: u128,
    pub top_up_max_wei: u128,
    pub treasury_floor_wei: u128,
}

impl CfConfig {
    /// The core's per-batch policy, with the treasury and per-lane relayer
    /// addresses derived from `OPERATOR_SECRET` (core vault, chain-agnostic).
    pub fn execution_policy(
        &self,
        chain_id: u64,
        lane: u8,
    ) -> Result<vela_relay_core::execution::ExecutionPolicy, String> {
        let secret = self
            .operator_secret
            .as_deref()
            .ok_or("OPERATOR_SECRET is required for execution")?;
        let treasury = vault::derive_address(secret)
            .map_err(|error| format!("invalid OPERATOR_SECRET: {error}"))?
            .parse()
            .map_err(|_| "derived treasury address is invalid".to_owned())?;
        let relayer = vault::derive_pool_relayer_address(secret, lane as usize)
            .map_err(|error| format!("invalid OPERATOR_SECRET: {error}"))?
            .parse()
            .map_err(|_| "derived relayer address is invalid".to_owned())?;
        Ok(vela_relay_core::execution::ExecutionPolicy {
            pool_width: self.relayer_count,
            max_bundle_operations: self.max_bundle_operations,
            gas_buffer_bps: self.gas_buffer_bps,
            fixed_gas_buffer: self.fixed_gas_buffer,
            settlement_inclusion_floor_bps: self.settlement_inclusion_floor_bps,
            settlement_hold_max_attempts: self.settlement_hold_max_attempts,
            top_up_max_wei: self.top_up_max_wei,
            is_tempo: vela_relay_core::tempo::is_tempo_chain(chain_id),
            treasury,
            relayer,
            relayer_float_cost_multiplier: self.relayer_float_cost_multiplier,
            relayer_float_target_wei: self.relayer_float_target_wei,
            relayer_float_min_wei: self.relayer_float_min_wei,
            treasury_floor_wei: self.treasury_floor_wei,
        })
    }
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

        // Same defaults and bounds as the docker parser.
        let settlement_markup_bps =
            u64_var(env, "VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS", 14_000)?;
        if settlement_markup_bps < 10_000 {
            return Err("VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS cannot be below 10000".into());
        }
        let settlement_inclusion_floor_bps = u64_var(
            env,
            "VELA_RELAY_EXECUTOR_SETTLEMENT_INCLUSION_FLOOR_BPS",
            15_000,
        )?;
        if settlement_inclusion_floor_bps <= 10_000 {
            return Err(
                "VELA_RELAY_EXECUTOR_SETTLEMENT_INCLUSION_FLOOR_BPS must be above 10000".into(),
            );
        }
        let relayer_float_min_wei = u128_var(
            env,
            "VELA_RELAY_EXECUTOR_FLOAT_MIN_WEI",
            500_000_000_000_000,
        )?;
        let relayer_float_target_wei = u128_var(
            env,
            "VELA_RELAY_EXECUTOR_FLOAT_TARGET_WEI",
            2_000_000_000_000_000,
        )?;
        if relayer_float_target_wei < relayer_float_min_wei {
            return Err(
                "VELA_RELAY_EXECUTOR_FLOAT_TARGET_WEI cannot be below VELA_RELAY_EXECUTOR_FLOAT_MIN_WEI"
                    .into(),
            );
        }

        Ok(Self {
            settlement_recipient,
            relayer_count,
            executor_enabled,
            execution_chains,
            alchemy_api_key: secret(env, "ALCHEMY_API_KEY").filter(|key| !key.trim().is_empty()),
            operator_secret,
            max_bundle_operations: usize_var(env, "VELA_RELAY_MAX_BUNDLE_OPERATIONS", 10)?,
            gas_buffer_bps: u64_var(env, "VELA_RELAY_EXECUTOR_GAS_BUFFER_BPS", 1_500)?,
            fixed_gas_buffer: u64_var(env, "VELA_RELAY_EXECUTOR_FIXED_GAS_BUFFER", 30_000)?,
            settlement_markup_bps,
            settlement_inclusion_floor_bps,
            settlement_hold_max_attempts: u32_var(
                env,
                "VELA_RELAY_EXECUTOR_SETTLEMENT_HOLD_MAX_ATTEMPTS",
                12,
            )?,
            relayer_float_cost_multiplier: u64_var(
                env,
                "VELA_RELAY_EXECUTOR_FLOAT_COST_MULTIPLIER",
                5,
            )?,
            relayer_float_target_wei,
            relayer_float_min_wei,
            top_up_max_wei: u128_var(
                env,
                "VELA_RELAY_EXECUTOR_TOP_UP_MAX_WEI",
                10_000_000_000_000_000_000,
            )?,
            treasury_floor_wei: u128_var(
                env,
                "VELA_RELAY_EXECUTOR_TREASURY_FLOOR_WEI",
                100_000_000_000_000,
            )?,
        })
    }
}

fn u64_var(env: &Env, name: &str, default: u64) -> Result<u64, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<u64>()
            .map_err(|error| format!("invalid {name}: {error}")),
    }
}

fn u32_var(env: &Env, name: &str, default: u32) -> Result<u32, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<u32>()
            .map_err(|error| format!("invalid {name}: {error}")),
    }
}

fn u128_var(env: &Env, name: &str, default: u128) -> Result<u128, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<u128>()
            .map_err(|error| format!("invalid {name}: {error}")),
    }
}

fn usize_var(env: &Env, name: &str, default: usize) -> Result<usize, String> {
    match var(env, name) {
        None => Ok(default),
        Some(value) => value
            .parse::<usize>()
            .map_err(|error| format!("invalid {name}: {error}")),
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
