//! Asset identifier — the off-chain stand-in for Move's `TypeName`.
//!
//! On chain, Sui keys balances and event payloads by `TypeName<T>`, a
//! canonical `address::module::type` path. Off-chain we keep it as the raw
//! string; we never re-derive types from it, only use it for routing and
//! display.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssetType(pub String);

impl AssetType {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AssetType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AssetType {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for AssetType {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_json() {
        let a = AssetType::new("0x2::sui::SUI");
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(j, "\"0x2::sui::SUI\"");
        let back: AssetType = serde_json::from_str(&j).unwrap();
        assert_eq!(back, a);
    }
}
