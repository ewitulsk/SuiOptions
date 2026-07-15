//! On-chain RFQ bidder (vault-implementation-guide doc 05 §3).
//!
//! The vault sells its weekly call slices through shared generic
//! `Auction<U, S>` objects (the `auction` package); this loop is the buy
//! side. It runs beside the WS quoting flow
//! and shares the same pricing brain (`pricing::price_rfq` with the
//! bid-markdown spread — the bot is *buying* the option, so its max bid
//! is the marked-down mid times the slice size).
//!
//! Per poll:
//!   1. **Discover** open auctions from api-service `GET /rfqs?status=open`
//!      (the indexer-fed fallback path; an event subscription can replace
//!      the poll later without touching the decision logic).
//!   2. **Read** each auction object from chain — best bid, deadline
//!      (anti-snipe moves it), and min-increment must be live, not
//!      indexer-lagged.
//!   3. **Price** the bucket via the matching market's spot/vol, then
//!      [`decide_bid`] (pure): respect the reserve and the on-chain
//!      min-increment ceiling rule, apply the initial-bid policy, rebid
//!      only when outbid and still under our max.
//!   4. **Escrow accounting**: every live best bid locks real settlement;
//!      `max_concurrent_escrow` caps the total across auctions. Outbid
//!      refunds release automatically on-chain (the auction returns the
//!      previous escrow when a higher bid lands).
//!
//! Bids fund from the wallet's settlement coins (doc 05 §3.4 — simpler
//! than routing through the Account). Settling won auctions is left to
//! the keeper, which cranks `settle_rfq` permissionlessly anyway.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::Value;
use sui_json_rpc_types::{SuiObjectDataOptions, SuiParsedData};
use sui_types::base_types::{ObjectID, SuiAddress};

use api_service_client::{ApiServiceClient, OpenRfq};
use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::auction::{bid, AuctionBidParams, AuctionTypes};

use pricing::smile::Smile;

use crate::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, serves_pair, PriceDecision, PricingConfig,
    RfqPricingInputs, Staleness,
};

const BPS_DENOM: u128 = 10_000;

fn default_poll_secs() -> u64 {
    10
}
fn default_shade_bps() -> u64 {
    300
}
fn default_rebid() -> bool {
    true
}
fn default_max_concurrent_escrow() -> u64 {
    5_000_000_000
}
fn default_min_deadline_lead_ms() -> u64 {
    30_000
}
fn default_gas_budget() -> u64 {
    200_000_000
}

/// First-bid sizing policy (doc 05 §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialBidPolicy {
    /// Open at exactly the floor (reserve) and climb only when contested.
    ReservePlus,
    /// Open at `max_bid × (1 − shade_bps/10⁴)` — looks competitive
    /// without giving the whole edge away.
    Shaded,
    /// Open at the full max bid.
    Max,
}

/// `[onchain_rfq]` section of the bot config. Disabled by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OnchainRfqConfig {
    pub enabled: bool,
    pub poll_secs: u64,
    pub initial_bid: InitialBidPolicy,
    pub shade_bps: u64,
    /// Top back up (to the min-increment floor) when outbid, while the
    /// required bid stays under our max.
    pub rebid: bool,
    /// Cap on total settlement locked across live best bids.
    pub max_concurrent_escrow: u64,
    /// Don't open/raise inside this window before the deadline — a bid we
    /// can't be outbid-refunded from in time is a bad trade vs gas.
    pub min_deadline_lead_ms: u64,
    pub gas_budget: u64,
}

impl Default for OnchainRfqConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_secs: default_poll_secs(),
            initial_bid: InitialBidPolicy::ReservePlus,
            shade_bps: default_shade_bps(),
            rebid: default_rebid(),
            max_concurrent_escrow: default_max_concurrent_escrow(),
            min_deadline_lead_ms: default_min_deadline_lead_ms(),
            gas_budget: default_gas_budget(),
        }
    }
}

/// Live auction state, read from the shared generic `Auction<E, B>`
/// object (the indexer view may lag bids; the chain can't).
#[derive(Debug, Clone, PartialEq)]
pub struct AuctionView {
    pub amount: u64,
    /// The on-chain `reserve_bid` (premium for RFQs, underlying for swaps).
    pub reserve_premium: u64,
    pub deadline_ms: u64,
    pub min_increment_bps: u64,
    /// Current best escrow; `None` when no bid yet.
    pub best_premium: Option<u64>,
    pub best_bidder: Option<SuiAddress>,
}

