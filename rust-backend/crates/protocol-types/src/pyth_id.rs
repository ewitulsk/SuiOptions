//! Pyth price feed identifier.
//!
//! Lives here (rather than in `pyth-client`) so the `deployments` loader in
//! `sui-tx` can parse `pythFeedId` strings into a typed value without
//! pulling in the entire Pyth HTTP/SSE client.

use std::fmt;

use anyhow::{anyhow, Context, Result};

/// 32-byte Pyth price feed identifier. Hermes accepts a leading `0x` in
/// query parameters and returns the raw 64-hex-char form (no prefix) in
/// JSON bodies. We normalize to lowercase, no prefix.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PriceFeedId(pub [u8; 32]);

impl PriceFeedId {
    pub fn from_hex(s: &str) -> Result<Self> {
        let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
        let bytes = hex::decode(trimmed).context("decoding pyth feed id hex")?;
        if bytes.len() != 32 {
            return Err(anyhow!(
                "pyth feed id must be 32 bytes ({} chars), got {}",
                64,
                trimmed.len()
            ));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(Self(out))
    }

    /// Lowercase hex without `0x` prefix — the form Hermes returns in JSON
    /// and the form its query-string parser accepts (it also accepts the
    /// `0x` prefix, but consistency makes the URLs grep-able).
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for PriceFeedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PriceFeedId({})", self.to_hex())
    }
}

impl fmt::Display for PriceFeedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_id_round_trips() {
        let raw = "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";
        let id = PriceFeedId::from_hex(raw).unwrap();
        assert_eq!(id.to_hex(), raw);
        // 0x-prefixed input parses the same.
        let id2 = PriceFeedId::from_hex(&format!("0x{raw}")).unwrap();
        assert_eq!(id, id2);
    }
}
