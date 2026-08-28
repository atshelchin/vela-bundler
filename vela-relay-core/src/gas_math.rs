//! EIP-1559 gas price arithmetic: fee-history interpretation, tier scaling,
//! and quantity parsing. The shell's `GasPriceManager` owns polling, caching,
//! and RPC failover; every price *calculation* lives here.

use std::{
    error::Error,
    fmt::{Display, Formatter},
};

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GasPricePolicy {
    pub base_fee_multiplier: u128,
    pub slow_multiplier: u128,
    pub standard_multiplier: u128,
    pub fast_multiplier: u128,
    pub priority_fee_divisor: u128,
}

impl Default for GasPricePolicy {
    fn default() -> Self {
        Self {
            base_fee_multiplier: 120,
            slow_multiplier: 100,
            standard_multiplier: 110,
            fast_multiplier: 120,
            priority_fee_divisor: 200,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GasPrice {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GasPriceTiers {
    pub slow: GasPrice,
    pub standard: GasPrice,
    pub fast: GasPrice,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GasPriceError {
    NoPriceAvailable,
    InvalidUpstreamResponse,
    ArithmeticOverflow,
    ResponseDeadlineExceeded,
}

impl Display for GasPriceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPriceAvailable => formatter.write_str("no gas price is available"),
            Self::InvalidUpstreamResponse => {
                formatter.write_str("upstream RPC returned an invalid gas price response")
            }
            Self::ArithmeticOverflow => formatter.write_str("gas price calculation overflowed"),
            Self::ResponseDeadlineExceeded => {
                formatter.write_str("the gas price response deadline was exceeded")
            }
        }
    }
}

impl Error for GasPriceError {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistory {
    pub base_fee_per_gas: Vec<String>,
    #[serde(default)]
    pub reward: Vec<Vec<String>>,
}

pub fn price_from_fee_history(
    fee_history: &FeeHistory,
    base_fee_multiplier: u128,
    priority_fee: u128,
) -> Result<GasPrice, GasPriceError> {
    let base_fee = fee_history
        .base_fee_per_gas
        .last()
        .ok_or(GasPriceError::InvalidUpstreamResponse)
        .and_then(|value| parse_quantity(value))?;
    let max_fee_per_gas = scale(base_fee, base_fee_multiplier)?
        .checked_add(priority_fee)
        .ok_or(GasPriceError::ArithmeticOverflow)?;

    Ok(GasPrice {
        max_fee_per_gas: max_fee_per_gas.max(priority_fee),
        max_priority_fee_per_gas: priority_fee,
    })
}

pub fn tiers(
    policy: &GasPricePolicy,
    network_price: GasPrice,
) -> Result<GasPriceTiers, GasPriceError> {
    Ok(GasPriceTiers {
        slow: scale_price(network_price, policy.slow_multiplier)?,
        standard: scale_price(network_price, policy.standard_multiplier)?,
        fast: scale_price(network_price, policy.fast_multiplier)?,
    })
}

pub fn scale_price(price: GasPrice, multiplier: u128) -> Result<GasPrice, GasPriceError> {
    let max_priority_fee_per_gas = scale(price.max_priority_fee_per_gas, multiplier)?;
    let max_fee_per_gas = scale(price.max_fee_per_gas, multiplier)?.max(max_priority_fee_per_gas);

    Ok(GasPrice {
        max_fee_per_gas,
        max_priority_fee_per_gas,
    })
}

/// The fallback tip when neither fee-history rewards nor
/// `eth_maxPriorityFeePerGas` yields a usable value.
pub fn fallback_priority_fee(base_fee: u128, priority_fee_divisor: u128) -> u128 {
    base_fee.div_ceil(priority_fee_divisor).max(1)
}

pub fn median_priority_fee(rewards: &[Vec<String>]) -> Option<u128> {
    let mut values = rewards
        .iter()
        .filter_map(|reward| reward.get(reward.len() / 2))
        .filter_map(|value| parse_quantity(value).ok())
        .collect::<Vec<_>>();

    if values.is_empty() {
        return None;
    }

    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        Some(values[middle - 1].saturating_add(values[middle]) / 2)
    } else {
        Some(values[middle])
    }
}

pub fn parse_quantity(value: &str) -> Result<u128, GasPriceError> {
    let value = value
        .strip_prefix("0x")
        .ok_or(GasPriceError::InvalidUpstreamResponse)?;

    if value.is_empty() {
        return Err(GasPriceError::InvalidUpstreamResponse);
    }

    u128::from_str_radix(value, 16).map_err(|_| GasPriceError::InvalidUpstreamResponse)
}

pub fn legacy_price_from_result(result: Value) -> Result<GasPrice, GasPriceError> {
    let value = result
        .as_str()
        .ok_or(GasPriceError::InvalidUpstreamResponse)
        .and_then(parse_quantity)?;

    Ok(GasPrice {
        max_fee_per_gas: value,
        max_priority_fee_per_gas: value,
    })
}

fn scale(value: u128, multiplier: u128) -> Result<u128, GasPriceError> {
    value
        .checked_mul(multiplier)
        .ok_or(GasPriceError::ArithmeticOverflow)
        .map(|value| value.div_ceil(100))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        FeeHistory, GasPrice, GasPricePolicy, legacy_price_from_result, median_priority_fee,
        parse_quantity, price_from_fee_history, tiers,
    };

    #[test]
    fn calculates_an_eip1559_price_from_fee_history() {
        let fee_history: FeeHistory = serde_json::from_value(json!({
            "baseFeePerGas": ["0x50", "0x64"],
            "reward": [["0x1", "0xa", "0x14"]]
        }))
        .unwrap();

        assert_eq!(
            price_from_fee_history(
                &fee_history,
                GasPricePolicy::default().base_fee_multiplier,
                10
            )
            .unwrap(),
            GasPrice {
                max_fee_per_gas: 130,
                max_priority_fee_per_gas: 10,
            }
        );
    }

    #[test]
    fn calculates_eip1559_tiers_with_independent_fee_caps() {
        let tiers = tiers(
            &GasPricePolicy::default(),
            GasPrice {
                max_fee_per_gas: 130,
                max_priority_fee_per_gas: 10,
            },
        )
        .unwrap();

        assert_eq!(tiers.slow.max_fee_per_gas, 130);
        assert_eq!(tiers.slow.max_priority_fee_per_gas, 10);
        assert_eq!(tiers.standard.max_fee_per_gas, 143);
        assert_eq!(tiers.standard.max_priority_fee_per_gas, 11);
        assert_eq!(tiers.fast.max_fee_per_gas, 156);
        assert_eq!(tiers.fast.max_priority_fee_per_gas, 12);
    }

    #[test]
    fn uses_the_median_priority_fee_from_fee_history_rewards() {
        let rewards: Vec<Vec<String>> = serde_json::from_value(json!([
            ["0x1", "0x4", "0x9"],
            ["0x1", "0x6", "0x9"],
            ["0x1", "0x8", "0x9"]
        ]))
        .unwrap();

        assert_eq!(median_priority_fee(&rewards), Some(6));
    }

    #[test]
    fn rejects_invalid_quantities() {
        assert!(parse_quantity("100").is_err());
        assert!(parse_quantity("0x").is_err());
        assert!(parse_quantity("0xnope").is_err());
    }

    #[test]
    fn falls_back_to_legacy_gas_price_response() {
        assert_eq!(
            legacy_price_from_result(json!("0x64")).unwrap(),
            GasPrice {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 100,
            }
        );
        assert!(legacy_price_from_result(json!({ "gasPrice": "0x64" })).is_err());
    }
}
