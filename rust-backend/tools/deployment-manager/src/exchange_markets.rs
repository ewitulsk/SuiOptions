//! Auto-created Base/TUSDC exchange markets.
//!
//! Every redeploy reconciles the env's `exchange.markets` map against the
//! token catalog: each non-TUSDC token gets a `{SYM}/TUSDC` market. A
//! previous market whose base+quote coin types still match the catalog is
//! carried forward untouched (its registry id is the order-signature
//! domain — never recreate needlessly); a market whose types went stale
//! (token republish) is recreated; a market whose token left the catalog
//! is dropped from the record, which the orderbook's whitelist sync then
//! disables in its DB.
//!
//! All creations ride ONE PTB. `create_market` shares a
//! `SettlementRegistry<Base, Quote>` per call, so the created objects are
//! disambiguated afterwards by their `Base` type parameter.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_tx::chain::{created_objects, ChainClient};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use crate::json_store::{ExchangeMarketRecord, ExchangeRecord, TokenSpec};
use crate::signer::Signer;

/// Quote side of every auto-created market.
pub const QUOTE_SYMBOL: &str = "TUSDC";

/// Default fee, matching the exchange test suite's convention (0.1%).
const DEFAULT_FEE_BPS: u64 = 10;

/// Price grid: 0.001 TUSDC (6 decimals) per `lot_size` base units.
const DEFAULT_TICK_SIZE: u64 = 1_000;

/// A market that must be created on-chain this run.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedMarket {
    /// Record key, e.g. "TBTC/TUSDC".
    pub symbol: String,
    /// Base coin type in catalog form (with 0x).
    pub base_type: String,
    pub tick_size: u64,
    pub min_size: u64,
    pub lot_size: u64,
    pub fee_bps: u64,
}

/// Reconciliation of previous markets against the current token catalog.
#[derive(Debug)]
pub struct MarketPlan {
    /// Still-valid previous markets, carried forward untouched.
    pub keep: BTreeMap<String, ExchangeMarketRecord>,
    pub create: Vec<PlannedMarket>,
    /// Previous market symbols whose tokens left the catalog.
    pub dropped: Vec<String>,
}

fn canonical(ty: &str) -> Result<String> {
    let tag = TypeTag::from_str(ty).with_context(|| format!("parsing move type {ty}"))?;
    Ok(tag.to_canonical_string(true))
}

/// Pure reconciliation: which previous markets survive, which pairs need a
/// fresh registry, which are dead. Errors only on an unparseable catalog
/// coin type; a previous record with garbage types just fails its match
/// and gets recreated.
pub fn plan_markets(
    previous: &BTreeMap<String, ExchangeMarketRecord>,
    token_info: &BTreeMap<String, TokenSpec>,
) -> Result<MarketPlan> {
    let quote = token_info
        .get(QUOTE_SYMBOL)
        .ok_or_else(|| anyhow!("token catalog has no {QUOTE_SYMBOL}"))?;
    let quote_type = canonical(&quote.coin_type)?;

    let mut keep = BTreeMap::new();
    let mut create = Vec::new();
    for (sym, spec) in token_info {
        if sym == QUOTE_SYMBOL {
            continue;
        }
        let symbol = format!("{sym}/{QUOTE_SYMBOL}");
        let base_type = canonical(&spec.coin_type)?;
        let still_valid = previous.get(&symbol).and_then(|m| {
            let b = canonical(&m.base).ok()?;
            let q = canonical(&m.quote).ok()?;
            (b == base_type && q == quote_type).then(|| m.clone())
        });
        match still_valid {
            Some(m) => {
                keep.insert(symbol, m);
            }
            None => {
                let lot_size = 10u64.pow(u32::from(spec.decimals));
                create.push(PlannedMarket {
                    symbol,
                    base_type: spec.coin_type.clone(),
                    tick_size: DEFAULT_TICK_SIZE,
                    // 0.001 of a whole base coin.
                    min_size: (lot_size / 1_000).max(1),
                    lot_size,
                    fee_bps: DEFAULT_FEE_BPS,
                });
            }
        }
    }
    let dropped = previous
        .keys()
        .filter(|k| !keep.contains_key(*k) && !create.iter().any(|c| &c.symbol == *k))
        .cloned()
        .collect();
    Ok(MarketPlan { keep, create, dropped })
}

