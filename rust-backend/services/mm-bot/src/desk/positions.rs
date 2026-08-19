//! `VaultPosition` custody (SO-418).
//!
//! v2 deposits mint transferable `VaultPosition` NFTs, so the bot's
//! wallet accumulates position objects (testnet seed deposits, commitment
//! releases). Transfers emit no events, so the indexer cannot know what
//! this wallet holds — custody is discovered with an OWNED-OBJECT query
//! by type and kept bounded by merging to one position per vault ×
//! (tranche, generation) — `vault_position::merge` requires all three to
//! match.
//!
//! `request_withdraw` consumes a whole position object in v2, so this
//! inventory is also exactly what the bot would spend to exit.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use sui_tx::chain::ChainClient;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::trading_vault::{self, TradingVaultRefs};

/// One wallet-held `VaultPosition`, fields read from chain.
#[derive(Clone, Debug)]
pub struct OwnedPosition {
    pub id: ObjectID,
    pub vault_id: ObjectID,
    /// Wire code: 0 untranched / 1 senior / 2 junior.
    pub tranche: u8,
    pub capital_generation: u64,
    pub shares: u128,
}

/// Every `VaultPosition` of `package` owned by `owner`, optionally
/// filtered to one vault. Owned-object query (gRPC `ListOwnedObjects`
/// with a `StructType` filter) + a JSON read per object for the fields.
pub async fn owned_positions(
    client: &ChainClient,
    owner: SuiAddress,
    trading_vault_package: ObjectID,
    vault_id: Option<ObjectID>,
) -> Result<Vec<OwnedPosition>> {
    let ty = sui_types::parse_sui_struct_tag(&format!(
        "{}::vault_position::VaultPosition",
        trading_vault_package.to_hex_literal()
    ))
    .context("building the VaultPosition type filter")?;
    let objects = client
        .owned_objects_of_type(owner, ty, 200)
        .await
        .context("listing owned VaultPositions")?;
    let mut out = Vec::with_capacity(objects.len());
    for obj in &objects {
        let id = obj.id();
        let Some((_, Some(json))) = client
            .try_get_object_json(id)
            .await
            .with_context(|| format!("reading VaultPosition {id}"))?
        else {
            continue; // consumed since the listing
        };
        match parse_position(id, &json) {
            Ok(p) => {
                if vault_id.map_or(true, |v| v == p.vault_id) {
                    out.push(p);
                }
            }
            Err(e) => {
                tracing::warn!(position = %id, error = %format!("{e:#}"), "unparseable VaultPosition; skipped")
            }
        }
    }
    Ok(out)
}

/// Merge the wallet's positions for `vault_id` down to one per
/// (tranche, generation), all merges in a single PTB. Returns how many
/// positions were folded away (0 = nothing to do, no tx submitted).
pub async fn merge_owned_positions(
    wrap: &SuiClientWrapper,
    trading_vault_package: ObjectID,
    vault_id: ObjectID,
    gas_budget: u64,
) -> Result<usize> {
    let mut positions = owned_positions(
        &wrap.client,
        wrap.signer.address,
        trading_vault_package,
        Some(vault_id),
    )
    .await?;
    // Deterministic keeper: lowest id survives each group.
    positions.sort_by_key(|p| p.id);
    let mut groups: HashMap<(u8, u64), Vec<&OwnedPosition>> = HashMap::new();
    for p in &positions {
        groups.entry((p.tranche, p.capital_generation)).or_default().push(p);
    }

    let refs = TradingVaultRefs {
        package: trading_vault_package,
        vault_id,
        protocol_config_id: ObjectID::ZERO, // unused by the merge builder
        deposit_type: "",
    };
    let mut pt = ProgrammableTransactionBuilder::new();
    let mut folded = 0usize;
    for group in groups.values() {
        let (keep, rest) = group.split_first().expect("groups are non-empty");
        for other in rest {
            trading_vault::build_merge_positions(&wrap.client, &mut pt, &refs, keep.id, other.id)
                .await?;
            folded += 1;
        }
    }
    if folded == 0 {
        return Ok(0);
    }
    let resp = sui_tx::tx::submit_ptb(
        &wrap.client,
        &wrap.signer,
        pt,
        gas_budget,
        "vault-position merge",
    )
    .await?;
    tracing::info!(
        vault = %vault_id.to_hex_literal(),
        folded,
        digest = %sui_tx::tx::tx_digest(&resp),
        "merged wallet VaultPositions (one per tranche × generation)"
    );
    Ok(folded)
}

