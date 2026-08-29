//! Operator-alert suppression rules: the fingerprint that deduplicates
//! executor issues (numbers and long hex literals normalized away so retries
//! of one failure class collapse), the bounded single-line reason, and the
//! frozen Telegram message text. Both shells consume these; the transports
//! (HTTP client, suppression store) stay shell-owned.

use alloy::primitives::keccak256;

const MAX_ALERT_REASON_BYTES: usize = 700;

/// One suppression identity per (chain, stage, failure class).
pub fn alert_fingerprint(chain_id: u64, stage: &str, reason: &str) -> String {
    let digest = keccak256(normalize_reason(reason).as_bytes());
    format!("{chain_id}:{stage}:{}", hex::encode(&digest[..16]))
}

/// The frozen operator message (docker `notify_executor_issue` text).
pub fn executor_alert_text(
    chain_id: u64,
    stage: &str,
    user_operation_hash: &str,
    reason: &str,
    cooldown_secs: u64,
) -> String {
    let reason = safe_alert_reason(reason);
    format!(
        "Vela Relay executor issue\nchain: {chain_id}\nstage: {stage}\nuser operation: {user_operation_hash}\nreason: {reason}\n\nIdentical chain/stage/reason alerts are suppressed for {cooldown_secs} seconds.",
    )
}

fn normalize_reason(reason: &str) -> String {
    let reason = replace_hex_literals(reason);
    let mut normalized = String::with_capacity(reason.len());
    let mut in_number = false;
    for character in reason.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_digit() {
            if !in_number {
                normalized.push('#');
                in_number = true;
            }
        } else {
            in_number = false;
            if character.is_whitespace() {
                if !normalized.ends_with(' ') {
                    normalized.push(' ');
                }
            } else {
                normalized.push(character);
            }
        }
    }
    normalized.trim().to_owned()
}

fn replace_hex_literals(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut copied_until = 0;
    let mut index = 0;
    while index + 2 <= bytes.len() {
        if bytes[index] != b'0' || !matches!(bytes[index + 1], b'x' | b'X') {
            index += 1;
            continue;
        }

        let start = index;
        index += 2;
        while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
            index += 1;
        }
        // Hashes and addresses make otherwise identical retry errors look unique. Leave short
        // values such as `0x0` intact because they can describe a distinct error condition.
        if index.saturating_sub(start) >= 10 {
            output.push_str(&value[copied_until..start]);
            output.push_str(" <hex> ");
            copied_until = index;
        }
    }
    output.push_str(&value[copied_until..]);
    output
}

pub fn safe_alert_reason(reason: &str) -> String {
    let filtered = reason
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if filtered.len() <= MAX_ALERT_REASON_BYTES {
        return filtered;
    }

    let end = filtered
        .char_indices()
        .take_while(|(index, character)| {
            index.saturating_add(character.len_utf8()) <= MAX_ALERT_REASON_BYTES.saturating_sub(3)
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    format!("{}...", &filtered[..end])
}

#[cfg(test)]
mod tests {
    use super::{alert_fingerprint, normalize_reason, safe_alert_reason};

    #[test]
    fn fingerprint_ignores_changing_numeric_values() {
        assert_eq!(
            alert_fingerprint(137, "funding", "balance is 10, required is 20"),
            alert_fingerprint(137, "funding", "balance is 11, required is 21"),
        );
    }

    #[test]
    fn fingerprint_ignores_changing_hashes() {
        assert_eq!(
            alert_fingerprint(
                137,
                "broadcast",
                "transaction 0x1234567890abcdef is pending"
            ),
            alert_fingerprint(
                137,
                "broadcast",
                "transaction 0xfedcba0987654321 is pending"
            ),
        );
    }

    #[test]
    fn fingerprint_keeps_different_failure_classes_separate() {
        assert_ne!(
            alert_fingerprint(137, "funding", "treasury balance too low"),
            alert_fingerprint(137, "simulation", "treasury balance too low"),
        );
    }

    #[test]
    fn reason_is_single_line_and_bounded() {
        let reason = format!("one\ntwo{}", "x".repeat(800));
        let safe = safe_alert_reason(&reason);
        assert!(!safe.contains('\n'));
        assert!(safe.len() <= 700);
        assert_eq!(normalize_reason("Balance 100 is low"), "balance # is low");
    }
}
