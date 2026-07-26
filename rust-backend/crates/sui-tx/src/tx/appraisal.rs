//! Appraisal composer for the curated trading vault (SO-289): discover a
//! vault's holdings on-chain and emit the full attestation-bearing
//! appraisal PTB — `begin_appraisal`, one Pyth update covering every
//! needed feed, `oracle_pyth::attest` per non-deposit asset, the
//! per-position adapter appraisals, returning the `Appraisal` argument
//! ready for `deposit` / `fulfill_withdrawals`.
//!
//! Discovery is chain-only (vault object fields + dynamic fields), so
//! the composer can't drift from custody state the way an indexer view
//! could. The caller supplies the price plumbing: a feed id per coin
//! type (token catalog), the resolved `PriceInfoObject` per feed, one
//! Hermes accumulator update covering all of them, and the option-coin
//! bucket lookup (call type → bucket) that only an indexer can answer.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use serde_json::Value;
use sui_sdk::rpc_types::{SuiMoveStruct, SuiMoveValue, SuiObjectDataOptions};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use crate::tx::pyth_update::{prepend_price_update, PythHandles};
use crate::tx::{clock_arg, shared_object_arg};

/// Package + shared-object identity the composer calls against.
#[derive(Debug, Clone)]
pub struct AppraisalRefs {
    pub trading_vault_pkg: ObjectID,
    pub oracle_pyth_pkg: ObjectID,
    pub deepbook_adapter_pkg: Option<ObjectID>,
    pub options_adapter_pkg: Option<ObjectID>,
    pub vault_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub oracle_registry_id: ObjectID,
    pub pyth_feed_registry_id: ObjectID,
    /// equity-oracle package (SO-299), for the external-account equity
    /// leg. `None` where the package isn't deployed.
    pub equity_oracle_pkg: Option<ObjectID>,
    /// The equity-oracle package's shared `EquityBook`.
    pub equity_book_id: Option<ObjectID>,
    /// The options-adapter package's shared `VolBook` (premium
    /// mark-to-market). Required whenever option-coin legs compose.
    pub vol_book_id: Option<ObjectID>,
    /// Trustless DeepBook-Margin equity leg (SO-299 phase C): set for a
    /// vault whose pinned witness is `dbm_oracle::DbmOracle` — the
    /// composer then records equity via the dbm-oracle adapter instead
    /// of `equity_oracle::record`.
    pub dbm: Option<DbmLegInfo>,
}

/// Chain identity of a vault's DeepBook-Margin external account, for the
/// on-chain-computed equity leg (`dbm_oracle::record{,_no_debt}`).
#[derive(Debug, Clone)]
pub struct DbmLegInfo {
    pub dbm_oracle_pkg: ObjectID,
    pub margin_manager_id: ObjectID,
    pub deepbook_pool_id: ObjectID,
    pub base_margin_pool_id: ObjectID,
    pub quote_margin_pool_id: ObjectID,
    /// Canonical base/quote coin types of the manager's pool.
    pub base_type: String,
    pub quote_type: String,
}

/// One custodied position, classified from its object type + adapter tag.
#[derive(Debug, Clone)]
pub enum PositionInfo {
    DeepBookCustody {
        id: ObjectID,
        assets: Vec<String>,
        /// (pool id, base type, quote type)
        pools: Vec<(ObjectID, String, String)>,
    },
    RfqTicket {
        id: ObjectID,
        escrow_type: String,
        auction_id: ObjectID,
        bucket_id: ObjectID,
        is_put: bool,
    },
    /// A live vault-funded bid on someone else's auction (SO-299): the
    /// escrow marks at cost while the auction outputs are routed to the
    /// ticket's own object address (see options_adapter::BidTicket).
    BidTicket {
        id: ObjectID,
        /// The bid asset (canonical) — what the escrow cost is in.
        escrow_type: String,
        /// What a win delivers to the ticket address.
        win_type: String,
        auction_id: ObjectID,
        escrow_amount: u64,
        win_amount: u64,
    },
    /// A written option position (options_adapter or vault_mm tagged).
    OptionPosition {
        id: ObjectID,
        bucket_id: ObjectID,
        is_put: bool,
        underlying: String,
        settlement: String,
        call_type: String,
        via_vault_mm: bool,
    },
    /// A held option coin (vault_mm writer flow).
    OptionCoin {
        id: ObjectID,
        call_type: String,
    },
}

#[derive(Debug, Clone)]
pub struct VaultHoldings {
    pub deposit_type: String,
    /// Non-deposit free-balance types (canonical `0x…`).
    pub free_assets: Vec<String>,
    pub positions: Vec<PositionInfo>,
    /// Registered external account's address (SO-299); `None` for vaults
    /// without one.
    pub external_account: Option<String>,
    /// Canonical type of the pinned equity-oracle witness. When set AND
    /// `external_exposure > 0`, the appraisal REQUIRES the external-equity
    /// leg (`external_pending` — consumption aborts 82 without it).
    pub external_equity_oracle: Option<String>,
    /// Units released to the external account and not yet returned
    /// (SO-310). Zero on a registered-but-unfunded account, which marks NO
    /// `external_pending` — composing the equity leg anyway aborts.
    pub external_exposure: u64,
}

