//! Direct Sui RPC reads for live vault state.
//!
//! api-service is otherwise a pure indexer-read layer, but a few vault fields
//! are *live* view values — balances and counters that change within a round
//! as RFQs settle and deposits land — so events can't keep a fresh copy. One
//! `sui_getObject` returns them all off the `Vault` object's parsed content.
//!
//! JSON-RPC parsed-content conventions (per `sui-json-rpc-types`, golden-tested
//! by the keeper's localnet e2e): u64/u128 → decimal string · `Balance<T>` →
//! its bare value · `Option` → null/inner · enums → `{"variant": name,
//! "fields": {…}}`.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use protocol_types::ids::ObjectId;

/// Live `Vault` view values that the indexer doesn't carry (it materialises
/// only what events state). All raw on-chain integers, atomic units.
#[derive(Debug, Clone)]
pub struct VaultLive {
    /// Ground-truth round phase from the on-chain `Phase` enum:
    /// `active` (selling/holding) | `settling` (between rounds, redeeming).
    pub phase: String,
    pub selling_ends_ms: u64,
    /// Open option RFQ auctions this round.
    pub open_rfqs: u64,
    /// Underlying available to deploy as RFQ collateral.
    pub deployable: u64,
    /// Settlement (premium + exercise) awaiting the proceeds swap.
    pub proceeds_settlement: u64,
    /// Underlying reserved for finalized withdrawals.
    pub withdrawal_pool: u64,
    /// Shares minted for deposits, awaiting claim.
    pub claimable_shares: u64,
    /// Shares escrowed for withdrawal (still exposed to round P&L).
    pub queued_withdraw_shares: u64,
    /// Config slice guardrail: max underlying per RFQ slice.
    pub max_slice_amount: u64,
    /// Config slice guardrail: max concurrent open RFQs.
    pub max_open_rfqs: u64,
}

/// Read one `Vault` object's live fields via `sui_getObject`. `Ok(None)` if the
/// node doesn't know the object (deleted / wrong network); `Err` on transport
/// or unexpected-shape failures. Callers degrade to omitting live fields.
pub async fn fetch_vault_live(
    http: &reqwest::Client,
    rpc_url: &str,
    vault_id: &ObjectId,
) -> Result<Option<VaultLive>> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sui_getObject",
        "params": [vault_id.to_hex(), { "showContent": true }],
    });
    let resp = http
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("sui_getObject request")?
        .error_for_status()
        .context("sui_getObject http status")?;
    let parsed: Value = resp.json().await.context("decoding sui_getObject")?;

    // `result.data.content.fields`, or `result.error` for a missing object.
    let result = parsed
        .get("result")
        .ok_or_else(|| anyhow!("sui_getObject missing result: {parsed}"))?;
    let Some(data) = result.get("data").filter(|d| !d.is_null()) else {
        return Ok(None);
    };
    let fields = data
        .get("content")
        .and_then(|c| c.get("fields"))
        .ok_or_else(|| anyhow!("vault object has no content.fields"))?;

    Ok(Some(parse_vault_live(fields)?))
}

fn parse_vault_live(fields: &Value) -> Result<VaultLive> {
    let config = field(fields, "config")?
        .get("fields")
        .ok_or_else(|| anyhow!("config has no fields"))?;
    Ok(VaultLive {
        phase: parse_phase(field(fields, "phase")?)?,
        selling_ends_ms: u64_field(fields, "selling_ends_ms")?,
        open_rfqs: u64_field(fields, "open_rfqs")?,
        deployable: u64_field(fields, "deployable")?,
        proceeds_settlement: u64_field(fields, "proceeds_settlement")?,
        withdrawal_pool: u64_field(fields, "withdrawal_pool")?,
        claimable_shares: u64_field(fields, "claimable_shares")?,
        queued_withdraw_shares: u64_field(fields, "queued_withdraw_shares")?,
        max_slice_amount: u64_field(config, "max_slice_amount")?,
        max_open_rfqs: u64_field(config, "max_open_rfqs")?,
    })
}

/// `Phase` is a Move enum → `{"variant": "Active"|"Settling", …}` (tolerate a
/// bare string too). Maps to the DTO's lowercase `active` | `settling`.
fn parse_phase(v: &Value) -> Result<String> {
    let variant = match v {
        Value::String(s) => s.as_str(),
        Value::Object(m) => m
            .get("variant")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("phase object has no variant: {v}"))?,
        other => return Err(anyhow!("unrecognized phase encoding {other}")),
    };
    match variant {
        "Active" => Ok("active".to_string()),
        "Settling" => Ok("settling".to_string()),
        other => Err(anyhow!("unknown phase variant {other}")),
    }
}

fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value> {
    v.get(name).ok_or_else(|| anyhow!("missing field {name}"))
}

/// u64 fields cross the RPC wire as decimal strings; tolerate a JSON number.
fn u64_field(v: &Value, name: &str) -> Result<u64> {
    match field(v, name)? {
        Value::String(s) => s.parse().with_context(|| format!("field {name}: u64 {s:?}")),
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("field {name}: non-u64 {n}")),
        other => Err(anyhow!("field {name}: expected u64, got {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Vault` object's parsed-JSON fields exactly as `sui_getObject` renders
    /// them (verified against a live testnet vault): u64s as strings, `Balance`
    /// bare, the `Phase` enum as a variant map, config nested under `fields`.
    fn vault_json(phase: &str) -> Value {
        json!({
            "round": "3",
            "phase": { "type": "0x..::vault::Phase", "variant": phase, "fields": {} },
            "selling_ends_ms": "1699990000000",
            "open_rfqs": "2",
            "deployable": "5000000000",
            "proceeds_settlement": "123456",
            "withdrawal_pool": "777",
            "claimable_shares": "88",
            "queued_withdraw_shares": "99",
            "config": {
                "type": "0x..::vault::VaultConfig",
                "fields": {
                    "max_slice_amount": "1000000000000",
                    "max_open_rfqs": "4",
                },
            },
        })
    }

    #[test]
    fn parses_live_fields() {
        let v = parse_vault_live(&vault_json("Active")).unwrap();
        assert_eq!(v.phase, "active");
        assert_eq!(v.selling_ends_ms, 1_699_990_000_000);
        assert_eq!(v.open_rfqs, 2);
        assert_eq!(v.deployable, 5_000_000_000);
        assert_eq!(v.proceeds_settlement, 123_456);
        assert_eq!(v.withdrawal_pool, 777);
        assert_eq!(v.claimable_shares, 88);
        assert_eq!(v.queued_withdraw_shares, 99);
        assert_eq!(v.max_slice_amount, 1_000_000_000_000);
        assert_eq!(v.max_open_rfqs, 4);
    }

    #[test]
    fn maps_settling_phase() {
        assert_eq!(parse_vault_live(&vault_json("Settling")).unwrap().phase, "settling");
    }

    #[test]
    fn tolerates_bare_string_phase() {
        let mut j = vault_json("Active");
        j["phase"] = json!("Settling");
        assert_eq!(parse_vault_live(&j).unwrap().phase, "settling");
    }
}
