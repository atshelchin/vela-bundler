//! String helpers for the RPC handlers over the decision core's settlement
//! vocabulary. Reimbursement parsing lives only in the core
//! (`vela_relay_core::settlement::parse_reimbursement`); the minimum-amount
//! adapters and address parsing moved into `vela_relay_core::quote` /
//! `vela_relay_core::estimate` with their consumers (spec 002). Only the hex
//! utility remains here for the quote handler's transport decoding.

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
    use super::decode_hex;

    // Reimbursement parsing tests live with the single parser in
    // `vela_relay_core`; the minimum-amount boundary test moved to
    // `vela_relay_core::quote` with the adapters (spec 002).
    #[test]
    fn decodes_prefixed_even_length_hex_only() {
        assert_eq!(decode_hex("0x0102"), Ok(vec![1, 2]));
        assert!(decode_hex("0102").is_err());
        assert!(decode_hex("0x102").is_err());
    }
}
