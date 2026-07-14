//! Mirror structs for the Anchor events emitted by the three Solana
//! programs (options_core / auction_venue / options_vault).
//!
//! Anchor's `#[event]` types derive Borsh only (no serde), so the indexer
//! keeps its own mirrors carrying BOTH derives: Borsh to decode the
//! `emit_cpi!` inner-instruction payloads, serde to produce the JSONB
//! `payload` column and the GraphQL wire form. Field names and order MUST
//! match `solana-contracts/programs/*/src/events.rs` exactly — Borsh is
//! positional. `tests/idl_fixtures.rs` cross-checks every mirror against
//! the committed Anchor IDL snapshots to catch drift.
//!
//! Wire conventions (mirroring the Sui indexer's `protocol_types`):
//!   - `Pubkey` → base58 string.
//!   - `u64` / `u128` → decimal strings (JS 53-bit precision safety).
//!   - `Vec<u8>` → lowercase hex string.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use borsh::BorshDeserialize;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// First 8 bytes of an `emit_cpi!` self-CPI's instruction data:
/// `sha256("anchor:event")[..8]`. Verified by a unit test below.
pub const EVENT_IX_TAG_LE: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

/// Which program an event belongs to. Discriminators are only matched
/// against events defined by the program that emitted the inner
/// instruction, so a same-named event in a foreign program can't confuse
/// the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Program {
    Core,
    Venue,
    Vault,
}

impl Program {
    pub fn as_str(&self) -> &'static str {
        match self {
            Program::Core => "options_core",
            Program::Venue => "auction_venue",
            Program::Vault => "options_vault",
        }
    }
}

/// 32-byte Solana pubkey. Serde form is base58; Borsh form is the raw
/// bytes (matching `anchor_lang::prelude::Pubkey`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshDeserialize)]
pub struct Pubkey(pub [u8; 32]);

impl Pubkey {
    pub fn to_base58(&self) -> String {
        bs58::encode(self.0).into_string()
    }

    pub fn from_base58(s: &str) -> Result<Self> {
        let bytes = bs58::decode(s)
            .into_vec()
            .with_context(|| format!("base58 decode of {s:?}"))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("pubkey {s:?} is {} bytes, want 32", v.len()))?;
        Ok(Pubkey(arr))
    }

    /// `Pubkey::default()` on-chain — the venue uses it as "no bucket"
    /// (pure swaps).
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 32]
    }
}

impl std::fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_base58())
    }
}

impl std::fmt::Display for Pubkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_base58())
    }
}

impl Serialize for Pubkey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_base58())
    }
}

impl<'de> Deserialize<'de> for Pubkey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(d)?;
        Pubkey::from_base58(&s).map_err(serde::de::Error::custom)
    }
}

/// `u64` whose JSON form is a decimal string (JS 53-bit safety). Borsh
/// layout is identical to a bare `u64` (newtype transparency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, BorshDeserialize)]
pub struct U64Str(pub u64);

impl Serialize for U64Str {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for U64Str {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(d)?;
        s.parse().map(U64Str).map_err(serde::de::Error::custom)
    }
}

/// `u128` twin of [`U64Str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, BorshDeserialize)]
pub struct U128Str(pub u128);

impl Serialize for U128Str {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for U128Str {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(d)?;
        s.parse().map(U128Str).map_err(serde::de::Error::custom)
    }
}

/// `Vec<u8>` whose JSON form is lowercase hex (signing pubkeys).
#[derive(Debug, Clone, PartialEq, Eq, BorshDeserialize)]
pub struct HexBytes(pub Vec<u8>);

impl Serialize for HexBytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use std::fmt::Write;
        let mut out = String::with_capacity(self.0.len() * 2);
        for b in &self.0 {
            let _ = write!(out, "{b:02x}");
        }
        s.serialize_str(&out)
    }
}

impl<'de> Deserialize<'de> for HexBytes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(serde::de::Error::custom("odd-length hex"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect::<std::result::Result<Vec<u8>, _>>()
            .map(HexBytes)
    }
}

