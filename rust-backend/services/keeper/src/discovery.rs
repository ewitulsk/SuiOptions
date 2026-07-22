//! Vault auto-discovery: the keeper finds its vaults instead of being
//! configured with them. As soon as a vault is created on chain, the
//! indexer materializes it from `VaultCreated` (C3/D2 `vaults` view) and
//! the next keeper tick picks it up — no config edit, no redeploy.
//!
//! Per discovered vault the keeper still needs two things that aren't
//! in the event: the shared Pyth `PriceInfoObject` ids for the vault's
//! two pinned feeds. Both are derivable on chain:
//!
//!   1. The vault object's config carries the pinned feed ids and the
//!      pair decimals (`state::VaultConfigView`) — authoritative, since
//!      they're what `oracle::spot_cross` enforces.
//!   2. Pyth's state object maps feed id → `PriceInfoObject` through a
//!      `Table<PriceIdentifier, ID>` hung off the state as the dynamic
//!      field named `b"price_info"` (the same lookup pyth-sui-js does in
//!      `getPriceFeedObjectId`). The table handle is resolved once and
//!      cached; each feed costs one dynamic-field read after that.
//!
//! Type-string forms (see move-type-normalization.md): the indexer
//! stores chain `TypeName`s (no `0x`) for both vaults and buckets, so
//! the *indexer-form* strings are kept for bucket filters (exact match
//! by construction), while the canonical `0x` forms feed PTB type args.

use anyhow::{anyhow, Context, Result};
use move_core_types::language_storage::TypeTag;
use serde_json::json;
use std::str::FromStr;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::dynamic_field::DynamicFieldName;

use protocol_types::PriceFeedId;

use crate::state::fetch_vault_view;

/// One vault the keeper cranks, fully resolved from chain + indexer
/// state at discovery time. Everything here is immutable for the
/// vault's life (feeds and decimals are pinned in `VaultConfig`; a
/// `PriceInfoObject` is never re-created for a feed).
#[derive(Debug, Clone)]
pub struct DiscoveredVault {
    pub vault_id: ObjectID,
    /// Chain `TypeName` form, exactly as the indexer stores bucket
    /// types — used for `buckets(assetType:…)` filters.
    pub underlying_type_indexer: String,
    pub settlement_type_indexer: String,
    /// Canonical `0x`-prefixed forms — used for PTB type args.
    pub underlying_type: String,
    pub settlement_type: String,
    pub share_type: String,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
    pub underlying_feed: PriceFeedId,
    pub settlement_feed: PriceFeedId,
    pub underlying_price_info: ObjectID,
    pub settlement_price_info: ObjectID,
}

/// The Pyth state's feed → `PriceInfoObject` table, resolved once.
#[derive(Debug, Clone)]
pub struct PriceInfoTable {
    table_id: ObjectID,
    /// `{pyth_pkg}::price_identifier::PriceIdentifier` — the table's key
    /// type, read off the table's own type string so package upgrades
    /// can't desync it from our config.
    identifier_type: TypeTag,
}

/// Resolve the `b"price_info"` table from the handles: the pinned id
/// when configured (a plain object read — survives RPC providers whose
/// dynamic-field index is broken), else the state's dynamic field.
pub async fn resolve_price_info_table_from(
    client: &SuiClient,
    handles: &sui_tx::tx::pyth_update::PythHandles,
) -> Result<PriceInfoTable> {
    match handles.price_info_table_id {
        Some(id) => resolve_price_info_table_pinned(client, id).await,
        None => resolve_price_info_table(client, handles.pyth_state_id).await,
    }
}

/// Pinned path: read the table object directly and parse its key type.
async fn resolve_price_info_table_pinned(
    client: &SuiClient,
    table_id: ObjectID,
) -> Result<PriceInfoTable> {
    let resp = client
        .read_api()
        .get_object_with_options(
            table_id,
            sui_json_rpc_types::SuiObjectDataOptions::new().with_type(),
        )
        .await
        .context("reading pinned price_info table object")?;
    let data = resp
        .data
        .ok_or_else(|| anyhow!("pinned price_info table {table_id} missing"))?;
    finish_table(table_id, data.type_.as_ref().map(|t| t.to_string()))
}

/// Resolve the `b"price_info"` table hung off the Pyth state object.
pub async fn resolve_price_info_table(
    client: &SuiClient,
    pyth_state_id: ObjectID,
) -> Result<PriceInfoTable> {
    let resp = client
        .read_api()
        .get_dynamic_field_object(
            pyth_state_id,
            DynamicFieldName {
                type_: TypeTag::from_str("vector<u8>").expect("static type tag"),
                value: json!("price_info"),
            },
        )
        .await
        .context("reading pyth state price_info dynamic field")?;
    let data = resp
        .data
        .ok_or_else(|| anyhow!("pyth state {pyth_state_id} has no price_info table"))?;
    let table_id = data.object_id;
    finish_table(table_id, data.type_.as_ref().map(|t| t.to_string()))
}

