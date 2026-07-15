//! On-chain proceeds-swap bidder (vault-implementation-guide doc 05 §3.1).
//!
//! The inverse of [`crate::onchain_rfq`]: the vault sells its settlement
//! proceeds for underlying through a generic `Auction<S, U>` (settlement
//! escrowed, underlying bids), and this loop is the buy side — it posts
//! *underlying* and wins *settlement*. Higher underlying wins, so the
//! auction is ascending in underlying exactly like the call RFQ is
//! ascending in premium; only the coins are flipped.
//!
//! Self-contained discovery: there is no api-service endpoint for swap
//! auctions, so the bot walks the auction package's `AuctionCreated`
//! events and reads each object (the same stateless pattern the keeper
//! uses). A proceeds swap is recognized by its legs: coupled, escrow ==
//! our settlement asset, bid != settlement (that excludes put RFQs, which
//! are `Auction<S, S>`); the object's generic params then give the exact
//! (S, U) pair, which the bot matches to a market it makes.
//!
//! Pricing: the bot values the escrowed settlement at the Pyth cross and
//! will give at most `fair_underlying × (1 − bid_margin_bps)` for it; it
//! bids the minimum underlying needed to win (the reserve, or a strict
//! increment over the standing best), so its edge is the gap between that
//! floor and fair value. `bid_margin_bps` must stay below the vault's swap
//! slippage band or the cap falls under the reserve and the bot never bids.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::StructTag;
use serde::Deserialize;
use serde_json::Value;
use sui_json_rpc_types::{EventFilter, SuiObjectDataOptions, SuiParsedData};
use sui_types::base_types::{MoveObjectType, ObjectID, ObjectType, SuiAddress};

use protocol_types::asset::canonicalize_move_type;
use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::auction::{bid, AuctionBidParams, AuctionTypes};

use api_service_client::ApiServiceClient;
use crate::onchain_rfq::{
    decide_bid, parse_auction_view, AuctionView, BidderMarket, OnchainRfqConfig,
};
use crate::pricing::{compute_spot_from_cache, Staleness};

const BPS_DENOM: u128 = 10_000;

fn default_lookback_secs() -> u64 {
    24 * 3_600
}
fn default_bid_margin_bps() -> u64 {
    20
}

/// `[onchain_swap]` config. Reuses the call bidder's knobs (poll, escrow
/// cap — now in *underlying* — rebid, deadline lead, gas) and adds the
/// margin below fair value the bot keeps.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OnchainSwapConfig {
    /// Shared auction-bidder knobs (escrow cap is underlying units here).
    pub bidder: OnchainRfqConfig,
    /// Most the bot pays = `fair_underlying × (1 − bid_margin_bps/10⁴)`.
    pub bid_margin_bps: u64,
    /// How far back to scan `AuctionCreated` events (swap auctions are
    /// short-lived; a day is generous).
    pub lookback_secs: u64,
}

impl Default for OnchainSwapConfig {
    fn default() -> Self {
        Self {
            bidder: OnchainRfqConfig::default(),
            bid_margin_bps: default_bid_margin_bps(),
            lookback_secs: default_lookback_secs(),
        }
    }
}

/// The most underlying the bot will give for `amount_s` settlement: the
/// Pyth value of the settlement, less the bot's margin. `spot_scaled` is
/// settlement-smallest-units per underlying-smallest-unit (the same form
/// `compute_spot_from_cache` returns), so `amount_s / spot` is underlying.
pub fn max_underlying_bid(amount_s: u64, spot_scaled: f64, bid_margin_bps: u64) -> Option<u64> {
    if !(spot_scaled > 0.0) {
        return None;
    }
    let fair = amount_s as f64 / spot_scaled;
    let capped = fair * (1.0 - bid_margin_bps as f64 / BPS_DENOM as f64);
    if !capped.is_finite() || capped < 1.0 {
        return None;
    }
    Some(capped.floor() as u64)
}

// ── chain reads ────────────────────────────────────────────────────────

/// One discovered open swap auction: its view plus the (U, S) coin types
/// pulled from the object's generic params.
struct OpenSwap {
    swap_id: ObjectID,
    view: AuctionView,
    underlying_type: String,
    settlement_type: String,
}

