//! Order-book midpoint sampling.
//!
//! Every `sample_interval`, dev-inspect the top of each watched pool's book
//! (`pool::get_level2_ticks_from_mid`, via `sui_tx::tx::deepbook::top_of_book`)
//! and persist the midpoint when both sides are present. One-sided or empty
//! books produce no row — the chart's carry-forward handles the gap. Unlike
//! fills, mids are point-in-time *state*, so there is no cursor or backfill:
//! history starts when sampling starts.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sui_tx::chain::ChainClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use tracing::{debug, warn};

use sui_tx::tx::deepbook::top_of_book;

use crate::db::models::MidRow;
use crate::state::{AppState, MidMsg, PoolMeta};

pub struct MidSamplerParams {
    pub state: Arc<AppState>,
    pub sui: ChainClient,
    /// DeepBook CURRENT (upgraded) package id — dev-inspect calls target it.
    pub deepbook_package: ObjectID,
    pub sample_interval: Duration,
}

pub fn spawn(p: MidSamplerParams) {
    tokio::spawn(async move {
        run(p).await;
    });
}

async fn run(p: MidSamplerParams) {
    let mut ticker = tokio::time::interval(p.sample_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let pools: Vec<(String, PoolMeta)> = p
            .state
            .watched
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (pool_id, meta) in pools {
            if let Err(e) = sample_one(&p, &pool_id, &meta).await {
                debug!(pool = %pool_id, error = %format!("{e:#}"), "mid sample failed; next tick");
            }
        }
    }
}

async fn sample_one(p: &MidSamplerParams, pool_id: &str, meta: &PoolMeta) -> anyhow::Result<()> {
    let pool = ObjectID::from_hex_literal(pool_id)?;
    let top = top_of_book(
        &p.sui,
        SuiAddress::ZERO,
        p.deepbook_package,
        pool,
        &meta.base_coin_type,
        &meta.quote_coin_type,
    )
    .await?;
    let (Some(bid_raw), Some(ask_raw)) = (top.best_bid_raw, top.best_ask_raw) else {
        return Ok(()); // one-sided or empty book — no mid exists
    };

    // Same raw→display formula as fill ingestion (watcher.rs).
    let scale = 10f64.powi(9 - meta.base_decimals as i32 + meta.quote_decimals as i32);
    let best_bid = bid_raw as f64 / scale;
    let best_ask = ask_raw as f64 / scale;
    let mid = (best_bid + best_ask) / 2.0;

    let now = Utc::now();
    let row = MidRow {
        time: now,
        pool_id: pool_id.to_string(),
        bucket_id: meta.bucket_id.clone(),
        best_bid,
        best_ask,
        mid,
    };
    let repo = p.state.repo.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || repo.insert_mid(&row)).await? {
        warn!(pool = %pool_id, error = %format!("{e:#}"), "mid insert failed");
        return Ok(());
    }
    let _ = p.state.mids_tx.send(MidMsg {
        pool_id: pool_id.to_string(),
        ts_ms: now.timestamp_millis(),
        mid,
    }); // no subscribers is fine
    Ok(())
}