/// Why [`decide_bid`] passed on an auction (for logging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoBid {
    Winning,
    DeadlineTooClose,
    FloorAboveMax { floor: u64, max_bid: u64 },
    RebidDisabled,
    EscrowCapped { needed: u64, headroom: u64 },
}

/// The pure bid decision. `locked_escrow` is the settlement already
/// committed across other auctions (live best bids + bids submitted this
/// tick); `max_bid` is the pricing brain's marked-down fair value for the
/// whole slice.
pub fn decide_bid(
    cfg: &OnchainRfqConfig,
    auction: &AuctionView,
    max_bid: u64,
    our_address: SuiAddress,
    locked_escrow: u64,
    now_ms: u64,
) -> Result<u64, NoBid> {
    if auction.best_bidder == Some(our_address) {
        return Err(NoBid::Winning);
    }
    if now_ms.saturating_add(cfg.min_deadline_lead_ms) >= auction.deadline_ms {
        return Err(NoBid::DeadlineTooClose);
    }
    let contested = auction.best_bidder.is_some();
    // Mirror rfq::bid's floor exactly: ceil-increment over the current
    // best (strictly greater), never below the reserve.
    let floor = match auction.best_premium {
        Some(prev) if contested => {
            let with_increment = ((prev as u128) * (BPS_DENOM + auction.min_increment_bps as u128)
                + BPS_DENOM
                - 1)
                / BPS_DENOM;
            (with_increment as u64).max(auction.reserve_premium).max(prev + 1)
        }
        _ => auction.reserve_premium,
    };
    if floor > max_bid {
        return Err(NoBid::FloorAboveMax { floor, max_bid });
    }
    let bid = if contested {
        if !cfg.rebid {
            return Err(NoBid::RebidDisabled);
        }
        floor
    } else {
        match cfg.initial_bid {
            InitialBidPolicy::ReservePlus => floor,
            InitialBidPolicy::Max => max_bid,
            InitialBidPolicy::Shaded => {
                let shaded = ((max_bid as u128) * (BPS_DENOM - cfg.shade_bps.min(10_000) as u128)
                    / BPS_DENOM) as u64;
                shaded.max(floor)
            }
        }
    };
    let headroom = cfg.max_concurrent_escrow.saturating_sub(locked_escrow);
    if bid > headroom {
        return Err(NoBid::EscrowCapped { needed: bid, headroom });
    }
    Ok(bid)
}

// ── chain reads ────────────────────────────────────────────────────────

fn field<'a>(v: &'a Value, name: &str) -> Result<&'a Value> {
    v.get(name).ok_or_else(|| anyhow!("missing field {name}"))
}

fn as_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("non-u64 {n}")),
        Value::String(s) => s.parse().with_context(|| format!("parsing u64 {s:?}")),
        other => Err(anyhow!("expected u64, got {other}")),
    }
}

/// Parse a generic `Auction`'s parsed-JSON fields (RPC conventions: u64
/// as string, `Balance` unwrapped to its value, `Option` as null/inner).
pub fn parse_auction_view(fields: &Value) -> Result<AuctionView> {
    let best_bidder = match field(fields, "best_bidder")? {
        Value::Null => None,
        Value::String(s) => Some(s.parse().with_context(|| format!("best_bidder {s:?}"))?),
        other => return Err(anyhow!("unexpected best_bidder {other}")),
    };
    let escrow = as_u64(field(fields, "bid_escrow")?).context("field bid_escrow")?;
    Ok(AuctionView {
        amount: as_u64(field(fields, "amount")?).context("field amount")?,
        reserve_premium: as_u64(field(fields, "reserve_bid")?)
            .context("field reserve_bid")?,
        deadline_ms: as_u64(field(fields, "deadline_ms")?).context("field deadline_ms")?,
        min_increment_bps: as_u64(field(fields, "min_increment_bps")?)
            .context("field min_increment_bps")?,
        best_premium: best_bidder.is_some().then_some(escrow),
        best_bidder,
    })
}