/// Shared tail: parse the table's key type off its type string.
fn finish_table(table_id: ObjectID, type_str: Option<String>) -> Result<PriceInfoTable> {
    // Type looks like `0x2::table::Table<{pkg}::price_identifier::PriceIdentifier, 0x2::object::ID>`.
    let type_str =
        type_str.ok_or_else(|| anyhow!("price_info table response missing type"))?;
    let key = type_str
        .split('<')
        .nth(1)
        .and_then(|inner| inner.split(',').next())
        .map(str::trim)
        .ok_or_else(|| anyhow!("unparseable price_info table type: {type_str}"))?;
    if !key.ends_with("::price_identifier::PriceIdentifier") {
        return Err(anyhow!("unexpected price_info table key type: {key}"));
    }
    let identifier_type = TypeTag::from_str(key)
        .with_context(|| format!("parsing PriceIdentifier type {key}"))?;
    Ok(PriceInfoTable { table_id, identifier_type })
}

/// Feed id → shared `PriceInfoObject` id, via the table.
pub async fn price_info_object_for(
    client: &SuiClient,
    table: &PriceInfoTable,
    feed: PriceFeedId,
) -> Result<ObjectID> {
    let resp = client
        .read_api()
        .get_dynamic_field_object(
            table.table_id,
            DynamicFieldName {
                type_: table.identifier_type.clone(),
                value: json!({ "bytes": feed.0.to_vec() }),
            },
        )
        .await
        .with_context(|| format!("looking up price info object for feed {feed}"))?;
    let data = resp.data.ok_or_else(|| {
        anyhow!(
            "feed {feed} has no PriceInfoObject on this network — \
             was the vault configured with the right (beta vs stable) feed set?"
        )
    })?;
    // The field object is `Field<PriceIdentifier, ID>`; its parsed JSON
    // `value` is the PriceInfoObject id string.
    let content = data
        .content
        .ok_or_else(|| anyhow!("price info field for {feed} missing content"))?;
    let fields = match content {
        sui_json_rpc_types::SuiParsedData::MoveObject(obj) => obj.fields.to_json_value(),
        other => return Err(anyhow!("price info field for {feed}: unexpected content {other:?}")),
    };
    let id = fields
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("price info field for {feed} has no value: {fields}"))?;
    id.parse()
        .with_context(|| format!("parsing PriceInfoObject id {id:?} for feed {feed}"))
}

/// Fully resolve one indexer vault row into a crankable
/// [`DiscoveredVault`]: read the vault object (pinned feeds, decimals),
/// then map both feeds to their `PriceInfoObject`s.
pub async fn resolve_vault(
    client: &SuiClient,
    row: &indexer_graphql::Vault,
    table: &PriceInfoTable,
) -> Result<DiscoveredVault> {
    let vault_id = ObjectID::new(*row.vault_id.as_bytes());
    let view = fetch_vault_view(client, vault_id).await?;
    let underlying_feed = feed_from_bytes(&view.config.underlying_feed_id)
        .with_context(|| format!("vault {vault_id} underlying feed"))?;
    let settlement_feed = feed_from_bytes(&view.config.settlement_feed_id)
        .with_context(|| format!("vault {vault_id} settlement feed"))?;
    let underlying_price_info = price_info_object_for(client, table, underlying_feed).await?;
    let settlement_price_info = price_info_object_for(client, table, settlement_feed).await?;
    Ok(DiscoveredVault {
        vault_id,
        underlying_type_indexer: row.underlying_type.as_str().to_string(),
        settlement_type_indexer: row.settlement_type.as_str().to_string(),
        underlying_type: row.underlying_type.to_canonical(),
        settlement_type: row.settlement_type.to_canonical(),
        share_type: row.share_type.to_canonical(),
        underlying_decimals: view.config.underlying_decimals,
        settlement_decimals: view.config.settlement_decimals,
        underlying_feed,
        settlement_feed,
        underlying_price_info,
        settlement_price_info,
    })
}

fn feed_from_bytes(bytes: &[u8]) -> Result<PriceFeedId> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("pinned feed id is {} bytes, want 32", bytes.len()))?;
    Ok(PriceFeedId(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_from_bytes_round_trips() {
        let hex = "50c67b3fd225db8912a424dd4baed60ffdde625ed2feaaf283724f9608fea266";
        let bytes: Vec<u8> = (0..64).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let feed = feed_from_bytes(&bytes).unwrap();
        assert_eq!(feed, PriceFeedId::from_hex(hex).unwrap());
        assert!(feed_from_bytes(&bytes[..31]).is_err());
    }

    /// Live-network check of the two dynamic-field lookups, pinned to
    /// the values verified by hand against the Sui testnet fullnode
    /// (Pyth state table + the beta SUI/USD feed). Run explicitly:
    /// `cargo test -p keeper --lib -- --ignored discovery`.
    #[tokio::test]
    #[ignore = "hits the live Sui testnet fullnode"]
    async fn resolves_sui_beta_feed_on_testnet() {
        let client = sui_sdk::SuiClientBuilder::default()
            .build("https://fullnode.testnet.sui.io:443")
            .await
            .unwrap();
        let pyth_state: ObjectID =
            "0x243759059f4c3111179da5878c12f68d612c21a8d54d85edc86164bb18be1c7c"
                .parse()
                .unwrap();
        let table = resolve_price_info_table(&client, pyth_state).await.unwrap();
        let sui_beta = PriceFeedId::from_hex(
            "50c67b3fd225db8912a424dd4baed60ffdde625ed2feaaf283724f9608fea266",
        )
        .unwrap();
        let id = price_info_object_for(&client, &table, sui_beta).await.unwrap();
        assert_eq!(
            id.to_string(),
            "0x1ebb295c789cc42b3b2a1606482cd1c7124076a0f5676718501fda8c7fd075a0"
        );
    }
}
