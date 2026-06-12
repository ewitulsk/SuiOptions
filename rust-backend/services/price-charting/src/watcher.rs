//! Pool discovery + fill ingestion + TTL eviction.
//!
//! Discovery: every `discovery_interval_secs`, refresh the watched-pool set
//! from api-service `/buckets` (`tradeable == true` only).
//!
//! Ingestion: DeepBook's `OrderFilled` is NOT generic (verified live —
//! DEEPBOOK-FINDINGS.md §B), so ONE exact `MoveEventType` query on the
//! original package id covers every pool; we tail it with a single global
//! cursor, keep fills whose `pool_id` is watched, and drop the rest. The
//! cursor advances in the same DB transaction as the batch it covers, and
//! the `(pool_id, tx_digest, event_index)` uniqueness makes overlap replays
//! idempotent — restart-safe with zero duplicates.
//!
//! First boot (no cursor): start tailing from the newest existing event.
//! Our pools are brand new, so there is no history to backfill, and scanning
//! testnet's full OrderFilled backlog (other people's pools) would take
//! hours for nothing.

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use sui_sdk::rpc_types::EventFilter;
use sui_sdk::SuiClient;
use sui_types::event::EventID;
use tracing::{debug, info, warn};

use api_service_client::ApiServiceClient;

use crate::db::models::TradeRow;
use crate::state::{AppState, PoolMeta, TradeMsg};

pub struct WatcherParams {
    pub state: Arc<AppState>,
    pub sui: SuiClient,
    pub api: ApiServiceClient,
    /// DeepBook ORIGINAL package id (event types resolve here).
    pub deepbook_original_package: String,
    pub discovery_interval: Duration,
    pub poll_interval: Duration,
    pub ttl_hours: i64,
}

pub fn spawn(p: WatcherParams) {
    tokio::spawn(async move {
        run(p).await;
    });
}

