//! Unified on-chain auction bidder — the Solana port of the Sui mm-bot's
//! `onchain_rfq` / `onchain_put_rfq` / `onchain_swap` trio.
//!
//! The venue replaces Sui's per-kind auction objects with ONE program
//! (`auction_venue`), so this is one bidder module with three
//! mode-specific pricing legs:
//!
//! - `covered_call`: price the slice as the buyer (`Side::Writer`
//!   economics — the vault is the writer, we buy), max bid = the
//!   marked-down Black-Scholes premium for the whole slice.
//! - `cash_secured_put`: the put leg of the same brain (intrinsic-floored
//!   in [`crate::pricing`]), same decide_bid.
//! - `swap`: `max_underlying_bid = amount_s / spot × (1 − margin_bps)` —
//!   the vault sells settlement proceeds, we post underlying.
//!
//! Discovery is the solana-indexer `auctions(status: open, mode: …)` view
//! (one uniform source, replacing both the Sui api-service `/rfqs` poll
//! and the swap event walk). Deadline / best-bid state is re-read from the
//! `Auction` account + bid vault via RPC just before every decision — the
//! indexer view may lag bids; the chain can't. The pure bid decision
//! ([`decide_bid`]) ports verbatim, with the on-chain min-increment
//! ceiling taken from the REAL program math
//! (`options_math::min_next_bid`), never re-derived.
//!
//! Bid submission: venue `bid` ix. `token_recipient` is our wallet pubkey
//! (the venue's settle path checks `option_dest.owner == token_recipient`,
//! i.e. it records the destination token account's *owner* wallet);
//! `previous_bidder_refund` is the standing best bidder's derived ATA for
//! the bid mint when a best bid exists, and `None` otherwise (the accounts
//! struct makes it optional and the handler only dereferences it when
//! refunding). Bids fund from the wallet's own ATA — the venue escrows
//! from `bidder_source`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;

use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use solana_indexer_graphql::IndexerClient;
use solana_tx::{pda, SolanaClientWrapper};

use crate::api_client::ApiClient;
use crate::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, PriceDecision, PricingConfig,
    RfqPricingInputs, Smile, Staleness,
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
fn default_swap_bid_margin_bps() -> u64 {
    20
}
fn default_swap_max_concurrent_escrow() -> u64 {
    5_000_000_000
}

/// First-bid sizing policy (ported verbatim from the Sui bidder).
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

/// `[onchain_auction]` section of the bot config — the unified bidder:
/// per-mode enables plus the decide_bid knobs shared by all three modes.
/// Disabled by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OnchainAuctionConfig {
    /// Bid the vaults' weekly covered-call slice auctions.
    pub covered_call: bool,
    /// Bid the cash-secured-put auctions.
    pub cash_secured_put: bool,
    /// Bid the settlement→underlying proceeds-swap auctions.
    pub swap: bool,
    pub poll_secs: u64,
    pub initial_bid: InitialBidPolicy,
    pub shade_bps: u64,
    /// Top back up (to the min-increment floor) when outbid, while the
    /// required bid stays under our max.
    pub rebid: bool,
    /// Cap on total settlement locked across live best bids of the
    /// covered_call + cash_secured_put modes (their bid mint is the
    /// settlement asset). Settlement smallest-units.
    pub max_concurrent_escrow: u64,
    /// Don't open/raise inside this window before the deadline — a bid we
    /// can't be outbid-refunded from in time is a bad trade vs gas.
    pub min_deadline_lead_ms: u64,
    /// Swap mode: most the bot pays = `fair_underlying × (1 −
    /// swap_bid_margin_bps/10⁴)`. Must stay below the vault's swap
    /// slippage band or the cap falls under the reserve and the bot never
    /// bids.
    pub swap_bid_margin_bps: u64,
    /// Swap mode's escrow cap — in *underlying* smallest-units (swap bids
    /// escrow the underlying).
    pub swap_max_concurrent_escrow: u64,
}

