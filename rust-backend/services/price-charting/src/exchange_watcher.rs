//! Hybrid-exchange market discovery + fill ingestion.
//!
//! Discovery: every `discovery_interval`, refresh the watched market set
//! from the orderbook service's `/v1/markets` — the DB-backed market
//! whitelist, so a delisted market stops charting without touching this
//! service — plus the token-info catalog for base/quote decimals. Markets
//! chart under `pool_id` = SettlementRegistry id.
//!
//! Ingestion mirrors the DeepBook watcher: ONE exact `MoveEventType` query
//! (`{package}::settlement::FillEvent`) covers every market, tailed with a
//! persisted cursor (`watch_cursor` row 2; row 1 is DeepBook's) that
//! advances in the same transaction as its batch; `(pool_id, tx_digest,
//! event_index)` uniqueness makes replays idempotent.
//!
//! Two exchange-specific wrinkles:
//! - Matched settlement emits TWO FillEvents per trade (one per order
//!   digest, `maker_sold_base` true then false) while open-orderbook fills
//!   emit one. The second event of a matched pair is skipped so volume
//!   isn't double-counted.
//! - The persisted cursor embeds the package id it belongs to. A
//!   `--deploy-exchange` republish changes the package and orphans the old
//!   stream position, so a mismatch re-initialises from the stream tip
//!   instead of wedging (the DeepBook watcher's stale-cursor failure mode).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use exchange_types::canonicalize_move_type;
use serde::Deserialize;
use sui_tx::events::EventClient;
use token_info_client::TokenInfoClient;
use tracing::{debug, info, warn};

use crate::db::models::TradeRow;
use crate::state::{AppState, PoolMeta, TradeMsg};
use crate::watcher::GRAPHQL_CURSOR_MARKER;

/// This watcher's `watch_cursor` row. Row 1 is the DeepBook watcher's.
const EXCHANGE_CURSOR_ID: i16 = 2;

pub struct ExchangeWatcherParams {
    pub state: Arc<AppState>,
    /// GraphQL event reads (gRPC has no events query).
    pub events: EventClient,
    /// Orderbook service base URL (`/v1/markets` = whitelist + package id).
    pub orderbook_url: String,
    /// token-info base URL — decimals for price conversion.
    pub token_info_url: String,
    pub discovery_interval: Duration,
    pub poll_interval: Duration,
}

pub fn spawn(p: ExchangeWatcherParams) {
    tokio::spawn(async move {
        run(p).await;
    });
}

/// `GET /v1/markets` response subset.
#[derive(Deserialize)]
struct MarketsResp {
    #[serde(rename = "packageId")]
    package_id: String,
    markets: Vec<exchange_types::Market>,
}

/// Ingestion stream state: the package the event filter (and cursor)
/// belongs to.
struct Stream {
    package: String,
    event_type: String,
    cursor: Option<String>,
}

async fn run(p: ExchangeWatcherParams) {
    let http = reqwest::Client::new();
    let token_info = TokenInfoClient::new(&p.token_info_url);

    let mut stream: Option<Stream> = None;
    let mut last_discovery = Instant::now() - p.discovery_interval;
    let mut ticker = tokio::time::interval(p.poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        if last_discovery.elapsed() >= p.discovery_interval {
            match refresh_watched(&p, &http, &token_info).await {
                Ok(package) => {
                    let stale = stream.as_ref().is_none_or(|s| s.package != package);
                    if stale {
                        stream = Some(init_stream(&p, package).await);
                    }
                }
                // Unreachable orderbook is a degraded mode, not an error
                // loop: markets keep charting from the last known set.
                Err(e) => debug!(error = %format!("{e:#}"), "exchange market discovery failed"),
            }
            last_discovery = Instant::now();
        }

        let Some(s) = stream.as_mut() else { continue };
        match ingest_once(&p, s).await {
            Ok(next) => s.cursor = next,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "exchange fill ingestion failed; retrying next tick")
            }
        }
    }
}

