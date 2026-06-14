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
    pub min_expiry_lead_ms: u64,
    pub max_expiry_lead_ms: u64,
    pub max_slice_amount: u64,
    pub max_open_rfqs: u64,
    pub round_ms: u64,
    pub hold_premium_in_settlement: bool,
    /// Pinned Pyth feed ids (32 raw bytes each) — what `oracle::spot_cross`
    /// enforces, so discovery treats them as authoritative.
    pub underlying_feed_id: Vec<u8>,
    pub settlement_feed_id: Vec<u8>,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
}

/// One live vault-coupled auction (the object still exists ⇒ unsettled).
#[derive(Debug, Clone, PartialEq)]
pub struct RfqView {
    pub rfq_id: ObjectID,
    pub bucket_id: ObjectID,
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
    let phase = field(fields, "phase")?;
    let settling = match phase {
        Value::String(s) => s == "Settling",
        Value::Object(m) => m.get("variant").and_then(|v| v.as_str()) == Some("Settling"),
        other => return Err(anyhow!("unrecognized phase encoding {other}")),
    };
    let positions_head = u64_field(fields, "positions_head")?;
    let positions_tail = u64_field(fields, "positions_tail")?;
    let config = field(fields, "config")?;
    // A nested struct may render bare or as {"type":…, "fields":…}.
    let config = config.get("fields").unwrap_or(config);

    Ok(VaultView {
        round: u64_field(fields, "round")?,
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
            min_expiry_lead_ms: u64_field(config, "min_expiry_lead_ms")?,
            max_expiry_lead_ms: u64_field(config, "max_expiry_lead_ms")?,
            max_slice_amount: u64_field(config, "max_slice_amount")?,
            max_open_rfqs: u64_field(config, "max_open_rfqs")?,
            round_ms: u64_field(config, "round_ms")?,
            hold_premium_in_settlement: as_bool(field(config, "hold_premium_in_settlement")?)?,
            underlying_feed_id: bytes_field(config, "underlying_feed_id")?,
            settlement_feed_id: bytes_field(config, "settlement_feed_id")?,
            underlying_decimals: u8_field(config, "underlying_decimals")?,
            settlement_decimals: u8_field(config, "settlement_decimals")?,
        },
    })
}

/// Parse an `RfqAuction` object's parsed-JSON field map.
pub fn parse_rfq_view(rfq_id: ObjectID, fields: &Value) -> Result<RfqView> {
    Ok(RfqView {
        rfq_id,
        bucket_id: as_id(field(fields, "bucket_id")?).context("field bucket_id")?,
        deadline_ms: u64_field(fields, "deadline_ms")?,
        amount: u64_field(fields, "amount")?,
    })
}

/// Parse a `SwapAuction` object's parsed-JSON field map.
pub fn parse_swap_rfq_view(swap_id: ObjectID, fields: &Value) -> Result<SwapRfqView> {
    Ok(SwapRfqView {
        swap_id,
        deadline_ms: u64_field(fields, "deadline_ms")?,
        amount_s: u64_field(fields, "amount_s")?,
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

/// Discover the vault's live coupled auctions, stateless: walk
/// `RfqCreated` events (newest first) down to `cutoff_ms`, keep those
/// with `origin == vault_id`, then read each object — still existing ⇒
/// still open. Restart- and race-safe by construction.
pub async fn discover_open_rfqs(
    client: &SuiClient,
    package: ObjectID,
    vault_id: ObjectID,
    cutoff_ms: u64,
) -> Result<Vec<RfqView>> {
    let filter = EventFilter::MoveEventType(StructTag {
        address: package.into(),
        module: Identifier::new("events").unwrap(),
        name: Identifier::new("RfqCreated").unwrap(),
        type_params: vec![],
    });

    let vault_hex = vault_id.to_string();
    let mut candidates: Vec<ObjectID> = Vec::new();
    let mut cursor = None;
    'pages: loop {
        let page = client
            .event_api()
            .query_events(filter.clone(), cursor, Some(100), true /* descending */)
            .await
            .context("querying RfqCreated events")?;
        for ev in &page.data {
            if ev.timestamp_ms.is_some_and(|t| t < cutoff_ms) {
                break 'pages;
            }
            let origin = ev
                .parsed_json
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if origin.eq_ignore_ascii_case(&vault_hex) {
                if let Some(id) = ev.parsed_json.get("rfq_id").and_then(|v| v.as_str()) {
                    candidates.push(id.parse().context("parsing rfq_id from event")?);
                }
            }
        }
        if !page.has_next_page {
            break;
        }
        cursor = page.next_cursor;
    }

    let mut open = Vec::new();
    for rfq_id in candidates {
        if let Some(fields) = fetch_fields(client, rfq_id).await? {
            open.push(parse_rfq_view(rfq_id, &fields)?);
        }
    }
    Ok(open)
}

/// Discover the vault's live coupled swap auctions, stateless: walk
/// `SwapRfqCreated` events (newest first) down to `cutoff_ms`, keep those
/// with `origin == vault_id`, then read each object — still existing ⇒
/// still open. Mirrors [`discover_open_rfqs`] for the proceeds-swap path.
pub async fn discover_open_swap_rfqs(
    client: &SuiClient,
    package: ObjectID,
    vault_id: ObjectID,
    cutoff_ms: u64,
) -> Result<Vec<SwapRfqView>> {
    let filter = EventFilter::MoveEventType(StructTag {
        address: package.into(),
        module: Identifier::new("events").unwrap(),
        name: Identifier::new("SwapRfqCreated").unwrap(),
        type_params: vec![],
    });

    let vault_hex = vault_id.to_string();
    let mut candidates: Vec<ObjectID> = Vec::new();
    let mut cursor = None;
    'pages: loop {
        let page = client
            .event_api()
            .query_events(filter.clone(), cursor, Some(100), true /* descending */)
            .await
            .context("querying SwapRfqCreated events")?;
        for ev in &page.data {
            if ev.timestamp_ms.is_some_and(|t| t < cutoff_ms) {
                break 'pages;
            }
            let origin = ev
                .parsed_json
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if origin.eq_ignore_ascii_case(&vault_hex) {
                if let Some(id) = ev.parsed_json.get("swap_id").and_then(|v| v.as_str()) {
                    candidates.push(id.parse().context("parsing swap_id from event")?);
                }
            }
        }
        if !page.has_next_page {
            break;
        }
        cursor = page.next_cursor;
    }

    let mut open = Vec::new();
    for swap_id in candidates {
        if let Some(fields) = fetch_fields(client, swap_id).await? {
            open.push(parse_swap_rfq_view(swap_id, &fields)?);
        }
    }
    Ok(open)
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
                "min_expiry_lead_ms": "259200000",
                "max_expiry_lead_ms": "777600000",
                "max_slice_amount": "2000000000",
                "max_open_rfqs": "2",
                "round_ms": "604800000",
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
                "bucket_id": BUCKET,
                "deadline_ms": "1699999000000",
                "amount": "250000000",
                "reserve_premium": "47619000",
            }),
        )
        .unwrap();
        assert_eq!(v.bucket_id, BUCKET.parse().unwrap());
        assert_eq!(v.deadline_ms, 1_699_999_000_000);
        assert_eq!(v.amount, 250_000_000);
    }
}