impl Default for OnchainAuctionConfig {
    fn default() -> Self {
        Self {
            covered_call: false,
            cash_secured_put: false,
            swap: false,
            poll_secs: default_poll_secs(),
            initial_bid: InitialBidPolicy::ReservePlus,
            shade_bps: default_shade_bps(),
            rebid: default_rebid(),
            max_concurrent_escrow: default_max_concurrent_escrow(),
            min_deadline_lead_ms: default_min_deadline_lead_ms(),
            swap_bid_margin_bps: default_swap_bid_margin_bps(),
            swap_max_concurrent_escrow: default_swap_max_concurrent_escrow(),
        }
    }
}

/// Which auction mode a bidder task serves (maps to the indexer's `mode`
/// filter string and the pricing leg).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BidderMode {
    CoveredCall,
    CashSecuredPut,
    Swap,
}

impl BidderMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BidderMode::CoveredCall => "covered_call",
            BidderMode::CashSecuredPut => "cash_secured_put",
            BidderMode::Swap => "swap",
        }
    }

    /// Alert flow tag: `tx-failed-solana-mm-bot-<flow>`.
    fn alert_flow(self) -> &'static str {
        match self {
            BidderMode::Swap => "swap",
            _ => "auction",
        }
    }

    fn escrow_cap(self, cfg: &OnchainAuctionConfig) -> u64 {
        match self {
            BidderMode::Swap => cfg.swap_max_concurrent_escrow,
            _ => cfg.max_concurrent_escrow,
        }
    }
}

/// Live auction state, read fresh from the `Auction` account + bid vault
/// (the indexer view may lag bids; the chain can't). The venue keeps the
/// best bid as the bid vault's token balance, not an account field.
#[derive(Debug, Clone, PartialEq)]
pub struct AuctionView {
    /// Escrowed amount (underlying for calls, cash collateral for puts,
    /// settlement proceeds for swaps).
    pub amount: u64,
    /// Option notional in underlying units (== amount for calls; 0 for swaps).
    pub notional: u64,
    pub reserve_bid: u64,
    pub deadline_ms: u64,
    pub min_increment_bps: u64,
    /// Current best bid (the bid vault balance); `None` when no bid yet.
    pub best_bid: Option<u64>,
    pub best_bidder: Option<Pubkey>,
}

impl AuctionView {
    /// Project the on-chain account + live bid-vault balance into the view
    /// the pure decision runs on.
    pub fn from_account(auction: &auction_venue::state::Auction, bid_vault_amount: u64) -> Self {
        Self {
            amount: auction.amount,
            notional: auction.notional,
            reserve_bid: auction.reserve_bid,
            deadline_ms: auction.deadline_ms,
            min_increment_bps: auction.min_increment_bps,
            best_bid: auction.best_bidder.is_some().then_some(bid_vault_amount),
            best_bidder: auction.best_bidder,
        }
    }
}