/// Refresh `state.watched_exchange` from the orderbook whitelist + token
/// catalog; returns the exchange package id.
async fn refresh_watched(
    p: &ExchangeWatcherParams,
    http: &reqwest::Client,
    token_info: &TokenInfoClient,
) -> Result<String> {
    let resp: MarketsResp = http
        .get(format!("{}/v1/markets", p.orderbook_url.trim_end_matches('/')))
        .send()
        .await
        .context("fetching orderbook /v1/markets")?
        .error_for_status()
        .context("orderbook /v1/markets status")?
        .json()
        .await
        .context("decoding orderbook /v1/markets")?;

    // canonical coin type → (coin_type, decimals).
    let snapshot = token_info.fetch().await.context("fetching token catalog")?;
    let mut by_type: HashMap<String, (String, u8)> = HashMap::new();
    for t in &snapshot.tokens {
        if let Ok(c) = canonicalize_move_type(&t.coin_type) {
            by_type.insert(c, (t.coin_type.clone(), t.decimals));
        }
    }

    let mut map = HashMap::new();
    for m in &resp.markets {
        // Market types are already canonical (the orderbook canonicalizes
        // at load), but normalize defensively.
        let base = canonicalize_move_type(&m.base).unwrap_or_else(|_| m.base.clone());
        let quote = canonicalize_move_type(&m.quote).unwrap_or_else(|_| m.quote.clone());
        let (Some((base_ct, base_dec)), Some((quote_ct, quote_dec))) =
            (by_type.get(&base), by_type.get(&quote))
        else {
            warn!(market = %m.symbol, "market tokens missing from token-info catalog; not charting");
            continue;
        };
        map.insert(
            m.registry_id.to_hex(),
            PoolMeta {
                bucket_id: m.symbol.clone(),
                base_decimals: *base_dec,
                quote_decimals: *quote_dec,
                base_coin_type: base_ct.clone(),
                quote_coin_type: quote_ct.clone(),
            },
        );
    }
    let count = map.len();
    *p.state.watched_exchange.write() = map;
    debug!(markets = count, "watched exchange market set refreshed");
    Ok(resp.package_id)
}

/// Cursor persistence embeds the package id (`{package}|{cursor}`) so a
/// republished exchange package self-heals to a tip re-init instead of
/// resuming an orphaned stream position.
async fn init_stream(p: &ExchangeWatcherParams, package: String) -> Stream {
    let event_type = format!("{package}::settlement::FillEvent");

    let repo = p.state.repo.clone();
    let persisted = tokio::task::spawn_blocking(move || repo.load_cursor(EXCHANGE_CURSOR_ID))
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten();
    if let Some((tagged, GRAPHQL_CURSOR_MARKER)) = persisted {
        match tagged.split_once('|') {
            Some((pkg, cursor)) if pkg == package => {
                info!(cursor, "resuming exchange fills from persisted cursor");
                return Stream { package, event_type, cursor: Some(cursor.to_owned()) };
            }
            _ => warn!(
                "persisted exchange cursor belongs to another package; reinitializing from tip"
            ),
        }
    }

    // Tip-init: the markets are fresh per deploy, so there is no history
    // worth backfilling (same call as the DeepBook watcher's).
    let cursor = match p.events.query_by_type(&event_type, None, 1, /* descending */ true).await {
        Ok(page) => page.next_cursor,
        Err(e) => {
            warn!(error = %e, "tip query failed; tailing exchange fills from genesis");
            None
        }
    };
    if cursor.is_some() {
        info!("no usable exchange cursor; tailing from stream tip");
    } else {
        info!("no exchange cursor and no prior FillEvents; tailing from genesis");
    }
    Stream { package, event_type, cursor }
}