/// Parse one `VaultPosition`'s JSON rendering into [`OwnedPosition`].
fn parse_position(id: ObjectID, json: &serde_json::Value) -> Result<OwnedPosition> {
    let vault_id = json
        .get("vault_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("missing vault_id"))
        .and_then(|s| ObjectID::from_hex_literal(s).map_err(|e| anyhow!("vault_id {s}: {e}")))?;
    let tranche = tranche_code(json.get("tranche").ok_or_else(|| anyhow!("missing tranche"))?)?;
    let capital_generation = json_u64(json, "capital_generation")?;
    let shares = json
        .get("shares")
        .ok_or_else(|| anyhow!("missing shares"))
        .and_then(|v| match v {
            serde_json::Value::Number(n) => {
                n.as_u64().map(u128::from).ok_or_else(|| anyhow!("non-u64 shares"))
            }
            serde_json::Value::String(s) => {
                u128::from_str(s).map_err(|e| anyhow!("shares {s}: {e}"))
            }
            other => Err(anyhow!("unexpected shares: {other}")),
        })?;
    Ok(OwnedPosition { id, vault_id, tranche, capital_generation, shares })
}

/// The `Tranche` enum arrives either as `{"@variant": "Junior"}` or as a
/// bare string, depending on the JSON renderer (gRPC rendering trap —
/// api-service parses both, so do we).
fn tranche_code(v: &serde_json::Value) -> Result<u8> {
    let name = match v {
        serde_json::Value::String(s) => s.as_str(),
        serde_json::Value::Object(o) => o
            .get("@variant")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow!("tranche object without @variant"))?,
        other => return Err(anyhow!("unexpected tranche rendering: {other}")),
    };
    match name {
        "Untranched" => Ok(0),
        "Senior" => Ok(1),
        "Junior" => Ok(2),
        other => Err(anyhow!("unknown tranche variant {other}")),
    }
}

fn json_u64(json: &serde_json::Value, name: &str) -> Result<u64> {
    match json.get(name).ok_or_else(|| anyhow!("missing {name}"))? {
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("non-u64 {name}")),
        serde_json::Value::String(s) => s.parse().map_err(|e| anyhow!("{name} {s}: {e}")),
        other => Err(anyhow!("unexpected {name}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_tranche_renderings_and_string_numbers() {
        let id = ObjectID::from_hex_literal("0x2").unwrap();
        let variant = serde_json::json!({
            "vault_id": "0x1",
            "tranche": { "@variant": "Junior" },
            "shares": "123456789012345678901",
            "cost_basis": "10",
            "locked_until_ms": "0",
            "capital_generation": "3",
        });
        let p = parse_position(id, &variant).unwrap();
        assert_eq!((p.tranche, p.capital_generation), (2, 3));
        assert_eq!(p.shares, 123456789012345678901u128);

        let bare = serde_json::json!({
            "vault_id": "0x1",
            "tranche": "Untranched",
            "shares": 5,
            "capital_generation": 0,
        });
        let p = parse_position(id, &bare).unwrap();
        assert_eq!((p.tranche, p.shares), (0, 5));

        let senior = serde_json::json!({
            "vault_id": "0x1",
            "tranche": "Senior",
            "shares": "1",
            "capital_generation": 0,
        });
        assert_eq!(parse_position(id, &senior).unwrap().tranche, 1);
    }

    #[test]
    fn rejects_unknown_variants() {
        assert!(tranche_code(&serde_json::json!("Mezzanine")).is_err());
        assert!(tranche_code(&serde_json::json!({ "variant": "Junior" })).is_err());
    }
}
