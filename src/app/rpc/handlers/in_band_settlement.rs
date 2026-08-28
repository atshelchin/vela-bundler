//! String helpers for the RPC handlers over the decision core's settlement
//! vocabulary. Reimbursement parsing itself lives only in the core
//! (`vela_relay_core::settlement::parse_reimbursement`, driven through the
//! admission program); this module keeps the string-facing minimum-amount
//! adapters and hex/address utilities the quote and estimate handlers use.

use vela_relay_core::settlement::{
    MIN_NATIVE_FRACTION_DECIMALS, MIN_STABLE_FRACTION_DECIMALS, minimum_amount,
};

pub use vela_relay_core::tempo::is_tempo_chain;

pub fn minimum_native_amount(native_decimals: u32) -> Option<u128> {
    minimum_amount(native_decimals, MIN_NATIVE_FRACTION_DECIMALS)
        .ok()
        .and_then(|value| u128::try_from(value).ok())
}

pub fn minimum_stablecoin_amount(token_decimals: u32) -> Option<u128> {
    minimum_amount(token_decimals, MIN_STABLE_FRACTION_DECIMALS)
        .ok()
        .and_then(|value| u128::try_from(value).ok())
}

pub fn parse_address(value: &str) -> Result<[u8; 20], ()> {
    let value = value.strip_prefix("0x").ok_or(())?;
    if value.len() != 40 {
        return Err(());
    }

    let bytes = hex::decode(value).map_err(|_| ())?;
    bytes.try_into().map_err(|_| ())
}

pub fn decode_hex(value: &str) -> Result<Vec<u8>, ()> {
    let value = value.strip_prefix("0x").ok_or(())?;
    if value.len() % 2 != 0 {
        return Err(());
    }

    hex::decode(value).map_err(|_| ())
}

mod hex {
    pub fn decode(value: &str) -> Result<Vec<u8>, ()> {
        let mut bytes = Vec::with_capacity(value.len() / 2);
        let mut chars = value.as_bytes().chunks_exact(2);
        for pair in &mut chars {
            let high = value_of(pair[0]).ok_or(())?;
            let low = value_of(pair[1]).ok_or(())?;
            bytes.push((high << 4) | low);
        }
        if !chars.remainder().is_empty() {
            return Err(());
        }
        Ok(bytes)
    }

    fn value_of(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{minimum_native_amount, minimum_stablecoin_amount};

    // Reimbursement parsing tests live with the single parser in
    // `vela_relay_core` (settlement + admission::string_reimbursement).
    #[test]
    fn calculates_minimum_amounts_in_smallest_units() {
        assert_eq!(minimum_native_amount(18), Some(10_000_000_000_000));
        assert_eq!(minimum_stablecoin_amount(6), Some(10_000));
        assert_eq!(minimum_stablecoin_amount(18), Some(10_000_000_000_000_000));
    }
}