/// `auction_venue::state::AuctionMode` mirror. Borsh unit-enum (u8 tag);
/// serde form matches the DB `auctions.mode` column values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshDeserialize, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuctionMode {
    Swap,
    CoveredCall,
    CashSecuredPut,
}

impl AuctionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuctionMode::Swap => "swap",
            AuctionMode::CoveredCall => "covered_call",
            AuctionMode::CashSecuredPut => "cash_secured_put",
        }
    }
}

/// Declares every event mirror plus the decode plumbing:
///   - one struct per event (Borsh + serde derives, wire adapters applied
///     per field type via `event_field!`);
///   - `DecodedEvent` — the typed union the DB folds match on;
///   - `tag()` — the stable `indexed_events.event_type` string;
///   - `payload()` / `from_payload()` — JSONB (de)serialization;
///   - per-program discriminator registries (`sha256("event:{Name}")[..8]`).
macro_rules! events {
    ($( $program:ident => { $( $name:ident { $( $field:ident : $fty:tt ),* $(,)? } )* } )*) => {
        $($(
            #[derive(Debug, Clone, PartialEq, BorshDeserialize, Serialize, Deserialize)]
            pub struct $name {
                $( pub $field: event_field!(@type $fty), )*
            }
        )*)*

        /// Typed union of every decoded event.
        #[derive(Debug, Clone, PartialEq)]
        pub enum DecodedEvent {
            $($( $name($name), )*)*
        }

        impl DecodedEvent {
            /// Stable tag stored in `indexed_events.event_type`.
            pub fn tag(&self) -> &'static str {
                match self {
                    $($( DecodedEvent::$name(_) => stringify!($name), )*)*
                }
            }

            /// Program that defines this event.
            pub fn program(&self) -> Program {
                match self {
                    $($( DecodedEvent::$name(_) => program_kind!($program), )*)*
                }
            }

            /// The JSONB payload — the bare struct fields, wire-encoded.
            pub fn payload(&self) -> Result<serde_json::Value> {
                Ok(match self {
                    $($( DecodedEvent::$name(e) => serde_json::to_value(e)?, )*)*
                })
            }

            /// Rebuild the typed event from a stored `(event_type, payload)`
            /// pair — the view-rebuild path after a fork eviction.
            pub fn from_payload(tag: &str, payload: &serde_json::Value) -> Result<Self> {
                Ok(match tag {
                    $($( stringify!($name) =>
                        DecodedEvent::$name(serde_json::from_value(payload.clone())?), )*)*
                    other => anyhow::bail!("unknown event tag {other:?}"),
                })
            }
        }

        /// disc → decoder, one registry per program.
        pub fn registry() -> &'static HashMap<(Program, [u8; 8]), fn(&[u8]) -> Result<DecodedEvent>> {
            static REGISTRY: OnceLock<
                HashMap<(Program, [u8; 8]), fn(&[u8]) -> Result<DecodedEvent>>,
            > = OnceLock::new();
            REGISTRY.get_or_init(|| {
                let mut m: HashMap<(Program, [u8; 8]), fn(&[u8]) -> Result<DecodedEvent>> =
                    HashMap::new();
                $($(
                    m.insert(
                        (program_kind!($program), event_discriminator(stringify!($name))),
                        |bytes| {
                            let ev = $name::try_from_slice(bytes)
                                .with_context(|| format!("borsh decode of {}", stringify!($name)))?;
                            Ok(DecodedEvent::$name(ev))
                        },
                    );
                )*)*
                m
            })
        }

        /// Every `(program, tag)` pair — drives the IDL drift check.
        #[cfg(test)]
        pub fn all_event_tags() -> Vec<(Program, &'static str)> {
            vec![ $($( (program_kind!($program), stringify!($name)), )*)* ]
        }
    };
}

macro_rules! program_kind {
    (core) => {
        Program::Core
    };
    (venue) => {
        Program::Venue
    };
    (vault) => {
        Program::Vault
    };
}