/// Reconcile + create. Rewrites `exchange.markets` to exactly the surviving
/// set. A catalog without TUSDC (an env with no test tokens) is a warning,
/// not an error — the redeploy proceeds with markets untouched.
pub async fn create_markets(
    client: &ChainClient,
    signer: &Signer,
    exchange: &mut ExchangeRecord,
    token_info: &BTreeMap<String, TokenSpec>,
    gas_budget: u64,
) -> Result<()> {
    if !token_info.contains_key(QUOTE_SYMBOL) {
        tracing::warn!(
            "token catalog has no {QUOTE_SYMBOL} — leaving exchange markets untouched"
        );
        return Ok(());
    }
    let plan = plan_markets(&exchange.markets, token_info)?;
    for sym in &plan.dropped {
        tracing::info!(market = %sym, "exchange market token left the catalog; dropping");
    }
    if plan.create.is_empty() {
        tracing::info!(kept = plan.keep.len(), "exchange markets already current");
        exchange.markets = plan.keep;
        return Ok(());
    }

    let package = ObjectID::from_hex_literal(&exchange.package_id)
        .context("parsing exchange package_id")?;
    let admin_cap = ObjectID::from_hex_literal(&exchange.admin_cap_id)
        .context("parsing exchange admin_cap_id")?;
    let quote_type = &token_info[QUOTE_SYMBOL].coin_type;
    let quote_tag =
        TypeTag::from_str(quote_type).with_context(|| format!("parsing {QUOTE_SYMBOL} type"))?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let admin = pt.obj(
        client
            .owned_object_arg(admin_cap)
            .await
            .context("fetching exchange AdminCap")?,
    )?;
    for m in &plan.create {
        let base_tag = TypeTag::from_str(&m.base_type)
            .with_context(|| format!("parsing base type for {}", m.symbol))?;
        let tick = pt.pure(m.tick_size)?;
        let min = pt.pure(m.min_size)?;
        let fee = pt.pure(m.fee_bps)?;
        pt.programmable_move_call(
            package,
            Identifier::new("registry")?,
            Identifier::new("create_market")?,
            vec![base_tag, quote_tag.clone()],
            vec![admin, tick, min, fee],
        );
        tracing::info!(market = %m.symbol, tick = m.tick_size, min = m.min_size, "creating exchange market");
    }
    let resp =
        sui_tx::tx::submit_ptb(client, signer, pt, gas_budget, "exchange create markets").await?;

    // Each call shared one SettlementRegistry<Base, Quote>; match created
    // objects back to plan entries by canonical Base type param.
    let mut by_base: BTreeMap<String, ObjectID> = BTreeMap::new();
    for c in created_objects(&resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&c.object_type) else { continue };
        if tag.module.as_str() != "registry" || tag.name.as_str() != "SettlementRegistry" {
            continue;
        }
        if let Some(base) = tag.type_params.first() {
            by_base.insert(base.to_canonical_string(true), c.object_id);
        }
    }
    let mut markets = plan.keep;
    for m in plan.create {
        let registry_id = by_base
            .get(&canonical(&m.base_type)?)
            .ok_or_else(|| anyhow!("no SettlementRegistry created for {}", m.symbol))?;
        markets.insert(
            m.symbol,
            ExchangeMarketRecord {
                registry_id: registry_id.to_string(),
                base: m.base_type,
                quote: quote_type.clone(),
                tick_size: m.tick_size,
                min_size: m.min_size,
                lot_size: m.lot_size,
                fee_bps: m.fee_bps,
            },
        );
    }
    tracing::info!(total = markets.len(), "exchange markets recorded");
    exchange.markets = markets;
    Ok(())
}

