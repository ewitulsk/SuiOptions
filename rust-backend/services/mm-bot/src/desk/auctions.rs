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
//! Vault-only mandate, escrow included (SO-299): bids are placed through
//! `options_adapter::bid_on_auction`, which escrows the bid FROM VAULT
//! free balances and mints a `BidTicket` position into vault custody.
//! Every auction output (outbid refund, early-settle refund, won option
//! coins) routes to the ticket's own address; the KEEPER's permissionless
//! cranks (`reclaim_*_ticket` / `redeem_won_ticket`) burn tickets back
//! into the vault — the desk never cranks them, it only observes.
//!
//! Reservation ledger: each live ticket's cost is reserved against NAV in
//! the book when the bid is placed, and released when the indexer's
//! position view shows the ticket burned (`active = false`). A rebid is a
//! NEW ticket on the same auction; the outbid ticket's reservation clears
//! the same way once the keeper reclaims it. `max_concurrent_escrow` caps
//! the total cost across live tickets.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::Value;
use sui_types::base_types::{ObjectID, SuiAddress};

use api_service_client::{ApiServiceClient, OpenRfq};
use pyth_client::{PriceCache, PriceFeedId};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::{clock_arg, owned_object_arg, shared_object_arg, submit_ptb};

use crate::pricing::{compute_spot_from_cache, serves_pair, Staleness};

use super::book::Book;
use super::model::{MarketModel, V1BidParams};
use super::quote::{self, Decision, RfqInputs};
use super::{CuratorRefs, DeskShared};

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

/// `[desk.auctions]` section. RETIRED: the on-chain auction venue is
/// deprecated — `options_adapter::bid_on_auction` no longer exists in
/// fresh deployments (see contracts/.deprecated/auction). Disabled by
/// default; do not enable against a post-retirement deployment.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuctionsConfig {
    pub enabled: bool,
    pub poll_secs: u64,
    pub initial_bid: InitialBidPolicy,
    pub shade_bps: u64,
    /// Top back up (to the min-increment floor) when outbid, while the
    /// required bid stays under our max. A rebid mints a NEW ticket.
    pub rebid: bool,
    /// Cap on total vault settlement escrowed across live bid tickets.
    pub max_concurrent_escrow: u64,
    /// Don't open/raise inside this window before the deadline.
    pub min_deadline_lead_ms: u64,
    pub gas_budget: u64,
}

