//! On-chain RFQ auction channel — the V1 desk's second intake (00-plan
//! Phase 3), reworked from the old `onchain_rfq` / `onchain_put_rfq`
//! bidders into ONE loop covering both call and put auctions.
//!
//! Per poll: discover open auctions from api-service (`/rfqs` +
//! `/put-rfqs`), read each auction object live from chain, price the
//! slice with the SAME V1 bid path as the WS writer flow (model fair at a
//! discounted vol → the max premium the desk pays), then run the old
//! `decide_bid` ladder mechanics (reserve floor, on-chain min-increment
//! ceiling rule, initial-bid policy, escrow cap, benign lost-race
//! classification).
//!
//! Vault-only mandate: the auction's `token_recipient` is the VAULT
//! address, so winner option coins land in vault custody (swept by the
//! keeper as positions). **Accepted gap (documented)**: the bid escrow
//! itself is funded from the bot wallet's settlement float, because the
//! generic `auction::bid` call has no vault-release adapter yet — all
//! OUTPUTS land in the vault; only the working float is wallet-side.
//! TODO(SO-299): route bid funding through a vault release adapter.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use sui_json_rpc_types::{SuiObjectDataOptions, SuiParsedData};
use sui_types::base_types::{ObjectID, SuiAddress};

use api_service_client::{ApiServiceClient, OpenRfq};
use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::auction::{bid, AuctionBidParams, AuctionTypes};

use crate::pricing::{compute_spot_from_cache, serves_pair, Staleness};

use super::model::{MarketModel, V1BidParams};
use super::quote::{self, Decision, RfqInputs};
use super::DeskShared;

const ALERT_ID: &str = "tx-failed-mm-bot-desk";
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

/// First-bid sizing policy (unchanged mechanics from the old bidder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitialBidPolicy {
    /// Open at exactly the floor (reserve) and climb only when contested.
    ReservePlus,
    /// Open at `max_bid × (1 − shade_bps/10⁴)`.
    Shaded,
    /// Open at the full max bid.
    Max,
}

/// `[desk.auctions]` section. Enabled by default when the desk runs —
/// the auctions are the vault slices' primary exit.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuctionsConfig {
    pub enabled: bool,
    pub poll_secs: u64,
    pub initial_bid: InitialBidPolicy,
    pub shade_bps: u64,
    /// Top back up (to the min-increment floor) when outbid, while the
    /// required bid stays under our max.
    pub rebid: bool,
    /// Cap on total settlement locked across live best bids.
    pub max_concurrent_escrow: u64,
    /// Don't open/raise inside this window before the deadline.
    pub min_deadline_lead_ms: u64,
    pub gas_budget: u64,
}

impl Default for AuctionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
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

