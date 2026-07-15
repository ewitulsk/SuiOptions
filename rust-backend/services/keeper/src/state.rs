//! Chain-state reads: build a [`VaultView`] (and the open-auction
//! [`RfqView`]s) per configured vault each tick.
//!
//! Objects are read through the JSON-RPC parsed content (one canonical
//! wire form, golden-tested by the localnet e2e). The JSON conventions
//! that matter, per `sui-json-rpc-types`:
//!   u64/u128 → decimal string · `ID` → address string · `UID` →
//!   `{"id": …}` · `Balance` → its value · `Option` → null or the inner
//!   value · enums → `{"variant": name, "fields": {…}}`.

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::StructTag;
use serde_json::Value;
use sui_json_rpc_types::{EventFilter, SuiObjectDataOptions, SuiParsedData};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use tracing::warn;

/// Everything the planner needs from one `Vault<U, S, V>` object.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultView {
    pub round: u64,
    pub settling: bool,
    pub current_bucket: Option<ObjectID>,
    /// 0 ⇒ no bucket was selected this round.
    pub current_expiry_ms: u64,
    pub selling_ends_ms: u64,
    pub open_rfqs: u64,
    /// Coupled proceeds-swap auctions not yet settled.
    pub open_swap_rfqs: u64,
    /// positions_tail − positions_head.
    pub pending_positions: u64,
    pub deployable: u64,
    pub proceeds_settlement: u64,
    pub pending_deposits: u64,
    pub queued_withdraw_shares: u64,
    pub config: VaultConfigView,
}

/// The slice of `VaultConfig` the keeper plans against, plus the pinned
/// oracle identity discovery resolves `PriceInfoObject`s from.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultConfigView {
    pub min_strike_bps_over_spot: u64,
    pub max_strike_bps_over_spot: u64,
    /// Floor the on-chain `open_rfq` sets the reserve premium at: bps of the
    /// slice notional. The keeper uses it to skip rounds whose snapped strike
    /// can't clear the reserve (see `select_bucket_or_finalize`).
    pub min_reserve_premium_bps: u64,
    pub min_expiry_lead_ms: u64,
    pub max_expiry_lead_ms: u64,
    pub max_slice_amount: u64,
    pub max_open_rfqs: u64,
    pub round_ms: u64,
    pub selling_window_ms: u64,
    pub hold_premium_in_settlement: bool,
    /// Pinned Pyth feed ids (32 raw bytes each) — what `oracle::spot_cross`
    /// enforces, so discovery treats them as authoritative.
    pub underlying_feed_id: Vec<u8>,
    pub settlement_feed_id: Vec<u8>,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
}

/// One live vault-coupled auction (the object still exists ⇒ unsettled).
/// The generic `Auction` carries no bucket id — a vault RFQ slice is
/// always on the vault's `current_bucket`.
#[derive(Debug, Clone, PartialEq)]
pub struct RfqView {
    pub rfq_id: ObjectID,
    pub deadline_ms: u64,
    pub amount: u64,
}

/// One live vault-coupled proceeds-swap auction (still exists ⇒ unsettled).
#[derive(Debug, Clone, PartialEq)]
pub struct SwapRfqView {
    pub swap_id: ObjectID,
    pub deadline_ms: u64,
    /// Settlement escrowed (for logging/metrics).
    pub amount_s: u64,
}

// ── JSON field helpers ─────────────────────────────────────────────────

fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value> {
    v.get(name).ok_or_else(|| anyhow!("missing field {name} in {v}"))
}

fn as_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("non-u64 number {n}")),
        Value::String(s) => s.parse().with_context(|| format!("parsing u64 from {s:?}")),
        other => Err(anyhow!("expected u64, got {other}")),
    }
}

fn as_bool(v: &Value) -> Result<bool> {
    v.as_bool().ok_or_else(|| anyhow!("expected bool, got {v}"))
}

fn as_id(v: &Value) -> Result<ObjectID> {
    let s = match v {
        Value::String(s) => s.as_str(),
        // UID renders as {"id": "0x…"}; tolerate an un-unwrapped ID too.
        Value::Object(m) => m
            .get("id")
            .or_else(|| m.get("bytes"))
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("expected id object, got {v}"))?,
        other => return Err(anyhow!("expected id, got {other}")),
    };
    s.parse().with_context(|| format!("parsing object id {s:?}"))
}