impl Default for AuctionsConfig {
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
/// `we_are_best` is whether the auction's best bidder is one of OUR live
/// tickets (the on-chain bidder identity is the ticket address, not the
/// bot wallet); `locked_escrow` is the settlement already committed
/// across live tickets; `max_bid` is the V1 bid for the whole slice.
pub fn decide_bid(
    cfg: &AuctionsConfig,
    auction: &AuctionView,
    max_bid: u64,
    we_are_best: bool,
    locked_escrow: u64,
    now_ms: u64,
) -> Result<u64, NoBid> {
    if we_are_best {
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
    client: &sui_tx::chain::ChainClient,
    rfq_id: ObjectID,
) -> Result<Option<AuctionView>> {
    let Some((_, json)) = client
        .try_get_object_json(rfq_id)
        .await
        .with_context(|| format!("reading auction {rfq_id}"))?
    else {
        return Ok(None); // settled mid-poll: the object is deleted
    };
    match json {
        Some(fields) => Ok(Some(parse_auction_view(&fields)?)),
        None => Err(anyhow!("auction {rfq_id} has no readable Move content")),
    }
}

// ── the bidder loop ────────────────────────────────────────────────────

/// One live vault-funded bid: a `BidTicket` in vault custody whose cost
/// is reserved against NAV until the keeper cranks burn it.
struct LiveBid {
    auction_id: ObjectID,
    amount: u64,
    reservation: u64,
}

pub struct AuctionBidderParams {
    pub cfg: AuctionsConfig,
    pub v1: V1BidParams,
    pub limits: super::limits::LimitsConfig,
    pub shared: Arc<DeskShared>,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    /// options_adapter package (`bid_on_auction`).
    pub options_adapter_package: ObjectID,
    /// Curator-session refs — the bid escrows from THIS vault's balances.
    pub curator: CuratorRefs,
    /// Reservation ledger (live ticket costs reserve NAV).
    pub book: Arc<RwLock<Book>>,
    pub api_url: String,
    pub indexer_url: String,
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
    let indexer = indexer_graphql::IndexerClient::new(p.indexer_url.clone());
    tracing::info!(
        vault = %p.curator.vault_id,
        poll_secs = p.cfg.poll_secs,
        policy = ?p.cfg.initial_bid,
        escrow_cap = p.cfg.max_concurrent_escrow,
        "desk auction bidder starting (vault-funded, call + put channels)"
    );
    // Live tickets, keyed by ticket id. In-memory only: a restart drops
    // the map (and the book's reservations with it) — live tickets still
    // count in NAV at cost via appraisal, so the ledger stays sound.
    let mut live: HashMap<ObjectID, LiveBid> = HashMap::new();
    let poll = Duration::from_secs(p.cfg.poll_secs.max(1));
    loop {
        if let Err(e) = tick(&p, &wrap, &api, &indexer, &mut live).await {
            tracing::warn!(error = %format!("{e:#}"), "auction bidder tick errored");
        }
        tokio::time::sleep(poll).await;
    }
}

/// Release reservations for tickets the keeper cranks have burned: the
/// indexer's position view keeps burned rows with `active = false`
/// (definitive evidence — never released on mere absence, which could be
/// indexer lag behind the placement).
async fn observe_ticket_burns(
    p: &AuctionBidderParams,
    indexer: &indexer_graphql::IndexerClient,
    live: &mut HashMap<ObjectID, LiveBid>,
) {
    if live.is_empty() {
        return;
    }
    let vault_pt = protocol_types::ids::ObjectId::new(p.curator.vault_id.into_bytes());
    let positions = match indexer.trading_vault_positions(vault_pt).await {
        Ok(pos) => pos,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "ticket-burn observation failed; retrying");
            return;
        }
    };
    let burned: Vec<ObjectID> = positions
        .iter()
        .filter(|pos| !pos.active)
        .map(|pos| ObjectID::new(*pos.position_id.as_bytes()))
        .filter(|id| live.contains_key(id))
        .collect();
    for ticket in burned {
        if let Some(bid) = live.remove(&ticket) {
            p.book.write().release_reservation(bid.reservation);
            tracing::info!(
                ticket = %ticket,
                auction = %bid.auction_id,
                amount = bid.amount,
                "bid ticket burned by keeper crank; reservation released"
            );
        }
    }
}

