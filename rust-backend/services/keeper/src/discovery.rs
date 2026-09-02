//! Pyth `PriceInfoObject` lookup for the trading-vault cranks.
//!
//! Pyth's state object maps feed id → `PriceInfoObject` through a
//! `Table<PriceIdentifier, ID>` hung off the state as the dynamic field
//! named `b"price_info"` (the same lookup pyth-sui-js does in
//! `getPriceFeedObjectId`). The table handle is resolved once and cached
//! ([`PriceInfoTable`]); each feed then costs one dynamic-field read
//! ([`price_info_object_for`]).
//!
//! The covered-call vault auto-discovery that used to live here
//! (`DiscoveredVault` / `resolve_vault`) was removed in SO-452.

use anyhow::{anyhow, Context, Result};
use move_core_types::language_storage::TypeTag;
use std::str::FromStr;
use sui_tx::chain::ChainClient;
use sui_types::base_types::ObjectID;

use protocol_types::PriceFeedId;

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
    client: &ChainClient,
    handles: &sui_tx::tx::pyth_update::PythHandles,
) -> Result<PriceInfoTable> {
    match handles.price_info_table_id {
        Some(id) => resolve_price_info_table_pinned(client, id).await,
        None => resolve_price_info_table(client, handles.pyth_state_id).await,
    }
}

/// Pinned path: read the table object directly and parse its key type.
async fn resolve_price_info_table_pinned(
    client: &ChainClient,
    table_id: ObjectID,
) -> Result<PriceInfoTable> {
    let object = client
        .get_object(table_id)
        .await
        .context("reading pinned price_info table object")?;
    finish_table(
        table_id,
        object.struct_tag().map(|t| t.to_canonical_string(true)),
    )
}

/// Resolve the `b"price_info"` table hung off the Pyth state object.
pub async fn resolve_price_info_table(
    client: &ChainClient,
    pyth_state_id: ObjectID,
) -> Result<PriceInfoTable> {
    // Derive the field id client-side rather than asking for a
    // dynamic-field index — same reason as `price_info_object_for` below:
    // some providers don't serve the index at all.
    let key_bytes = bcs::to_bytes(b"price_info".to_vec().as_slice())
        .context("bcs of the price_info field name")?;
    let field_id = sui_types::dynamic_field::derive_dynamic_field_id(
        pyth_state_id,
        &TypeTag::from_str("vector<u8>").expect("static type tag"),
        &key_bytes,
    )
    .context("deriving pyth price_info field id")?;
    let (_, json) = client
        .get_object_json(field_id)
        .await
        .context("reading pyth state price_info dynamic field")?;
    // `Field<vector<u8>, Table<..>>` — the table id is the field's value.
    let table_id: ObjectID = json
        .as_ref()
        .and_then(|j| j.pointer("/value/id"))
        .or_else(|| json.as_ref().and_then(|j| j.pointer("/value")))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("pyth state {pyth_state_id} has no price_info table"))?
        .parse()
        .context("parsing price_info table id")?;
    let table = client
        .get_object(table_id)
        .await
        .context("reading pyth price_info table object")?;
    finish_table(
        table_id,
        table.struct_tag().map(|t| t.to_canonical_string(true)),
    )
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
    client: &ChainClient,
    table: &PriceInfoTable,
    feed: PriceFeedId,
) -> Result<ObjectID> {
    // Derive the field id client-side and fetch it as a plain object:
    // some RPC providers (publicnode) don't serve the dynamic-field
    // index at all. `PriceIdentifier` BCS == its single `bytes` vector.
    let key_bytes = bcs::to_bytes(&feed.0.to_vec()).context("bcs of feed id")?;
    let field_id = sui_types::dynamic_field::derive_dynamic_field_id(
        table.table_id,
        &table.identifier_type,
        &key_bytes,
    )
    .context("deriving price info field id")?;
    let fields = client
        .try_get_object_json(field_id)
        .await
        .with_context(|| format!("looking up price info object for feed {feed}"))?
        .and_then(|(_, json)| json)
        .ok_or_else(|| {
            anyhow!(
                "feed {feed} has no PriceInfoObject on this network — \
                 was the vault configured with the right (beta vs stable) feed set?"
            )
        })?;
    let id = fields
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("price info field for {feed} has no value: {fields}"))?;
    id.parse()
        .with_context(|| format!("parsing PriceInfoObject id {id:?} for feed {feed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live-network check of the two dynamic-field lookups, pinned to
    /// the values verified by hand against the Sui testnet fullnode
    /// (Pyth state table + the beta SUI/USD feed). Run explicitly:
    /// `cargo test -p keeper --lib -- --ignored discovery`.
    #[tokio::test]
    #[ignore = "hits the live Sui testnet fullnode"]
    async fn resolves_sui_beta_feed_on_testnet() {
        let client = ChainClient::new(sui_tx::Network::Testnet.grpc_url()).unwrap();
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
