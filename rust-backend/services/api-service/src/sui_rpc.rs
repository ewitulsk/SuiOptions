//! Direct Sui RPC reads for live vault state.
//!
//! api-service is otherwise a pure indexer-read layer, but a few vault fields
//! are *live* view values — balances and counters that change within a round
//! as RFQs settle and deposits land — so events can't keep a fresh copy. One
//! GraphQL `object` query returns them all off the `Vault` object's JSON
//! contents. (This read used JSON-RPC `sui_getObject` until Sui deactivated
//! JSON-RPC on public testnet fullnodes in July 2026 — see
//! docs/sui-json-rpc-migration.md.)
//!
//! GraphQL `contents.json` conventions (shared with gRPC's `json` rendering,
//! golden-tested below against a live testnet vault): u64/u128 → decimal
//! string · struct fields nested directly (no `fields` wrapper) · `Balance<T>`
//! / `Supply<T>` → `{"value": …}` · `Option` → null/inner · enums →
//! `{"@variant": name, …}` · `vector<u8>` → base64 string.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use protocol_types::asset::canonicalize_move_type;
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

const VAULT_LIVE_QUERY: &str = "query($addr: SuiAddress!) {\
 object(address: $addr) { asMoveObject { contents { json } } } }";

/// One free balance held by a `TradingVault` — a `vault::BalanceKey<T>`
/// dynamic field whose value is the `Balance<T>` (SO-313).
#[derive(Debug, Clone)]
pub struct VaultBalance {
    /// Canonical `0x…::mod::T` coin type from the key's type argument.
    pub coin_type: String,
    pub amount: u64,
}

/// BCS of a `BalanceKey<T>` value. The Move struct has no fields, so the
/// compiler gives it a `dummy_field: bool` — one `false` byte, base64 `AA==`.
/// (Verified against the live testnet vault: an empty `bcs` misses.)
const BALANCE_KEY_BCS: &str = "AA==";

/// Guard on the per-type lookup fan-out. `asset_types` is bounded by what a
/// curator can trade into; anything past this is a malformed read, not a vault.
const MAX_VAULT_BALANCES: usize = 64;

/// The vault object's own Move type + contents, which together give the
/// trading-vault package id (for building `BalanceKey<T>`) and `asset_types`.
const VAULT_ASSET_TYPES_QUERY: &str = "query($addr: SuiAddress!) {\
 object(address: $addr) { asMoveObject { contents { type { repr } json } } } }";

/// Read a `TradingVault`'s free balances (SO-313).
///
/// The vault's `asset_types` (`VecSet<TypeName>`) is exactly the set of types
/// with a live `BalanceKey<T>` dynamic field — `put_balance_internal` inserts
/// on first deposit and `take_balance_internal` removes the type when the
/// balance hits zero (`contracts/trading-vault/sources/vault.move:1265`). So
/// reading it and then fetching those keys by name is one object read plus a
/// batched field lookup, rather than paging every dynamic field on the vault
/// (positions and adapter tags share that namespace). Same mechanism the
/// appraisal composer uses to discover holdings
/// (`frontend/src/tx/appraisal.ts:346`).
///
/// The deposit asset is included — it holds a `BalanceKey` like any other.
/// `Ok(None)` if the node doesn't know the object; `Err` on transport or
/// unexpected-shape failures.
pub async fn fetch_vault_balances(
    http: &reqwest::Client,
    graphql_url: &str,
    vault_id: &ObjectId,
) -> Result<Option<Vec<VaultBalance>>> {
    let body = json!({
        "query": VAULT_ASSET_TYPES_QUERY,
        "variables": { "addr": vault_id.to_hex() },
    });
    let parsed = post_graphql(http, graphql_url, &body, "vault asset-types query").await?;
    let Some(object) = parsed.get("object").filter(|o| !o.is_null()) else {
        return Ok(None);
    };
    let contents = object
        .get("asMoveObject")
        .and_then(|m| m.get("contents"))
        .filter(|c| !c.is_null())
        .ok_or_else(|| anyhow!("vault object has no contents"))?;
    let vault_type = contents
        .pointer("/type/repr")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("vault object has no type repr"))?;
    let package = vault_type
        .split_once("::")
        .map(|(pkg, _)| pkg)
        .ok_or_else(|| anyhow!("unparseable vault type {vault_type}"))?;
    let json = contents
        .get("json")
        .filter(|j| !j.is_null())
        .ok_or_else(|| anyhow!("vault object has no contents json"))?;

    let asset_types = parse_asset_types(json)?;
    if asset_types.len() > MAX_VAULT_BALANCES {
        return Err(anyhow!(
            "vault reports {} asset types, past the {MAX_VAULT_BALANCES} bound",
            asset_types.len()
        ));
    }
    if asset_types.is_empty() {
        return Ok(Some(Vec::new()));
    }
    fetch_balance_keys(http, graphql_url, vault_id, package, &asset_types)
        .await
        .map(Some)
}