async fn tick(
    p: &AuctionBidderParams,
    wrap: &SuiClientWrapper,
    api: &ApiServiceClient,
    indexer: &indexer_graphql::IndexerClient,
    live: &mut HashMap<ObjectID, LiveBid>,
) -> Result<()> {
    observe_ticket_burns(p, indexer, live).await;

    // SO-418 risk gate: bids escrow settlement OUT of vault free balances,
    // which the risk-off gate set blocks on-chain — stop before reading
    // the board or burning gas. Ticket burns above still release
    // reservations while parked.
    if p.shared.risk_off.load(std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!("auction bidder idle: vault is risk-off");
        return Ok(());
    }

    let mut open = api.open_rfqs().await.context("polling open rfqs")?;
    match api.open_put_rfqs().await {
        Ok(puts) => open.extend(puts),
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "put-rfq poll failed; calls only"),
    }
    if open.is_empty() {
        return Ok(());
    }
    // Drop auctions originating from a paused OR risk-off vault before any
    // chain read (SO-418: `risk_off_vault_ids` is the superset of the old
    // paused check — deposits paused, risk state, commitment breach, or
    // lifecycle not open).
    let paused = api.risk_off_vault_ids().await.context("polling risk-off vaults")?;
    let now = now_ms();

    // Live views first: the winning check + rebid floors need the chain's
    // current best bid.
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
    // Settlement locked across our live tickets (each escrowed from vault
    // balances until its ticket burns).
    let mut locked: u64 = live.values().map(|b| b.amount).sum();

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
        if bucket.call_coin_type.is_empty() {
            tracing::debug!(rfq = %rfq.rfq_id.to_hex(), "no option coin type; cannot pin a win type");
            continue;
        }
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
            Decision::Quote { premium, .. } => premium,
            Decision::Decline { reason } => {
                tracing::debug!(rfq = %rfq.rfq_id.to_hex(), %reason, "declined to price auction");
                continue;
            }
        };

        // The on-chain bidder identity is the ticket address: we're best
        // exactly when the best bidder is one of our live tickets.
        let auction_id = sui_object_id(&rfq.rfq_id)?;
        let we_are_best = view.best_bidder.is_some_and(|best| {
            live.iter().any(|(ticket, b)| {
                b.auction_id == auction_id && SuiAddress::from(*ticket) == best
            })
        });
        let premium = match decide_bid(&p.cfg, &view, max_bid, we_are_best, locked, now) {
            Ok(b) => b,
            Err(reason) => {
                tracing::debug!(rfq = %rfq.rfq_id.to_hex(), ?reason, max_bid, "no bid");
                continue;
            }
        };

        // Reserve the ticket cost against NAV before escrowing it.
        let reservation = match p.book.write().reserve(premium, u64::MAX, now) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(rfq = %rfq.rfq_id.to_hex(), premium, ?e, "bid refused by reservation ledger");
                continue;
            }
        };

        // Call auctions are `Auction<Underlying, Settlement>`; put
        // auctions escrow settlement collateral: `Auction<Settlement,
        // Settlement>`. The win type is the bucket's option coin.
        let escrow_type = if bucket.is_put {
            &bucket.settlement_coin_type
        } else {
            &bucket.asset_coin_type
        };
        match place_bid(
            p,
            wrap,
            auction_id,
            escrow_type,
            &bucket.settlement_coin_type,
            &bucket.call_coin_type,
            premium,
            view.amount,
            sui_object_id(&rfq.bucket_id)?,
            bucket.is_put,
        )
        .await
        {
            Ok((digest, ticket_id)) => {
                locked = locked.saturating_add(premium);
                live.insert(ticket_id, LiveBid { auction_id, amount: premium, reservation });
                metrics::counter!("mm_desk_auction_bids_total").increment(1);
                tracing::info!(
                    rfq = %rfq.rfq_id.to_hex(),
                    ticket = %ticket_id,
                    premium,
                    max_bid,
                    locked,
                    is_put = bucket.is_put,
                    digest = %digest,
                    "vault-funded auction bid placed (BidTicket in vault custody)"
                );
            }
            Err(e) => {
                p.book.write().release_reservation(reservation);
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

/// `options_adapter::bid_on_auction<E, B, W>` — escrow `bid_amount` from
/// vault balances into the auction, minting a `BidTicket` position.
/// Returns `(digest, ticket_id)`.
#[allow(clippy::too_many_arguments)]
async fn place_bid(
    p: &AuctionBidderParams,
    wrap: &SuiClientWrapper,
    auction_id: ObjectID,
    escrow_type: &str,
    bid_type: &str,
    win_type: &str,
    bid_amount: u64,
    win_amount: u64,
    bucket_id: ObjectID,
    is_put: bool,
) -> Result<(String, ObjectID)> {
    use move_core_types::identifier::Identifier;
    use move_core_types::language_storage::TypeTag;
    use std::str::FromStr;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

    let mut pt = ProgrammableTransactionBuilder::new();
    let vault = pt.obj(shared_object_arg(&wrap.client, p.curator.vault_id, true).await?)?;
    let cap = pt.obj(owned_object_arg(&wrap.client, p.curator.curator_cap).await?)?;
    let reg = pt.obj(
        shared_object_arg(&wrap.client, p.curator.integration_registry, false).await?,
    )?;
    let auction = pt.obj(shared_object_arg(&wrap.client, auction_id, true).await?)?;
    let bid_amount_arg = pt.pure(&bid_amount)?;
    let win_amount_arg = pt.pure(&win_amount)?;
    let bucket_arg = pt.pure(&bucket_id)?;
    let is_put_arg = pt.pure(&is_put)?;
    let clock = clock_arg(&mut pt)?;
    pt.programmable_move_call(
        p.options_adapter_package,
        Identifier::new("options_adapter").unwrap(),
        Identifier::new("bid_on_auction").unwrap(),
        vec![
            TypeTag::from_str(escrow_type)?,
            TypeTag::from_str(bid_type)?,
            TypeTag::from_str(win_type)?,
        ],
        vec![vault, cap, reg, auction, bid_amount_arg, win_amount_arg, bucket_arg, is_put_arg, clock],
    );
    let resp =
        submit_ptb(&wrap.client, &wrap.signer, pt, p.cfg.gas_budget, "desk auction bid").await?;
    let suffix = "::options_adapter::BidTicket";
    let ticket_id = sui_tx::chain::created_objects(&resp)
        .into_iter()
        .find(|c| c.object_type.ends_with(suffix))
        .map(|c| c.object_id)
        .ok_or_else(|| anyhow!("bid_on_auction succeeded but no BidTicket in object changes"))?;
    Ok((sui_tx::tx::tx_digest(&resp).to_string(), ticket_id))
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
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, false, 0, NOW), Ok(47_619_000));
        let mut c = cfg();
        c.initial_bid = InitialBidPolicy::Max;
        assert_eq!(decide_bid(&c, &a, 60_000_000, false, 0, NOW), Ok(60_000_000));
        c.initial_bid = InitialBidPolicy::Shaded;
        assert_eq!(decide_bid(&c, &a, 60_000_000, false, 0, NOW), Ok(58_200_000));
        assert_eq!(decide_bid(&c, &a, 47_700_000, false, 0, NOW), Ok(47_619_000));
    }

    #[test]
    fn rebid_matches_onchain_ceiling_rule() {
        let a = auction(Some((50_000_000, 2)));
        // ceil(50_000_000 × 1.01) = 50_500_000.
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, false, 0, NOW), Ok(50_500_000));
        let mut a2 = auction(Some((33, 2)));
        a2.reserve_premium = 1;
        assert_eq!(decide_bid(&cfg(), &a2, 1_000, false, 0, NOW), Ok(34));
        let mut a3 = auction(Some((50_000_000, 2)));
        a3.min_increment_bps = 0;
        assert_eq!(decide_bid(&cfg(), &a3, 60_000_000, false, 0, NOW), Ok(50_000_001));
    }

    #[test]
    fn passes_when_winning_capped_or_priced_out() {
        // Our live ticket is the best bidder → no self-topping.
        let a = auction(Some((50_000_000, 1)));
        assert_eq!(decide_bid(&cfg(), &a, 60_000_000, true, 0, NOW), Err(NoBid::Winning));
        let a = auction(Some((59_900_000, 2)));
        assert!(matches!(
            decide_bid(&cfg(), &a, 60_000_000, false, 0, NOW),
            Err(NoBid::FloorAboveMax { .. })
        ));
        let a = auction(None);
        assert!(matches!(
            decide_bid(&cfg(), &a, 60_000_000, false, 4_960_000_000, NOW),
            Err(NoBid::EscrowCapped { .. })
        ));
        let a = auction(None);
        assert_eq!(
            decide_bid(&cfg(), &a, 60_000_000, false, 0, 980_000),
            Err(NoBid::DeadlineTooClose)
        );
        let mut c = cfg();
        c.rebid = false;
        let a = auction(Some((50_000_000, 2)));
        assert_eq!(decide_bid(&c, &a, 60_000_000, false, 0, NOW), Err(NoBid::RebidDisabled));
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
