//! CCTP v1 message decoding (big-endian wire format, shared by every domain).
//!
//! Header: version u32 | source_domain u32 | destination_domain u32 |
//! nonce u64 | sender 32B | recipient 32B | destination_caller 32B | body.
//!
//! BurnMessage body: version u32 | burn_token 32B | mint_recipient 32B |
//! amount u256 | message_sender 32B.

use anyhow::{anyhow, bail, Result};

pub const HEADER_LEN: usize = 116;
pub const BURN_BODY_LEN: usize = 132;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CctpMessage {
    pub version: u32,
    pub source_domain: u32,
    pub destination_domain: u32,
    pub nonce: u64,
    pub sender: [u8; 32],
    pub recipient: [u8; 32],
    pub destination_caller: [u8; 32],
    pub burn: BurnBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnBody {
    pub version: u32,
    pub burn_token: [u8; 32],
    pub mint_recipient: [u8; 32],
    /// USDC base units. The wire field is u256 but CCTP amounts fit u64.
    pub amount: u64,
    pub message_sender: [u8; 32],
}

pub fn decode(message: &[u8]) -> Result<CctpMessage> {
    if message.len() < HEADER_LEN + BURN_BODY_LEN {
        bail!(
            "message too short: {} bytes (need {})",
            message.len(),
            HEADER_LEN + BURN_BODY_LEN
        );
    }
    let u32_at = |o: usize| u32::from_be_bytes(message[o..o + 4].try_into().unwrap());
    let bytes32_at = |o: usize| -> [u8; 32] { message[o..o + 32].try_into().unwrap() };

    let body = &message[HEADER_LEN..];
    let amount_bytes: [u8; 32] = body[68..100].try_into().unwrap();
    if amount_bytes[..24].iter().any(|b| *b != 0) {
        bail!("burn amount exceeds u64");
    }
    let amount = u64::from_be_bytes(amount_bytes[24..].try_into().unwrap());

    Ok(CctpMessage {
        version: u32_at(0),
        source_domain: u32_at(4),
        destination_domain: u32_at(8),
        nonce: u64::from_be_bytes(message[12..20].try_into().unwrap()),
        sender: bytes32_at(20),
        recipient: bytes32_at(52),
        destination_caller: bytes32_at(84),
        burn: BurnBody {
            version: u32::from_be_bytes(body[0..4].try_into().unwrap()),
            burn_token: body[4..36].try_into().unwrap(),
            mint_recipient: body[36..68].try_into().unwrap(),
            amount,
            message_sender: body[100..132].try_into().unwrap(),
        },
    })
}

/// Parse `0x`-prefixed (or bare) hex into bytes.
pub fn hex_bytes(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).map_err(|e| anyhow!("bad hex: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header fields + the burn body taken from circlefin/sui-cctp's
    /// deposit_for_burn unit test (source 0 → destination 2, amount 100,
    /// recipient 0xb2, sender 0xa1).
    #[test]
    fn decodes_burn_message() {
        let body = hex_bytes(
            "00000001aa9d562b0a114a7cfa31074ac0ac0a543a25b034ba38830c82e7163775c94c86\
             00000000000000000000000000000000000000000000000000000000000000b2\
             0000000000000000000000000000000000000000000000000000000000000064\
             00000000000000000000000000000000000000000000000000000000000000a1",
        )
        .unwrap();

        let mut msg = Vec::new();
        msg.extend_from_slice(&1u32.to_be_bytes()); // version
        msg.extend_from_slice(&8u32.to_be_bytes()); // source: sui
        msg.extend_from_slice(&5u32.to_be_bytes()); // destination: solana
        msg.extend_from_slice(&42u64.to_be_bytes()); // nonce
        msg.extend_from_slice(&[0xAA; 32]); // sender
        msg.extend_from_slice(&[0xBB; 32]); // recipient
        msg.extend_from_slice(&[0u8; 32]); // destination_caller
        msg.extend_from_slice(&body);

        let decoded = decode(&msg).unwrap();
        assert_eq!(decoded.version, 1);
        assert_eq!(decoded.source_domain, 8);
        assert_eq!(decoded.destination_domain, 5);
        assert_eq!(decoded.nonce, 42);
        assert_eq!(decoded.burn.version, 1);
        assert_eq!(decoded.burn.amount, 100);
        assert_eq!(decoded.burn.mint_recipient[31], 0xb2);
        assert_eq!(decoded.burn.message_sender[31], 0xa1);
        assert_eq!(decoded.destination_caller, [0u8; 32]);
    }

    #[test]
    fn rejects_short_message() {
        assert!(decode(&[0u8; 50]).is_err());
    }

    #[test]
    fn rejects_amount_over_u64() {
        let mut msg = vec![0u8; HEADER_LEN + BURN_BODY_LEN];
        msg[HEADER_LEN + 68] = 1; // set a high byte of the u256 amount
        assert!(decode(&msg).is_err());
    }
}
