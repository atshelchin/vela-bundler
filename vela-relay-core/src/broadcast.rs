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

// --- Executor upstream-error classification -------------------------------
//
// The predicates the executor transport applies to a JSON-RPC error object
// before failing over or classifying a broadcast. Distinct from the
// admission-side `estimate::is_execution_revert` (which also accepts
// "execution error"); both vocabularies are frozen independently.

/// A JSON-RPC error that is genuine EVM execution output (code 3, an
/// "execution reverted" message, or an EntryPoint `FailedOp`): definitive for
/// the call, so the transport must not fail over past it.
pub fn is_executor_revert(code: Option<i64>, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    code == Some(3) || message.contains("execution reverted") || message.contains("failedop")
}

/// The node already holds these exact bytes: the broadcast is ambiguous, not
/// rejected (the transaction may be mined or pending).
pub fn is_broadcast_already_known(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("already known")
        || message.contains("known transaction")
        || message.contains("already imported")
}

/// Nonce-shaped rejections are ambiguous: the same transaction (or a
/// successor) may already occupy the nonce.
pub fn is_broadcast_nonce_ambiguous(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("nonce too low") || message.contains("replacement transaction underpriced")
}

/// Rejections that prove the node did not admit the transaction; only when
/// every endpoint answers in this class may the broadcast be judged rejected.
pub fn is_definitive_broadcast_rejection(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("insufficient funds")
        || message.contains("intrinsic gas")
        || message.contains("fee cap")
        || message.contains("max fee per gas")
        || message.contains("transaction type not supported")
}

/// The frozen upstream-error rendering ("RPC code {code}: {message…}") that
/// flows into broadcast diagnostics and record fields.
pub fn upstream_error_diagnostic(code: Option<i64>, message: Option<&str>) -> String {
    let code = code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".into());
    let message = message.unwrap_or("upstream error");
    format!("RPC code {code}: {}", truncate_rpc_diagnostic(message, 256))
}

/// Deduplicates per-endpoint diagnostics in arrival order and joins them into
/// the single bounded string persisted with a broadcast outcome.
pub fn join_broadcast_diagnostics(diagnostics: Vec<String>) -> String {
    let mut unique = Vec::new();
    for diagnostic in diagnostics {
        if !unique.contains(&diagnostic) {
            unique.push(diagnostic);
        }
    }
    truncate_rpc_diagnostic(&unique.join("; "), 512)
}

/// The transport-side truncation (ellipsis suffix, char-boundary safe) —
/// deliberately distinct from `task::truncate_diagnostic`'s "..." suffix.
pub fn truncate_rpc_diagnostic(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

/// Extracts the revert payload from a JSON-RPC error's `data` field, in the
/// shapes trusted upstreams use (a bare string, or an object keyed by `data`,
/// `result`, or `returnData`).
pub fn revert_data(value: &Option<serde_json::Value>) -> Option<String> {
    use serde_json::Value;
    match value.as_ref()? {
        Value::String(value) => Some(value.clone()),
        Value::Object(object) => ["data", "result", "returnData"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(Value::as_str).map(str::to_owned)),
        _ => None,
    }
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
    fn distinguishes_ambiguous_and_definitive_broadcast_errors() {
        assert!(super::is_broadcast_nonce_ambiguous("nonce too low"));
        assert!(!super::is_definitive_broadcast_rejection("nonce too low"));
        assert!(super::is_definitive_broadcast_rejection(
            "insufficient funds for gas * price + value"
        ));
    }

    #[test]
    fn renders_and_joins_upstream_diagnostics_with_bounded_length() {
        assert_eq!(
            super::upstream_error_diagnostic(Some(-32000), Some("nonce too low")),
            "RPC code -32000: nonce too low"
        );
        assert_eq!(
            super::upstream_error_diagnostic(None, None),
            "RPC code unknown: upstream error"
        );
        assert_eq!(
            super::join_broadcast_diagnostics(vec![
                "first".into(),
                "second".into(),
                "first".into()
            ]),
            "first; second"
        );
        let truncated = super::truncate_rpc_diagnostic(&"é".repeat(200), 256);
        assert!(truncated.ends_with('…'));
        assert!(truncated.len() <= 256 + '…'.len_utf8());
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