/// Option<T> renders as null / the inner value.
fn as_opt(v: &Value) -> Option<&Value> {
    if v.is_null() {
        None
    } else {
        Some(v)
    }
}

fn u64_field(v: &Value, name: &str) -> Result<u64> {
    as_u64(field(v, name)?).with_context(|| format!("field {name}"))
}

fn u8_field(v: &Value, name: &str) -> Result<u8> {
    u8::try_from(u64_field(v, name)?).with_context(|| format!("field {name} out of u8 range"))
}

/// `vector<u8>` renders as a JSON array of numbers; tolerate a hex
/// string too.
fn bytes_field(v: &Value, name: &str) -> Result<Vec<u8>> {
    match field(v, name)? {
        Value::Array(items) => items
            .iter()
            .map(|i| {
                i.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| anyhow!("field {name}: non-byte element {i}"))
            })
            .collect(),
        Value::String(s) => {
            let s = s.strip_prefix("0x").unwrap_or(s);
            (0..s.len())
                .step_by(2)
                .map(|i| {
                    u8::from_str_radix(s.get(i..i + 2).unwrap_or(""), 16)
                        .with_context(|| format!("field {name}: bad hex"))
                })
                .collect()
        }
        other => Err(anyhow!("field {name}: expected bytes, got {other}")),
    }
}

// ── parsers ────────────────────────────────────────────────────────────

/// Parse a `Vault` object's parsed-JSON field map.
pub fn parse_vault_view(fields: &Value) -> Result<VaultView> {
    let round = u64_field(fields, "round")?;
    // Move enums come back from the JSON-RPC parsed content as an *empty*
    // object on the pinned SDK (sui-sdk 1.71.x / framework-mainnet): the
    // variant name is dropped, so `phase` deserializes to `{}` — see
    // `examples/dump_phase.rs`. The variant is therefore only trustworthy
    // when one is actually present (a string, or an object with a
    // `variant` key — e.g. an SDK that later fixes this).
    let phase = field(fields, "phase")?;
    let variant = match phase {
        Value::String(s) => Some(s.as_str()),
        Value::Object(m) => m.get("variant").and_then(|v| v.as_str()),
        other => return Err(anyhow!("unrecognized phase encoding {other}")),
    };
    // When the variant is unreadable (the lossy `{}` form), fall back to
    // the structural invariant: a vault that has not finalized its first
    // round (round 0) is in the genesis `Settling` phase — `finalize_round`
    // is what advances it to round 1 / `Active`, and `select_bucket`
    // requires `Active`, so no bucket is ever selected while round == 0.
    // Round ≥ 1 settling always carries a selected bucket, which the
    // planner detects via `current_bucket` + expiry, so the lossy read is
    // harmless there. Without this, genesis vaults are mis-read as Active
    // and the keeper spams `select_bucket` → abort 35 `vault_wrong_phase`.
    let settling = match variant {
        Some(v) => v == "Settling",
        None => round == 0,
    };
    let positions_head = u64_field(fields, "positions_head")?;
    let positions_tail = u64_field(fields, "positions_tail")?;
    let config = field(fields, "config")?;
    // A nested struct may render bare or as {"type":…, "fields":…}.
    let config = config.get("fields").unwrap_or(config);

    Ok(VaultView {
        round,
        settling,
        current_bucket: as_opt(field(fields, "current_bucket")?)
            .map(as_id)
            .transpose()
            .context("field current_bucket")?,
        current_expiry_ms: u64_field(fields, "current_expiry_ms")?,
        selling_ends_ms: u64_field(fields, "selling_ends_ms")?,
        open_rfqs: u64_field(fields, "open_rfqs")?,
        open_swap_rfqs: u64_field(fields, "open_swap_rfqs")?,
        pending_positions: positions_tail
            .checked_sub(positions_head)
            .ok_or_else(|| anyhow!("positions_head {positions_head} > tail {positions_tail}"))?,
        deployable: u64_field(fields, "deployable")?,
        proceeds_settlement: u64_field(fields, "proceeds_settlement")?,
        pending_deposits: u64_field(fields, "pending_deposits")?,
        queued_withdraw_shares: u64_field(fields, "queued_withdraw_shares")?,
        config: VaultConfigView {
            min_strike_bps_over_spot: u64_field(config, "min_strike_bps_over_spot")?,
            max_strike_bps_over_spot: u64_field(config, "max_strike_bps_over_spot")?,
            min_reserve_premium_bps: u64_field(config, "min_reserve_premium_bps")?,
            min_expiry_lead_ms: u64_field(config, "min_expiry_lead_ms")?,
            max_expiry_lead_ms: u64_field(config, "max_expiry_lead_ms")?,
            max_slice_amount: u64_field(config, "max_slice_amount")?,
            max_open_rfqs: u64_field(config, "max_open_rfqs")?,
            round_ms: u64_field(config, "round_ms")?,
            selling_window_ms: u64_field(config, "selling_window_ms")?,
            hold_premium_in_settlement: as_bool(field(config, "hold_premium_in_settlement")?)?,
            underlying_feed_id: bytes_field(config, "underlying_feed_id")?,
            settlement_feed_id: bytes_field(config, "settlement_feed_id")?,
            underlying_decimals: u8_field(config, "underlying_decimals")?,
            settlement_decimals: u8_field(config, "settlement_decimals")?,
        },
    })
}