impl VaultHoldings {
    /// Every non-deposit asset that needs a price attestation.
    pub fn assets_needing_attestation(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self.free_assets.iter().cloned().collect();
        for p in &self.positions {
            match p {
                PositionInfo::DeepBookCustody { assets, pools, .. } => {
                    out.extend(assets.iter().cloned());
                    for (_, base, quote) in pools {
                        out.insert(base.clone());
                        out.insert(quote.clone());
                    }
                }
                PositionInfo::RfqTicket { escrow_type, .. } => {
                    out.insert(escrow_type.clone());
                }
                PositionInfo::BidTicket { escrow_type, .. } => {
                    out.insert(escrow_type.clone());
                }
                PositionInfo::OptionPosition { underlying, settlement, .. } => {
                    out.insert(underlying.clone());
                    out.insert(settlement.clone());
                }
                PositionInfo::OptionCoin { .. } => {}
            }
        }
        out.remove(&self.deposit_type);
        out
    }

    pub fn is_cash_only(&self) -> bool {
        self.free_assets.is_empty() && self.positions.is_empty()
    }
}

/// Bucket identity for one option-coin type (indexer-supplied): lets the
/// composer price `Coin<CALL_X>` holdings through
/// `options_oracle::attest_call/put` instead of a (nonexistent) Pyth feed.
#[derive(Debug, Clone)]
pub struct OptionBucketInfo {
    pub bucket_id: ObjectID,
    /// Canonical underlying coin type.
    pub underlying: String,
    /// Canonical settlement coin type.
    pub settlement: String,
    pub is_put: bool,
}

/// The types that need PYTH attestations given the option-coin bucket map:
/// mapped option-coin types are replaced by their bucket's underlying +
/// settlement legs (the coin itself prices via the options oracle), held
/// option-coin positions contribute their legs, and a DBM equity leg
/// contributes its manager's base + quote — the last only while the leg
/// itself composes (exposure > 0; see [`compose_appraisal`]).
pub fn pyth_assets_needed(
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    dbm: Option<&DbmLegInfo>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for t in holdings.assets_needing_attestation() {
        match option_buckets.get(&t) {
            Some(b) => {
                out.insert(b.underlying.clone());
                out.insert(b.settlement.clone());
            }
            None => {
                out.insert(t);
            }
        }
    }
    for p in &holdings.positions {
        if let PositionInfo::OptionCoin { call_type, .. } = p {
            if let Some(b) = option_buckets.get(call_type) {
                out.insert(b.underlying.clone());
                out.insert(b.settlement.clone());
            }
        }
    }
    if let Some(d) = dbm.filter(|_| holdings.external_exposure > 0) {
        out.insert(d.base_type.clone());
        out.insert(d.quote_type.clone());
    }
    out.remove(&holdings.deposit_type);
    out
}

fn canon(s: &str) -> String {
    protocol_types::asset::canonicalize_move_type(s)
}

fn move_field<'a>(s: &'a SuiMoveStruct, name: &str) -> Result<&'a SuiMoveValue> {
    match s {
        SuiMoveStruct::WithFields(m) | SuiMoveStruct::WithTypes { fields: m, .. } => m
            .get(name)
            .ok_or_else(|| anyhow!("object missing field {name}")),
        _ => bail!("unexpected move struct shape"),
    }
}

/// A `VecSet<TypeName>` field → canonical type strings.
fn type_name_set(v: &SuiMoveValue) -> Result<Vec<String>> {
    let json = serde_json::to_value(v)?;
    let contents = json
        .pointer("/fields/contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for entry in contents {
        let name = entry
            .pointer("/fields/name")
            .and_then(Value::as_str)
            .or_else(|| entry.as_str())
            .ok_or_else(|| anyhow!("unparseable TypeName entry: {entry}"))?;
        out.push(canon(name));
    }
    Ok(out)
}

/// A `VecSet<ID>` field → object ids.
fn id_set(v: &SuiMoveValue) -> Result<Vec<ObjectID>> {
    let json = serde_json::to_value(v)?;
    let contents = json
        .pointer("/fields/contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    contents
        .iter()
        .map(|e| {
            let s = e.as_str().ok_or_else(|| anyhow!("unparseable ID entry: {e}"))?;
            ObjectID::from_hex_literal(s).context("parsing id set entry")
        })
        .collect()
}

async fn object_fields(client: &SuiClient, id: ObjectID) -> Result<SuiMoveStruct> {
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new().with_content().with_type())
        .await
        .with_context(|| format!("fetching object {id}"))?;
    let data = resp.data.ok_or_else(|| anyhow!("object {id} missing"))?;
    match data.content {
        Some(sui_sdk::rpc_types::SuiParsedData::MoveObject(obj)) => Ok(obj.fields),
        _ => bail!("object {id} has no parsed move content"),
    }
}