/// `asset_types` is a `VecSet<TypeName>` → `{"contents": ["addr::mod::T", …]}`.
/// The entries are chain `TypeName`s, so they arrive WITHOUT the `0x` prefix
/// (see `.claude/move-type-normalization.md`) — canonicalize before they reach
/// a catalog lookup or a `BalanceKey<T>` type argument.
fn parse_asset_types(vault_json: &Value) -> Result<Vec<String>> {
    let raw = vault_json
        .get("asset_types")
        .ok_or_else(|| anyhow!("vault contents has no asset_types"))?;
    let items = raw
        .get("contents")
        .unwrap_or(raw)
        .as_array()
        .ok_or_else(|| anyhow!("asset_types is not a VecSet: {raw}"))?;
    items
        .iter()
        .map(|t| {
            // A `TypeName` renders as the bare string; tolerate the struct
            // form (`{"name": …}`) other renderers use.
            let s = match t {
                Value::String(s) => s.as_str(),
                other => other
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("unparseable asset_types entry {other}"))?,
            };
            Ok(canonicalize_move_type(s))
        })
        .collect()
}

/// Fetch one `BalanceKey<T>` per asset type in a single aliased query, so N
/// holdings still cost one round trip.
async fn fetch_balance_keys(
    http: &reqwest::Client,
    graphql_url: &str,
    vault_id: &ObjectId,
    package: &str,
    asset_types: &[String],
) -> Result<Vec<VaultBalance>> {
    let selections: String = asset_types
        .iter()
        .enumerate()
        .map(|(i, t)| {
            format!(
                " b{i}: dynamicField(name: {{ type: \"{package}::vault::BalanceKey<{t}>\", \
                 bcs: \"{BALANCE_KEY_BCS}\" }}) {{ value {{ ... on MoveValue {{ json }} }} }}"
            )
        })
        .collect();
    let query = format!(
        "query($addr: SuiAddress!) {{ object(address: $addr) {{ asMoveObject {{{selections} }} }} }}"
    );
    let body = json!({ "query": query, "variables": { "addr": vault_id.to_hex() } });
    let parsed = post_graphql(http, graphql_url, &body, "vault balance-keys query").await?;
    let fields = parsed
        .pointer("/object/asMoveObject")
        .filter(|m| !m.is_null())
        .ok_or_else(|| anyhow!("vault object disappeared between balance reads"))?;
    parse_balance_keys(fields, asset_types)
}

/// Read the aliased `dynamicField` results back into balances, in
/// `asset_types` order.
fn parse_balance_keys(fields: &Value, asset_types: &[String]) -> Result<Vec<VaultBalance>> {
    let mut out = Vec::with_capacity(asset_types.len());
    for (i, coin_type) in asset_types.iter().enumerate() {
        let field = fields
            .get(format!("b{i}"))
            .ok_or_else(|| anyhow!("balance-keys query returned no alias b{i}"))?;
        // A type in `asset_types` with no `BalanceKey` field shouldn't happen —
        // the two move together on chain — but it means zero, not a read
        // failure, so skip it rather than fail the whole read.
        if field.is_null() {
            continue;
        }
        // A field that IS present but unrenderable is a read gap: failing is
        // better than silently under-reporting a holding, which is the bug
        // SO-313 exists to fix.
        let value = field
            .pointer("/value/json")
            .ok_or_else(|| anyhow!("BalanceKey {coin_type} has no MoveValue json"))?;
        let amount =
            balance_value(value).with_context(|| format!("BalanceKey {coin_type} value"))?;
        out.push(VaultBalance {
            coin_type: coin_type.clone(),
            amount,
        });
    }
    Ok(out)
}