pub(crate) async fn fetch_auction_view(
    client: &sui_sdk::SuiClient,
    rfq_id: ObjectID,
) -> Result<Option<AuctionView>> {
    let resp = client
        .read_api()
        .get_object_with_options(rfq_id, SuiObjectDataOptions::new().with_content())
        .await
        .with_context(|| format!("reading auction {rfq_id}"))?;
    let Some(data) = resp.data else {
        return Ok(None); // settled mid-poll: the object is deleted
    };
    match data.content {
        Some(SuiParsedData::MoveObject(obj)) => {
            Ok(Some(parse_auction_view(&obj.fields.to_json_value())?))
        }
        other => Err(anyhow!("auction {rfq_id} has unexpected content: {other:?}")),
    }
}

// ── the bidder loop ────────────────────────────────────────────────────

/// One underlying the bidder serves — same shape as the WS/DeepBook
/// markets, sharing the vol buffer.
pub struct BidderMarket {
    pub symbol: String,
    pub coin_type: String,
    pub feed: PriceFeedId,
    pub decimals: u8,
    pub vol_buf: Arc<RwLock<RollingVolBuffer>>,
    /// Long-window buffer; quoted sigma is max(short, long).
    pub vol_buf_long: Arc<RwLock<RollingVolBuffer>>,
    /// Sigma used while `vol_buf` is cold (per-symbol config override).
    pub fallback_vol: f64,
    /// Vol smile for this underlying (per-symbol config override).
    pub smile: Smile,
}

pub struct BidderParams {
    pub cfg: OnchainRfqConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    /// Generic `auction` package (for the `auction::bid` call).
    pub package: ObjectID,
    pub api_url: String,
    pub price_cache: PriceCache,
    pub markets: Vec<BidderMarket>,
    pub settlement_feed: PriceFeedId,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    pub pricing: PricingConfig,
    pub staleness: Staleness,
}

pub fn spawn_bidder(p: BidderParams) {
    tokio::spawn(async move {
        if let Err(e) = run(p).await {
            tracing::error!(error = %format!("{e:#}"), "onchain rfq bidder exited");
        }
    });
}

async fn run(p: BidderParams) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(&p.api_url);
    let our_address = wrap.signer.address;
    tracing::info!(
        address = %our_address,
        poll_secs = p.cfg.poll_secs,
        policy = ?p.cfg.initial_bid,
        escrow_cap = p.cfg.max_concurrent_escrow,
        "onchain rfq bidder starting"
    );
    let poll = Duration::from_secs(p.cfg.poll_secs.max(1));
    loop {
        if let Err(e) = tick(&p, &wrap, &api, our_address).await {
            tracing::warn!(error = %format!("{e:#}"), "bidder tick errored");
        }
        tokio::time::sleep(poll).await;
    }
}

