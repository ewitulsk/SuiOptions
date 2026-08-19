use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A 32-byte Sui address.
///
/// BCS: 32 raw bytes (matches Move `address`).
/// JSON: `0x`-prefixed 64-hex string.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SuiAddress(pub [u8; 32]);

/// A Sui object ID. Identical wire representation to an address.
pub type ObjectId = SuiAddress;

/// A 32-byte order digest (blake2b-256 of the domain-separated order).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; 32]);

#[derive(Debug, thiserror::Error)]
#[error("invalid hex value: {0}")]
pub struct ParseHexError(String);

fn parse_hex_32(s: &str, pad: bool) -> Result<[u8; 32], ParseHexError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let padded;
    let hex_str = if pad && stripped.len() < 64 {
        padded = format!("{:0>64}", stripped);
        &padded
    } else {
        stripped
    };
    let bytes = hex::decode(hex_str).map_err(|_| ParseHexError(s.to_string()))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ParseHexError(s.to_string()))?;
    Ok(arr)
}

impl SuiAddress {
    pub const ZERO: SuiAddress = SuiAddress([0u8; 32]);

    /// Parse a hex address, left-padding short forms (`0x2` -> 32 bytes).
    pub fn parse(s: &str) -> Result<Self, ParseHexError> {
        Ok(SuiAddress(parse_hex_32(s, true)?))
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 32]
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl Digest {
    pub fn parse(s: &str) -> Result<Self, ParseHexError> {
        Ok(Digest(parse_hex_32(s, false)?))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl fmt::Debug for SuiAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Display for SuiAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

// BCS is not human-readable: emit the raw fixed 32 bytes so the encoding is
// byte-identical to Move `address`. JSON is human-readable: hex string.
impl Serialize for SuiAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_hex())
        } else {
            // serde tuple of fixed length => no length prefix, matches Move.
            use serde::ser::SerializeTuple;
            let mut t = serializer.serialize_tuple(32)?;
            for b in &self.0 {
                t.serialize_element(b)?;
            }
            t.end()
        }
    }
}

impl<'de> Deserialize<'de> for SuiAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            SuiAddress::parse(&s).map_err(serde::de::Error::custom)
        } else {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = [u8; 32];
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("32 bytes")
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut arr = [0u8; 32];
                    for (i, slot) in arr.iter_mut().enumerate() {
                        *slot = seq
                            .next_element()?
                            .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                    }
                    Ok(arr)
                }
            }
            deserializer.deserialize_tuple(32, V).map(SuiAddress)
        }
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_hex())
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Digest::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A canonicalized Move type string: `0x` + full 64-hex address + `::module::Name`.
pub type TypeTagStr = String;

#[derive(Debug, thiserror::Error)]
#[error("invalid move type string: {0}")]
pub struct ParseTypeError(String);

/// Normalize a Move coin type string to the exact form the contracts compare
/// against — the byte-for-byte output of `exchange::order::canonical_type<T>()`:
/// `0x` + zero-padded 64-hex address + `::module::Name`, and for generic
/// instantiations the type arguments rendered recursively with **bare**
/// (un-prefixed) padded addresses, comma-separated, no spaces. Only the
/// outermost address carries `0x` — Move builds the string as
/// `"0x" + type_name::with_original_ids::<T>()`, and `type_name` renders every
/// nested address bare.
///
/// Move/coin type strings arrive in several non-byte-equal forms (chain
/// `TypeName` without `0x` at any depth, event/RPC strings with `0x` at every
/// depth, short vs padded addresses, ", "-spaced generics); always compare via
/// this function. Getting the inner-address form wrong is not cosmetic: the
/// signed order token strings must byte-match `canonical_type` or settlement
/// aborts `ETokenMismatch` on-chain.
pub fn canonicalize_move_type(s: &str) -> Result<TypeTagStr, ParseTypeError> {
    let err = || ParseTypeError(s.to_string());

    enum Tok<'a> {
        Atom(&'a str),
        Open,
        Close,
        Comma,
    }
    let mut toks = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        match c {
            '<' | '>' | ',' => {
                if let Some(st) = start.take() {
                    toks.push(Tok::Atom(&s[st..i]));
                }
                toks.push(match c {
                    '<' => Tok::Open,
                    '>' => Tok::Close,
                    _ => Tok::Comma,
                });
            }
            c if c.is_whitespace() => {
                if let Some(st) = start.take() {
                    toks.push(Tok::Atom(&s[st..i]));
                }
            }
            _ => {
                if start.is_none() {
                    start = Some(i);
                }
            }
        }
    }
    if let Some(st) = start {
        toks.push(Tok::Atom(&s[st..]));
    }

    let mut out = String::with_capacity(s.len() + 64);
    let mut depth: u32 = 0;
    // Whether a completed type sits immediately before the cursor (required
    // ahead of `<`, `,`, `>` and at end; forbidden ahead of another atom).
    let mut have_item = false;
    for t in toks {
        match t {
            Tok::Atom(a) => {
                if have_item {
                    return Err(err());
                }
                out.push_str(&canonicalize_atom(a, depth == 0).ok_or_else(err)?);
                have_item = true;
            }
            Tok::Open => {
                if !have_item {
                    return Err(err());
                }
                out.push('<');
                depth += 1;
                have_item = false;
            }
            Tok::Close => {
                if depth == 0 || !have_item {
                    return Err(err());
                }
                out.push('>');
                depth -= 1;
            }
            Tok::Comma => {
                if depth == 0 || !have_item {
                    return Err(err());
                }
                out.push(',');
                have_item = false;
            }
        }
    }
    if depth != 0 || !have_item {
        return Err(err());
    }
    Ok(out)
}