/// POST a GraphQL body and return its `data`, mapping HTTP, transport and
/// GraphQL-level errors to `Err`.
async fn post_graphql(
    http: &reqwest::Client,
    graphql_url: &str,
    body: &Value,
    what: &'static str,
) -> Result<Value> {
    let resp = http
        .post(graphql_url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("{what} request"))?
        .error_for_status()
        .with_context(|| format!("{what} http status"))?;
    let parsed: Value = resp
        .json()
        .await
        .with_context(|| format!("decoding {what}"))?;
    if let Some(errors) = parsed.get("errors").filter(|e| !e.is_null()) {
        return Err(anyhow!("{what} errors: {errors}"));
    }
    parsed
        .get("data")
        .filter(|d| !d.is_null())
        .cloned()
        .ok_or_else(|| anyhow!("{what} missing data: {parsed}"))
}

/// `Balance<T>` renders as the bare u64 (verified against a live testnet
/// vault); tolerate the `{"value": …}` struct form other renderers use.
fn balance_value(v: &Value) -> Result<u64> {
    let scalar = match v {
        Value::Object(m) => m
            .get("value")
            .ok_or_else(|| anyhow!("balance object has no value field: {v}"))?,
        other => other,
    };
    match scalar {
        Value::String(s) => s.parse().with_context(|| format!("u64 {s:?}")),
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("non-u64 {n}")),
        other => Err(anyhow!("expected u64 balance, got {other}")),
    }
}

/// Read one `Vault` object's live fields via the Sui GraphQL RPC. `Ok(None)`
/// if the node doesn't know the object (deleted / wrong network); `Err` on
/// transport or unexpected-shape failures. Callers degrade to omitting live
/// fields.
pub async fn fetch_vault_live(
    http: &reqwest::Client,
    graphql_url: &str,
    vault_id: &ObjectId,
) -> Result<Option<VaultLive>> {
    let body = json!({
        "query": VAULT_LIVE_QUERY,
        "variables": { "addr": vault_id.to_hex() },
    });
    let resp = http
        .post(graphql_url)
        .json(&body)
        .send()
        .await
        .context("vault object query request")?
        .error_for_status()
        .context("vault object query http status")?;
    let parsed: Value = resp.json().await.context("decoding vault object query")?;

    if let Some(errors) = parsed.get("errors").filter(|e| !e.is_null()) {
        return Err(anyhow!("vault object query errors: {errors}"));
    }
    // `data.object` is null for an unknown object; `contents.json` is the
    // Move struct's fields rendered as JSON.
    let data = parsed
        .get("data")
        .ok_or_else(|| anyhow!("vault object query missing data: {parsed}"))?;
    let Some(object) = data.get("object").filter(|o| !o.is_null()) else {
        return Ok(None);
    };
    let fields = object
        .get("asMoveObject")
        .and_then(|m| m.get("contents"))
        .and_then(|c| c.get("json"))
        .filter(|j| !j.is_null())
        .ok_or_else(|| anyhow!("vault object has no contents json"))?;

    Ok(Some(parse_vault_live(fields)?))
}

