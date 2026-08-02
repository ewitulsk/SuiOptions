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
use sui_tx::chain::ChainClient;
use sui_tx::events::EventClient;
use sui_types::event::EventID;
use tracing::{debug, info, warn};

use api_service_client::ApiServiceClient;

use crate::db::models::TradeRow;
use crate::state::{AppState, PoolMeta, TradeMsg};

pub struct WatcherParams {
    pub state: Arc<AppState>,
    pub sui: ChainClient,
    /// GraphQL event reads (gRPC has no events query).
    pub events: EventClient,
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
    if let Err(e) = sui_types::parse_sui_struct_tag(&event_type) {
        tracing::error!(error = %e, event_type, "bad OrderFilled type; watcher exiting");
        return;
    }

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

        match ingest_once(&p, &event_type, cursor.clone()).await {
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

/// Marker stored in `watch_cursor.cursor_ev` to say "cursor_tx holds an
/// opaque GraphQL cursor", distinguishing it from the pre-migration rows
/// that held a `(tx_digest, event_seq)` pair. JSON-RPC's `EventID` cursor
/// has no GraphQL equivalent, so a legacy row cannot be resumed from — it
/// is dropped and the watcher re-initialises from the stream tip (the same
/// self-heal operators already trigger by clearing the row).
const GRAPHQL_CURSOR_MARKER: i64 = -1;

/// Resume from the persisted cursor, else start tailing from the stream tip.
async fn load_or_init_cursor(p: &WatcherParams) -> Option<String> {
    let repo = p.state.repo.clone();
    let persisted = tokio::task::spawn_blocking(move || repo.load_cursor())
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten();
    match persisted {
        Some((cursor, GRAPHQL_CURSOR_MARKER)) => {
            info!(%cursor, "resuming from persisted cursor");
            return Some(cursor);
        }
        Some((cursor_tx, cursor_ev)) => {
            warn!(
                %cursor_tx, cursor_ev,
                "persisted cursor predates the GraphQL event API; reinitializing from tip"
            );
        }
        None => {}
    }
    // Tip-init: newest OrderFilled (any pool) becomes the starting cursor.
    let event_type = format!("{}::order_info::OrderFilled", p.deepbook_original_package);
    if let Ok(page) = p
        .events
        .query_by_type(&event_type, None, 1, /* descending */ true)
        .await
    {
        if page.next_cursor.is_some() {
            info!("no cursor; tailing from stream tip");
            return page.next_cursor;
        }
    }
    info!("no cursor and no prior OrderFilled events; tailing from genesis");
    None
}

async fn ingest_once(
    p: &WatcherParams,
    event_type: &str,
    mut cursor: Option<String>,
) -> Result<Option<String>> {
    loop {
        let page = p
            .events
            .query_by_type(event_type, cursor.as_deref(), 100, /* ascending */ false)
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
                    tx_digest: ev.tx_digest.to_string(),
                    event_index: ev.event_seq as i64,
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

        let Some(next_cursor) = page.next_cursor.clone() else {
            // A non-empty page with no cursor cannot be advanced past;
            // stop here and retry from the same place next tick rather
            // than silently re-ingesting.
            warn!("event page carried no cursor; holding position");
            return Ok(cursor);
        };
        let repo = p.state.repo.clone();
        let batch = rows.clone();
        let cur = (next_cursor.clone(), GRAPHQL_CURSOR_MARKER);
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
