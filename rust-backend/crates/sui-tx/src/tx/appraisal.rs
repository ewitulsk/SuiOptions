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
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;

use crate::tx::{clock_arg, shared_object_arg};
use crate::chain::ChainClient;

/// Package + shared-object identity the composer calls against.
#[derive(Debug, Clone)]
pub struct AppraisalRefs {
    pub trading_vault_pkg: ObjectID,
    pub deepbook_adapter_pkg: Option<ObjectID>,
    pub options_adapter_pkg: Option<ObjectID>,
    /// exchange-adapter package (SO-373), for ExchangeCustody legs.
    /// `None` where the package isn't deployed.
    pub exchange_adapter_pkg: Option<ObjectID>,
    pub vault_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub oracle_registry_id: ObjectID,
    /// equity-oracle package (SO-299), for the external-account equity
    /// leg. `None` where the package isn't deployed.
    pub equity_oracle_pkg: Option<ObjectID>,
    /// The equity-oracle package's shared `EquityBook`.
    pub equity_book_id: Option<ObjectID>,
    /// The options-adapter package's shared `VolBook` (premium
    /// mark-to-market). Required whenever option-coin legs compose.
    pub vol_book_id: Option<ObjectID>,
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
    /// Exchange-adapter custody (SO-370): authority over a SHARED
    /// BalanceManager, valued from the manager's live balances. A direct
    /// custody (SO-372) tracks no assets and values to zero.
    ExchangeCustody {
        id: ObjectID,
        bm_id: ObjectID,
        assets: Vec<String>,
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
                PositionInfo::ExchangeCustody { assets, .. } => {
                    out.extend(assets.iter().cloned());
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
/// settlement legs (the coin itself prices via the options oracle), and
/// held option-coin positions contribute their legs.
pub fn price_assets_needed(
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
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
    out.remove(&holdings.deposit_type);
    out
}

fn canon(s: &str) -> String {
    protocol_types::asset::canonicalize_move_type(s)
}

/// One field off a Move object's JSON rendering.
///
/// The gRPC/GraphQL `json` rendering nests struct fields DIRECTLY — there is
/// no `fields` wrapper the way JSON-RPC's `SuiMoveStruct` had (conventions
/// documented and golden-tested in `api-service/src/sui_rpc.rs`). Every
/// pointer path in this module is written against that rendering.
fn move_field<'a>(s: &'a Value, name: &str) -> Result<&'a Value> {
    s.get(name)
        .ok_or_else(|| anyhow!("object missing field {name}"))
}

/// A `TypeName`-valued JSON node → the type string.
///
/// The gRPC json rendering collapses `TypeName` to the BARE string
/// (golden-tested in `api-service/src/sui_rpc.rs`); the struct-shaped
/// `{"name": …}` form is tolerated for renderings that don't. Reading
/// only the struct shape is exactly the SO-337 regression that broke
/// `discover_holdings` on every vault ("vault config missing
/// deposit_asset") — use this helper for every TypeName field.
fn type_name_str(v: &Value) -> Option<&str> {
    v.as_str()
        .or_else(|| v.pointer("/name").and_then(Value::as_str))
}

/// A `VecSet<TypeName>` field → canonical type strings.
fn type_name_set(v: &Value) -> Result<Vec<String>> {
    let contents = v
        .pointer("/contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for entry in contents {
        let name = entry
            .pointer("/name")
            .and_then(Value::as_str)
            .or_else(|| entry.as_str())
            .ok_or_else(|| anyhow!("unparseable TypeName entry: {entry}"))?;
        out.push(canon(name));
    }
    Ok(out)
}

/// A `VecSet<ID>` field → object ids.
fn id_set(v: &Value) -> Result<Vec<ObjectID>> {
    let contents = v
        .pointer("/contents")
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

async fn object_fields(client: &ChainClient, id: ObjectID) -> Result<Value> {
    let (_, json) = client
        .get_object_json(id)
        .await
        .with_context(|| format!("fetching object {id}"))?;
    json.ok_or_else(|| anyhow!("object {id} has no parsed move content"))
}

async fn object_type(client: &ChainClient, id: ObjectID) -> Result<String> {
    let obj = client
        .get_object(id)
        .await
        .with_context(|| format!("fetching object {id} type"))?;
    Ok(obj
        .struct_tag()
        .ok_or_else(|| anyhow!("object {id} missing type"))?
        .to_canonical_string(/* with_prefix */ true))
}

/// Chain-only holdings discovery. `bucket_for_call_type` answers which
/// bucket a held option coin belongs to (indexer-supplied; pass an empty
/// map when the vault can't hold vault_mm option coins).
pub async fn discover_holdings(
    client: &ChainClient,
    vault_id: ObjectID,
) -> Result<VaultHoldings> {
    let vault = object_fields(client, vault_id).await?;
    let config_json = move_field(&vault, "config")?.clone();
    // SO-370 renamed the config field deposit_asset → accounting_asset;
    // tolerate the old name so a stale rendering doesn't wedge discovery.
    let deposit_type = canon(
        config_json
            .get("accounting_asset")
            .or_else(|| config_json.get("deposit_asset"))
            .and_then(type_name_str)
            .ok_or_else(|| anyhow!("vault config missing accounting_asset"))?,
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
                    .pointer("/account")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let oracle = j
                    .pointer("/equity_oracle/name")
                    .or_else(|| j.pointer("/equity_oracle"))
                    .and_then(Value::as_str)
                    .map(canon);
                let exposure = j
                    .pointer("/exposure")
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
    // gRPC lists the `Field<K, V>` objects; one read of each field object
    // yields both its name (the key struct) and its value.
    let entries = client
        .dynamic_fields(vault_id)
        .await
        .context("listing vault dynamic fields")?;
    for entry in &entries {
        let is_tag = entry.name_type_ends_with("::vault::PositionTagKey");
        let is_pos = entry.name_type_ends_with("::vault::PositionKey");
        if !is_tag && !is_pos {
            continue;
        }
        let field = object_fields(client, entry.field_id)
            .await
            .with_context(|| format!("reading vault dynamic field {}", entry.field_id))?;
        // Both key structs wrap the position id as their sole field. For
        // a dynamic OBJECT field the key sits one level deeper — inside
        // `dynamic_object_field::Wrapper { name: K }` — so tolerate both
        // nestings (positions are dofs, adapter tags are plain dfs).
        let pos_id = field
            .pointer("/name/id")
            .or_else(|| field.pointer("/name/name/id"))
            .and_then(Value::as_str)
            .and_then(|s| ObjectID::from_hex_literal(s).ok())
            .ok_or_else(|| anyhow!("unparseable position key name for {}", entry.field_id))?;
        if is_tag {
            let tag = field
                .pointer("/value")
                .and_then(type_name_str)
                .map(canon)
                .ok_or_else(|| anyhow!("unparseable adapter tag for {pos_id}"))?;
            tags.insert(pos_id, tag);
        } else {
            position_ids.push(pos_id);
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
                    .and_then(|(_, rest)| rest.strip_suffix('>'))
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
        } else if ty.ends_with("::exchange_adapter::ExchangeCustody") {
            let fields = object_fields(client, pos_id).await?;
            let assets = type_name_set(move_field(&fields, "assets")?)?
                .into_iter()
                .collect();
            let bm_id = move_field(&fields, "bm_id")?
                .as_str()
                .and_then(|s| ObjectID::from_hex_literal(s).ok())
                .ok_or_else(|| anyhow!("exchange custody missing bm_id"))?;
            positions.push(PositionInfo::ExchangeCustody { id: pos_id, bm_id, assets });
        } else if ty.ends_with("::options_adapter::RfqTicket") {
            let fields = object_fields(client, pos_id).await?;
            let escrow_json = move_field(&fields, "escrow_type")?.clone();
            let escrow_type = canon(
                type_name_str(&escrow_json)
                    .ok_or_else(|| anyhow!("ticket missing escrow_type"))?,
            );
            let id_field = |name: &str| -> Result<ObjectID> {
                let j = move_field(&fields, name)?.clone();
                j.as_str()
                    .and_then(|s| ObjectID::from_hex_literal(s).ok())
                    .ok_or_else(|| anyhow!("ticket missing {name}"))
            };
            let is_put = move_field(&fields, "is_put")?.clone()
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
                let j = move_field(&fields, name)?.clone();
                Ok(canon(
                    type_name_str(&j)
                        .ok_or_else(|| anyhow!("bid ticket missing {name}"))?,
                ))
            };
            let u64_field = |name: &str| -> Result<u64> {
                let j = move_field(&fields, name)?.clone();
                j.as_str()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| j.as_u64())
                    .ok_or_else(|| anyhow!("bid ticket missing {name}"))
            };
            let auction_id = {
                let j = move_field(&fields, "auction_id")?.clone();
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
            let bucket_json = move_field(&fields, "bucket_id")?.clone();
            let bucket_id = bucket_json
                .as_str()
                .and_then(|s| ObjectID::from_hex_literal(s).ok())
                .ok_or_else(|| anyhow!("position missing bucket_id"))?;
            let bucket_ty = object_type(client, bucket_id).await?;
            let is_put = bucket_ty.contains("::put_bucket::PutBucket");
            let inner = bucket_ty
                .split_once('<')
                .and_then(|(_, rest)| rest.strip_suffix('>'))
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
                .and_then(|(_, rest)| rest.strip_suffix('>'))
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

/// Emit the full appraisal and return its Argument plus the per-asset
/// `PriceAttestation` arguments (SO-370: attestations are `copy`, so a
/// non-accounting deposit's option — and `begin_fulfillment`'s atts
/// vector — reuse the SAME attest results the appraisal legs consumed;
/// mirrors the TS composer's `{ appraisal, attestations }`). The caller
/// then appends `deposit` / `fulfill_withdrawals` / the fulfillment
/// potato with them.
///
/// `extra_attest` names assets that MUST get an attest leg beyond what
/// the holdings need — e.g. a non-accounting deposit's asset when the
/// vault doesn't hold it yet. Unlike optimistic holdings legs these
/// hard-error when the provider can't price them.
#[allow(clippy::too_many_arguments)]
pub async fn compose_appraisal(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &AppraisalRefs,
    holdings: &VaultHoldings,
    legs: Option<crate::tx::oracle::OracleLegs<'_>>,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    extra_attest: &[String],
) -> Result<(Argument, BTreeMap<String, Argument>)> {
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
    let mut priced_needed: Vec<String> =
        price_assets_needed(holdings, option_buckets).into_iter().collect();
    for extra in extra_attest {
        let extra = canon(extra);
        if extra != holdings.deposit_type && !priced_needed.contains(&extra) {
            priced_needed.push(extra);
        }
    }
    let mut attestations: BTreeMap<String, Argument> = BTreeMap::new();
    let attestable: Vec<String> = match &legs {
        Some(l) => l.attestable(&priced_needed),
        None => Vec::new(),
    };
    // Hard requirement only where an amount is KNOWN nonzero client-side:
    // non-deposit free balances (their Balance dfs are pruned at zero)
    // and the caller's explicit extras (a deposit's own asset).
    for asset in &holdings.free_assets {
        if !attestable.contains(asset) && !option_buckets.contains_key(asset) {
            return Err(anyhow!(
                "free balance {asset} needs a price attestation but no feed/leg is available"
            ));
        }
    }
    for extra in extra_attest {
        let extra = canon(extra);
        if extra != holdings.deposit_type && !attestable.contains(&extra) {
            return Err(anyhow!(
                "requested attestation for {extra} but no feed/leg is available"
            ));
        }
    }
    if !attestable.is_empty() {
        let legs = legs.as_ref().expect("attestable implies legs");
        // Provider-agnostic: the legs value decides which prefix is
        // emitted and which adapter's `attest` runs (SO-335). Nothing
        // below this point knows or cares which oracle priced the book.
        attestations = crate::tx::oracle::emit_price_legs(
            client,
            pt,
            legs,
            &crate::tx::oracle::OracleRefs {
                oracle_registry_id: refs.oracle_registry_id,
            },
            &attestable,
            &holdings.deposit_type,
            clock,
        )
        .await
        .with_context(|| format!("building {} price legs", legs.provider()))?;
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
    // Compose the pinned oracle's leg (the attested EquityBook path) and
    // refuse outright (distinctive error, no silent incomplete appraisal)
    // when the pinned witness doesn't match the leg we'd build.
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
            PositionInfo::ExchangeCustody { id, bm_id, assets } => {
                let xa = refs
                    .exchange_adapter_pkg
                    .ok_or_else(|| anyhow!("exchange adapter package unavailable"))?;
                let custody_id = pt.pure(id)?;
                let ca = pt.programmable_move_call(
                    xa,
                    Identifier::new("exchange_adapter").unwrap(),
                    Identifier::new("begin_custody_appraisal").unwrap(),
                    vec![],
                    vec![vault_ro, custody_id],
                );
                if !assets.is_empty() {
                    let bm = pt.obj(shared_object_arg(client, *bm_id, false).await?)?;
                    for asset in assets {
                        let opt = opt_for(pt, &attestations, asset, &holdings.deposit_type);
                        pt.programmable_move_call(
                            xa,
                            Identifier::new("exchange_adapter").unwrap(),
                            Identifier::new("value_asset").unwrap(),
                            vec![TypeTag::from_str(asset)?],
                            vec![vault_ro, cfg, ca, bm, opt, clock],
                        );
                    }
                }
                pt.programmable_move_call(
                    xa,
                    Identifier::new("exchange_adapter").unwrap(),
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
    Ok((appraisal, attestations))
}

/// Compose the appraisal with Switchboard price legs resolved live from
/// oracle-service (SO-346): coverage from the caller's `/oracle/descriptor`,
/// signed quote payload from `/oracle/legs`. One shared body for the
/// keeper's fulfillment/mark cranks, the smoke's legs, and staging-mm-bot's
/// vault-direct funding deposits — the leg-building must never drift
/// between them.
///
/// Same none-leg posture as the Pyth path: assets without a descriptor
/// feed get `option::none` legs and the on-chain checks decide (only a
/// nonzero unpriced component aborts). `extra_attest` assets hard-error
/// inside `compose_appraisal` when unpriceable, exactly as documented
/// there. The descriptor is caller-supplied (the keeper TTL-caches it);
/// callers must have checked `descriptor.provider == Switchboard`.
pub async fn compose_switchboard_appraisal(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    refs: &AppraisalRefs,
    holdings: &VaultHoldings,
    option_buckets: &BTreeMap<String, OptionBucketInfo>,
    descriptor: &oracle_client::OracleDescriptor,
    oracle: &oracle_client::OracleClient,
    extra_attest: &[String],
) -> Result<(Argument, BTreeMap<String, Argument>)> {
    // Cash-only and nothing extra to attest: no price legs at all (the
    // keeper and smoke also short-circuit this before provider dispatch).
    if price_assets_needed(holdings, option_buckets).is_empty() && extra_attest.is_empty() {
        return compose_appraisal(client, pt, refs, holdings, None, option_buckets, &[]).await;
    }
    let adapter = descriptor.adapter.as_ref().ok_or_else(|| {
        anyhow!(
            "live provider {} has no adapter deployed on this network — cannot build price legs",
            descriptor.provider
        )
    })?;
    let adapter_pkg = ObjectID::from_hex_literal(&adapter.adapter_package_id)
        .context("parsing descriptor adapter package id")?;
    let feed_registry_id = ObjectID::from_hex_literal(&adapter.feed_registry_id)
        .context("parsing descriptor feed registry id")?;

    // The deposit asset's feed must ride along: `attest<Asset, Dep>`
    // crosses each asset against it inside one `Quotes` bundle.
    let mut all_types: Vec<String> =
        price_assets_needed(holdings, option_buckets).into_iter().collect();
    all_types.extend(extra_attest.iter().map(|t| canon(t)));
    all_types.push(holdings.deposit_type.clone());
    let mut request: Vec<String> = Vec::new();
    let mut feed_hashes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for t in &all_types {
        if feed_hashes.contains_key(t) {
            continue;
        }
        let Some(hash) = descriptor.feeds.get(t) else {
            tracing::debug!(vault = %refs.vault_id, asset = %t, "no switchboard feed; passing none leg");
            continue;
        };
        let bytes = hex::decode(hash.trim().trim_start_matches("0x"))
            .with_context(|| format!("descriptor feed hash for {t} is not hex"))?;
        if bytes.len() != 32 {
            bail!("descriptor feed hash for {t} is {} bytes; expected 32", bytes.len());
        }
        feed_hashes.insert(t.clone(), bytes);
        request.push(t.clone());
    }
    if request.is_empty() {
        bail!(
            "no switchboard feed hash for any priced asset (deposit {})",
            holdings.deposit_type
        );
    }
    let legs = oracle.legs(&request).await.context("fetching /oracle/legs")?;
    let oracle_client::OracleLegsResponse::Switchboard(sw) = legs else {
        bail!(
            "/oracle/legs answered for a different provider than the descriptor — \
             provider flipped mid-compose; retry"
        );
    };
    let payload = switchboard_payload(&sw)?;
    let switchboard_pkg = ObjectID::from_hex_literal(&sw.switchboard_package_id)
        .context("parsing on_demand package id")?;
    compose_appraisal(
        client,
        pt,
        refs,
        holdings,
        Some(crate::tx::oracle::OracleLegs::Switchboard(crate::tx::oracle::SwitchboardLegs {
            adapter_pkg,
            feed_registry_id,
            switchboard_pkg,
            payload: &payload,
            feed_hashes: &feed_hashes,
        })),
        option_buckets,
        extra_attest,
    )
    .await
}

/// `/oracle/legs` wire → the submit shape `run_N` takes. Object ids
/// parse here (the wire is string-typed for JS safety); arity/shape
/// checks stay in `SwitchboardQuotePayload::validate` at PTB build time.
fn switchboard_payload(
    sw: &oracle_client::SwitchboardLegsPayload,
) -> Result<crate::tx::oracle::SwitchboardQuotePayload> {
    let q = &sw.quote;
    Ok(crate::tx::oracle::SwitchboardQuotePayload {
        feed_ids: q.feed_id_bytes()?,
        values: q.values_u128()?,
        values_neg: q.values_neg.clone(),
        min_oracle_samples: q.min_oracle_samples.clone(),
        signatures: q.signature_bytes()?,
        slot: q.slot,
        timestamp_seconds: q.timestamp_seconds,
        oracle_ids: q
            .oracle_ids
            .iter()
            .map(|o| {
                ObjectID::from_hex_literal(o)
                    .with_context(|| format!("parsing oracle object id {o:?}"))
            })
            .collect::<Result<Vec<_>>>()?,
        queue_id: ObjectID::from_hex_literal(&sw.queue_id).context("parsing queue object id")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switchboard_payload_assembles_the_submit_shape() {
        use base64::Engine as _;
        let sw = oracle_client::SwitchboardLegsPayload {
            switchboard_package_id: "0xea".into(),
            queue_id: "0xe645d8979dac2fb901fb7c7b0ef3c9fad5dfaaf7ae2b0ce38a0b5ec63b819a99"
                .into(),
            feed_hashes: [("0x1::a::A".to_string(), "ab".repeat(32))].into(),
            quote: oracle_client::SwitchboardQuoteWire {
                feed_ids: vec!["ab".repeat(32)],
                values: vec!["63456010000000000000000".into()],
                values_neg: vec![false],
                min_oracle_samples: vec![1],
                signatures_b64: vec![
                    base64::engine::general_purpose::STANDARD.encode([7u8; 64]),
                ],
                slot: 42,
                timestamp_seconds: 1_785_700_471,
                oracle_ids: vec!["0x11".into()],
            },
        };
        let p = switchboard_payload(&sw).unwrap();
        p.validate().unwrap();
        assert_eq!(p.run_function().unwrap(), "run_1");
        assert_eq!(p.feed_ids, vec![vec![0xab; 32]]);
        assert_eq!(p.values, vec![63_456_010_000_000_000_000_000u128]);
        assert_eq!(p.oracle_ids, vec![ObjectID::from_hex_literal("0x11").unwrap()]);

        // A bad object id is a composition error, not a chain abort.
        let mut bad = sw.clone();
        bad.quote.oracle_ids = vec!["not-an-id".into()];
        assert!(switchboard_payload(&bad).is_err());
    }

    #[test]
    fn type_name_reads_both_renderings() {
        // gRPC json: bare string (the live shape that broke
        // discover_holdings when only the struct form was read).
        let bare = serde_json::json!("f81e::tusdc::TUSDC");
        assert_eq!(type_name_str(&bare), Some("f81e::tusdc::TUSDC"));
        // Struct-shaped fallback.
        let wrapped = serde_json::json!({"name": "f81e::tusdc::TUSDC"});
        assert_eq!(type_name_str(&wrapped), Some("f81e::tusdc::TUSDC"));
        assert_eq!(type_name_str(&serde_json::json!({"other": 1})), None);
    }

    #[test]
    fn external_account_contributes_no_pyth_legs() {
        // The external-equity leg prices itself (attested EquityBook), so a
        // cash-only vault with a registered account stays priceless — no
        // price table needed to appraise it, funded or not.
        let holdings = VaultHoldings {
            deposit_type: "0xa::tusdc::TUSDC".into(),
            free_assets: vec![],
            positions: vec![],
            external_account: Some("0xee".into()),
            external_equity_oracle: Some("0xe0::equity_oracle::EquityOracle".into()),
            external_exposure: 1,
        };
        assert!(price_assets_needed(&holdings, &BTreeMap::new()).is_empty());

        let unfunded = VaultHoldings { external_exposure: 0, ..holdings };
        assert!(price_assets_needed(&unfunded, &BTreeMap::new()).is_empty());
    }
}
