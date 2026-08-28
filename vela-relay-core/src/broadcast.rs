//! Broadcast-outcome judgement: validating signed bytes before broadcast and
//! deciding what an ambiguous or rejected broadcast means. The shell owns the
//! RPC probes; the rules that interpret their answers live here.

use std::{
    fmt::{Display, Formatter},
    str::FromStr,
};

use alloy::primitives::{B256, Bytes, keccak256};

/// A "nonce too low" diagnostic is the one rejection that can mean the
/// transaction (or a successor) already landed.
pub fn nonce_too_low(reason: &str) -> bool {
    reason.to_ascii_lowercase().contains("nonce too low")
}

pub fn parse_hex_bytes(value: &str) -> Option<Bytes> {
    let digits = value.strip_prefix("0x")?;
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    hex::decode(digits).ok().map(Into::into)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawTransactionError {
    InvalidBytes,
    UnsupportedType,
    InvalidHash,
    HashMismatch,
}

impl Display for RawTransactionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        // Byte-frozen: the shell folds these into its executor error text.
        match self {
            Self::InvalidBytes => formatter.write_str("prepared raw transaction is invalid"),
            Self::UnsupportedType => formatter
                .write_str("prepared transaction is not a supported type 0x02 or Tempo type 0x76"),
            Self::InvalidHash => formatter.write_str("prepared transaction hash is invalid"),
            Self::HashMismatch => {
                formatter.write_str("prepared transaction hash does not match raw bytes")
            }
        }
    }
}

impl std::error::Error for RawTransactionError {}

/// A prepared intent's bytes must round-trip to its recorded hash before any
/// broadcast; rebroadcasting different bytes under an old nonce could double
/// spend the lane.
pub fn validate_raw_transaction(
    raw_transaction: &str,
    transaction_hash: &str,
) -> Result<Vec<u8>, RawTransactionError> {
    let raw = parse_hex_bytes(raw_transaction)
        .filter(|raw| !raw.is_empty())
        .ok_or(RawTransactionError::InvalidBytes)?;
    if !matches!(raw.first(), Some(0x02 | 0x76)) {
        return Err(RawTransactionError::UnsupportedType);
    }
    let expected =
        B256::from_str(transaction_hash).map_err(|_| RawTransactionError::InvalidHash)?;
    if keccak256(&raw) != expected {
        return Err(RawTransactionError::HashMismatch);
    }
    Ok(raw.to_vec())
}

/// What to do next after an ambiguous or rejected broadcast, judged from the
/// node's diagnostic and the result of the transaction-known probe. The
/// stale-nonce probe is deliberately a second step: it costs an extra RPC and
/// only matters for "nonce too low".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnprovenBroadcast {
    /// The transaction is observable on the network: treat as confirmed.
    Confirmed,
    /// The diagnostic says "nonce too low": probe whether the lane nonce has
    /// moved past this intent; a stale intent is cleared and retried fresh.
    CheckStaleNonce,
    /// Not provable either way: retain the exact outbox and retry later.
    RetainOutbox,
}

pub fn resolve_unproven_broadcast(reason: &str, transaction_known: bool) -> UnprovenBroadcast {
    if transaction_known {
        UnprovenBroadcast::Confirmed
    } else if nonce_too_low(reason) {
        UnprovenBroadcast::CheckStaleNonce
    } else {
        UnprovenBroadcast::RetainOutbox
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::keccak256;

    use super::{
        RawTransactionError, UnprovenBroadcast, nonce_too_low, parse_hex_bytes,
        resolve_unproven_broadcast, validate_raw_transaction,
    };

    #[test]
    fn recognizes_a_nonce_too_low_broadcast_diagnostic() {
        assert!(nonce_too_low(
            "RPC code -32000: nonce too low: next nonce 1, tx nonce 0"
        ));
        assert!(nonce_too_low("NONCE TOO LOW"));
        assert!(!nonce_too_low("replacement transaction underpriced"));
    }

    #[test]
    fn validates_raw_transaction_type_and_hash() {
        let raw = [0x02, 0x01, 0x02, 0x03];
        let hash = keccak256(raw).to_string();
        assert_eq!(validate_raw_transaction("0x02010203", &hash).unwrap(), raw);
        assert_eq!(
            validate_raw_transaction("0x01010203", &hash),
            Err(RawTransactionError::UnsupportedType)
        );
        assert_eq!(
            validate_raw_transaction("0x02010203", &alloy::primitives::B256::ZERO.to_string()),
            Err(RawTransactionError::HashMismatch)
        );
        assert_eq!(
            validate_raw_transaction("", &hash),
            Err(RawTransactionError::InvalidBytes)
        );
        assert_eq!(
            validate_raw_transaction("0x02010203", "not-a-hash"),
            Err(RawTransactionError::InvalidHash)
        );
        assert!(parse_hex_bytes("0x1").is_none());
    }

    #[test]
    fn unproven_broadcasts_resolve_by_observability_then_nonce_diagnostic() {
        assert_eq!(
            resolve_unproven_broadcast("anything", true),
            UnprovenBroadcast::Confirmed
        );
        assert_eq!(
            resolve_unproven_broadcast("nonce too low", false),
            UnprovenBroadcast::CheckStaleNonce
        );
        assert_eq!(
            resolve_unproven_broadcast("already known", false),
            UnprovenBroadcast::RetainOutbox
        );
    }
}