/// Read a proceeds-swap `Auction<S, U>` object: its view + the (U, S)
/// coin types. `None` if it no longer exists (settled mid-poll).
async fn fetch_open_swap(
    client: &sui_sdk::SuiClient,
    swap_id: ObjectID,
) -> Result<Option<OpenSwap>> {
    let resp = client
        .read_api()
        .get_object_with_options(
            swap_id,
            SuiObjectDataOptions::new().with_content().with_type(),
        )
        .await
        .with_context(|| format!("reading swap auction {swap_id}"))?;
    let Some(data) = resp.data else {
        return Ok(None);
    };
    let (settlement_type, underlying_type) = match &data.type_ {
        Some(ObjectType::Struct(tag)) => swap_type_params(tag)?,
        other => return Err(anyhow!("swap {swap_id} has unexpected type {other:?}")),
    };
    match data.content {
        Some(SuiParsedData::MoveObject(obj)) => Ok(Some(OpenSwap {
            swap_id,
            view: parse_auction_view(&obj.fields.to_json_value())?,
            underlying_type,
            settlement_type,
        })),
        other => Err(anyhow!("swap {swap_id} has unexpected content: {other:?}")),
    }
}

/// `(S, U)` coin-type strings from a proceeds-swap `Auction<S, U>` object
/// type (escrow = settlement, bids = underlying).
fn swap_type_params(ty: &MoveObjectType) -> Result<(String, String)> {
    let params = ty.type_params();
    if params.len() != 2 {
        return Err(anyhow!("Auction expected 2 type params, got {}", params.len()));
    }
    Ok((
        params[0].to_canonical_string(/* with_prefix */ true),
        params[1].to_canonical_string(true),
    ))
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

/// Is this `AuctionCreated` event a vault proceeds swap for our
/// settlement asset? Coupled, escrow == settlement, bid != settlement
/// (a put RFQ escrows AND bids settlement; a call RFQ escrows
/// underlying — neither matches).
fn is_proceeds_swap_event(parsed: &Value, settlement_canonical: &str) -> bool {
    if parsed.get("coupled").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }
    let (Some(escrow), Some(bid)) = (
        parsed.get("escrow_type").and_then(type_name_str),
        parsed.get("bid_type").and_then(type_name_str),
    ) else {
        return false;
    };
    let escrow = canonicalize_move_type(escrow);
    let bid = canonicalize_move_type(bid);
    escrow == settlement_canonical && bid != settlement_canonical
}

/// Walk the auction package's `AuctionCreated` events newest-first down
/// to `cutoff_ms`, collecting candidate proceeds-swap auctions (any vault
/// — the bot filters by market after reading the object).
async fn discover_swap_ids(
    client: &sui_sdk::SuiClient,
    package: ObjectID,
    settlement_coin_type: &str,
    cutoff_ms: u64,
) -> Result<Vec<(ObjectID, String)>> {
    let filter = EventFilter::MoveEventType(StructTag {
        address: package.into(),
        module: Identifier::new("events").unwrap(),
        name: Identifier::new("AuctionCreated").unwrap(),
        type_params: vec![],
    });
    let settlement = canonicalize_move_type(settlement_coin_type);
    let mut ids = Vec::new();
    let mut cursor = None;
    'pages: loop {
        let page = client
            .event_api()
            .query_events(filter.clone(), cursor, Some(100), true)
            .await
            .context("querying AuctionCreated events")?;
        for ev in &page.data {
            if ev.timestamp_ms.is_some_and(|t| t < cutoff_ms) {
                break 'pages;
            }
            if !is_proceeds_swap_event(&ev.parsed_json, &settlement) {
                continue;
            }
            if let Some(id) = ev.parsed_json.get("auction_id").and_then(|v| v.as_str()) {
                let origin = ev
                    .parsed_json
                    .get("origin")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                ids.push((id.parse().context("parsing auction_id")?, origin));
            }
        }
        if !page.has_next_page {
            break;
        }
        cursor = page.next_cursor;
    }
    Ok(ids)
}

// ── the bidder loop ────────────────────────────────────────────────────

pub struct SwapBidderParams {
    pub cfg: OnchainSwapConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    pub package: ObjectID,
    pub api_url: String,
    pub price_cache: PriceCache,
    pub markets: Vec<BidderMarket>,
    pub settlement_feed: PriceFeedId,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    pub staleness: Staleness,
}

pub fn spawn_bidder(p: SwapBidderParams) {
    tokio::spawn(async move {
        if let Err(e) = run(p).await {
            tracing::error!(error = %format!("{e:#}"), "onchain swap bidder exited");
        }
    });
}

