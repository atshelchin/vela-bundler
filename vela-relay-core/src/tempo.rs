use alloy::{
    primitives::{Address, U256, address},
    sol,
    sol_types::SolCall,
};

/// Tempo mainnet and its Moderato testnet use native account-abstraction (`0x76`) transactions.
pub const fn is_tempo_chain(chain_id: u64) -> bool {
    matches!(chain_id, 4_217 | 42_431)
}

/// Tempo's protocol-default USD fee token. Amounts are micro-pathUSD (six decimals).
pub const PATH_USD: Address = address!("20c0000000000000000000000000000000000000");
pub const PATH_USD_DECIMALS: u32 = 6;
pub const PATH_USD_SYMBOL: &str = "pathUSD";
pub const TEMPO_BASE_FEE_ATTO: u128 = 20_000_000_000;
pub const TEMPO_COST_BUFFER_GAS: u64 = 80_000;
pub const TEMPO_FLOAT_MIN: u128 = 100_000;
pub const TEMPO_FLOAT_TARGET: u128 = 300_000;
pub const TEMPO_TREASURY_FLOOR: u128 = 200_000;

sol! {
    interface IERC20Tempo {
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
    }
}

pub fn path_usd_balance_calldata(account: Address) -> alloy::primitives::Bytes {
    IERC20Tempo::balanceOfCall { account }.abi_encode().into()
}

pub fn path_usd_transfer_calldata(to: Address, amount: U256) -> alloy::primitives::Bytes {
    IERC20Tempo::transferCall { to, amount }.abi_encode().into()
}

/// Buffer applied to a pathUSD top-up `eth_estimateGas` result.
pub const TEMPO_TOP_UP_GAS_BUFFER_BPS: u64 = 12_000;

/// Applies [`TEMPO_TOP_UP_GAS_BUFFER_BPS`] to a gas estimate; `None` on
/// overflow.
pub fn buffered_top_up_gas_limit(estimated_gas: u64) -> Option<u64> {
    estimated_gas
        .checked_mul(TEMPO_TOP_UP_GAS_BUFFER_BPS)
        .map(|value| value / 10_000)
}

/// Tempo's outer `0x76` gas limit deliberately comes from the UserOperations'
/// declared limits, rather than `eth_estimateGas`: EntryPoint catches an
/// inner OOG and an estimate can therefore succeed while a user's actual
/// execution runs out of gas.
pub fn tempo_handle_ops_gas_limit(
    operations: &[&crate::abi::PackedOperation],
) -> Result<u64, &'static str> {
    let declared = operations.iter().try_fold(U256::ZERO, |total, packed| {
        let limits = packed.packed.accountGasLimits.as_slice();
        let verification = U256::from_be_slice(&limits[..16]);
        let call = U256::from_be_slice(&limits[16..]);
        total
            .checked_add(verification)?
            .checked_add(call)?
            .checked_add(packed.packed.preVerificationGas)
    });
    let declared = declared.ok_or("Tempo declared gas overflow")?;
    let gas = declared
        .checked_mul(U256::from(64u8))
        .map(|value| value / U256::from(63u8))
        .and_then(|value| {
            value.checked_add(U256::from(operations.len()).checked_mul(U256::from(50_000u64))?)
        })
        .and_then(|value| value.checked_add(U256::from(60_000u64)))
        .ok_or("Tempo outer gas limit overflow")?;
    u64::try_from(gas).map_err(|_| "Tempo outer gas limit exceeds uint64")
}

/// The outer `0x76` fee cap: 1.5× the current attodollar base fee.
pub fn tempo_outer_max_fee(base_fee_atto: U256) -> Result<u128, &'static str> {
    let fee = base_fee_atto
        .checked_add(base_fee_atto / U256::from(2u8))
        .ok_or("Tempo outer fee overflow")?;
    u128::try_from(fee).map_err(|_| "Tempo outer fee exceeds uint128")
}

/// Converts a gas cost quoted in attodollars per gas into micro-pathUSD,
/// rounding up. `None` on overflow.
pub fn tempo_cost_in_path_usd(gas: U256, price_atto: U256) -> Option<U256> {
    let numerator = gas
        .checked_mul(price_atto)?
        .checked_mul(U256::from(10u8).pow(U256::from(PATH_USD_DECIMALS)))?;
    let denominator = U256::from(10u8).pow(U256::from(18u8));
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    quotient.checked_add(U256::from(u8::from(!remainder.is_zero())))
}