async fn object_type(client: &SuiClient, id: ObjectID) -> Result<String> {
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new().with_type())
        .await
        .with_context(|| format!("fetching object {id} type"))?;
    Ok(resp
        .data
        .and_then(|d| d.type_)
        .ok_or_else(|| anyhow!("object {id} missing type"))?
        .to_string())
}

/// Chain-only holdings discovery. `bucket_for_call_type` answers which
/// bucket a held option coin belongs to (indexer-supplied; pass an empty
/// map when the vault can't hold vault_mm option coins).
pub async fn discover_holdings(
    client: &SuiClient,
    vault_id: ObjectID,
) -> Result<VaultHoldings> {
    let vault = object_fields(client, vault_id).await?;
    let config_json = serde_json::to_value(move_field(&vault, "config")?)?;
    let deposit_type = canon(
        config_json
            .pointer("/fields/deposit_asset/fields/name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("vault config missing deposit_asset"))?,
    );
    let mut free_assets = type_name_set(move_field(&vault, "asset_types")?)?;
    free_assets.retain(|t| *t != deposit_type);

    // External-account registration (SO-299). The field is an
    // `Option<ExternalAccount>` — absent/null on vaults without one (and
    // on pre-SO-299 deployments, where the field itself is missing).
    let (external_account, external_equity_oracle, external_exposure) =
        match move_field(&vault, "external") {
            Ok(v) => {
                let j = serde_json::to_value(v)?;
                let account = j
                    .pointer("/fields/account")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let oracle = j
                    .pointer("/fields/equity_oracle/fields/name")
                    .or_else(|| j.pointer("/fields/equity_oracle"))
                    .and_then(Value::as_str)
                    .map(canon);
                let exposure = j
                    .pointer("/fields/exposure")
                    .and_then(|e| {
                        e.as_str().and_then(|s| s.parse().ok()).or_else(|| e.as_u64())
                    })
                    .unwrap_or(0);
                (account, oracle, exposure)
            }
            Err(_) => (None, None, 0),
        };

    // Walk the vault's dynamic fields: adapter tags (plain df, value =
    // TypeName) and positions (dof, object_type = the custody struct).
    let mut tags: BTreeMap<ObjectID, String> = BTreeMap::new();
    let mut position_ids: Vec<ObjectID> = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            .read_api()
            .get_dynamic_fields(vault_id, cursor, None)
            .await
            .context("listing vault dynamic fields")?;
        for entry in &page.data {
            let name_type = entry.name.type_.to_string();
            if name_type.ends_with("::vault::PositionTagKey") {
                let pos_id = entry
                    .name
                    .value
                    .pointer("/id")
                    .and_then(Value::as_str)
                    .and_then(|s| ObjectID::from_hex_literal(s).ok())
                    .ok_or_else(|| anyhow!("unparseable PositionTagKey name"))?;
                // Read the tag value (a TypeName).
                let tag_obj = client
                    .read_api()
                    .get_dynamic_field_object(vault_id, entry.name.clone())
                    .await
                    .context("reading position tag")?;
                let tag = tag_obj
                    .data
                    .and_then(|d| d.content)
                    .and_then(|c| match c {
                        sui_sdk::rpc_types::SuiParsedData::MoveObject(o) => {
                            serde_json::to_value(o.fields).ok()
                        }
                        _ => None,
                    })
                    .and_then(|j| {
                        j.pointer("/value/fields/name")
                            .or(j.pointer("/fields/value/fields/name"))
                            .and_then(Value::as_str)
                            .map(canon)
                    })
                    .ok_or_else(|| anyhow!("unparseable adapter tag for {pos_id}"))?;
                tags.insert(pos_id, tag);
            } else if name_type.ends_with("::vault::PositionKey") {
                let pos_id = entry
                    .name
                    .value
                    .pointer("/id")
                    .and_then(Value::as_str)
                    .and_then(|s| ObjectID::from_hex_literal(s).ok())
                    .ok_or_else(|| anyhow!("unparseable PositionKey name"))?;
                position_ids.push(pos_id);
            }
        }
        if page.has_next_page {
            cursor = page.next_cursor;
        } else {
            break;
        }
    }

    let mut positions = Vec::new();
    for pos_id in position_ids {
        let ty = object_type(client, pos_id).await?;
        let tag = tags.get(&pos_id).cloned().unwrap_or_default();
        if ty.ends_with("::deepbook_adapter::DeepBookCustody") {
            let fields = object_fields(client, pos_id).await?;
            let assets = type_name_set(move_field(&fields, "assets")?)?
                .into_iter()
                .collect();
            let pool_ids = id_set(move_field(&fields, "active_pools")?)?;
            let mut pools = Vec::new();
            for pool_id in pool_ids {
                let pool_ty = object_type(client, pool_id).await?;
                let inner = pool_ty
                    .split_once('<')
                    .map(|(_, rest)| rest.trim_end_matches('>'))
                    .ok_or_else(|| anyhow!("unparseable pool type {pool_ty}"))?;
                let mut parts = split_type_args(inner);
                if parts.len() != 2 {
                    bail!("expected 2 pool type args in {pool_ty}");
                }
                let quote = canon(&parts.pop().unwrap());
                let base = canon(&parts.pop().unwrap());
                pools.push((pool_id, base, quote));
            }
            positions.push(PositionInfo::DeepBookCustody { id: pos_id, assets, pools });
        } else if ty.ends_with("::options_adapter::RfqTicket") {
            let fields = object_fields(client, pos_id).await?;
            let escrow_json = serde_json::to_value(move_field(&fields, "escrow_type")?)?;
            let escrow_type = canon(
                escrow_json
                    .pointer("/fields/name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("ticket missing escrow_type"))?,
            );
            let id_field = |name: &str| -> Result<ObjectID> {
                let j = serde_json::to_value(move_field(&fields, name)?)?;
                j.as_str()
                    .and_then(|s| ObjectID::from_hex_literal(s).ok())
                    .ok_or_else(|| anyhow!("ticket missing {name}"))
            };
            let is_put = serde_json::to_value(move_field(&fields, "is_put")?)?
                .as_bool()
                .unwrap_or(false);
            positions.push(PositionInfo::RfqTicket {
                id: pos_id,
                escrow_type,
                auction_id: id_field("auction_id")?,
                bucket_id: id_field("bucket_id")?,
                is_put,
            });
        } else if ty.ends_with("::options_adapter::BidTicket") {
            let fields = object_fields(client, pos_id).await?;
            let type_field = |name: &str| -> Result<String> {
                let j = serde_json::to_value(move_field(&fields, name)?)?;
                Ok(canon(
                    j.pointer("/fields/name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("bid ticket missing {name}"))?,
                ))
            };
            let u64_field = |name: &str| -> Result<u64> {
                let j = serde_json::to_value(move_field(&fields, name)?)?;
                j.as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| j.as_u64())
                    .ok_or_else(|| anyhow!("bid ticket missing {name}"))
            };
            let auction_id = {
                let j = serde_json::to_value(move_field(&fields, "auction_id")?)?;
                j.as_str()
                    .and_then(|s| ObjectID::from_hex_literal(s).ok())
                    .ok_or_else(|| anyhow!("bid ticket missing auction_id"))?
            };
            positions.push(PositionInfo::BidTicket {
                id: pos_id,
                escrow_type: type_field("escrow_type")?,
                win_type: type_field("win_type")?,
                auction_id,
                escrow_amount: u64_field("escrow_amount")?,
                win_amount: u64_field("win_amount")?,
            });
        } else if ty.ends_with("::position::Position") {
            let fields = object_fields(client, pos_id).await?;
            let bucket_json = serde_json::to_value(move_field(&fields, "bucket_id")?)?;
            let bucket_id = bucket_json
                .as_str()
                .and_then(|s| ObjectID::from_hex_literal(s).ok())
                .ok_or_else(|| anyhow!("position missing bucket_id"))?;
            let bucket_ty = object_type(client, bucket_id).await?;
            let is_put = bucket_ty.contains("::put_bucket::PutBucket");
            let inner = bucket_ty
                .split_once('<')
                .map(|(_, rest)| rest.trim_end_matches('>'))
                .ok_or_else(|| anyhow!("unparseable bucket type {bucket_ty}"))?;
            let parts = split_type_args(inner);
            if parts.len() != 3 {
                bail!("expected 3 bucket type args in {bucket_ty}");
            }
            positions.push(PositionInfo::OptionPosition {
                id: pos_id,
                bucket_id,
                is_put,
                underlying: canon(&parts[0]),
                settlement: canon(&parts[1]),
                call_type: canon(&parts[2]),
                via_vault_mm: tag.ends_with("::vault_mm::VaultMm"),
            });
        } else if ty.starts_with("0x2::coin::Coin<") || ty.contains("::coin::Coin<") {
            let inner = ty
                .split_once('<')
                .map(|(_, rest)| rest.trim_end_matches('>'))
                .unwrap_or_default();
            positions.push(PositionInfo::OptionCoin { id: pos_id, call_type: canon(inner) });
        } else {
            bail!("unrecognized custodied position type {ty} ({pos_id})");
        }
    }

    Ok(VaultHoldings {
        deposit_type,
        free_assets,
        positions,
        external_account,
        external_equity_oracle,
        external_exposure,
    })
}

/// Split top-level generic args, respecting nesting.
fn split_type_args(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in inner.chars() {
        match c {
            '<' => {
                depth += 1;
                cur.push(c);
            }
            '>' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur = String::new();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Everything price-shaped the caller resolved up front.
pub struct PriceLegs<'a> {
    pub pyth: &'a PythHandles,
    /// One Hermes accumulator update covering every feed below.
    pub accumulator_update: &'a [u8],
    /// canonical coin type → its shared PriceInfoObject.
    pub price_infos: &'a BTreeMap<String, ObjectID>,
}

/// Emit the full appraisal and return its Argument. The caller then
/// appends `deposit` / `fulfill_withdrawals` with it.
#[allow(clippy::too_many_arguments)]
pub async fn compose_appraisal(
    client: &SuiClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &AppraisalRefs,
    holdings: &VaultHoldings,
    legs: Option<PriceLegs<'_>>,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
) -> Result<Argument> {
    let deposit_tag = TypeTag::from_str(&holdings.deposit_type)
        .context("parsing deposit type")?;
    let vault_ro = pt.obj(shared_object_arg(client, refs.vault_id, false).await?)?;
    let cfg = pt.obj(shared_object_arg(client, refs.protocol_config_id, false).await?)?;
    let clock = clock_arg(pt)?;

    // Attestations, shared across every leg that prices the same asset.
    // Attest every needed asset a feed exists for; unpriceable assets
    // get `option::none` legs and the ON-CHAIN checks decide — the
    // adapters abort only when an unpriced component is actually
    // nonzero (e.g. a pool's call-coin base with zero locked passes; a
    // real unpriceable inventory correctly wedges the appraisal).
    let needed = holdings.assets_needing_attestation();
    let pyth_needed = pyth_assets_needed(holdings, option_buckets, refs.dbm.as_ref());
    let mut attestations: BTreeMap<String, Argument> = BTreeMap::new();
    let attestable: Vec<String> = match &legs {
        Some(l) => pyth_needed
            .iter()
            .filter(|t| l.price_infos.contains_key(*t))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    // Hard requirement only where an amount is KNOWN nonzero client-side:
    // non-deposit free balances (their Balance dfs are pruned at zero).
    for asset in &holdings.free_assets {
        if !attestable.contains(asset) && !option_buckets.contains_key(asset) {
            return Err(anyhow!(
                "free balance {asset} needs a price attestation but no feed/leg is available"
            ));
        }
    }
    if !attestable.is_empty() {
        let legs = legs.as_ref().expect("attestable implies legs");
        let deposit_info_id = *legs
            .price_infos
            .get(&holdings.deposit_type)
            .ok_or_else(|| anyhow!("no PriceInfoObject for the deposit asset"))?;
        let mut update_ids = vec![deposit_info_id];
        for t in &attestable {
            let info = legs.price_infos[t];
            if !update_ids.contains(&info) {
                update_ids.push(info);
            }
        }
        prepend_price_update(client, pt, legs.pyth, legs.accumulator_update, &update_ids)
            .await
            .context("building pyth update prefix")?;

        let feed_reg = pt.obj(shared_object_arg(client, refs.pyth_feed_registry_id, false).await?)?;
        let oracle_reg = pt.obj(shared_object_arg(client, refs.oracle_registry_id, false).await?)?;
        let deposit_info = pt.obj(shared_object_arg(client, deposit_info_id, false).await?)?;
        for asset in &attestable {
            let asset_info = pt.obj(shared_object_arg(client, legs.price_infos[asset], false).await?)?;
            let asset_tag = TypeTag::from_str(asset).context("parsing asset type")?;
            let att = pt.programmable_move_call(
                refs.oracle_pyth_pkg,
                Identifier::new("oracle_pyth").unwrap(),
                Identifier::new("attest").unwrap(),
                vec![asset_tag, deposit_tag.clone()],
                vec![feed_reg, oracle_reg, asset_info, deposit_info, clock],
            );
            attestations.insert(asset.clone(), att);
        }
    }

    let attestation_type = TypeTag::from_str(&format!(
        "{}::price::PriceAttestation",
        refs.trading_vault_pkg
    ))
    .context("attestation type tag")?;
    // Option<PriceAttestation> plumbing.
    let some = |pt: &mut ProgrammableTransactionBuilder, att: Argument| {
        pt.programmable_move_call(
            ObjectID::from_hex_literal("0x1").unwrap(),
            Identifier::new("option").unwrap(),
            Identifier::new("some").unwrap(),
            vec![attestation_type.clone()],
            vec![att],
        )
    };
    let none = |pt: &mut ProgrammableTransactionBuilder| {
        pt.programmable_move_call(
            ObjectID::from_hex_literal("0x1").unwrap(),
            Identifier::new("option").unwrap(),
            Identifier::new("none").unwrap(),
            vec![attestation_type.clone()],
            vec![],
        )
    };
    let opt_for = |pt: &mut ProgrammableTransactionBuilder,
                   attestations: &BTreeMap<String, Argument>,
                   ty: &str,
                   deposit: &str| {
        if ty == deposit {
            none(pt)
        } else if let Some(att) = attestations.get(ty) {
            some(pt, *att)
        } else {
            none(pt)
        }
    };

    // Option-coin attestations (SO-297): every mapped option-coin type the
    // vault holds prices through `options_oracle::attest_call/put` —
    // intrinsic from the bucket's terms plus the pyth legs above. Legs
    // equal to the deposit asset (or moot on an expired bucket) pass
    // `none`; a live bucket with a missing leg aborts on-chain, which is
    // the correct wedge.
    let option_types: Vec<(String, &OptionBucketInfo)> = needed
        .iter()
        .filter_map(|t| option_buckets.get(t).map(|b| (t.clone(), b)))
        .collect();
    if !option_types.is_empty() {
        let oa = refs
            .options_adapter_pkg
            .ok_or_else(|| anyhow!("options adapter package unavailable for option-coin legs"))?;
        let vol_book_id = refs
            .vol_book_id
            .ok_or_else(|| anyhow!("vol book unavailable for option-coin legs"))?;
        let oracle_reg = pt.obj(shared_object_arg(client, refs.oracle_registry_id, false).await?)?;
        let vol_book = pt.obj(shared_object_arg(client, vol_book_id, false).await?)?;
        for (coin_type, b) in &option_types {
            let bucket = pt.obj(shared_object_arg(client, b.bucket_id, false).await?)?;
            let u_opt = opt_for(pt, &attestations, &b.underlying, &holdings.deposit_type);
            let s_opt = opt_for(pt, &attestations, &b.settlement, &holdings.deposit_type);
            let function = if b.is_put { "attest_put" } else { "attest_call" };
            let att = pt.programmable_move_call(
                oa,
                Identifier::new("options_oracle").unwrap(),
                Identifier::new(function).unwrap(),
                vec![
                    TypeTag::from_str(&b.underlying)?,
                    TypeTag::from_str(&b.settlement)?,
                    TypeTag::from_str(coin_type)?,
                    deposit_tag.clone(),
                ],
                vec![oracle_reg, bucket, vol_book, u_opt, s_opt, clock],
            );
            attestations.insert(coin_type.clone(), att);
        }
    }

    // begin_appraisal AFTER the price update so nothing in the update
    // path can touch vault state mid-snapshot (it can't anyway; order is
    // for clarity).
    let appraisal = pt.programmable_move_call(
        refs.trading_vault_pkg,
        Identifier::new("vault").unwrap(),
        Identifier::new("begin_appraisal").unwrap(),
        vec![deposit_tag.clone()],
        vec![vault_ro],
    );

    // External-account equity leg (SO-299): a FUNDED external account marks
    // the appraisal `external_pending` at begin_appraisal, and consumption
    // aborts (82, appraisal_incomplete) without `record_external_equity`.
    // Compose the pinned oracle's leg — the trustless DBM adapter when
    // the caller supplied `refs.dbm`, else the attested EquityBook path —
    // and refuse outright (distinctive error, no silent incomplete
    // appraisal) when the pinned witness doesn't match the leg we'd build.
    //
    // A registered-but-unfunded account (exposure == 0, SO-310) marks
    // nothing: recording equity for it aborts (already_appraised), so the
    // leg is skipped entirely — that's how a vault's FIRST deposit composes
    // before the EquityBook has any entry to record.
    if let Some(witness) = holdings
        .external_equity_oracle
        .as_ref()
        .filter(|_| holdings.external_exposure > 0)
    {
        if let Some(dbm) = &refs.dbm {
            let expected = canon(&format!("{}::dbm_oracle::DbmOracle", dbm.dbm_oracle_pkg));
            if canon(witness) != expected {
                return Err(anyhow!(
                    "vault pins external equity oracle {witness} but the DBM leg config \
                     expects {expected}"
                ));
            }
            // Debt side from the manager's borrowed-share fields: zero on
            // both ⇒ `record_no_debt`; else `record` with the borrowed
            // side's margin pool as DebtAsset (`calculate_debts` aborts on
            // the wrong pool, so the selection is chain-checked too).
            let mgr = object_fields(client, dbm.margin_manager_id).await?;
            let shares = |name: &str| -> Result<u64> {
                let j = serde_json::to_value(move_field(&mgr, name)?)?;
                j.as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| j.as_u64())
                    .ok_or_else(|| anyhow!("margin manager missing {name}"))
            };
            let base_shares = shares("borrowed_base_shares")?;
            let quote_shares = shares("borrowed_quote_shares")?;

            let oracle_reg =
                pt.obj(shared_object_arg(client, refs.oracle_registry_id, false).await?)?;
            let manager = pt.obj(shared_object_arg(client, dbm.margin_manager_id, false).await?)?;
            let pool = pt.obj(shared_object_arg(client, dbm.deepbook_pool_id, false).await?)?;
            let b_opt = opt_for(pt, &attestations, &dbm.base_type, &holdings.deposit_type);
            let q_opt = opt_for(pt, &attestations, &dbm.quote_type, &holdings.deposit_type);
            let base_tag = TypeTag::from_str(&dbm.base_type).context("dbm base_type")?;
            let quote_tag = TypeTag::from_str(&dbm.quote_type).context("dbm quote_type")?;
            if base_shares == 0 && quote_shares == 0 {
                pt.programmable_move_call(
                    dbm.dbm_oracle_pkg,
                    Identifier::new("dbm_oracle").unwrap(),
                    Identifier::new("record_no_debt").unwrap(),
                    vec![base_tag, quote_tag],
                    vec![vault_ro, oracle_reg, cfg, appraisal, manager, pool, b_opt, q_opt, clock],
                );
            } else {
                let (debt_pool_id, debt_tag) = if base_shares > 0 {
                    (dbm.base_margin_pool_id, base_tag.clone())
                } else {
                    (dbm.quote_margin_pool_id, quote_tag.clone())
                };
                let margin_pool = pt.obj(shared_object_arg(client, debt_pool_id, false).await?)?;
                pt.programmable_move_call(
                    dbm.dbm_oracle_pkg,
                    Identifier::new("dbm_oracle").unwrap(),
                    Identifier::new("record").unwrap(),
                    vec![base_tag, quote_tag, debt_tag],
                    vec![
                        vault_ro, oracle_reg, cfg, appraisal, manager, pool, margin_pool, b_opt,
                        q_opt, clock,
                    ],
                );
            }
        } else {
            let Some(eo_pkg) = refs.equity_oracle_pkg else {
                return Err(anyhow!(
                    "unsupported external equity oracle: {witness} (equity-oracle package unresolved)"
                ));
            };
            let expected = canon(&format!("{eo_pkg}::equity_oracle::EquityOracle"));
            if canon(witness) != expected {
                return Err(anyhow!("unsupported external equity oracle: {witness}"));
            }
            let book_id = refs.equity_book_id.ok_or_else(|| {
                anyhow!("equity-oracle EquityBook id unresolved — cannot compose the equity leg")
            })?;
            let book = pt.obj(shared_object_arg(client, book_id, false).await?)?;
            let oracle_reg =
                pt.obj(shared_object_arg(client, refs.oracle_registry_id, false).await?)?;
            // equity_oracle::record(vault, book, reg, &mut appraisal, clock)
            pt.programmable_move_call(
                eo_pkg,
                Identifier::new("equity_oracle").unwrap(),
                Identifier::new("record").unwrap(),
                vec![],
                vec![vault_ro, book, oracle_reg, appraisal, clock],
            );
        }
    }

    // Free balances.
    for asset in &holdings.free_assets {
        let att = *attestations
            .get(asset)
            .ok_or_else(|| anyhow!("missing attestation for free balance {asset}"))?;
        let asset_tag = TypeTag::from_str(asset)?;
        pt.programmable_move_call(
            refs.trading_vault_pkg,
            Identifier::new("vault").unwrap(),
            Identifier::new("appraise_balance").unwrap(),
            vec![asset_tag],
            vec![vault_ro, cfg, appraisal, att, clock],
        );
    }

    // Positions.
    for p in &holdings.positions {
        match p {
            PositionInfo::DeepBookCustody { id, assets, pools } => {
                let dba = refs
                    .deepbook_adapter_pkg
                    .ok_or_else(|| anyhow!("deepbook adapter package unavailable"))?;
                let custody_id = pt.pure(id)?;
                let ca = pt.programmable_move_call(
                    dba,
                    Identifier::new("deepbook_adapter").unwrap(),
                    Identifier::new("begin_custody_appraisal").unwrap(),
                    vec![],
                    vec![vault_ro, custody_id],
                );
                for asset in assets {
                    let opt = opt_for(pt, &attestations, asset, &holdings.deposit_type);
                    let tag = TypeTag::from_str(asset)?;
                    pt.programmable_move_call(
                        dba,
                        Identifier::new("deepbook_adapter").unwrap(),
                        Identifier::new("value_asset").unwrap(),
                        vec![tag],
                        vec![vault_ro, cfg, ca, opt, clock],
                    );
                }
                for (pool_id, base, quote) in pools {
                    let pool = pt.obj(shared_object_arg(client, *pool_id, false).await?)?;
                    let b_opt = opt_for(pt, &attestations, base, &holdings.deposit_type);
                    let q_opt = opt_for(pt, &attestations, quote, &holdings.deposit_type);
                    let d_opt = none(pt);
                    pt.programmable_move_call(
                        dba,
                        Identifier::new("deepbook_adapter").unwrap(),
                        Identifier::new("value_pool_locked").unwrap(),
                        vec![TypeTag::from_str(base)?, TypeTag::from_str(quote)?],
                        vec![vault_ro, cfg, ca, pool, b_opt, q_opt, d_opt, clock],
                    );
                }
                pt.programmable_move_call(
                    dba,
                    Identifier::new("deepbook_adapter").unwrap(),
                    Identifier::new("finalize_custody_appraisal").unwrap(),
                    vec![],
                    vec![vault_ro, appraisal, ca],
                );
            }
            PositionInfo::RfqTicket { id, escrow_type, .. } => {
                let oa = refs
                    .options_adapter_pkg
                    .ok_or_else(|| anyhow!("options adapter package unavailable"))?;
                let ticket_id = pt.pure(id)?;
                let opt = opt_for(pt, &attestations, escrow_type, &holdings.deposit_type);
                pt.programmable_move_call(
                    oa,
                    Identifier::new("options_adapter").unwrap(),
                    Identifier::new("appraise_rfq_ticket").unwrap(),
                    vec![TypeTag::from_str(escrow_type)?],
                    vec![vault_ro, cfg, appraisal, ticket_id, opt, clock],
                );
            }
            PositionInfo::BidTicket { id, escrow_type, .. } => {
                let oa = refs
                    .options_adapter_pkg
                    .ok_or_else(|| anyhow!("options adapter package unavailable"))?;
                let ticket_id = pt.pure(id)?;
                let opt = opt_for(pt, &attestations, escrow_type, &holdings.deposit_type);
                pt.programmable_move_call(
                    oa,
                    Identifier::new("options_adapter").unwrap(),
                    Identifier::new("appraise_bid_ticket").unwrap(),
                    vec![TypeTag::from_str(escrow_type)?],
                    vec![vault_ro, cfg, appraisal, ticket_id, opt, clock],
                );
            }
            PositionInfo::OptionPosition {
                id,
                bucket_id,
                is_put,
                underlying,
                settlement,
                call_type,
                via_vault_mm,
            } => {
                let (pkg, module) = if *via_vault_mm {
                    (refs.trading_vault_pkg, "vault_mm")
                } else {
                    (
                        refs.options_adapter_pkg
                            .ok_or_else(|| anyhow!("options adapter package unavailable"))?,
                        "options_adapter",
                    )
                };
                let function = if *is_put { "appraise_put_position" } else { "appraise_call_position" };
                let bucket = pt.obj(shared_object_arg(client, *bucket_id, false).await?)?;
                let pos_id = pt.pure(id)?;
                let u_opt = opt_for(pt, &attestations, underlying, &holdings.deposit_type);
                let s_opt = opt_for(pt, &attestations, settlement, &holdings.deposit_type);
                pt.programmable_move_call(
                    pkg,
                    Identifier::new(module).unwrap(),
                    Identifier::new(function).unwrap(),
                    vec![
                        TypeTag::from_str(underlying)?,
                        TypeTag::from_str(settlement)?,
                        TypeTag::from_str(call_type)?,
                    ],
                    vec![vault_ro, cfg, appraisal, bucket, pos_id, u_opt, s_opt, clock],
                );
            }
            PositionInfo::OptionCoin { id, call_type } => {
                let Some(b) = option_buckets.get(call_type) else {
                    bail!(
                        "held option coin {id} ({call_type}) has no bucket mapping — \
                         cannot appraise"
                    );
                };
                let bucket = pt.obj(shared_object_arg(client, b.bucket_id, false).await?)?;
                let pos_id = pt.pure(id)?;
                let u_opt = opt_for(pt, &attestations, &b.underlying, &holdings.deposit_type);
                let s_opt = opt_for(pt, &attestations, &b.settlement, &holdings.deposit_type);
                let function = if b.is_put { "appraise_put_coin" } else { "appraise_call_coin" };
                pt.programmable_move_call(
                    refs.trading_vault_pkg,
                    Identifier::new("vault_mm").unwrap(),
                    Identifier::new(function).unwrap(),
                    vec![
                        TypeTag::from_str(&b.underlying)?,
                        TypeTag::from_str(&b.settlement)?,
                        TypeTag::from_str(call_type)?,
                    ],
                    vec![vault_ro, cfg, appraisal, bucket, pos_id, u_opt, s_opt, clock],
                );
            }
        }
    }
    Ok(appraisal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbm_leg_adds_base_and_quote_to_pyth_needs() {
        let holdings = VaultHoldings {
            deposit_type: "0xa::tusdc::TUSDC".into(),
            free_assets: vec![],
            positions: vec![],
            external_account: Some("0xee".into()),
            external_equity_oracle: Some("0xd0::dbm_oracle::DbmOracle".into()),
            external_exposure: 1,
        };
        let none = pyth_assets_needed(&holdings, &BTreeMap::new(), None);
        assert!(none.is_empty());

        let dbm = DbmLegInfo {
            dbm_oracle_pkg: ObjectID::from_hex_literal("0xd0").unwrap(),
            margin_manager_id: ObjectID::from_hex_literal("0x11").unwrap(),
            deepbook_pool_id: ObjectID::from_hex_literal("0x12").unwrap(),
            base_margin_pool_id: ObjectID::from_hex_literal("0x13").unwrap(),
            quote_margin_pool_id: ObjectID::from_hex_literal("0x14").unwrap(),
            base_type: "0x2::sui::SUI".into(),
            // The quote IS the deposit asset: it must NOT need a pyth leg.
            quote_type: "0xa::tusdc::TUSDC".into(),
        };
        let needed = pyth_assets_needed(&holdings, &BTreeMap::new(), Some(&dbm));
        assert_eq!(
            needed.into_iter().collect::<Vec<_>>(),
            vec!["0x2::sui::SUI".to_string()]
        );

        // SO-310: an unfunded external account composes no equity leg, so
        // its base/quote need no Pyth legs either — an otherwise cash-only
        // vault stays priceless (no price table required to appraise it).
        let unfunded = VaultHoldings { external_exposure: 0, ..holdings };
        assert!(pyth_assets_needed(&unfunded, &BTreeMap::new(), Some(&dbm)).is_empty());
    }
}