async fn tick(
    p: &BidderParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    our_address: SuiAddress,
) -> Result<()> {
    let open = api.open_rfqs().await.context("polling open rfqs")?;
    if open.is_empty() {
        return Ok(());
    }
    // Hard cutover: drop auctions originating from a paused vault before
    // any chain read or bid. Uncoupled (seller-origin) auctions never
    // match a vault id, so they're unaffected.
    let paused = api
        .paused_vault_ids()
        .await
        .context("polling paused vaults")?;
    let now = now_ms();

    // Live views first: locked escrow must be computed across ALL our
    // standing best bids before any new bid is sized.
    let mut views: Vec<(OpenRfq, AuctionView)> = Vec::with_capacity(open.len());
    for rfq in open {
        if let Ok(vault) = protocol_types::ids::ObjectId::from_hex(&rfq.origin) {
            if paused.contains(&vault) {
                continue;
            }
        }
        match fetch_auction_view(&wrap.client, sui_object_id(rfq.rfq_id)?).await {
            Ok(Some(view)) => views.push((rfq, view)),
            Ok(None) => {} // settled since the poll
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "auction read failed"),
        }
    }
    let mut locked: u64 = views
        .iter()
        .filter(|(_, v)| v.best_bidder == Some(our_address))
        .filter_map(|(_, v)| v.best_premium)
        .sum();

    for (rfq, view) in views {
        let Some(bucket) = api.bucket_pricing(rfq.bucket_id).await? else {
            continue;
        };
        let Some(market) = p.markets.iter().find(|m| {
            serves_pair(
                &bucket.asset_coin_type,
                &bucket.settlement_coin_type,
                &m.coin_type,
                &p.settlement_coin_type,
            )
        }) else {
            continue; // not a pair we make markets in
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
                tracing::debug!(reason = e.as_str(), "skipping auction: spot unavailable");
                continue;
            }
        };
        let sigma = resolve_sigma(
            market.vol_buf.read().current_annualized(),
            market.vol_buf_long.read().current_annualized(),
            market.fallback_vol,
        );
        let inputs = RfqPricingInputs {
            write_amount: view.amount,
            side: Side::Writer, // retail/vault is the writer; we buy
            strike: bucket.strike,
            strike_scale: bucket.strike_scale,
            expiry_ms: bucket.expiry_ms,
            is_put: false, // the call auction bidder; puts go through onchain_put_rfq
        };
        let cfg_m = PricingConfig { smile: market.smile, ..p.pricing };
        let max_bid = match price_rfq(&cfg_m, &inputs, spot_scaled, sigma, now) {
            PriceDecision::Quote { premium, .. } => premium,
            PriceDecision::Decline { reason } => {
                tracing::debug!(rfq = %rfq.rfq_id.to_hex(), reason, "declined to price");
                continue;
            }
        };

        let premium = match decide_bid(&p.cfg, &view, max_bid, our_address, locked, now) {
            Ok(b) => b,
            Err(reason) => {
                tracing::debug!(rfq = %rfq.rfq_id.to_hex(), ?reason, max_bid, "no bid");
                continue;
            }
        };

        let Some(funding) = settlement_funding_coin(wrap, &p.settlement_coin_type, premium).await?
        else {
            tracing::warn!(premium, "no settlement coin large enough to fund the bid");
            continue;
        };
        // A covered-call RFQ auction is `Auction<Underlying, Settlement>`.
        let params = AuctionBidParams {
            package: p.package,
            types: AuctionTypes {
                escrow_type: &bucket.asset_coin_type,
                bid_type: &bucket.settlement_coin_type,
            },
            auction_id: sui_object_id(rfq.rfq_id)?,
            funding_coin: funding,
            amount: premium,
            token_recipient: our_address,
            gas_budget: p.cfg.gas_budget,
        };
        match bid(&wrap.client, &wrap.signer, &params).await {
            Ok(resp) => {
                locked = locked.saturating_add(premium);
                tracing::info!(
                    rfq = %rfq.rfq_id.to_hex(),
                    premium,
                    max_bid,
                    locked,
                    digest = %resp.digest,
                    "bid placed"
                );
            }
            Err(e) => {
                // A lost race (someone outbid between read and submit)
                // aborts with auction bid_too_low — replanned next poll,
                // no alert.
                if crate::is_benign_bid_loss(&e) {
                    tracing::warn!(rfq = %rfq.rfq_id.to_hex(), premium, error = %format!("{e:#}"), "bid failed (outbid)");
                } else {
                    tracing::error!(
                        alert_id = "tx-failed-mm-bot-rfq",
                        rfq = %rfq.rfq_id.to_hex(),
                        premium,
                        error = %format!("{e:#}"),
                        "rfq bid tx failed"
                    );
                }
            }
        }
    }
    Ok(())
}

/// The wallet's largest settlement coin with at least `premium` on it.
pub(crate) async fn settlement_funding_coin(
    wrap: &SuiClientWrapper,
    settlement_coin_type: &str,
    premium: u64,
) -> Result<Option<ObjectID>> {
    let coins = wrap
        .client
        .coin_read_api()
        .get_coins(
            wrap.signer.address,
            Some(settlement_coin_type.to_string()),
            None,
            None,
        )
        .await
        .context("listing settlement coins")?;
    Ok(coins
        .data
        .into_iter()
        .filter(|c| c.balance >= premium)
        .max_by_key(|c| c.balance)
        .map(|c| c.coin_object_id))
}