/// Anchor-deserialize a raw `Auction` account (discriminator checked).
pub fn decode_auction(data: &[u8]) -> Result<auction_venue::state::Auction> {
    auction_venue::state::Auction::try_deserialize(&mut &data[..])
        .map_err(|e| anyhow!("deserializing Auction account: {e}"))
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

/// The pure bid decision — ported verbatim from the Sui bidder.
/// `locked_escrow` is the bid-mint amount already committed across other
/// auctions (live best bids + bids submitted this tick); `max_bid` is the
/// pricing brain's marked-down fair value for the whole slice.
/// `escrow_cap` is the mode's `max_concurrent_escrow`.
pub fn decide_bid(
    cfg: &OnchainAuctionConfig,
    escrow_cap: u64,
    auction: &AuctionView,
    max_bid: u64,
    our_address: Pubkey,
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
    // Mirror the venue's `bid` floor exactly: the on-chain ceil increment
    // (`options_math::min_next_bid` — the REAL program function) over the
    // current best, strictly greater than it, never below the reserve.
    let floor = match auction.best_bid {
        Some(prev) if contested => {
            let with_increment =
                options_math::min_next_bid(prev, auction.min_increment_bps).unwrap_or(u64::MAX);
            with_increment
                .max(auction.reserve_bid)
                .max(prev.saturating_add(1))
        }
        _ => auction.reserve_bid,
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
    let headroom = escrow_cap.saturating_sub(locked_escrow);
    if bid > headroom {
        return Err(NoBid::EscrowCapped { needed: bid, headroom });
    }
    Ok(bid)
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

// ── the bidder loop ────────────────────────────────────────────────────

/// One underlying the bidder serves — same shape as the WS quoter's
/// markets, sharing the vol buffers.
pub struct BidderMarket {
    pub symbol: String,
    /// Underlying SPL mint (base58) — matched byte-exact against buckets /
    /// auction mints.
    pub mint: String,
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
    pub mode: BidderMode,
    pub cfg: OnchainAuctionConfig,
    pub secrets: runtime_config::Secrets,
    pub network: solana_tx::Network,
    pub indexer_graphql_url: String,
    pub api_url: String,
    pub price_cache: PriceCache,
    pub markets: Vec<BidderMarket>,
    pub settlement_feed: PriceFeedId,
    /// Settlement SPL mint (base58).
    pub settlement_mint: String,
    pub settlement_decimals: u8,
    pub pricing: PricingConfig,
    pub staleness: Staleness,
}

pub fn spawn_bidder(p: BidderParams) {
    tokio::spawn(async move {
        let mode = p.mode;
        if let Err(e) = run(p).await {
            tracing::error!(mode = mode.as_str(), error = %format!("{e:#}"), "onchain auction bidder exited");
        }
    });
}

async fn run(p: BidderParams) -> Result<()> {
    let wrap = SolanaClientWrapper::connect(&p.secrets, p.network)?;
    let indexer = IndexerClient::new(p.indexer_graphql_url.clone());
    let api = ApiClient::new(&p.api_url);
    let our_address = wrap.signer.pubkey();
    tracing::info!(
        mode = p.mode.as_str(),
        address = %our_address,
        poll_secs = p.cfg.poll_secs,
        policy = ?p.cfg.initial_bid,
        escrow_cap = p.mode.escrow_cap(&p.cfg),
        "onchain auction bidder starting"
    );
    let poll = Duration::from_secs(p.cfg.poll_secs.max(1));
    loop {
        if let Err(e) = tick(&p, &wrap, &indexer, &api, our_address).await {
            tracing::warn!(mode = p.mode.as_str(), error = %format!("{e:#}"), "bidder tick errored");
        }
        tokio::time::sleep(poll).await;
    }
}

/// One discovered open auction: the live view plus what we need to price
/// and submit against it.
struct OpenAuction {
    auction: Pubkey,
    /// The options_core bucket (adapter modes); `None` for pure swaps.
    bucket_id: Option<String>,
    escrow_mint: String,
    bid_mint: String,
    view: AuctionView,
}

/// Read one auction's live state: the `Auction` account plus the bid
/// vault's token balance (the venue keeps the best bid as that balance).
/// `None` when the account is gone (settled mid-poll).
async fn fetch_auction_view(
    wrap: &SolanaClientWrapper,
    auction_id: &Pubkey,
) -> Result<Option<AuctionView>> {
    let account = match wrap.client.get_account(auction_id).await {
        Ok(a) => a,
        Err(e) => {
            // Settled auctions are closed (rent reclaimed) — absent account
            // is the expected terminal state, not an error.
            let msg = e.to_string();
            if msg.contains("AccountNotFound") || msg.contains("could not find account") {
                return Ok(None);
            }
            return Err(anyhow!("reading auction {auction_id}: {e}"));
        }
    };
    let auction = decode_auction(&account.data)?;
    let bid_vault = pda::bid_vault(&auction_venue::ID, auction_id);
    let bid_vault_amount = match wrap.client.get_token_account_balance(&bid_vault).await {
        Ok(b) => b
            .amount
            .parse::<u64>()
            .with_context(|| format!("parsing bid vault balance {:?}", b.amount))?,
        Err(e) => return Err(anyhow!("reading bid vault {bid_vault}: {e}")),
    };
    Ok(Some(AuctionView::from_account(&auction, bid_vault_amount)))
}

fn parse_pubkey(s: &str, what: &str) -> Result<Pubkey> {
    s.parse::<Pubkey>()
        .map_err(|e| anyhow!("{what} is not a base58 pubkey ({s}): {e}"))
}

async fn tick(
    p: &BidderParams,
    wrap: &SolanaClientWrapper,
    indexer: &IndexerClient,
    api: &ApiClient,
    our_address: Pubkey,
) -> Result<()> {
    // Discovery: the indexer's materialized auction view, one mode per task.
    let open = indexer
        .auctions(Some("open"), Some(p.mode.as_str()), None, None)
        .await
        .context("polling open auctions from solana-indexer")?;
    if open.is_empty() {
        return Ok(());
    }
    // Hard cutover: drop auctions originating from a paused vault before
    // any chain read or bid. Coupled auctions are created BY the vault (the
    // auction's `creator` is the vault PDA); standalone auctions never
    // match a vault id, so they're unaffected.
    let paused: HashSet<String> = api
        .paused_vault_ids()
        .await
        .context("polling paused vaults")?;
    let now = now_ms();

    // Live views first: locked escrow must be computed across ALL our
    // standing best bids before any new bid is sized.
    let mut views: Vec<OpenAuction> = Vec::with_capacity(open.len());
    for a in open {
        if paused.contains(&a.creator) {
            continue;
        }
        let auction_id = parse_pubkey(&a.auction_id, "auction_id")?;
        match fetch_auction_view(wrap, &auction_id).await {
            Ok(Some(view)) => views.push(OpenAuction {
                auction: auction_id,
                bucket_id: a.bucket_id,
                escrow_mint: a.escrow_mint,
                bid_mint: a.bid_mint,
                view,
            }),
            Ok(None) => {} // settled since the poll
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "auction read failed"),
        }
    }
    let mut locked: u64 = views
        .iter()
        .filter(|o| o.view.best_bidder == Some(our_address))
        .filter_map(|o| o.view.best_bid)
        .sum();

    let escrow_cap = p.mode.escrow_cap(&p.cfg);
    for o in views {
        let Some((max_bid, bid_mint)) = size_max_bid(p, api, &o, now).await? else {
            continue;
        };

        let bid_amount =
            match decide_bid(&p.cfg, escrow_cap, &o.view, max_bid, our_address, locked, now) {
                Ok(b) => b,
                Err(reason) => {
                    tracing::debug!(auction = %o.auction, ?reason, max_bid, "no bid");
                    continue;
                }
            };

        // Fund from the wallet's own bid-mint ATA — the venue escrows from
        // `bidder_source`.
        let balance = crate::bootstrap::ata_balance(wrap, &our_address, &bid_mint).await?;
        if balance < bid_amount {
            tracing::warn!(
                auction = %o.auction,
                bid_amount,
                balance,
                mint = %bid_mint,
                "wallet balance too low to fund the bid"
            );
            continue;
        }
        let bidder_source = pda::ata(&our_address, &bid_mint);
        // The outbid party's refund rides along in OUR transaction: their
        // derived ATA for the bid mint, required exactly when a best bid
        // stands (the accounts struct makes it optional otherwise).
        let previous_bidder_refund = o
            .view
            .best_bidder
            .map(|prev| pda::ata(&prev, &bid_mint));
        let ix = solana_tx::ix::bid(
            &our_address,
            &o.auction,
            &bidder_source,
            previous_bidder_refund,
            bid_amount,
            // The venue's settle path checks the option/escrow destination
            // token account's OWNER against this — our wallet.
            our_address,
        );
        match wrap.send_and_confirm(&[ix], &[], "venue bid").await {
            Ok(signature) => {
                locked = locked.saturating_add(bid_amount);
                metrics::counter!("solana_mm_bot_bids_total", "mode" => p.mode.as_str())
                    .increment(1);
                tracing::info!(
                    mode = p.mode.as_str(),
                    auction = %o.auction,
                    bid_amount,
                    max_bid,
                    locked,
                    %signature,
                    "bid placed"
                );
            }
            Err(e) => {
                // Outbid between read and submit (BidTooLow), or the
                // deadline crossed / auction settled mid-flight — the
                // benign lost-race family. Everything else pages.
                if crate::is_benign_bid_loss(&e) {
                    tracing::warn!(
                        mode = p.mode.as_str(),
                        auction = %o.auction,
                        bid_amount,
                        error = %format!("{e:#}"),
                        "bid failed (benign race)"
                    );
                } else {
                    tracing::error!(
                        alert_id = format!("tx-failed-solana-mm-bot-{}", p.mode.alert_flow()),
                        mode = p.mode.as_str(),
                        auction = %o.auction,
                        bid_amount,
                        error = %format!("{e:#}"),
                        "bid tx failed"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Mode-specific pricing: our maximum bid for this auction (in the bid
/// mint), plus the parsed bid mint. `None` ⇒ skip (unserved pair, stale
/// spot, declined price).
async fn size_max_bid(
    p: &BidderParams,
    api: &ApiClient,
    o: &OpenAuction,
    now: u64,
) -> Result<Option<(u64, Pubkey)>> {
    match p.mode {
        BidderMode::CoveredCall | BidderMode::CashSecuredPut => {
            // Resolve the bucket's true pricing inputs from solana-api-service
            // by address — never trust the discovery row's fields.
            let Some(bucket_id) = o.bucket_id.as_deref() else {
                tracing::warn!(auction = %o.auction, "option auction without a bucket; skipping");
                return Ok(None);
            };
            let Some(bucket) = api.bucket_pricing(bucket_id).await? else {
                return Ok(None);
            };
            let want_put = p.mode == BidderMode::CashSecuredPut;
            if bucket.is_put != want_put {
                tracing::warn!(
                    auction = %o.auction,
                    bucket = %bucket_id,
                    "bucket kind does not match the auction mode; skipping"
                );
                return Ok(None);
            }
            let Some(market) = p.markets.iter().find(|m| {
                crate::pricing::serves_pair(
                    &bucket.asset_mint,
                    &bucket.settlement_mint,
                    &m.mint,
                    &p.settlement_mint,
                )
            }) else {
                return Ok(None); // not a pair we make markets in
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
                    return Ok(None);
                }
            };
            let sigma = resolve_sigma(
                market.vol_buf.read().current_annualized(),
                market.vol_buf_long.read().current_annualized(),
                market.fallback_vol,
            );
            let inputs = RfqPricingInputs {
                // The option notional in underlying units — == the escrow
                // for calls; for puts the escrow is the cash collateral, so
                // the notional field is the size to price.
                write_amount: o.view.notional,
                side: Side::Writer, // the vault is the writer; we buy
                strike: bucket.strike,
                strike_scale: bucket.strike_scale,
                expiry_ms: bucket.expiry_ms,
                is_put: want_put,
            };
            let cfg_m = PricingConfig { smile: market.smile, ..p.pricing };
            let max_bid = match price_rfq(&cfg_m, &inputs, spot_scaled, sigma, now) {
                PriceDecision::Quote { premium, .. } => premium,
                PriceDecision::Decline { reason } => {
                    tracing::debug!(auction = %o.auction, reason, "declined to price");
                    return Ok(None);
                }
            };
            let bid_mint = parse_pubkey(&o.bid_mint, "bid_mint")?;
            Ok(Some((max_bid, bid_mint)))
        }
        BidderMode::Swap => {
            // The vault sells settlement (escrow) for underlying (bid mint):
            // match the pair off the auction's own mints.
            let Some(market) = p.markets.iter().find(|m| {
                crate::pricing::serves_pair(
                    &o.bid_mint,
                    &o.escrow_mint,
                    &m.mint,
                    &p.settlement_mint,
                )
            }) else {
                return Ok(None);
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
                    return Ok(None);
                }
            };
            let Some(max_bid) =
                max_underlying_bid(o.view.amount, spot_scaled, p.cfg.swap_bid_margin_bps)
            else {
                return Ok(None);
            };
            let bid_mint = parse_pubkey(&o.bid_mint, "bid_mint")?;
            Ok(Some((max_bid, bid_mint)))
        }
    }
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
    use anchor_lang::{AnchorSerialize, Discriminator};
    use auction_venue::state::{Auction, AuctionMode};

    fn addr(b: u8) -> Pubkey {
        Pubkey::new_from_array([b; 32])
    }

    fn cfg() -> OnchainAuctionConfig {
        OnchainAuctionConfig::default()
    }

    fn auction(best: Option<(u64, u8)>) -> AuctionView {
        AuctionView {
            amount: 250_000_000,
            notional: 250_000_000,
            reserve_bid: 47_619_000,
            deadline_ms: 1_000_000,
            min_increment_bps: 100,
            best_bid: best.map(|(p, _)| p),
            best_bidder: best.map(|(_, b)| addr(b)),
        }
    }

    const NOW: u64 = 100_000; // well before the 1_000_000 deadline
    const CAP: u64 = 5_000_000_000; // the default escrow cap

    // -- decide_bid (ported verbatim from the Sui bidder) -----------------

    #[test]
    fn first_bid_follows_policy() {
        let a = auction(None);
        let us = addr(1);
        // reserve_plus opens at the reserve.
        assert_eq!(decide_bid(&cfg(), CAP, &a, 60_000_000, us, 0, NOW), Ok(47_619_000));
        // max opens at the max bid.
        let mut c = cfg();
        c.initial_bid = InitialBidPolicy::Max;
        assert_eq!(decide_bid(&c, CAP, &a, 60_000_000, us, 0, NOW), Ok(60_000_000));
        // shaded opens 3% under max, floored at the reserve.
        c.initial_bid = InitialBidPolicy::Shaded;
        assert_eq!(decide_bid(&c, CAP, &a, 60_000_000, us, 0, NOW), Ok(58_200_000));
        assert_eq!(decide_bid(&c, CAP, &a, 47_700_000, us, 0, NOW), Ok(47_619_000));
    }

    #[test]
    fn rebid_matches_onchain_ceiling_rule() {
        let us = addr(1);
        let a = auction(Some((50_000_000, 2)));
        // ceil(50_000_000 × 1.01) = 50_500_000 — via options_math::min_next_bid.
        assert_eq!(decide_bid(&cfg(), CAP, &a, 60_000_000, us, 0, NOW), Ok(50_500_000));
        // Odd previous bid: ceiling, not floor. ceil(33 × 1.01) = 34.
        let mut a2 = auction(Some((33, 2)));
        a2.reserve_bid = 1;
        assert_eq!(decide_bid(&cfg(), CAP, &a2, 1_000, us, 0, NOW), Ok(34));
        // Zero increment still forces strict improvement.
        let mut a3 = auction(Some((50_000_000, 2)));
        a3.min_increment_bps = 0;
        assert_eq!(decide_bid(&cfg(), CAP, &a3, 60_000_000, us, 0, NOW), Ok(50_000_001));
    }

    #[test]
    fn passes_when_winning_capped_or_priced_out() {
        let us = addr(1);
        // Already the best bidder.
        let a = auction(Some((50_000_000, 1)));
        assert_eq!(decide_bid(&cfg(), CAP, &a, 60_000_000, us, 0, NOW), Err(NoBid::Winning));
        // Floor above our max.
        let a = auction(Some((59_900_000, 2)));
        assert!(matches!(
            decide_bid(&cfg(), CAP, &a, 60_000_000, us, 0, NOW),
            Err(NoBid::FloorAboveMax { .. })
        ));
        // Escrow cap: 5e9 default, already 4.96e9 locked.
        let a = auction(None);
        assert!(matches!(
            decide_bid(&cfg(), CAP, &a, 60_000_000, us, 4_960_000_000, NOW),
            Err(NoBid::EscrowCapped { .. })
        ));
        // Deadline too close.
        let a = auction(None);
        assert_eq!(
            decide_bid(&cfg(), CAP, &a, 60_000_000, us, 0, 980_000),
            Err(NoBid::DeadlineTooClose)
        );
        // Rebid disabled: pass on contested auctions.
        let mut c = cfg();
        c.rebid = false;
        let a = auction(Some((50_000_000, 2)));
        assert_eq!(
            decide_bid(&c, CAP, &a, 60_000_000, us, 0, NOW),
            Err(NoBid::RebidDisabled)
        );
    }

    #[test]
    fn decide_bid_floor_agrees_with_program_math() {
        // The floor above the standing best must be exactly the venue's
        // `min_next_bid(previous, inc).max(reserve)` with the strict `>`
        // handled by max(prev + 1) — cross-check straight against the
        // program crate's function.
        let us = addr(1);
        for (prev, inc) in [(100u64, 50u64), (1, 1), (10_000, 50), (99_999, 0)] {
            let mut a = auction(Some((prev, 2)));
            a.reserve_bid = 0;
            a.min_increment_bps = inc;
            let expect = options_math::min_next_bid(prev, inc)
                .unwrap()
                .max(prev + 1);
            assert_eq!(
                decide_bid(&cfg(), CAP, &a, u64::MAX, us, 0, NOW),
                Ok(expect),
                "prev={prev} inc={inc}"
            );
        }
    }

    // -- swap max-bid math -------------------------------------------------

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
    fn swap_bidder_reuses_call_floor_rules() {
        // A no-bid auction with reserve 208, our cap 211 ⇒ bid the reserve.
        let view = AuctionView {
            amount: 10_030_000,
            notional: 0,
            reserve_bid: 208,
            deadline_ms: 1_000_000,
            min_increment_bps: 500,
            best_bid: None,
            best_bidder: None,
        };
        let us = addr(1);
        assert_eq!(decide_bid(&cfg(), CAP, &view, 211, us, 0, 100_000), Ok(208));
        // Cap below reserve ⇒ priced out.
        assert!(matches!(
            decide_bid(&cfg(), CAP, &view, 207, us, 0, 100_000),
            Err(NoBid::FloorAboveMax { .. })
        ));
    }

    // -- Auction account decode (fixture bytes via the program crate) -------

    fn sample_auction(best_bidder: Option<Pubkey>) -> Auction {
        Auction {
            creator: addr(0x0f),
            salt: 7,
            mode: AuctionMode::CoveredCall,
            bucket: addr(0xb1),
            escrow_mint: addr(0xe0),
            bid_mint: addr(0xd0),
            amount: 250_000_000,
            notional: 250_000_000,
            reserve_bid: 47_619_000,
            deadline_ms: 1_000_000,
            snipe_window_ms: 60_000,
            snipe_extension_ms: 60_000,
            max_deadline_ms: 1_600_000,
            min_increment_bps: 100,
            best_bidder,
            best_token_recipient: best_bidder,
            position_recipient: addr(0x0f),
            proceeds_token: addr(0xaa),
            refund_token: addr(0xbb),
            settle_authority: Some(addr(0x0f)),
            bump: 254,
        }
    }

    /// Serialize an Auction exactly as the chain stores it (8-byte Anchor
    /// discriminator + Borsh body).
    fn account_bytes(a: &Auction) -> Vec<u8> {
        let mut data = Auction::DISCRIMINATOR.to_vec();
        a.serialize(&mut data).unwrap();
        data
    }

    #[test]
    fn parses_auction_account_from_fixture_bytes() {
        // No standing bid: bid vault empty ⇒ best is None even though the
        // vault balance is a real number (0 here).
        let bytes = account_bytes(&sample_auction(None));
        let decoded = decode_auction(&bytes).unwrap();
        let v = AuctionView::from_account(&decoded, 0);
        assert_eq!(v.amount, 250_000_000);
        assert_eq!(v.reserve_bid, 47_619_000);
        assert_eq!(v.deadline_ms, 1_000_000);
        assert_eq!(v.min_increment_bps, 100);
        assert_eq!(v.best_bid, None);
        assert_eq!(v.best_bidder, None);

        // Standing bid: best bid IS the live bid-vault balance.
        let bytes = account_bytes(&sample_auction(Some(addr(2))));
        let decoded = decode_auction(&bytes).unwrap();
        let v = AuctionView::from_account(&decoded, 51_000_000);
        assert_eq!(v.best_bid, Some(51_000_000));
        assert_eq!(v.best_bidder, Some(addr(2)));
    }

    #[test]
    fn decode_rejects_wrong_discriminator() {
        let mut bytes = account_bytes(&sample_auction(None));
        bytes[0] ^= 0xff;
        assert!(decode_auction(&bytes).is_err());
    }
}