async fn run(p: SwapBidderParams) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(&p.api_url);
    let our_address = wrap.signer.address;
    tracing::info!(
        address = %our_address,
        poll_secs = p.cfg.bidder.poll_secs,
        margin_bps = p.cfg.bid_margin_bps,
        escrow_cap = p.cfg.bidder.max_concurrent_escrow,
        "onchain swap bidder starting"
    );
    let poll = Duration::from_secs(p.cfg.bidder.poll_secs.max(1));
    loop {
        if let Err(e) = tick(&p, &wrap, &api, our_address).await {
            tracing::warn!(error = %format!("{e:#}"), "swap bidder tick errored");
        }
        tokio::time::sleep(poll).await;
    }
}

async fn tick(
    p: &SwapBidderParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    our_address: SuiAddress,
) -> Result<()> {
    let now = now_ms();
    let cutoff = now.saturating_sub(p.cfg.lookback_secs.saturating_mul(1_000));
    let candidates =
        discover_swap_ids(&wrap.client, p.package, &p.settlement_coin_type, cutoff).await?;
    // Hard cutover: ignore proceeds-swaps from a paused (decommissioned)
    // vault, keyed off the AuctionCreated `origin` (the vault id).
    let paused = api
        .paused_vault_ids()
        .await
        .context("polling paused vaults")?;

    let mut opens: Vec<OpenSwap> = Vec::new();
    for (id, origin) in candidates {
        if let Ok(vault) = protocol_types::ids::ObjectId::from_hex(&origin) {
            if paused.contains(&vault) {
                continue;
            }
        }
        match fetch_open_swap(&wrap.client, id).await {
            Ok(Some(o)) => opens.push(o),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "swap read failed"),
        }
    }
    // Underlying we are already best-bidding, summed per market.
    let mut locked: u64 = opens
        .iter()
        .filter(|o| o.view.best_bidder == Some(our_address))
        .filter_map(|o| o.view.best_premium)
        .sum();

    for o in opens {
        let Some(market) = p.markets.iter().find(|m| {
            crate::pricing::serves_pair(
                &o.underlying_type,
                &o.settlement_type,
                &m.coin_type,
                &p.settlement_coin_type,
            )
        }) else {
            continue;
        };

        let spot_scaled = match compute_spot_from_cache(
            &p.price_cache,
            market.feed,
            p.settlement_feed,
            market.decimals,
            p.settlement_decimals,
            p.staleness,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(reason = e.as_str(), "skipping swap: spot unavailable");
                continue;
            }
        };
        let Some(max_bid) = max_underlying_bid(o.view.amount, spot_scaled, p.cfg.bid_margin_bps)
        else {
            continue;
        };

        let underlying = match decide_bid(&p.cfg.bidder, &o.view, max_bid, our_address, locked, now)
        {
            Ok(b) => b,
            Err(reason) => {
                tracing::debug!(swap = %o.swap_id, ?reason, max_bid, "no swap bid");
                continue;
            }
        };

        let Some(funding) =
            underlying_funding_coin(wrap, &o.underlying_type, underlying).await?
        else {
            tracing::warn!(underlying, "no underlying coin large enough to fund the swap bid");
            continue;
        };
        // A proceeds swap is `Auction<Settlement, Underlying>`: the vault's
        // settlement escrowed, our underlying bid.
        let params = AuctionBidParams {
            package: p.package,
            types: AuctionTypes {
                escrow_type: &o.settlement_type,
                bid_type: &o.underlying_type,
            },
            auction_id: o.swap_id,
            funding_coin: funding,
            amount: underlying,
            token_recipient: our_address,
            gas_budget: p.cfg.bidder.gas_budget,
        };
        match bid(&wrap.client, &wrap.signer, &params).await {
            Ok(resp) => {
                locked = locked.saturating_add(underlying);
                tracing::info!(
                    swap = %o.swap_id, underlying, max_bid, locked, digest = %resp.digest,
                    "swap bid placed"
                );
            }
            Err(e) => {
                // Outbid between read and submit aborts with auction
                // bid_too_low — replanned next poll, no alert.
                if crate::is_benign_bid_loss(&e) {
                    tracing::warn!(swap = %o.swap_id, underlying, error = %format!("{e:#}"), "swap bid failed (outbid)");
                } else {
                    tracing::error!(
                        alert_id = "tx-failed-mm-bot-swap",
                        swap = %o.swap_id,
                        underlying,
                        error = %format!("{e:#}"),
                        "swap bid tx failed"
                    );
                }
            }
        }
    }
    Ok(())
}