/// Seed the exchange ingress `Whitelist` (guarded launch): one PTB calling
/// `whitelist::add_member` for the deployer + configured members, deduped.
/// Mirrors the core ProtocolConfig seeding in the activation PTB — the two
/// lists are one logical list and must start with the same cohort.
pub async fn seed_whitelist(
    client: &ChainClient,
    signer: &Signer,
    exchange: &ExchangeRecord,
    ingress_members: &[SuiAddress],
    gas_budget: u64,
) -> Result<()> {
    let whitelist = ObjectID::from_hex_literal(
        exchange
            .whitelist_id
            .as_deref()
            .ok_or_else(|| anyhow!("exchange record has no whitelistId"))?,
    )
    .context("parsing exchange whitelistId")?;
    let package = ObjectID::from_hex_literal(&exchange.package_id)
        .context("parsing exchange package_id")?;
    let admin_cap = ObjectID::from_hex_literal(&exchange.admin_cap_id)
        .context("parsing exchange admin_cap_id")?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let admin = pt.obj(
        client
            .owned_object_arg(admin_cap)
            .await
            .context("fetching exchange AdminCap")?,
    )?;
    let wl = pt.obj(
        client
            .shared_object_arg(whitelist, /* mutable */ true)
            .await
            .context("fetching exchange Whitelist")?,
    )?;
    let mut members = vec![signer.address];
    for m in ingress_members {
        if !members.contains(m) {
            members.push(*m);
        }
    }
    for member in &members {
        let addr = pt.pure(*member)?;
        pt.programmable_move_call(
            package,
            Identifier::new("whitelist")?,
            Identifier::new("add_member")?,
            vec![],
            vec![admin, wl, addr],
        );
    }
    let resp =
        sui_tx::tx::submit_ptb(client, signer, pt, gas_budget, "exchange whitelist seed").await?;
    tracing::info!(
        members = members.len(),
        digest = %sui_tx::tx::tx_digest(&resp),
        "exchange whitelist seeded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(coin_type: &str, decimals: u8) -> TokenSpec {
        TokenSpec {
            coin_type: coin_type.into(),
            decimals,
            pyth_feed_id: None,
            switchboard_feed_id: None,
        }
    }

    fn catalog() -> BTreeMap<String, TokenSpec> {
        BTreeMap::from([
            ("TBTC".into(), spec("0xaa::tbtc::TBTC", 8)),
            ("TSUI".into(), spec("0xaa::tsui::TSUI", 9)),
            ("TUSDC".into(), spec("0xaa::tusdc::TUSDC", 6)),
        ])
    }

    fn record(base: &str, quote: &str) -> ExchangeMarketRecord {
        ExchangeMarketRecord {
            registry_id: "0x1".into(),
            base: base.into(),
            quote: quote.into(),
            tick_size: 1_000,
            min_size: 100_000,
            lot_size: 100_000_000,
            fee_bps: 10,
        }
    }

    #[test]
    fn fresh_catalog_creates_every_pair() {
        let plan = plan_markets(&BTreeMap::new(), &catalog()).unwrap();
        assert!(plan.keep.is_empty());
        assert_eq!(
            plan.create.iter().map(|m| m.symbol.as_str()).collect::<Vec<_>>(),
            vec!["TBTC/TUSDC", "TSUI/TUSDC"]
        );
        let tbtc = &plan.create[0];
        assert_eq!(tbtc.lot_size, 100_000_000);
        assert_eq!(tbtc.min_size, 100_000);
        assert_eq!(tbtc.tick_size, 1_000);
    }

    #[test]
    fn matching_market_is_kept_across_0x_forms() {
        // Same address, non-canonical (short) form: must still match.
        let prev = BTreeMap::from([(
            "TBTC/TUSDC".to_string(),
            record("0xaa::tbtc::TBTC", "0xaa::tusdc::TUSDC"),
        )]);
        let plan = plan_markets(&prev, &catalog()).unwrap();
        assert!(plan.keep.contains_key("TBTC/TUSDC"));
        assert_eq!(plan.create.iter().map(|m| m.symbol.as_str()).collect::<Vec<_>>(), vec![
            "TSUI/TUSDC"
        ]);
        assert!(plan.dropped.is_empty());
    }

    #[test]
    fn stale_types_are_recreated_and_dead_markets_dropped() {
        let prev = BTreeMap::from([
            // Old token package — stale, recreate.
            ("TBTC/TUSDC".to_string(), record("0xbb::tbtc::TBTC", "0xbb::tusdc::TUSDC")),
            // Token no longer in the catalog — drop.
            ("TWAL/TUSDC".to_string(), record("0xbb::twal::TWAL", "0xbb::tusdc::TUSDC")),
        ]);
        let plan = plan_markets(&prev, &catalog()).unwrap();
        assert!(plan.keep.is_empty());
        assert!(plan.create.iter().any(|m| m.symbol == "TBTC/TUSDC"));
        assert_eq!(plan.dropped, vec!["TWAL/TUSDC".to_string()]);
    }

    #[test]
    fn missing_tusdc_is_an_error() {
        let mut cat = catalog();
        cat.remove("TUSDC");
        assert!(plan_markets(&BTreeMap::new(), &cat).is_err());
    }
}
