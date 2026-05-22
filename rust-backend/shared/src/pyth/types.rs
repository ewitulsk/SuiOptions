//! Pyth Hermes wire types.
//!
//! Mirrors what Hermes returns in the `parsed` array of either
//! `/v2/updates/price/latest` or `/v2/updates/price/stream`. The `binary`
//! field of those responses (a hex-encoded VAA payload meant for on-chain
//! verification) is intentionally not modeled here — we only consume
//! prices off-chain.

use std::fmt;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

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

/// One element of the `parsed` array. Numeric fields come over the wire
/// as JSON strings (because they're 64-bit), so we deserialize them as
/// strings and parse on access.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PriceUpdate {
    /// Feed id, lowercase hex (no `0x`). Hermes emits 64 hex chars.
    pub id: String,
    pub price: PythPrice,
    #[serde(default)]
    pub ema_price: Option<PythPrice>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PythPrice {
    /// Stringified i64 — Hermes serializes as a JSON string to preserve
    /// precision across JS clients.
    pub price: String,
    /// Stringified u64.
    pub conf: String,
    pub expo: i32,
    /// Unix seconds.
    pub publish_time: i64,
}

impl PythPrice {
    /// Convert the integer `price * 10^expo` to a float in natural units
    /// (USD, etc.).
    pub fn price_f64(&self) -> Result<f64> {
        let p: i64 = self.price.parse().context("parsing pyth price as i64")?;
        Ok(p as f64 * 10f64.powi(self.expo))
    }

    /// Confidence interval (1-sigma standard deviation of the price
    /// estimate) in the same natural units as `price_f64`.
    pub fn conf_f64(&self) -> Result<f64> {
        let c: u64 = self.conf.parse().context("parsing pyth conf as u64")?;
        Ok(c as f64 * 10f64.powi(self.expo))
    }

    /// Publish time as Unix milliseconds.
    pub fn publish_time_ms(&self) -> i64 {
        self.publish_time.saturating_mul(1000)
    }
}

impl PriceUpdate {
    /// Parse the wire-format `id` (no `0x` prefix) back into a typed
    /// [`PriceFeedId`].
    pub fn feed_id(&self) -> Result<PriceFeedId> {
        PriceFeedId::from_hex(&self.id)
    }
}

/// Top-level shape of `/v2/updates/price/latest` and each `data:` line of
/// `/v2/updates/price/stream`. The `binary` field is captured opaquely
/// because we don't consume it.
#[derive(Clone, Debug, Deserialize)]
pub struct HermesEnvelope {
    #[serde(default)]
    pub parsed: Vec<PriceUpdate>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_f64_matches_docs_example() {
        // From the Pyth docs: BTC/USD price=6140993501000, expo=-8 → $61409.93501
        let p = PythPrice {
            price: "6140993501000".into(),
            conf: "3287868567".into(),
            expo: -8,
            publish_time: 1_714_746_101,
        };
        let v = p.price_f64().unwrap();
        assert!((v - 61_409.935_01).abs() < 1e-3, "got {v}");
    }

    #[test]
    fn feed_id_round_trips() {
        let raw = "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";
        let id = PriceFeedId::from_hex(raw).unwrap();
        assert_eq!(id.to_hex(), raw);
        // 0x-prefixed input parses the same.
        let id2 = PriceFeedId::from_hex(&format!("0x{raw}")).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn envelope_parses_fixture() {
        // Trimmed shape from `/v2/updates/price/latest?ids[]=…&ids[]=…`.
        let body = r#"{
            "binary": { "encoding": "hex", "data": ["aa"] },
            "parsed": [
                {
                    "id": "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
                    "price": { "price": "6140993501000", "conf": "3287868567", "expo": -8, "publish_time": 1714746101 },
                    "ema_price": { "price": "6140000000000", "conf": "3000000000", "expo": -8, "publish_time": 1714746101 }
                }
            ]
        }"#;
        let env: HermesEnvelope = serde_json::from_str(body).unwrap();
        assert_eq!(env.parsed.len(), 1);
        assert_eq!(env.parsed[0].id.len(), 64);
        let v = env.parsed[0].price.price_f64().unwrap();
        assert!(v > 60_000.0 && v < 62_000.0);
    }
}
