//! On-chain cash-secured-PUT RFQ bidder — the put mirror of
//! [`crate::onchain_rfq`].
//!
//! Standalone put auctions are surfaced by api-service under
//! `GET /rfqs?status=open&kind=put` (`ApiServiceClient::open_put_rfqs`). The
//! auction object has the same `RfqAuction` shape as the call auctions, so the
//! chain-read (`parse_auction_view` / `fetch_auction_view`) and the pure bid
//! decision (`decide_bid`) are reused verbatim from `onchain_rfq`.
//!
//! Two things differ from the call bidder:
//!   1. **Pricing**: the bucket is a put, so the pricing brain runs the
//!      Black-Scholes *put* leg (`RfqPricingInputs { is_put: true, .. }`).
//!   2. **Bid PTB**: `rfq_put::bid` instead of `rfq::bid`. The `Put` coin type
//!      flows in via `bucket.call_coin_type` (api-service serves the per-bucket
//!      put coin under that field for put buckets).
//!
//! **Escrow accounting is identical to the call bidder**: the bid escrows the
//! *premium* (settlement), NOT the put collateral. `max_concurrent_escrow`
//! caps the total premium locked across live best bids, exactly as for calls.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::RwLock;
use sui_types::base_types::SuiAddress;

use api_service_client::{ApiServiceClient, OpenRfq};
use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::rfq_put::{bid, PutRfqBidParams, PutRfqTypes};

use crate::onchain_rfq::{
    decide_bid, fetch_auction_view, now_ms, settlement_funding_coin, sui_object_id, AuctionView,
    OnchainRfqConfig,
};
use crate::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, serves_pair, PriceDecision, PricingConfig,
    RfqPricingInputs, Staleness,
};

/// One underlying the put bidder serves — same shape as the call bidder's
/// market, sharing the vol buffer.
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
}

pub struct BidderParams {
    /// Reuses the call bidder's config shape; lives under `[onchain_put_rfq]`.
    pub cfg: OnchainRfqConfig,
    pub secrets: runtime_config::Secrets,
    pub network: Network,
    /// Options-protocol package (for the `rfq_put::bid` call).
    pub package: sui_types::base_types::ObjectID,
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
            tracing::error!(error = %format!("{e:#}"), "onchain put rfq bidder exited");
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
        "onchain put rfq bidder starting"
    );
    let poll = Duration::from_secs(p.cfg.poll_secs.max(1));
    loop {
        if let Err(e) = tick(&p, &wrap, &api, our_address).await {
            tracing::warn!(error = %format!("{e:#}"), "put bidder tick errored");
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
    let open = api.open_put_rfqs().await.context("polling open put rfqs")?;
    if open.is_empty() {
        return Ok(());
    }
    // Hard cutover: drop auctions from a paused vault before any chain read or
    // bid. Seller-origin (uncoupled) auctions never match a vault id.
    let paused = api
        .paused_vault_ids()
        .await
        .context("polling paused vaults")?;
    let now = now_ms();

    // Live views first: locked escrow must be summed across ALL our standing
    // best bids before any new bid is sized.
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
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "put auction read failed"),
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
        if !bucket.is_put {
            tracing::warn!(
                bucket = %rfq.bucket_id.to_hex(),
                "open_put_rfqs returned a non-put bucket; skipping"
            );
            continue;
        }
        if bucket.call_coin_type.is_empty() {
            tracing::warn!(
                bucket = %rfq.bucket_id.to_hex(),
                "api-service didn't return the put coin type; cannot build the bid PTB"
            );
            continue;
        }

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
                tracing::debug!(reason = e.as_str(), "skipping put auction: spot unavailable");
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
            side: Side::Writer, // retail/vault is the writer; we buy the put
            strike: bucket.strike,
            strike_scale: bucket.strike_scale,
            expiry_ms: bucket.expiry_ms,
            is_put: true,
        };
        let max_bid = match price_rfq(&p.pricing, &inputs, spot_scaled, sigma, now) {
            PriceDecision::Quote { premium, .. } => premium,
            PriceDecision::Decline { reason } => {
                tracing::debug!(rfq = %rfq.rfq_id.to_hex(), reason, "declined to price put");
                continue;
            }
        };

        let premium = match decide_bid(&p.cfg, &view, max_bid, our_address, locked, now) {
            Ok(b) => b,
            Err(reason) => {
                tracing::debug!(rfq = %rfq.rfq_id.to_hex(), ?reason, max_bid, "no put bid");
                continue;
            }
        };

        // Escrow is the PREMIUM (same accounting as the call bidder) — fund it
        // from the wallet's settlement coins, NOT the put collateral.
        let Some(funding) = settlement_funding_coin(wrap, &p.settlement_coin_type, premium).await?
        else {
            tracing::warn!(premium, "no settlement coin large enough to fund the put bid");
            continue;
        };
        let params = PutRfqBidParams {
            package: p.package,
            types: PutRfqTypes {
                underlying_type: &bucket.asset_coin_type,
                settlement_type: &bucket.settlement_coin_type,
                put_type: &bucket.call_coin_type,
            },
            rfq_id: sui_object_id(rfq.rfq_id)?,
            funding_coin: funding,
            premium,
            put_recipient: our_address,
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
                    "put bid placed"
                );
            }
            Err(e) => {
                if crate::is_benign_bid_loss(&e) {
                    tracing::warn!(rfq = %rfq.rfq_id.to_hex(), premium, error = %format!("{e:#}"), "put bid failed (outbid)");
                } else {
                    tracing::error!(
                        alert_id = "tx-failed-mm-bot-put-rfq",
                        rfq = %rfq.rfq_id.to_hex(),
                        premium,
                        error = %format!("{e:#}"),
                        "put rfq bid tx failed"
                    );
                }
            }
        }
    }
    Ok(())
}