async fn ingest_once(p: &ExchangeWatcherParams, s: &mut Stream) -> Result<Option<String>> {
    let mut cursor = s.cursor.clone();
    // Matched-pair dedup state, carried across page boundaries within a
    // call (the pair's two events are adjacent in the stream).
    let mut prev: Option<ParsedExchangeFill> = None;
    loop {
        let page = p
            .events
            .query_by_type(&s.event_type, cursor.as_deref(), 100, /* ascending */ false)
            .await
            .context("querying FillEvents")?;
        if page.data.is_empty() {
            return Ok(cursor);
        }

        let mut rows: Vec<TradeRow> = Vec::new();
        let mut msgs: Vec<TradeMsg> = Vec::new();
        {
            let watched = p.state.watched_exchange.read();
            for ev in &page.data {
                let Some(parsed) = parse_fill(&ev.parsed_json) else {
                    continue;
                };
                if is_matched_pair_echo(prev.as_ref(), &parsed) {
                    prev = Some(parsed);
                    continue;
                }
                prev = Some(parsed.clone());
                let Some(meta) = watched.get(&parsed.registry) else {
                    continue; // not a whitelisted market
                };
                // DeepBook price_raw convention: display = raw / 10^(9-bd+qd),
                // i.e. raw = quote * 10^9 / base — decimals-independent.
                let raw =
                    (u128::from(parsed.quote_amount) * 1_000_000_000 / u128::from(parsed.base_amount))
                        .min(u128::from(u64::MAX)) as u64;
                let scale =
                    10f64.powi(9 - i32::from(meta.base_decimals) + i32::from(meta.quote_decimals));
                let price = raw as f64 / scale;
                rows.push(TradeRow {
                    time: Utc
                        .timestamp_millis_opt(parsed.timestamp_ms)
                        .single()
                        .unwrap_or_else(Utc::now),
                    pool_id: parsed.registry.clone(),
                    bucket_id: meta.bucket_id.clone(),
                    price,
                    price_raw: BigDecimal::from(raw),
                    base_qty: BigDecimal::from(parsed.base_amount),
                    quote_qty: BigDecimal::from(parsed.quote_amount),
                    base_decimals: i16::from(meta.base_decimals),
                    // The kept event is the maker_sold_base side of a match
                    // (taker bought base) or a lone open-orderbook fill.
                    taker_is_bid: parsed.maker_sold_base,
                    tx_digest: ev.tx_digest.to_string(),
                    event_index: ev.event_seq as i64,
                });
                msgs.push(TradeMsg {
                    pool_id: parsed.registry.clone(),
                    ts_ms: parsed.timestamp_ms,
                    price,
                    base_qty: parsed.base_amount as f64
                        / 10f64.powi(i32::from(meta.base_decimals)),
                    taker_is_bid: parsed.maker_sold_base,
                });
            }
        }

        let Some(next_cursor) = page.next_cursor.clone() else {
            warn!("exchange event page carried no cursor; holding position");
            return Ok(cursor);
        };
        let repo = p.state.repo.clone();
        let batch = rows.clone();
        let cur = (format!("{}|{next_cursor}", s.package), GRAPHQL_CURSOR_MARKER);
        let inserted = tokio::task::spawn_blocking(move || {
            repo.insert_trades(&batch, cur, EXCHANGE_CURSOR_ID)
        })
        .await??;
        if inserted > 0 {
            debug!(inserted, "ingested exchange fills");
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

#[derive(Debug, Clone)]
struct ParsedExchangeFill {
    tx_digest: String,
    /// Normalized registry hex — the chart pool id.
    registry: String,
    maker: String,
    taker: String,
    base_amount: u64,
    quote_amount: u64,
    maker_sold_base: bool,
    timestamp_ms: i64,
}

/// The second event of a matched pair: same tx, flags true→false, same
/// amounts, and the sides mirrored. Skipping it charts one trade per match
/// while keeping lone open-orderbook fills (either flag) intact.
fn is_matched_pair_echo(prev: Option<&ParsedExchangeFill>, ev: &ParsedExchangeFill) -> bool {
    let Some(prev) = prev else { return false };
    !ev.maker_sold_base
        && prev.maker_sold_base
        && prev.tx_digest == ev.tx_digest
        && prev.registry == ev.registry
        && prev.base_amount == ev.base_amount
        && prev.quote_amount == ev.quote_amount
        && prev.maker == ev.taker
        && prev.taker == ev.maker
}

/// Pull the fields we chart from FillEvent's JSON. u64s arrive as decimal
/// strings under GraphQL (number fallback for robustness); addresses as hex
/// strings — normalized so they match `Market::registry_id.to_hex()`.
fn parse_fill(json: &serde_json::Value) -> Option<ParsedExchangeFill> {
    let s = |k: &str| match json.get(k)? {
        serde_json::Value::String(v) => v.parse::<u64>().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    };
    let addr = |k: &str| {
        exchange_types::SuiAddress::parse(json.get(k)?.as_str()?)
            .ok()
            .map(|a| a.to_hex())
    };
    let fill = ParsedExchangeFill {
        tx_digest: String::new(), // filled by the caller's event envelope
        registry: addr("registry")?,
        maker: addr("maker")?,
        taker: addr("taker")?,
        base_amount: s("base_amount")?,
        quote_amount: s("quote_amount")?,
        maker_sold_base: json.get("maker_sold_base")?.as_bool()?,
        timestamp_ms: s("timestamp_ms")? as i64,
    };
    (fill.base_amount > 0).then_some(fill)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_json(maker: &str, taker: &str, sold_base: bool) -> serde_json::Value {
        serde_json::json!({
            "registry": "0x5c",
            "digest": vec![0u8; 32],
            "maker": maker,
            "taker": taker,
            "base_amount": "100000000",      // 1.0 TBTC (8 decimals)
            "quote_amount": "50000000000",   // 50_000 TUSDC (6 decimals)
            "maker_sold_base": sold_base,
            "taker_token_filled_total": "100000000",
            "timestamp_ms": "1781050772926"
        })
    }

    #[test]
    fn parses_fill_and_price_convention() {
        let f = parse_fill(&fill_json("0xa", "0xb", true)).unwrap();
        assert_eq!(f.base_amount, 100_000_000);
        assert_eq!(f.quote_amount, 50_000_000_000);
        assert!(f.maker_sold_base);
        assert_eq!(f.timestamp_ms, 1_781_050_772_926);
        // Addresses normalize to full-width hex (matches Market::to_hex()).
        assert_eq!(f.registry.len(), 66);

        // raw = quote * 1e9 / base; display = raw / 10^(9 - 8 + 6):
        // 1 TBTC @ 50_000 TUSDC → 50_000.0.
        let raw = (u128::from(f.quote_amount) * 1_000_000_000 / u128::from(f.base_amount)) as u64;
        let price = raw as f64 / 10f64.powi(9 - 8 + 6);
        assert!((price - 50_000.0).abs() < 1e-6);
    }

    #[test]
    fn zero_base_and_malformed_are_skipped() {
        let mut j = fill_json("0xa", "0xb", true);
        j["base_amount"] = "0".into();
        assert!(parse_fill(&j).is_none());
        assert!(parse_fill(&serde_json::json!({"registry": "0x5c"})).is_none());
    }

    #[test]
    fn matched_pair_second_event_is_deduped() {
        let mut a = parse_fill(&fill_json("0xa", "0xb", true)).unwrap();
        let mut b = parse_fill(&fill_json("0xb", "0xa", false)).unwrap();
        a.tx_digest = "tx1".into();
        b.tx_digest = "tx1".into();
        assert!(is_matched_pair_echo(Some(&a), &b));

        // Lone open-orderbook fill with the same flag is NOT an echo…
        assert!(!is_matched_pair_echo(None, &b));
        // …nor is a false-flagged fill in a different tx.
        let mut c = b.clone();
        c.tx_digest = "tx2".into();
        assert!(!is_matched_pair_echo(Some(&a), &c));
        // …nor mirrored sides with different amounts (two unrelated fills).
        let mut d = b.clone();
        d.base_amount += 1;
        assert!(!is_matched_pair_echo(Some(&a), &d));
    }
}
