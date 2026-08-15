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

/// `0x`-prefix + lowercase + 64-pad EVERY address segment of a Move type
/// string — at any nesting depth. See [`AssetType::to_canonical`].
/// Free-standing so callers holding a bare `&str` coin type can
/// canonicalize without wrapping.
///
/// Generic instantiations matter since any-strike option coins
/// (`0x…::option_coin::OptionCall<0x…::tbtc::TBTC, …>`): chain `TypeName`s
/// arrive with bare addresses at every depth while event/RPC strings carry
/// `0x` at every depth — an outer-only canonicalization makes the two
/// forms unequal and silently breaks every map keyed on coin types
/// (DeepBook pool resolution, appraisal legs, wallet-holdings matching).
/// Whitespace is dropped (", "-separated display forms normalize to the
/// same bytes as the chain's space-less rendering).
///
/// Non-type atoms (primitives, `vector`) and anything that doesn't look
/// like `addr::module::Name` pass through unchanged.
pub fn canonicalize_move_type(s: &str) -> String {
    fn flush(atom: &mut String, out: &mut String) {
        if atom.is_empty() {
            return;
        }
        out.push_str(&canonicalize_atom(atom));
        atom.clear();
    }
    let mut out = String::with_capacity(s.len() + 64);
    let mut atom = String::new();
    for c in s.chars() {
        match c {
            '<' | '>' | ',' => {
                flush(&mut atom, &mut out);
                out.push(c);
            }
            c if c.is_whitespace() => flush(&mut atom, &mut out),
            _ => atom.push(c),
        }
    }
    flush(&mut atom, &mut out);
    out
}

/// **Chain form**: canonical, but with `0x` stripped from every address
/// segment — `0000…0002::sui::SUI`. This is what Move's
/// `type_name::with_defining_ids` produces, and therefore what a `TypeName`
/// BCS-encodes to, so it is the form any SIGNED payload must carry (see
/// `crate::bucket_spec`). It is also the form the indexer stores, which is
/// why its string-matching filters must be fed chain form rather than the
/// `0x` form we emit to clients — the SO-163 / #479 trap, in its two guises.
pub fn chain_form_move_type(s: &str) -> String {
    canonicalize_move_type(s).replace("0x", "")
}

/// Canonicalize one `addr::module::Name` atom; pass through anything else.
fn canonicalize_atom(atom: &str) -> String {
    match atom.split_once("::") {
        Some((addr, rest)) if !addr.is_empty() => {
            let hex = addr.strip_prefix("0x").unwrap_or(addr);
            if hex.chars().all(|c| c.is_ascii_hexdigit()) {
                format!("0x{:0>64}::{rest}", hex.to_ascii_lowercase())
            } else {
                atom.to_string()
            }
        }
        _ => atom.to_string(),
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

    #[test]
    fn canonical_recurses_into_generic_type_args() {
        // Chain-TypeName form: bare addresses at EVERY depth.
        let pkg = "ab".repeat(32);
        let tok = "9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86";
        let chain = format!(
            "{pkg}::option_coin::OptionCall<{tok}::tbtc::TBTC,2::sui::SUI,{pkg}::enc0::B02>"
        );
        // Event/RPC form: 0x-prefixed at every depth, possibly ", "-spaced.
        let rpc = format!(
            "0x{pkg}::option_coin::OptionCall<0x{tok}::tbtc::TBTC, 0x{:0>64}::sui::SUI, 0x{pkg}::enc0::B02>",
            "2"
        );
        let want = format!(
            "0x{pkg}::option_coin::OptionCall<0x{tok}::tbtc::TBTC,0x{:0>64}::sui::SUI,0x{pkg}::enc0::B02>",
            "2"
        );
        // The SO-163 regression class: both forms must collapse to the
        // same canonical bytes or every coin-type map lookup goes dark.
        assert_eq!(canonicalize_move_type(&chain), want);
        assert_eq!(canonicalize_move_type(&rpc), want);
        assert_eq!(canonicalize_move_type(&want), want);
    }

    #[test]
    fn canonical_passes_primitives_and_vectors_through() {
        assert_eq!(canonicalize_move_type("u64"), "u64");
        assert_eq!(
            canonicalize_move_type("vector<0x2::sui::SUI>"),
            format!("vector<0x{:0>64}::sui::SUI>", "2")
        );
    }
}