/// The pure bid decision — unchanged mechanics from the old bidder.
/// `locked_escrow` is the settlement already committed across other
/// auctions; `max_bid` is the V1 bid for the whole slice.
pub fn decide_bid(
    cfg: &AuctionsConfig,
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

/// Parse a generic `Auction`'s parsed-JSON fields.
pub fn parse_auction_view(fields: &Value) -> Result<AuctionView> {
    let best_bidder = match field(fields, "best_bidder")? {
        Value::Null => None,
        Value::String(s) => Some(s.parse().with_context(|| format!("best_bidder {s:?}"))?),
        other => return Err(anyhow!("unexpected best_bidder {other}")),
    };
    let escrow = as_u64(field(fields, "bid_escrow")?).context("field bid_escrow")?;
    Ok(AuctionView {
        amount: as_u64(field(fields, "amount")?).context("field amount")?,
        reserve_premium: as_u64(field(fields, "reserve_bid")?).context("field reserve_bid")?,
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

pub struct AuctionBidderParams {
    pub cfg: AuctionsConfig,
    pub v1: V1BidParams,
    pub limits: super::limits::LimitsConfig,
    pub shared: Arc<DeskShared>,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    /// Generic `auction` package (for the `auction::bid` call).
    pub package: ObjectID,
    /// The trading vault's address — every won slice's option coins land
    /// here (vault-only mandate; keeper sweeps them into custody).
    pub vault_address: SuiAddress,
    pub api_url: String,
    pub price_cache: PriceCache,
    pub models: Arc<Vec<MarketModel>>,
    pub settlement_feed: PriceFeedId,
    pub settlement_coin_type: String,
    pub settlement_decimals: u8,
    /// Per-model (underlying) feeds/decimals aligned with `models`.
    pub market_feeds: Vec<(PriceFeedId, u8)>,
    pub staleness: Staleness,
    pub expected_holding_years: f64,
    pub slippage_bps: f64,
}

pub fn spawn_bidder(p: AuctionBidderParams) {
    tokio::spawn(async move {
        if let Err(e) = run(p).await {
            tracing::error!(error = %format!("{e:#}"), "desk auction bidder exited");
        }
    });
}

async fn run(p: AuctionBidderParams) -> Result<()> {
    let wrap = SuiClientWrapper::connect(&p.secrets, p.network).await?;
    let api = ApiServiceClient::new(&p.api_url);
    let our_address = wrap.signer.address;
    tracing::info!(
        address = %our_address,
        vault = %p.vault_address,
        poll_secs = p.cfg.poll_secs,
        policy = ?p.cfg.initial_bid,
        escrow_cap = p.cfg.max_concurrent_escrow,
        "desk auction bidder starting (call + put channels)"
    );
    let poll = Duration::from_secs(p.cfg.poll_secs.max(1));
    loop {
        if let Err(e) = tick(&p, &wrap, &api, our_address).await {
            tracing::warn!(error = %format!("{e:#}"), "auction bidder tick errored");
        }
        tokio::time::sleep(poll).await;
    }
}

async fn tick(
    p: &AuctionBidderParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    our_address: SuiAddress,
) -> Result<()> {
    let mut open = api.open_rfqs().await.context("polling open rfqs")?;
    match api.open_put_rfqs().await {
        Ok(puts) => open.extend(puts),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "put-rfq poll failed; calls only"),
    }
    if open.is_empty() {
        return Ok(());
    }
    // Drop auctions originating from a paused vault before any chain read.
    let paused = api.paused_vault_ids().await.context("polling paused vaults")?;
    let now = now_ms();

    // Live views first: locked escrow across ALL our standing best bids
    // must be known before any new bid is sized.
    let mut views: Vec<(OpenRfq, AuctionView)> = Vec::with_capacity(open.len());
    for rfq in open {
        if let Ok(vault) = protocol_types::ids::ObjectId::from_hex(&rfq.origin) {
            if paused.contains(&vault) {
                continue;
            }
        }
        match fetch_auction_view(&wrap.client, sui_object_id(&rfq.rfq_id)?).await {
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
        let Some(bucket) = api.bucket_pricing(rfq.bucket_id.clone()).await? else {
            continue;
        };
        let Some(mi) = p.models.iter().position(|m| {
            serves_pair(
                &bucket.asset_coin_type,
                &bucket.settlement_coin_type,
                &m.coin_type,
                &p.settlement_coin_type,
            )
        }) else {
            continue; // not a pair the desk serves
        };
        let (feed, decimals) = p.market_feeds[mi];
        let spot = match compute_spot_from_cache(
            &p.price_cache,
            feed,
            p.settlement_feed,
            decimals,
            p.settlement_decimals,
            p.staleness,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(reason = e.as_str(), "skipping auction: spot unavailable");
                continue;
            }
        };

        // Max bid: the SAME V1 writer-flow decision as the WS channel.
        let ctx = p.shared.flow_context(spot).await;
        let inputs = RfqInputs {
            write_amount: view.amount,
            is_put: bucket.is_put,
            strike: bucket.strike,
            strike_scale: bucket.strike_scale,
            expiry_ms: bucket.expiry_ms,
        };
        let max_bid = match quote::price_writer_flow(
            &p.models[mi],
            &p.v1,
            &p.limits,
            &ctx,
            &inputs,
            now,
        ) {
            Decision::Quote { premium } => premium,
            Decision::Decline { reason } => {
                tracing::debug!(rfq = %rfq.rfq_id.to_hex(), %reason, "declined to price auction");
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
        // Call auctions are `Auction<Underlying, Settlement>`; put
        // auctions escrow settlement collateral: `Auction<Settlement,
        // Settlement>`.
        let escrow_type = if bucket.is_put {
            &bucket.settlement_coin_type
        } else {
            &bucket.asset_coin_type
        };
        let params = AuctionBidParams {
            package: p.package,
            types: AuctionTypes {
                escrow_type,
                bid_type: &bucket.settlement_coin_type,
            },
            auction_id: sui_object_id(&rfq.rfq_id)?,
            funding_coin: funding,
            amount: premium,
            // Vault-only mandate: the winner's outputs land in the vault.
            token_recipient: p.vault_address,
            gas_budget: p.cfg.gas_budget,
        };
        match bid(&wrap.client, &wrap.signer, &params).await {
            Ok(resp) => {
                locked = locked.saturating_add(premium);
                metrics::counter!("mm_desk_auction_bids_total").increment(1);
                tracing::info!(
                    rfq = %rfq.rfq_id.to_hex(),
                    premium,
                    max_bid,
                    locked,
                    is_put = bucket.is_put,
                    digest = %resp.digest,
                    "auction bid placed (outputs → vault)"
                );
            }
            Err(e) => {
                if crate::is_benign_bid_loss(&e) {
                    tracing::warn!(rfq = %rfq.rfq_id.to_hex(), premium, error = %format!("{e:#}"), "bid failed (outbid)");
                } else {
                    tracing::error!(
                        alert_id = ALERT_ID,
                        rfq = %rfq.rfq_id.to_hex(),
                        premium,
                        error = %format!("{e:#}"),
                        "auction bid tx failed"
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

pub(crate) fn sui_object_id(id: &protocol_types::ids::ObjectId) -> Result<ObjectID> {
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

    fn cfg() -> AuctionsConfig {
        AuctionsConfig::default()
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
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, us, 0, NOW), Ok(47_619_000));
        let mut c = cfg();
        c.initial_bid = InitialBidPolicy::Max;
        assert_eq!(decide_bid(&c, &a, 60_000_000, us, 0, NOW), Ok(60_000_000));
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
        let mut a2 = auction(Some((33, 2)));
        a2.reserve_premium = 1;
        assert_eq!(decide_bid(&cfg(), &a2, 1_000, us, 0, NOW), Ok(34));
        let mut a3 = auction(Some((50_000_000, 2)));
        a3.min_increment_bps = 0;
        assert_eq!(decide_bid(&cfg(), &a3, 60_000_000, us, 0, NOW), Ok(50_000_001));
    }

    #[test]
    fn passes_when_winning_capped_or_priced_out() {
        let us = addr(1);
        let a = auction(Some((50_000_000, 1)));
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, us, 0, NOW), Err(NoBid::Winning));
        let a = auction(Some((59_900_000, 2)));
        assert!(matches!(
            decide_bid(&cfg(), &a, 60_000_000, us, 0, NOW),
            Err(NoBid::FloorAboveMax { .. })
        ));
        let a = auction(None);
        assert!(matches!(
            decide_bid(&cfg(), &a, 60_000_000, us, 4_960_000_000, NOW),
            Err(NoBid::EscrowCapped { .. })
        ));
        let a = auction(None);
        assert_eq!(
            decide_bid(&cfg(), &a, 60_000_000, us, 0, 980_000),
            Err(NoBid::DeadlineTooClose)
        );
        let mut c = cfg();
        c.rebid = false;
        let a = auction(Some((50_000_000, 2)));
        assert_eq!(decide_bid(&c, &a, 60_000_000, us, 0, NOW), Err(NoBid::RebidDisabled));
    }

    #[test]
    fn parses_auction_object_json() {
        let v = parse_auction_view(&json!({
            "amount": "250000000",
            "reserve_bid": "47619000",
            "deadline_ms": "1000000",
            "min_increment_bps": "100",
            "best_bidder": null,
            "bid_escrow": "0",
        }))
        .unwrap();
        assert_eq!(v.best_premium, None);
        assert_eq!(v.deadline_ms, 1_000_000);

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