fn parse_vault_live(fields: &Value) -> Result<VaultLive> {
    let config = field(fields, "config")?;
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

/// `Phase` is a Move enum → `{"@variant": "Active"|"Settling", …}` (tolerate a
/// bare string too). Maps to the DTO's lowercase `active` | `settling`.
fn parse_phase(v: &Value) -> Result<String> {
    let variant = match v {
        Value::String(s) => s.as_str(),
        Value::Object(m) => m
            .get("@variant")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("phase object has no @variant: {v}"))?,
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

/// u64 fields cross the wire as decimal strings; tolerate a JSON number.
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

    /// A `Vault` object's fields exactly as the GraphQL RPC renders
    /// `contents.json` (verified against a live testnet vault): u64s as
    /// strings, the `Phase` enum as an `@variant` map, config's fields nested
    /// directly with no `fields` wrapper.
    fn vault_json(phase: &str) -> Value {
        json!({
            "round": "3",
            "phase": { "@variant": phase },
            "selling_ends_ms": "1699990000000",
            "open_rfqs": "2",
            "deployable": "5000000000",
            "proceeds_settlement": "123456",
            "withdrawal_pool": "777",
            "claimable_shares": "88",
            "queued_withdraw_shares": "99",
            "config": {
                "max_slice_amount": "1000000000000",
                "max_open_rfqs": "4",
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

    // ── free-balance walk (SO-313) ──────────────────────────────────────

    /// Unprefixed, exactly as a chain `TypeName` renders inside `asset_types`.
    const TBTC: &str = "95f83a70fc0d15e13c9517ed346022c4d26a90427f86eebedb564111f8512cf9::tbtc::TBTC";
    const TUSDC: &str =
        "95f83a70fc0d15e13c9517ed346022c4d26a90427f86eebedb564111f8512cf9::tusdc::TUSDC";

    /// `asset_types` as the GraphQL RPC renders it for the SO-313 repro vault:
    /// a `VecSet` wrapper around bare, `0x`-LESS `TypeName` strings.
    #[test]
    fn parses_asset_types_and_canonicalizes_them() {
        let out = parse_asset_types(&json!({
            "asset_types": { "contents": [TUSDC, TBTC] },
        }))
        .unwrap();
        assert_eq!(out, vec![format!("0x{TUSDC}"), format!("0x{TBTC}")]);
    }

    /// A framework type arrives short (`0x2::sui::SUI`); it must come out
    /// address-padded so it compares byte-equal against the catalog and
    /// event-sourced types (`.claude/move-type-normalization.md`).
    #[test]
    fn pads_short_addresses_in_asset_types() {
        let out = parse_asset_types(&json!({ "asset_types": { "contents": ["0x2::sui::SUI"] } }))
            .unwrap();
        assert_eq!(
            out[0],
            "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"
        );
    }

    #[test]
    fn tolerates_struct_shaped_type_names() {
        let out = parse_asset_types(&json!({
            "asset_types": { "contents": [{ "name": TBTC }] },
        }))
        .unwrap();
        assert_eq!(out, vec![format!("0x{TBTC}")]);
    }

    /// The aliased `dynamicField` results, keyed `b0…bN` in `asset_types`
    /// order. `Balance<T>` renders as a bare decimal string.
    #[test]
    fn reads_aliased_balance_keys_in_order() {
        let types = vec![format!("0x{TUSDC}"), format!("0x{TBTC}")];
        let fields = json!({
            "b0": { "value": { "json": "99900513529" } },
            "b1": { "value": { "json": "153000" } },
        });
        let out = parse_balance_keys(&fields, &types).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].coin_type, format!("0x{TUSDC}"));
        assert_eq!(out[0].amount, 99_900_513_529);
        assert_eq!(out[1].coin_type, format!("0x{TBTC}"));
        assert_eq!(out[1].amount, 153_000);
    }

    #[test]
    fn tolerates_struct_shaped_balance() {
        let types = vec![format!("0x{TBTC}")];
        let fields = json!({ "b0": { "value": { "json": { "value": "153000" } } } });
        assert_eq!(parse_balance_keys(&fields, &types).unwrap()[0].amount, 153_000);
    }

    /// A type listed with no field on chain means zero, so it drops out of the
    /// list rather than failing the read.
    #[test]
    fn skips_a_missing_balance_key() {
        let types = vec![format!("0x{TUSDC}"), format!("0x{TBTC}")];
        let fields = json!({ "b0": null, "b1": { "value": { "json": "153000" } } });
        let out = parse_balance_keys(&fields, &types).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].coin_type, format!("0x{TBTC}"));
    }

    /// A field that IS present but unrenderable is a read gap: under-reporting
    /// a holding is exactly the bug SO-313 fixes, so fail loudly instead.
    #[test]
    fn rejects_a_present_but_unreadable_balance_key() {
        let types = vec![format!("0x{TBTC}")];
        assert!(parse_balance_keys(&json!({ "b0": { "value": null } }), &types).is_err());
        assert!(parse_balance_keys(&json!({}), &types).is_err());
    }
}
