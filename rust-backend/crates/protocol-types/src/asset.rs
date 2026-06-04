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

    /// Canonical `0x`-prefixed Move type literal, for emitting to clients
    /// (and feeding `suix_getBalance` / PTB type args).
    ///
    /// Chain `TypeName`s arrive *without* the `0x` prefix
    /// (`9b72…::tbtc::TBTC`). The Sui JSON-RPC rejects that form as an
    /// invalid struct type, and clients pass coin types straight into
    /// `suix_getBalance`, so anything we emit must be a valid literal:
    /// `0x`-prefixed, lowercase, address left-padded to 64 hex chars.
    /// Matches the frontend's `normalizeStructTag`.
    pub fn to_canonical(&self) -> String {
        canonicalize_move_type(&self.0)
    }
}

/// `0x`-prefix + lowercase + 64-pad the address segment of a Move type
/// string. See [`AssetType::to_canonical`]. Free-standing so callers
/// holding a bare `&str` coin type can canonicalize without wrapping.
///
/// Returns the input unchanged if it has no `::` (defensive — shouldn't
/// happen for a real coin type).
pub fn canonicalize_move_type(s: &str) -> String {
    match s.split_once("::") {
        Some((addr, rest)) => {
            let hex = addr.strip_prefix("0x").unwrap_or(addr).to_ascii_lowercase();
            format!("0x{hex:0>64}::{rest}")
        }
        None => s.to_string(),
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

    #[test]
    fn canonical_adds_0x_to_chain_typename() {
        // The exact form chain events emit: no 0x, already 64-hex padded.
        let raw = "9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86";
        let a = AssetType::new(format!("{raw}::tbtc::TBTC"));
        assert_eq!(a.to_canonical(), format!("0x{raw}::tbtc::TBTC"));
    }

    #[test]
    fn canonical_pads_short_framework_type() {
        assert_eq!(
            canonicalize_move_type("0x2::sui::SUI"),
            format!("0x{:0>64}::sui::SUI", "2")
        );
    }

    #[test]
    fn canonical_is_idempotent() {
        let once = canonicalize_move_type("0x2::sui::SUI");
        assert_eq!(canonicalize_move_type(&once), once);
    }
}