/// Parse a vault RFQ-slice `Auction<U, S>` object's parsed-JSON field map.
pub fn parse_rfq_view(rfq_id: ObjectID, fields: &Value) -> Result<RfqView> {
    Ok(RfqView {
        rfq_id,
        deadline_ms: u64_field(fields, "deadline_ms")?,
        amount: u64_field(fields, "amount")?,
    })
}

/// Parse a proceeds-swap `Auction<S, U>` object's parsed-JSON field map
/// (`amount` is the escrowed settlement).
pub fn parse_swap_rfq_view(swap_id: ObjectID, fields: &Value) -> Result<SwapRfqView> {
    Ok(SwapRfqView {
        swap_id,
        deadline_ms: u64_field(fields, "deadline_ms")?,
        amount_s: u64_field(fields, "amount")?,
    })
}

// ── chain fetchers ─────────────────────────────────────────────────────

/// Read one object's parsed-JSON field map; `Ok(None)` if it no longer
/// exists (settled auctions are deleted on-chain).
async fn fetch_fields(client: &SuiClient, id: ObjectID) -> Result<Option<Value>> {
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new().with_content())
        .await
        .with_context(|| format!("reading object {id}"))?;
    let Some(data) = resp.data else {
        return Ok(None);
    };
    match data.content {
        Some(SuiParsedData::MoveObject(obj)) => Ok(Some(obj.fields.to_json_value())),
        other => Err(anyhow!("object {id} has unexpected content: {other:?}")),
    }
}

pub async fn fetch_vault_view(client: &SuiClient, vault_id: ObjectID) -> Result<VaultView> {
    let fields = fetch_fields(client, vault_id)
        .await?
        .ok_or_else(|| anyhow!("vault {vault_id} not found on chain"))?;
    parse_vault_view(&fields).with_context(|| format!("parsing vault {vault_id}"))
}

/// A `TypeName` in event JSON: the inner ascii string, either bare or
/// wrapped as `{"name": "..."}`. Chain TypeNames carry no `0x` prefix —
/// compare canonically (move-type-normalization.md).
fn type_name_str(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        Value::Object(m) => m.get("name").and_then(|n| n.as_str()),
        _ => None,
    }
}