async fn run(p: WatcherParams) {
    let event_type = format!("{}::order_info::OrderFilled", p.deepbook_original_package);
    let filter = match event_type.parse() {
        Ok(tag) => EventFilter::MoveEventType(tag),
        Err(e) => {
            tracing::error!(error = %e, event_type, "bad OrderFilled type; watcher exiting");
            return;
        }
    };

    let mut cursor = load_or_init_cursor(&p).await;
    let mut last_discovery = Instant::now() - p.discovery_interval;
    let mut last_evict = Instant::now();
    let mut ticker = tokio::time::interval(p.poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        if last_discovery.elapsed() >= p.discovery_interval {
            if let Err(e) = refresh_watched(&p).await {
                warn!(error = %format!("{e:#}"), "pool discovery failed; keeping previous set");
            }
            last_discovery = Instant::now();
        }

        match ingest_once(&p, &filter, cursor.clone()).await {
            Ok(next) => cursor = next,
            Err(e) => warn!(error = %format!("{e:#}"), "fill ingestion failed; retrying next tick"),
        }

        // Hourly TTL sweep for pools that left the tradeable set.
        if last_evict.elapsed() >= Duration::from_secs(3_600) {
            last_evict = Instant::now();
            let keep: Vec<String> = p.state.watched.read().keys().cloned().collect();
            let repo = p.state.repo.clone();
            let ttl = p.ttl_hours;
            match tokio::task::spawn_blocking(move || repo.evict_stale(&keep, ttl)).await {
                Ok(Ok((pools, rows))) if pools > 0 => {
                    info!(pools, rows, "TTL-evicted stale pool data")
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => warn!(error = %format!("{e:#}"), "TTL eviction failed"),
                Err(e) => warn!(error = %e, "TTL eviction join failed"),
            }
        }
    }
}

async fn refresh_watched(p: &WatcherParams) -> Result<()> {
    let buckets = p.api.tradeable_buckets().await?;
    let mut map = std::collections::HashMap::new();
    for b in buckets {
        map.insert(
            b.pool_id.clone(),
            PoolMeta {
                bucket_id: b.bucket_id.to_hex(),
                base_decimals: b.asset_decimals.unwrap_or(8),
                quote_decimals: b.settlement_decimals.unwrap_or(6),
                base_coin_type: b.call_coin_type.clone(),
                quote_coin_type: b.settlement_coin_type.clone(),
            },
        );
    }
    let count = map.len();
    *p.state.watched.write() = map;
    debug!(pools = count, "watched pool set refreshed");
    Ok(())
}

/// Resume from the persisted cursor, else start tailing from the stream tip.
async fn load_or_init_cursor(p: &WatcherParams) -> Option<EventID> {
    let repo = p.state.repo.clone();
    let persisted = tokio::task::spawn_blocking(move || repo.load_cursor())
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten();
    if let Some((tx, ev)) = persisted {
        match sui_types::digests::TransactionDigest::from_str(&tx) {
            Ok(digest) => {
                info!(cursor_tx = %tx, cursor_ev = ev, "resuming from persisted cursor");
                return Some(EventID { tx_digest: digest, event_seq: ev as u64 });
            }
            Err(e) => warn!(error = %e, cursor_tx = %tx, "bad persisted cursor; reinitializing"),
        }
    }
    // Tip-init: newest OrderFilled (any pool) becomes the starting cursor.
    let event_type = format!("{}::order_info::OrderFilled", p.deepbook_original_package);
    let filter = event_type
        .parse()
        .ok()
        .map(EventFilter::MoveEventType);
    if let Some(f) = filter {
        if let Ok(page) = p.sui.event_api().query_events(f, None, Some(1), true).await {
            if let Some(ev) = page.data.first() {
                info!(cursor_tx = %ev.id.tx_digest, "no cursor; tailing from stream tip");
                return Some(ev.id);
            }
        }
    }
    info!("no cursor and no prior OrderFilled events; tailing from genesis");
    None
}

async fn ingest_once(
    p: &WatcherParams,
    filter: &EventFilter,
    mut cursor: Option<EventID>,
) -> Result<Option<EventID>> {
    loop {
        let page = p
            .sui
            .event_api()
            .query_events(filter.clone(), cursor, Some(100), false)
            .await
            .context("querying OrderFilled events")?;
        if page.data.is_empty() {
            return Ok(cursor);
        }

        let mut rows: Vec<TradeRow> = Vec::new();
        let mut msgs: Vec<TradeMsg> = Vec::new();
        {
            let watched = p.state.watched.read();
            for ev in &page.data {
                let Some(parsed) = parse_fill(&ev.parsed_json) else {
                    continue;
                };
                let Some(meta) = watched.get(&parsed.pool_id) else {
                    continue; // someone else's pool
                };
                let scale = 10f64.powi(9 - meta.base_decimals as i32 + meta.quote_decimals as i32);
                let price = parsed.price_raw as f64 / scale;
                rows.push(TradeRow {
                    time: Utc
                        .timestamp_millis_opt(parsed.timestamp_ms)
                        .single()
                        .unwrap_or_else(Utc::now),
                    pool_id: parsed.pool_id.clone(),
                    bucket_id: meta.bucket_id.clone(),
                    price,
                    price_raw: BigDecimal::from(parsed.price_raw),
                    base_qty: BigDecimal::from(parsed.base_quantity),
                    quote_qty: BigDecimal::from(parsed.quote_quantity),
                    base_decimals: meta.base_decimals as i16,
                    taker_is_bid: parsed.taker_is_bid,
                    tx_digest: ev.id.tx_digest.to_string(),
                    event_index: ev.id.event_seq as i64,
                });
                msgs.push(TradeMsg {
                    pool_id: parsed.pool_id,
                    ts_ms: parsed.timestamp_ms,
                    price,
                    base_qty: parsed.base_quantity as f64
                        / 10f64.powi(meta.base_decimals as i32),
                    taker_is_bid: parsed.taker_is_bid,
                });
            }
        }

        let next_cursor = page
            .next_cursor
            .or_else(|| page.data.last().map(|e| e.id))
            .expect("non-empty page has a cursor");
        let repo = p.state.repo.clone();
        let batch = rows.clone();
        let cur = (next_cursor.tx_digest.to_string(), next_cursor.event_seq as i64);
        let inserted =
            tokio::task::spawn_blocking(move || repo.insert_trades(&batch, cur)).await??;
        if inserted > 0 {
            debug!(inserted, "ingested fills");
        }
        metrics::counter!("price_charting_bars_broadcast_total").increment(msgs.len() as u64);
        for m in msgs {
            let _ = p.state.trades_tx.send(m); // no subscribers is fine
        }

        cursor = Some(next_cursor);
        if !page.has_next_page {
            return Ok(cursor);
        }
    }
}

struct ParsedFill {
    pool_id: String,
    price_raw: u64,
    base_quantity: u64,
    quote_quantity: u64,
    taker_is_bid: bool,
    timestamp_ms: i64,
}

/// Pull the fields we chart from OrderFilled's JSON (u64s arrive as decimal
/// strings). Shape captured live in
/// `tools/deepbook-pool-test/fixtures/order_filled.testnet.json`.
fn parse_fill(json: &serde_json::Value) -> Option<ParsedFill> {
    let s = |k: &str| json.get(k)?.as_str()?.parse::<u64>().ok();
    Some(ParsedFill {
        pool_id: json.get("pool_id")?.as_str()?.to_string(),
        price_raw: s("price")?,
        base_quantity: s("base_quantity")?,
        quote_quantity: s("quote_quantity")?,
        taker_is_bid: json.get("taker_is_bid")?.as_bool()?,
        timestamp_ms: s("timestamp")? as i64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_captured_orderfilled_shape() {
        // Field subset of fixtures/order_filled.testnet.json.
        let json: serde_json::Value = serde_json::json!({
            "pool_id": "0x1c19362ca52b8ffd7a33cee805a67d40f31e6ba303753fd3a4cfdfacea7163a5",
            "price": "755000",
            "base_quantity": "1000000000",
            "quote_quantity": "755000",
            "taker_is_bid": true,
            "timestamp": "1781050772926"
        });
        let f = parse_fill(&json).unwrap();
        assert_eq!(f.price_raw, 755_000);
        assert_eq!(f.base_quantity, 1_000_000_000);
        assert!(f.taker_is_bid);
        assert_eq!(f.timestamp_ms, 1_781_050_772_926);

        // SUI(9)/DBUSDC(6): scale 10^6 → $0.755 — the verified formula.
        let scale = 10f64.powi(9 - 9 + 6);
        assert!((f.price_raw as f64 / scale - 0.755).abs() < 1e-9);
    }

    #[test]
    fn malformed_fills_are_skipped_not_fatal() {
        assert!(parse_fill(&serde_json::json!({"pool_id": "0x1"})).is_none());
        assert!(parse_fill(&serde_json::json!({})).is_none());
    }
}