/// Canonicalize one `addr::module::Name` atom. `prefixed` selects the
/// outermost (`0x`-carrying) rendering; nested type args render bare.
fn canonicalize_atom(atom: &str, prefixed: bool) -> Option<String> {
    let mut parts = atom.splitn(2, "::");
    let addr = parts.next()?;
    let rest = parts.next()?;
    let addr_hex = addr.strip_prefix("0x").unwrap_or(addr);
    if addr_hex.is_empty()
        || addr_hex.len() > 64
        || !addr_hex.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let mut mods = rest.split("::");
    let (module, name) = (mods.next(), mods.next());
    match (module, name, mods.next()) {
        (Some(m), Some(n), None) if !m.is_empty() && !n.is_empty() => {
            let prefix = if prefixed { "0x" } else { "" };
            Some(format!("{prefix}{:0>64}::{}::{}", addr_hex.to_lowercase(), m, n))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_bcs_is_32_raw_bytes() {
        let a = SuiAddress::parse("0x2").unwrap();
        let bytes = bcs::to_bytes(&a).unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[31], 2);
        let back: SuiAddress = bcs::from_bytes(&bytes).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn canonicalize() {
        assert_eq!(
            canonicalize_move_type("0x2::sui::SUI").unwrap(),
            format!("0x{}2::sui::SUI", "0".repeat(63))
        );
        // chain TypeName form (no 0x) canonicalizes to the same string
        assert_eq!(
            canonicalize_move_type(&format!("{}2::sui::SUI", "0".repeat(63))).unwrap(),
            canonicalize_move_type("0x2::sui::SUI").unwrap()
        );
        assert!(canonicalize_move_type("nonsense").is_err());
        assert!(canonicalize_move_type("0x2::sui").is_err());
    }

    /// The signed form: outer `0x`, inner args bare — exactly
    /// `exchange::order::canonical_type<T>()` for a generic option coin.
    /// Golden layout cross-checked against `contracts/exchange/sources/
    /// order.move` ("0x" + `type_name::with_original_ids`) and the marker
    /// rendering asserted on-chain by `option_coin::expected_type_bytes`.
    #[test]
    fn canonicalize_option_coin_generics() {
        let pkg = "ab".repeat(32);
        let tok = "9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86";
        let sui = format!("{:0>64}", "2");
        let want = format!(
            "0x{pkg}::option_coin::OptionCall<{tok}::tbtc::TBTC,{sui}::sui::SUI,{pkg}::enc0::B02>"
        );
        // Already-signed exchange form is a fixed point.
        assert_eq!(canonicalize_move_type(&want).unwrap(), want);
        // Chain TypeName form: bare addresses at every depth, no padding on
        // the framework address.
        let chain = format!(
            "{pkg}::option_coin::OptionCall<{tok}::tbtc::TBTC,2::sui::SUI,{pkg}::enc0::B02>"
        );
        assert_eq!(canonicalize_move_type(&chain).unwrap(), want);
        // Event/RPC display form: 0x at every depth, ", "-spaced.
        let rpc = format!(
            "0x{pkg}::option_coin::OptionCall<0x{tok}::tbtc::TBTC, 0x2::sui::SUI, 0x{pkg}::enc0::B02>"
        );
        assert_eq!(canonicalize_move_type(&rpc).unwrap(), want);
    }

    #[test]
    fn canonicalize_nested_generics() {
        let a = format!("{:0>64}", "a");
        let want = format!("0x{a}::m::A<{a}::m::B<{a}::m::C>,{a}::m::D>");
        assert_eq!(
            canonicalize_move_type(&format!("0xa::m::A<0xa::m::B<0xa::m::C>, 0xa::m::D>"))
                .unwrap(),
            want
        );
        assert_eq!(canonicalize_move_type(&want).unwrap(), want);
    }

    #[test]
    fn canonicalize_rejects_malformed_generics() {
        assert!(canonicalize_move_type("0x2::sui::SUI<").is_err());
        assert!(canonicalize_move_type("0x2::sui::SUI>").is_err());
        assert!(canonicalize_move_type("0x2::m::A<>").is_err());
        assert!(canonicalize_move_type("0x2::m::A<0x2::m::B,>").is_err());
        assert!(canonicalize_move_type("0x2::m::A<0x2::m::B").is_err());
        assert!(canonicalize_move_type("0x2::m::A,0x2::m::B").is_err());
        assert!(canonicalize_move_type("0x2::m::A<u64>").is_err());
        assert!(canonicalize_move_type("0x2 ::sui::SUI").is_err());
    }
}