/// Maps the field-type shorthand used in `events!` to real types. The
/// wire adapters live in the types themselves: `Pubkey` → base58,
/// `U64Str`/`U128Str` → decimal strings, `HexBytes` → hex.
macro_rules! event_field {
    (@type pubkey) => { Pubkey };
    (@type opt_pubkey) => { Option<Pubkey> };
    (@type u64) => { U64Str };
    (@type u128) => { U128Str };
    (@type u8) => { u8 };
    (@type bool) => { bool };
    (@type string) => { String };
    (@type bytes) => { HexBytes };
    (@type auction_mode) => { AuctionMode };
}

/// Anchor event discriminator: `sha256("event:{name}")[..8]`.
pub fn event_discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("event:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

/// Decode one `emit_cpi!` inner-instruction payload (`data` starts at the
/// event discriminator, i.e. after the 8-byte event-ix tag). `None` when
/// the discriminator isn't one of ours — foreign CPI data must never
/// stall the pipeline.
pub fn decode_event(program: Program, data: &[u8]) -> Result<Option<DecodedEvent>> {
    if data.len() < 8 {
        return Ok(None);
    }
    let mut disc = [0u8; 8];
    disc.copy_from_slice(&data[..8]);
    match registry().get(&(program, disc)) {
        Some(decode) => decode(&data[8..]).map(Some),
        None => Ok(None),
    }
}

// Field order transcribed from solana-contracts/programs/*/src/events.rs —
// Borsh is positional, so order is load-bearing.
events! {
    core => {
        BucketCreated {
            bucket: pubkey, underlying_mint: pubkey, settlement_mint: pubkey,
            call_mint: pubkey, expiry_ms: u64, strike: u128, strike_scale: u8,
        }
        WriteExecuted {
            bucket: pubkey, signer_account: pubkey, signer_token_recipient: pubkey,
            executor: pubkey, position: pubkey, position_recipient: pubkey,
            call_token_recipient: pubkey, write_amount: u64, gross_premium: u64,
            fee: u64, net_premium: u64, range_start: u128, range_end: u128, nonce: u64,
        }
        CollateralizedWrite {
            bucket: pubkey, writer: pubkey, position: pubkey, amount: u64,
            range_start: u128, range_end: u128,
        }
        Exercised {
            bucket: pubkey, exerciser: pubkey, amount: u64, settlement_paid: u64,
            cursor_after: u128,
        }
        Redeemed {
            bucket: pubkey, position: pubkey, redeemer: pubkey, range_start: u128,
            range_end: u128, underlying_returned: u64, settlement_returned: u64,
        }
        ExpiredOptionBurned { bucket: pubkey, burner: pubkey, amount: u64 }
        BucketCleaned { bucket: pubkey }
        BucketInvalidated { bucket: pubkey, timestamp_ms: u64, admin: pubkey, reason: string }
        BucketRevalidated { bucket: pubkey, timestamp_ms: u64, admin: pubkey, reason: string }
        PutBucketCreated {
            bucket: pubkey, underlying_mint: pubkey, settlement_mint: pubkey,
            put_mint: pubkey, expiry_ms: u64, strike: u128, strike_scale: u8,
        }
        PutWriteExecuted {
            bucket: pubkey, signer_account: pubkey, signer_token_recipient: pubkey,
            executor: pubkey, position: pubkey, position_recipient: pubkey,
            put_token_recipient: pubkey, write_amount: u64, collateral: u64,
            gross_premium: u64, fee: u64, net_premium: u64, range_start: u128,
            range_end: u128, nonce: u64,
        }
        PutCollateralizedWrite {
            bucket: pubkey, writer: pubkey, position: pubkey, write_amount: u64,
            collateral: u64, range_start: u128, range_end: u128,
        }
        PutExercised {
            bucket: pubkey, exerciser: pubkey, amount: u64, settlement_paid: u64,
            cursor_after: u128,
        }
        PutRedeemed {
            bucket: pubkey, position: pubkey, redeemer: pubkey, range_start: u128,
            range_end: u128, underlying_returned: u64, settlement_returned: u64,
        }
        PutExpiredOptionBurned { bucket: pubkey, burner: pubkey, amount: u64 }
        PutBucketCleaned { bucket: pubkey, dust_swept: u64 }
        PutBucketInvalidated { bucket: pubkey, timestamp_ms: u64, admin: pubkey, reason: string }
        PutBucketRevalidated { bucket: pubkey, timestamp_ms: u64, admin: pubkey, reason: string }
        AccountCreated {
            account: pubkey, owner: pubkey, signing_scheme: u8, signing_pubkey: bytes,
        }
        AccountDeposit { account: pubkey, mint: pubkey, amount: u64 }
        AccountWithdraw { account: pubkey, mint: pubkey, amount: u64 }
        SigningKeyRotated { account: pubkey, new_scheme: u8, new_pubkey: bytes }
        FeeUpdated { old_bps: u64, new_bps: u64 }
        AdminChanged { old_admin: pubkey, new_admin: pubkey }
        TreasuryWithdrawn { mint: pubkey, amount: u64, recipient: pubkey }
        ProtocolFeeDeposited { mint: pubkey, amount: u64, payer: pubkey }
        PositionTransferred { position: pubkey, old_owner: pubkey, new_owner: pubkey }
    }
    venue => {
        AuctionCreated {
            auction: pubkey, mode: auction_mode, bucket: pubkey, creator: pubkey,
            escrow_mint: pubkey, bid_mint: pubkey, amount: u64, notional: u64,
            reserve_bid: u64, deadline_ms: u64, max_deadline_ms: u64,
            min_increment_bps: u64, settle_authority: opt_pubkey,
        }
        AuctionBid {
            auction: pubkey, bidder: pubkey, token_recipient: pubkey, bid: u64,
            previous_bid: u64, deadline_ms: u64,
        }
        AuctionSettled {
            auction: pubkey, mode: auction_mode, bucket: pubkey, winner: pubkey,
            token_recipient: pubkey, position: pubkey, position_recipient: pubkey,
            amount: u64, notional: u64, gross_bid: u64, fee: u64, net_proceeds: u64,
        }
        AuctionUnsold {
            auction: pubkey, mode: auction_mode, bucket: pubkey, amount: u64,
            reserve_bid: u64, bid_refunded: bool,
        }
    }
    vault => {
        VaultCreated {
            vault: pubkey, underlying_mint: pubkey, settlement_mint: pubkey,
            share_mint: pubkey, mgmt_fee_bps_annual: u64, perf_fee_bps: u64,
            round_ms: u64, selling_window_ms: u64, min_strike_bps_over_spot: u64,
            max_strike_bps_over_spot: u64,
        }
        VaultConfigUpdated { vault: pubkey, round: u64 }
        VaultConfigApplied {
            vault: pubkey, round: u64, mgmt_fee_bps_annual: u64, perf_fee_bps: u64,
            round_ms: u64, selling_window_ms: u64, min_strike_bps_over_spot: u64,
            max_strike_bps_over_spot: u64,
        }
        VaultDepositsPaused { vault: pubkey, paused: bool }
        VaultDeposit { vault: pubkey, depositor: pubkey, round: u64, amount: u64 }
        SharesClaimed { vault: pubkey, claimer: pubkey, round: u64, amount: u64, shares: u64 }
        WithdrawInitiated { vault: pubkey, withdrawer: pubkey, round: u64, shares: u64 }
        WithdrawCompleted {
            vault: pubkey, withdrawer: pubkey, round: u64, shares: u64, amount: u64,
        }
        InstantWithdraw { vault: pubkey, withdrawer: pubkey, round: u64, amount: u64 }
        VaultBucketSelected {
            vault: pubkey, round: u64, bucket: pubkey, strike: u128, strike_scale: u8,
            expiry_ms: u64, selling_ends_ms: u64, spot: u128, spot_scale: u8,
        }
        VaultPositionRedeemed {
            vault: pubkey, round: u64, position: pubkey, underlying: u64, settlement: u64,
        }
        VaultRfqOpened {
            vault: pubkey, round: u64, auction: pubkey, slice_amount: u64,
            reserve_premium: u64,
        }
        VaultRfqSettled {
            vault: pubkey, round: u64, auction: pubkey, position: pubkey, amount: u64,
            net_premium: u64,
        }
        VaultRfqUnsold { vault: pubkey, round: u64, auction: pubkey, amount: u64 }
        VaultSwapOpened {
            vault: pubkey, round: u64, auction: pubkey, amount_s: u64,
            reserve_underlying: u64,
        }
        VaultSwapSettled {
            vault: pubkey, round: u64, auction: pubkey, bidder: pubkey,
            settlement_out: u64, underlying_in: u64,
        }
        VaultSwapUnfilled { vault: pubkey, round: u64, auction: pubkey, amount_s: u64 }
        VaultFeesCharged { vault: pubkey, round: u64, mgmt_fee: u64, perf_fee: u64 }
        VaultRoundFinalized {
            vault: pubkey, round: u64, pps: u128, aum: u64, shares: u64, premium_s: u64,
            premium_u: u64, withdrawals_owed: u64, shares_burned: u64,
            deposits_processed: u64, shares_minted: u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ix_tag_is_sha256_of_anchor_event() {
        // Anchor reads sha256("anchor:event")[..8] as a big-endian u64
        // (0x1d9acb512ea545e4) and writes it little-endian — the on-wire
        // form is the reversed digest prefix.
        let digest = Sha256::digest(b"anchor:event");
        let mut reversed: [u8; 8] = digest[..8].try_into().unwrap();
        reversed.reverse();
        assert_eq!(reversed, EVENT_IX_TAG_LE);
    }

    #[test]
    fn discriminators_are_distinct_within_each_program() {
        let mut seen = std::collections::HashSet::new();
        for (program, tag) in all_event_tags() {
            assert!(
                seen.insert((program, event_discriminator(tag))),
                "duplicate discriminator for {tag}"
            );
        }
        assert_eq!(seen.len(), 50, "expected 50 events across the 3 programs");
    }

    #[test]
    fn decode_round_trips_a_write_executed() {
        // Hand-encode Borsh: pubkeys are raw 32 bytes, ints little-endian.
        let mut bytes = Vec::new();
        for i in 0u8..7 {
            bytes.extend_from_slice(&[i + 1; 32]); // 7 pubkey fields
        }
        for v in [10u64, 20, 3, 17] {
            bytes.extend_from_slice(&v.to_le_bytes()); // write_amount..net_premium
        }
        for v in [100u128, 110] {
            bytes.extend_from_slice(&v.to_le_bytes()); // range_start, range_end
        }
        bytes.extend_from_slice(&42u64.to_le_bytes()); // nonce

        let mut data = event_discriminator("WriteExecuted").to_vec();
        data.extend_from_slice(&bytes);
        let ev = decode_event(Program::Core, &data).unwrap().unwrap();
        let DecodedEvent::WriteExecuted(w) = &ev else {
            panic!("wrong variant {ev:?}");
        };
        assert_eq!(w.write_amount.0, 10);
        assert_eq!(w.net_premium.0, 17);
        assert_eq!(w.range_end.0, 110);
        assert_eq!(w.nonce.0, 42);
        assert_eq!(w.bucket, Pubkey([1; 32]));

        // Payload round-trip (JSONB → typed) drives the fork-rebuild path.
        let payload = ev.payload().unwrap();
        assert_eq!(payload["write_amount"], "10");
        assert_eq!(payload["range_end"], "110");
        assert_eq!(payload["bucket"], Pubkey([1; 32]).to_base58());
        let back = DecodedEvent::from_payload("WriteExecuted", &payload).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn foreign_discriminator_decodes_to_none() {
        let mut data = event_discriminator("SomebodyElsesEvent").to_vec();
        data.extend_from_slice(&[0u8; 64]);
        assert!(decode_event(Program::Core, &data).unwrap().is_none());
        // Same-name event under the wrong program is also ignored.
        let mut data = event_discriminator("AuctionBid").to_vec();
        data.extend_from_slice(&[0u8; 128]);
        assert!(decode_event(Program::Core, &data).unwrap().is_none());
    }

    #[test]
    fn auction_mode_borsh_and_serde_forms() {
        let ev_bytes = [1u8]; // CoveredCall
        let mode = AuctionMode::try_from_slice(&ev_bytes).unwrap();
        assert_eq!(mode, AuctionMode::CoveredCall);
        assert_eq!(serde_json::to_value(mode).unwrap(), "covered_call");
    }
}