/// Which kind of vault-coupled auction an `AuctionCreated` event
/// announces, told apart by its legs: an RFQ slice is `Auction<U, S>`
/// (underlying escrowed, premium bids); a proceeds swap is
/// `Auction<S, U>` (settlement escrowed, underlying bids).
pub fn classify_vault_auction(
    parsed: &Value,
    underlying_canonical: &str,
    settlement_canonical: &str,
) -> Option<AuctionKind> {
    let escrow = protocol_types::asset::canonicalize_move_type(
        parsed.get("escrow_type").and_then(type_name_str)?,
    );
    if escrow == underlying_canonical {
        Some(AuctionKind::Rfq)
    } else if escrow == settlement_canonical {
        Some(AuctionKind::Swap)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuctionKind {
    Rfq,
    Swap,
}

/// Discover the vault's live coupled auctions (RFQ slices AND proceeds
/// swaps), stateless: walk the auction package's `AuctionCreated` events
/// (newest first) down to `cutoff_ms`, keep those with `origin ==
/// vault_id`, classify by escrow leg, then read each object — still
/// existing ⇒ still open. Restart- and race-safe by construction.
pub async fn discover_open_auctions(
    client: &SuiClient,
    auction_package: ObjectID,
    vault_id: ObjectID,
    underlying_type: &str,
    settlement_type: &str,
    cutoff_ms: u64,
) -> Result<(Vec<RfqView>, Vec<SwapRfqView>)> {
    let filter = EventFilter::MoveEventType(StructTag {
        address: auction_package.into(),
        module: Identifier::new("events").unwrap(),
        name: Identifier::new("AuctionCreated").unwrap(),
        type_params: vec![],
    });

    let underlying = protocol_types::asset::canonicalize_move_type(underlying_type);
    let settlement = protocol_types::asset::canonicalize_move_type(settlement_type);
    let vault_hex = vault_id.to_string();
    let mut candidates: Vec<(ObjectID, AuctionKind)> = Vec::new();
    let mut cursor = None;
    'pages: loop {
        let page = client
            .event_api()
            .query_events(filter.clone(), cursor, Some(100), true /* descending */)
            .await
            .context("querying AuctionCreated events")?;
        for ev in &page.data {
            if ev.timestamp_ms.is_some_and(|t| t < cutoff_ms) {
                break 'pages;
            }
            let origin = ev
                .parsed_json
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !origin.eq_ignore_ascii_case(&vault_hex) {
                continue;
            }
            let Some(kind) = classify_vault_auction(&ev.parsed_json, &underlying, &settlement)
            else {
                warn!(vault = %vault_id, event = %ev.parsed_json, "unclassifiable vault auction event");
                continue;
            };
            if let Some(id) = ev.parsed_json.get("auction_id").and_then(|v| v.as_str()) {
                candidates.push((id.parse().context("parsing auction_id from event")?, kind));
            }
        }
        if !page.has_next_page {
            break;
        }
        cursor = page.next_cursor;
    }

    let mut rfqs = Vec::new();
    let mut swaps = Vec::new();
    for (id, kind) in candidates {
        if let Some(fields) = fetch_fields(client, id).await? {
            match kind {
                AuctionKind::Rfq => rfqs.push(parse_rfq_view(id, &fields)?),
                AuctionKind::Swap => swaps.push(parse_swap_rfq_view(id, &fields)?),
            }
        }
    }
    Ok((rfqs, swaps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BUCKET: &str = "0x00000000000000000000000000000000000000000000000000000000000000b1";

    /// A Vault object's parsed-JSON fields exactly as the RPC renders
    /// them (u64s as strings, Balance unwrapped, enum as variant map).
    fn vault_json(phase: &str, bucket: Option<&str>) -> Value {
        json!({
            "round": "3",
            "phase": { "variant": phase, "fields": {} },
            "current_bucket": bucket,
            "current_expiry_ms": "1700000000000",
            "selling_ends_ms": "1699990000000",
            "open_rfqs": "1",
            "open_swap_rfqs": "0",
            "positions_head": "2",
            "positions_tail": "4",
            "deployable": "5000000000",
            "proceeds_settlement": "123456",
            "pending_deposits": "777",
            "queued_withdraw_shares": "88",
            "config": {
                "min_strike_bps_over_spot": "300",
                "max_strike_bps_over_spot": "6000",
                "min_reserve_premium_bps": "10",
                "min_expiry_lead_ms": "259200000",
                "max_expiry_lead_ms": "777600000",
                "max_slice_amount": "2000000000",
                "max_open_rfqs": "2",
                "round_ms": "604800000",
                "selling_window_ms": "43200000",
                "hold_premium_in_settlement": false,
                "underlying_feed_id": vec![0x50u8; 32],
                "settlement_feed_id": vec![0x41u8; 32],
                "underlying_decimals": 9,
                "settlement_decimals": 6,
            },
        })
    }

    #[test]
    fn parses_active_vault_with_bucket() {
        let v = parse_vault_view(&vault_json("Active", Some(BUCKET))).unwrap();
        assert!(!v.settling);
        assert_eq!(v.round, 3);
        assert_eq!(v.current_bucket, Some(BUCKET.parse().unwrap()));
        assert_eq!(v.pending_positions, 2);
        assert_eq!(v.deployable, 5_000_000_000);
        assert_eq!(v.pending_deposits, 777);
        assert_eq!(v.queued_withdraw_shares, 88);
        assert_eq!(v.config.max_open_rfqs, 2);
        assert!(!v.config.hold_premium_in_settlement);
        assert_eq!(v.config.underlying_feed_id, vec![0x50u8; 32]);
        assert_eq!(v.config.settlement_feed_id, vec![0x41u8; 32]);
        assert_eq!(v.config.underlying_decimals, 9);
        assert_eq!(v.config.settlement_decimals, 6);
    }

    #[test]
    fn parses_settling_vault_without_bucket() {
        let mut j = vault_json("Settling", None);
        j["config"]["hold_premium_in_settlement"] = json!(true);
        let v = parse_vault_view(&j).unwrap();
        assert!(v.settling);
        assert_eq!(v.current_bucket, None);
        assert!(v.config.hold_premium_in_settlement);
    }

    /// The real lossy enum encoding the pinned SDK produces (`phase: {}`,
    /// variant name dropped). A genesis round (round 0, no bucket) must
    /// still read as Settling — otherwise the keeper loops on
    /// `select_bucket` (abort 35 `vault_wrong_phase`). This is the case the
    /// old `{"variant":…}` fixtures never exercised.
    #[test]
    fn lossy_enum_genesis_round_is_settling() {
        let mut j = vault_json("Active", None);
        j["phase"] = json!({}); // variant name dropped, as the SDK renders it
        j["round"] = json!("0");
        let v = parse_vault_view(&j).unwrap();
        assert!(v.settling, "genesis (round 0) must read as Settling despite lossy enum");
        assert_eq!(v.round, 0);
        assert_eq!(v.current_bucket, None);
    }

    /// A finalized, active round (round ≥ 1) with the same lossy `{}` enum
    /// and no bucket yet must read as NOT settling — the legitimate
    /// select-a-bucket state, distinguished from genesis only by the round.
    #[test]
    fn lossy_enum_active_round_without_bucket_is_not_settling() {
        let mut j = vault_json("Active", None);
        j["phase"] = json!({});
        j["round"] = json!("4");
        let v = parse_vault_view(&j).unwrap();
        assert!(!v.settling);
        assert_eq!(v.round, 4);
    }

    /// When a variant name IS present it wins over the round-0 fallback —
    /// forward-compatible with an SDK that fixes the enum encoding.
    #[test]
    fn readable_variant_overrides_round_fallback() {
        let v = parse_vault_view(&vault_json("Settling", Some(BUCKET))).unwrap();
        assert!(v.settling);
        // Active at round 0 can't happen on-chain, but a readable variant
        // must still take precedence over the structural fallback.
        let mut j = vault_json("Active", None);
        j["round"] = json!("0");
        let v = parse_vault_view(&j).unwrap();
        assert!(!v.settling);
    }

    #[test]
    fn rejects_missing_fields_and_bad_cursors() {
        let mut j = vault_json("Active", None);
        j.as_object_mut().unwrap().remove("deployable");
        assert!(parse_vault_view(&j).is_err());

        let mut j = vault_json("Active", None);
        j["positions_head"] = json!("9"); // head > tail
        assert!(parse_vault_view(&j).is_err());
    }

    #[test]
    fn parses_rfq_view() {
        let rfq_id: ObjectID =
            "0x00000000000000000000000000000000000000000000000000000000000000aa"
                .parse()
                .unwrap();
        let v = parse_rfq_view(
            rfq_id,
            &json!({
                "deadline_ms": "1699999000000",
                "amount": "250000000",
                "reserve_bid": "47619000",
            }),
        )
        .unwrap();
        assert_eq!(v.deadline_ms, 1_699_999_000_000);
        assert_eq!(v.amount, 250_000_000);
    }

    #[test]
    fn classifies_vault_auctions_by_escrow_leg() {
        use protocol_types::asset::canonicalize_move_type;
        let u = canonicalize_move_type("0xaa::tbtc::TBTC");
        let s = canonicalize_move_type("0xbb::tusdc::TUSDC");
        // Chain TypeNames carry no 0x prefix; the compare must bridge that.
        let rfq = json!({
            "escrow_type": {"name": "aa::tbtc::TBTC"},
            "bid_type": {"name": "bb::tusdc::TUSDC"},
        });
        assert_eq!(classify_vault_auction(&rfq, &u, &s), Some(AuctionKind::Rfq));
        let swap = json!({
            "escrow_type": {"name": "bb::tusdc::TUSDC"},
            "bid_type": {"name": "aa::tbtc::TBTC"},
        });
        assert_eq!(classify_vault_auction(&swap, &u, &s), Some(AuctionKind::Swap));
        let foreign = json!({
            "escrow_type": {"name": "cc::other::OTHER"},
            "bid_type": {"name": "bb::tusdc::TUSDC"},
        });
        assert_eq!(classify_vault_auction(&foreign, &u, &s), None);
    }
}