/// The wallet's largest underlying coin with at least `amount` on it.
async fn underlying_funding_coin(
    wrap: &SuiClientWrapper,
    underlying_coin_type: &str,
    amount: u64,
) -> Result<Option<ObjectID>> {
    let coins = wrap
        .client
        .coin_read_api()
        .get_coins(wrap.signer.address, Some(underlying_coin_type.to_string()), None, None)
        .await
        .context("listing underlying coins")?;
    Ok(coins
        .data
        .into_iter()
        .filter(|c| c.balance >= amount)
        .max_by_key(|c| c.balance)
        .map(|c| c.coin_object_id))
}

// Re-export so main.rs can build `BidderMarket`s without importing both.
pub use crate::onchain_rfq::BidderMarket as Market;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onchain_rfq::NoBid;
    use serde_json::json;

    #[test]
    fn max_bid_is_fair_value_less_margin() {
        // 10_030_000 settlement at spot 47_619 (settlement per underlying)
        // ⇒ fair = 210.6 underlying; 20 bps margin ⇒ floor(210.6 × 0.998).
        let m = max_underlying_bid(10_030_000, 47_619.0, 20).unwrap();
        assert_eq!(m, 210); // 210.6 × 0.998 = 210.18 → floor 210
        // Dust rounds to nothing ⇒ no bid.
        assert_eq!(max_underlying_bid(1, 47_619.0, 20), None);
        assert_eq!(max_underlying_bid(10_030_000, 0.0, 20), None);
    }

    #[test]
    fn recognizes_proceeds_swap_events() {
        // Chain TypeNames carry no 0x prefix; ours are canonical — the
        // comparison must bridge that (move-type-normalization.md).
        let settlement = canonicalize_move_type("0xs::tusdc::TUSDC");
        let swap = json!({
            "auction_id": "0xaa",
            "origin": "0xf0",
            "escrow_type": {"name": "s::tusdc::TUSDC"},
            "bid_type": {"name": "u::tbtc::TBTC"},
            "coupled": true,
        });
        assert!(is_proceeds_swap_event(&swap, &settlement));

        // A call RFQ slice escrows underlying — not a swap.
        let call_rfq = json!({
            "escrow_type": {"name": "u::tbtc::TBTC"},
            "bid_type": {"name": "s::tusdc::TUSDC"},
            "coupled": true,
        });
        assert!(!is_proceeds_swap_event(&call_rfq, &settlement));

        // A put RFQ escrows AND bids settlement — not a swap.
        let put_rfq = json!({
            "escrow_type": {"name": "s::tusdc::TUSDC"},
            "bid_type": {"name": "s::tusdc::TUSDC"},
            "coupled": true,
        });
        assert!(!is_proceeds_swap_event(&put_rfq, &settlement));

        // Uncoupled auctions are someone else's business.
        let uncoupled = json!({
            "escrow_type": {"name": "s::tusdc::TUSDC"},
            "bid_type": {"name": "u::tbtc::TBTC"},
            "coupled": false,
        });
        assert!(!is_proceeds_swap_event(&uncoupled, &settlement));

        // TypeNames rendered as bare strings parse too.
        let bare = json!({
            "escrow_type": "s::tusdc::TUSDC",
            "bid_type": "u::tbtc::TBTC",
            "coupled": true,
        });
        assert!(is_proceeds_swap_event(&bare, &settlement));
    }

    #[test]
    fn bidder_reuses_call_floor_rules() {
        // A no-bid auction with reserve 208, our cap 211 ⇒ bid the reserve.
        let view = AuctionView {
            amount: 10_030_000,
            reserve_premium: 208,
            deadline_ms: 1_000_000,
            min_increment_bps: 500,
            best_premium: None,
            best_bidder: None,
        };
        let us = SuiAddress::from_bytes([1u8; 32]).unwrap();
        let cfg = OnchainSwapConfig::default();
        assert_eq!(decide_bid(&cfg.bidder, &view, 211, us, 0, 100_000), Ok(208));
        // Cap below reserve ⇒ priced out.
        assert!(matches!(
            decide_bid(&cfg.bidder, &view, 207, us, 0, 100_000),
            Err(NoBid::FloorAboveMax { .. })
        ));
    }
}
