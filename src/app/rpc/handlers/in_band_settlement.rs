//! String-facing adapter over the decision core's in-band settlement parsing.
//!
//! The RPC handlers speak hex strings and `u128`; the core speaks
//! `Address`/`U256`. Everything business — MultiSend decoding, the trusted
//! MultiSend gate, leg crediting, minimum amounts — lives in
//! `vela_relay_core::settlement`; this module only converts representations.
//! Amounts above `u128::MAX` saturate, preserving the RPC layer's historical
//! wire behavior.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, U256};
use vela_relay_core::settlement::{
    MIN_NATIVE_FRACTION_DECIMALS, MIN_STABLE_FRACTION_DECIMALS, minimum_amount,
};

pub use vela_relay_core::tempo::is_tempo_chain;

#[derive(Debug, PartialEq, Eq)]
pub struct InBandReimbursement {
    pub native: u128,
    pub stablecoins: BTreeMap<String, u128>,
}

/// Decode reimbursement legs from Safe `executeUserOp` calldata.
///
/// A transfer counts only when it is inside a `DELEGATECALL` to the canonical Safe
/// MultiSend contract. This prevents a caller from presenting transfer-shaped bytes
/// that do not actually execute against the Safe's balance. Malformed calldata,
/// unknown recipients, and overflowing reimbursements all read as "nothing paid",
/// exactly as the executor's evaluation treats them.
pub fn parse_reimbursement(
    call_data: &str,
    recipient: &str,
    stablecoin_allowlist: impl IntoIterator<Item = String>,
) -> InBandReimbursement {
    let empty = InBandReimbursement {
        native: 0,
        stablecoins: BTreeMap::new(),
    };

    let Ok(recipient) = parse_address(recipient) else {
        return empty;
    };
    let stablecoin_allowlist = stablecoin_allowlist
        .into_iter()
        .filter_map(|address| parse_address(&address).ok())
        .map(Address::from)
        .collect::<BTreeSet<_>>();
    let Ok(call_data) = decode_hex(call_data) else {
        return empty;
    };

    match vela_relay_core::settlement::parse_reimbursement(
        &call_data,
        Address::from(recipient),
        &stablecoin_allowlist,
    ) {
        Ok(reimbursement) => InBandReimbursement {
            native: saturate_u128(reimbursement.native),
            stablecoins: reimbursement
                .stablecoins
                .into_iter()
                .map(|(token, amount)| (format_address(token.into()), saturate_u128(amount)))
                .collect(),
        },
        Err(_) => empty,
    }
}

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

fn saturate_u128(value: U256) -> u128 {
    u128::try_from(value).unwrap_or(u128::MAX)
}

fn format_address(address: [u8; 20]) -> String {
    format!("0x{}", hex::encode(address))
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

    pub fn encode(value: [u8; 20]) -> String {
        const TABLE: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(40);
        for byte in value {
            output.push(TABLE[(byte >> 4) as usize] as char);
            output.push(TABLE[(byte & 0x0f) as usize] as char);
        }
        output
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
    use super::{minimum_native_amount, minimum_stablecoin_amount, parse_reimbursement};

    const TRUSTED_MULTISEND: &str = "0x38869bf66a61cf6bdb996a6ae40d5853fd43b526";
    const RECIPIENT: &str = "0x1111111111111111111111111111111111111111";
    const STABLECOIN: &str = "0x2222222222222222222222222222222222222222";

    #[test]
    fn counts_only_native_and_allowlisted_stablecoin_legs_to_the_recipient() {
        let call_data = encode_safe_multisend(&[
            Entry::native(RECIPIENT, 10_000_000_000_000),
            Entry::erc20(STABLECOIN, RECIPIENT, 10_000),
            Entry::erc20(
                "0x3333333333333333333333333333333333333333",
                RECIPIENT,
                99_999,
            ),
        ]);

        let reimbursement = parse_reimbursement(&call_data, RECIPIENT, [STABLECOIN.into()]);

        assert_eq!(reimbursement.native, 10_000_000_000_000);
        assert_eq!(reimbursement.stablecoins[STABLECOIN], 10_000);
        assert_eq!(reimbursement.stablecoins.len(), 1);
    }

    #[test]
    fn rejects_transfer_shaped_data_without_a_delegatecall_to_trusted_multisend() {
        let call_data = encode_safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, 10_000)]);
        let tampered = call_data.replacen("01", "00", 1);

        let reimbursement = parse_reimbursement(&tampered, RECIPIENT, [STABLECOIN.into()]);

        assert_eq!(reimbursement.native, 0);
        assert!(reimbursement.stablecoins.is_empty());
    }

    #[test]
    fn calculates_minimum_amounts_in_smallest_units() {
        assert_eq!(minimum_native_amount(18), Some(10_000_000_000_000));
        assert_eq!(minimum_stablecoin_amount(6), Some(10_000));
        assert_eq!(minimum_stablecoin_amount(18), Some(10_000_000_000_000_000));
    }

    struct Entry {
        to: &'static str,
        value: u128,
        data: Vec<u8>,
    }

    impl Entry {
        fn native(to: &'static str, value: u128) -> Self {
            Self {
                to,
                value,
                data: Vec::new(),
            }
        }

        fn erc20(token: &'static str, recipient: &'static str, amount: u128) -> Self {
            let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
            data.extend(word_address(recipient));
            data.extend(word_u128(amount));
            Self {
                to: token,
                value: 0,
                data,
            }
        }
    }

    fn encode_safe_multisend(entries: &[Entry]) -> String {
        let mut packed = Vec::new();
        for entry in entries {
            packed.push(0);
            packed.extend(address(entry.to));
            packed.extend(word_u128(entry.value));
            packed.extend(word_u128(entry.data.len() as u128));
            packed.extend(&entry.data);
        }

        let mut multisend = vec![0x8d, 0x80, 0xff, 0x0a];
        multisend.extend(word_u128(32));
        multisend.extend(word_u128(packed.len() as u128));
        multisend.extend(packed);
        pad_to_word(&mut multisend);

        let mut call_data = vec![0x7b, 0xb3, 0x74, 0x28];
        call_data.extend(word_address(TRUSTED_MULTISEND));
        call_data.extend(word_u128(0));
        call_data.extend(word_u128(128));
        call_data.extend(word_u128(1));
        call_data.extend(word_u128(multisend.len() as u128));
        call_data.extend(multisend);
        pad_to_word(&mut call_data[4..]);

        format!("0x{}", encode_hex(&call_data))
    }

    fn address(value: &str) -> Vec<u8> {
        super::decode_hex(value).unwrap()
    }

    fn word_address(value: &str) -> Vec<u8> {
        let mut word = vec![0; 12];
        word.extend(address(value));
        word
    }

    fn word_u128(value: u128) -> Vec<u8> {
        let mut word = vec![0; 16];
        word.extend(value.to_be_bytes());
        word
    }

    fn pad_to_word(data: &mut [u8]) {
        let _ = data;
    }

    fn encode_hex(value: &[u8]) -> String {
        const TABLE: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(value.len() * 2);
        for byte in value {
            output.push(TABLE[(byte >> 4) as usize] as char);
            output.push(TABLE[(byte & 0x0f) as usize] as char);
        }
        output
    }
}