pub(crate) fn sui_object_id(id: protocol_types::ids::ObjectId) -> Result<ObjectID> {
    Ok(ObjectID::new(*id.as_bytes()))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn addr(b: u8) -> SuiAddress {
        SuiAddress::from_bytes([b; 32]).unwrap()
    }

    fn cfg() -> OnchainRfqConfig {
        OnchainRfqConfig::default()
    }

    fn auction(best: Option<(u64, u8)>) -> AuctionView {
        AuctionView {
            amount: 250_000_000,
            reserve_premium: 47_619_000,
            deadline_ms: 1_000_000,
            min_increment_bps: 100,
            best_premium: best.map(|(p, _)| p),
            best_bidder: best.map(|(_, b)| addr(b)),
        }
    }

    const NOW: u64 = 100_000; // well before the 1_000_000 deadline

    #[test]
    fn first_bid_follows_policy() {
        let a = auction(None);
        let us = addr(1);
        // reserve_plus opens at the reserve.
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, us, 0, NOW), Ok(47_619_000));
        // max opens at the max bid.
        let mut c = cfg();
        c.initial_bid = InitialBidPolicy::Max;
        assert_eq!(decide_bid(&c, &a, 60_000_000, us, 0, NOW), Ok(60_000_000));
        // shaded opens 3% under max, floored at the reserve.
        c.initial_bid = InitialBidPolicy::Shaded;
        assert_eq!(decide_bid(&c, &a, 60_000_000, us, 0, NOW), Ok(58_200_000));
        assert_eq!(decide_bid(&c, &a, 47_700_000, us, 0, NOW), Ok(47_619_000));
    }

    #[test]
    fn rebid_matches_onchain_ceiling_rule() {
        let us = addr(1);
        let a = auction(Some((50_000_000, 2)));
        // ceil(50_000_000 × 1.01) = 50_500_000.
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, us, 0, NOW), Ok(50_500_000));
        // Odd previous bid: ceiling, not floor. ceil(33 × 1.01) = 34.
        let mut a2 = auction(Some((33, 2)));
        a2.reserve_premium = 1;
        assert_eq!(decide_bid(&cfg(), &a2, 1_000, us, 0, NOW), Ok(34));
        // Zero increment still forces strict improvement.
        let mut a3 = auction(Some((50_000_000, 2)));
        a3.min_increment_bps = 0;
        assert_eq!(decide_bid(&cfg(), &a3, 60_000_000, us, 0, NOW), Ok(50_000_001));
    }

    #[test]
    fn passes_when_winning_capped_or_priced_out() {
        let us = addr(1);
        // Already the best bidder.
        let a = auction(Some((50_000_000, 1)));
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, us, 0, NOW), Err(NoBid::Winning));
        // Floor above our max.
        let a = auction(Some((59_900_000, 2)));
        assert!(matches!(
            decide_bid(&cfg(), &a, 60_000_000, us, 0, NOW),
            Err(NoBid::FloorAboveMax { .. })
        ));
        // Escrow cap: 5e9 default, already 4.96e9 locked.
        let a = auction(None);
        assert!(matches!(
            decide_bid(&cfg(), &a, 60_000_000, us, 4_960_000_000, NOW),
            Err(NoBid::EscrowCapped { .. })
        ));
        // Deadline too close.
        let a = auction(None);
        assert_eq!(
            decide_bid(&cfg(), &a, 60_000_000, us, 0, 980_000),
            Err(NoBid::DeadlineTooClose)
        );
        // Rebid disabled: pass on contested auctions.
        let mut c = cfg();
        c.rebid = false;
        let a = auction(Some((50_000_000, 2)));
        assert_eq!(
            decide_bid(&c, &a, 60_000_000, us, 0, NOW),
            Err(NoBid::RebidDisabled)
        );
    }

    #[test]
    fn parses_auction_object_json() {
        // RPC conventions: u64 as strings, Balance unwrapped, Option null.
        // Fields per `auction::auction::Auction`.
        let v = parse_auction_view(&json!({
            "amount": "250000000",
            "reserve_bid": "47619000",
            "created_ms": "1",
            "deadline_ms": "1000000",
            "snipe_window_ms": "60000",
            "snipe_extension_ms": "60000",
            "max_deadline_ms": "1600000",
            "min_increment_bps": "100",
            "best_bidder": null,
            "best_token_recipient": null,
            "bid_escrow": "0",
            "proceeds_recipient": "0x0",
            "refund_recipient": "0x0",
            "origin": "0x00000000000000000000000000000000000000000000000000000000000000f0",
            "settle_authority": {"name": "abc::vault::VaultAuth"},
        }))
        .unwrap();
        assert_eq!(v.best_premium, None);
        assert_eq!(v.deadline_ms, 1_000_000);
        assert_eq!(v.min_increment_bps, 100);

        let v = parse_auction_view(&json!({
            "amount": "250000000",
            "reserve_bid": "47619000",
            "deadline_ms": "1120000",
            "min_increment_bps": "100",
            "best_bidder": addr(2).to_string(),
            "bid_escrow": "51000000",
        }))
        .unwrap();
        assert_eq!(v.best_premium, Some(51_000_000));
        assert_eq!(v.best_bidder, Some(addr(2)));
    }
}
