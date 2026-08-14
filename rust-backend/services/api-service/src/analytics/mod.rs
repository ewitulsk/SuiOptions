//! Analytics endpoints (SO-389): generalized (instrument × metric ×
//! params) time series served from the data-room gold layer.
//!
//! Routes stay under api-service's existing public path for now (rename
//! to an analytics service later is a routing change, not a code move).

pub mod lake;

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use lake::Lake;

/// Fixed metric registry. Adding a metric = a variant here plus a reader
/// on [`Lake`]; the endpoints and catalog shape don't change.
const SPOT_FREQS: &[(&str, i64)] = &[("1s", 1), ("1m", 60), ("1h", 3_600)];
const RV_WINDOWS_S: &[i64] = &[3_600, 86_400, 604_800];
const RV_INTERVALS_S: &[i64] = &[1, 5, 15, 60, 300];
const RV_ESTIMATORS: &[&str] = &["close_close", "rv_subsampled"];
/// Server-side downsampling cap for one series response.
const MAX_POINTS: usize = 3_000;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn bad_request(msg: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}

fn unavailable() -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "analytics data unavailable" })),
    )
}

fn lake_of(state: &AppState) -> Result<&Arc<Lake>, ApiError> {
    state.analytics.as_ref().ok_or_else(unavailable)
}

// -- catalog -------------------------------------------------------------

#[derive(Serialize)]
struct CatalogInstrument {
    instrument_id: String,
    exchange: String,
    symbol: String,
    metrics: serde_json::Value,
    first_date: String,
    last_date: String,
}

pub async fn catalog(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let lake = lake_of(&state)?;
    // Instruments + coverage from the 1h bars partitions (every asset
    // with any silver trades has 1h bars).
    let keys = lake.list("gold/v1/bars/freq=3600s/").await.map_err(|e| {
        tracing::warn!("analytics catalog list failed: {e:#}");
        unavailable()
    })?;

    let mut by_pair: std::collections::BTreeMap<(String, String), (String, String)> =
        Default::default();
    for k in keys.iter() {
        let Some(ex) = k
            .split("/exchange=")
            .nth(1)
            .and_then(|s| s.split('/').next())
        else {
            continue;
        };
        let Some(sym) = k.split("/symbol=").nth(1).and_then(|s| s.split('/').next()) else {
            continue;
        };
        let Some(date) = k.split("/date=").nth(1).and_then(|s| s.split('/').next()) else {
            continue;
        };
        let e = by_pair
            .entry((ex.to_string(), sym.to_string()))
            .or_insert_with(|| (date.to_string(), date.to_string()));
        if date < e.0.as_str() {
            e.0 = date.to_string();
        }
        if date > e.1.as_str() {
            e.1 = date.to_string();
        }
    }

    let metrics = json!({
        "spot": { "freqs": SPOT_FREQS.iter().map(|(n, _)| n).collect::<Vec<_>>() },
        "rv": {
            "windows_s": RV_WINDOWS_S,
            "sample_intervals_s": RV_INTERVALS_S,
            "estimators": RV_ESTIMATORS,
        },
        "iv": { "indices": ["dvol"] },
    });
    let instruments: Vec<CatalogInstrument> = by_pair
        .into_iter()
        .map(|((exchange, symbol), (first, last))| {
            let mut m = metrics.clone();
            if symbol.ends_with("-PERP") {
                m["funding"] = json!({ "kinds": ["settled"] });
                m["basis"] = json!({});
            }
            CatalogInstrument {
                instrument_id: format!("{}.{exchange}", symbol.to_lowercase()),
                exchange,
                symbol,
                metrics: m,
                first_date: first,
                last_date: last,
            }
        })
        .collect();
    Ok(Json(json!({ "instruments": instruments })))
}

// -- series --------------------------------------------------------------

#[derive(Deserialize)]
pub struct SeriesQuery {
    pub instrument_id: String,
    pub metric: String,
    pub from: Option<String>,
    pub to: Option<String>,
    // spot
    pub freq: Option<String>,
    // rv
    pub window_s: Option<i64>,
    pub sample_interval_s: Option<i64>,
    pub estimator: Option<String>,
}

fn date_range(from: &Option<String>, to: &Option<String>) -> Result<Vec<String>, ApiError> {
    let to_d = match to {
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| bad_request("bad `to` date, want YYYY-MM-DD"))?,
        None => Utc::now().date_naive(),
    };
    let from_d = match from {
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| bad_request("bad `from` date, want YYYY-MM-DD"))?,
        None => to_d - Duration::days(30),
    };
    if from_d > to_d {
        return Err(bad_request("`from` after `to`"));
    }
    let mut out = Vec::new();
    let mut d = from_d;
    while d <= to_d {
        out.push(d.format("%Y-%m-%d").to_string());
        d += Duration::days(1);
    }
    Ok(out)
}

/// Uniform stride keeping first/last — plenty for a chart.
fn downsample(points: Vec<(i64, f64)>) -> (Vec<(i64, f64)>, usize) {
    if points.len() <= MAX_POINTS {
        return (points, 0);
    }
    let n = points.len();
    let stride = n.div_ceil(MAX_POINTS);
    let mut out: Vec<(i64, f64)> = points.iter().step_by(stride).copied().collect();
    if out.last() != points.last() {
        out.push(*points.last().unwrap());
    }
    let dropped = n - out.len();
    (out, dropped)
}

/// "btc-usdc.binance" → ("BTC-USDC", "binance")
fn split_instrument(id: &str) -> Result<(String, String), ApiError> {
    let (sym, ex) = id
        .rsplit_once('.')
        .ok_or_else(|| bad_request("bad instrument_id, want <symbol>.<exchange>"))?;
    Ok((sym.to_uppercase(), ex.to_string()))
}

pub async fn series(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let lake = lake_of(&state)?;
    let dates = date_range(&q.from, &q.to)?;
    let (symbol, exchange) = split_instrument(&q.instrument_id)?;

    let (points, params, units) = match q.metric.as_str() {
        "spot" => {
            let freq_name = q.freq.as_deref().unwrap_or("1h");
            let freq_s = SPOT_FREQS
                .iter()
                .find(|(n, _)| *n == freq_name)
                .map(|(_, s)| *s)
                .ok_or_else(|| bad_request("bad freq, want 1s|1m|1h"))?;
            let pts = lake
                .spot_series(&exchange, &symbol, freq_s, &dates)
                .await
                .map_err(|e| {
                    tracing::warn!("analytics spot read failed: {e:#}");
                    unavailable()
                })?;
            (pts, json!({ "freq": freq_name }), "quote")
        }
        "rv" => {
            let window_s = q.window_s.unwrap_or(86_400);
            let interval_s = q.sample_interval_s.unwrap_or(60);
            let estimator = q
                .estimator
                .clone()
                .unwrap_or_else(|| "rv_subsampled".into());
            if !RV_WINDOWS_S.contains(&window_s) {
                return Err(bad_request("bad window_s"));
            }
            if !RV_INTERVALS_S.contains(&interval_s) {
                return Err(bad_request("bad sample_interval_s"));
            }
            if !RV_ESTIMATORS.contains(&estimator.as_str()) {
                return Err(bad_request("bad estimator"));
            }
            let pts = lake
                .rv_series(&q.instrument_id, window_s, interval_s, &estimator, &dates)
                .await
                .map_err(|e| {
                    tracing::warn!("analytics rv read failed: {e:#}");
                    unavailable()
                })?;
            (
                pts,
                json!({
                    "window_s": window_s,
                    "sample_interval_s": interval_s,
                    "estimator": estimator,
                }),
                "annualized_vol",
            )
        }
        "iv" => {
            // Venue-computed implied vol index (Deribit DVOL), matched by
            // the instrument's base: btc-… → BTC-DVOL. Served as an
            // annualized FRACTION for axis parity with rv.
            let base = q
                .instrument_id
                .split(['-', '.'])
                .next()
                .unwrap_or_default()
                .to_uppercase();
            if base.is_empty() {
                return Err(bad_request("bad instrument_id"));
            }
            let symbol = format!("{base}-DVOL");
            let pts = lake
                .vol_index_series("deribit", &symbol, &dates)
                .await
                .map_err(|e| {
                    tracing::warn!("analytics iv read failed: {e:#}");
                    unavailable()
                })?;
            if pts.is_empty() {
                // Distinguish "no index for this base" from a quiet range:
                // either way the client hides the line; empty points is
                // fine and 400 is reserved for unmappable ids.
            }
            let pts = pts.into_iter().map(|(ts, pct)| (ts, pct / 100.0)).collect();
            (pts, json!({ "index": symbol }), "annualized_vol")
        }
        "funding" => {
            // Settled funding, annualized: rate × (24/interval) × 365.
            let rows = lake
                .funding_series(&exchange, &symbol, "settled", &dates)
                .await
                .map_err(|e| {
                    tracing::warn!("analytics funding read failed: {e:#}");
                    unavailable()
                })?;
            let interval = rows.first().map(|r| r.interval_hours).unwrap_or(8.0);
            let pts: Vec<(i64, f64)> = rows
                .iter()
                .map(|r| (r.ts / 1_000_000, r.rate * (24.0 / r.interval_hours) * 365.0))
                .collect();
            (
                pts,
                json!({ "kind": "settled", "interval_hours": interval }),
                "annualized_rate",
            )
        }
        "basis" => {
            // Perp premium as a raw fraction. Venue-native where possible:
            // Hyperliquid streams mark+oracle in its ctx frames; Binance
            // joins perp vs USDC-spot hourly bars (USDT/USDC cross is
            // bps-level noise — the legs are named in params).
            if exchange == "hyperliquid" {
                let rows = lake
                    .funding_series(&exchange, &symbol, "predicted", &dates)
                    .await
                    .map_err(|e| {
                        tracing::warn!("analytics basis read failed: {e:#}");
                        unavailable()
                    })?;
                let pts: Vec<(i64, f64)> = rows
                    .iter()
                    .filter_map(|r| match (r.mark_price, r.index_price) {
                        (Some(m), Some(ix)) if ix > 0.0 => Some((r.ts / 1_000_000, (m - ix) / ix)),
                        _ => None,
                    })
                    .collect();
                (pts, json!({ "method": "mark_index" }), "fraction")
            } else {
                let base = symbol
                    .trim_end_matches("-PERP")
                    .split('-')
                    .next()
                    .unwrap_or_default();
                if base.is_empty() {
                    return Err(bad_request("bad perp symbol"));
                }
                let spot_symbol = format!("{base}-USDC");
                let perp = lake
                    .spot_series(&exchange, &symbol, 3_600, &dates)
                    .await
                    .map_err(|e| {
                        tracing::warn!("analytics basis perp read failed: {e:#}");
                        unavailable()
                    })?;
                let spot = lake
                    .spot_series(&exchange, &spot_symbol, 3_600, &dates)
                    .await
                    .map_err(|e| {
                        tracing::warn!("analytics basis spot read failed: {e:#}");
                        unavailable()
                    })?;
                let spot_by_ts: std::collections::HashMap<i64, f64> = spot.into_iter().collect();
                let pts: Vec<(i64, f64)> = perp
                    .into_iter()
                    .filter_map(|(ts, p)| {
                        spot_by_ts
                            .get(&ts)
                            .filter(|s| **s > 0.0)
                            .map(|s| (ts, (p - s) / s))
                    })
                    .collect();
                (
                    pts,
                    json!({ "method": "bars", "legs": format!("{symbol}/{spot_symbol}") }),
                    "fraction",
                )
            }
        }
        other => return Err(bad_request(format!("unknown metric {other}"))),
    };

    let (points, dropped) = downsample(points);
    Ok(Json(json!({
        "instrument_id": q.instrument_id,
        "metric": q.metric,
        "params": params,
        "points": points,
        "meta": { "units": units, "points_dropped": dropped },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_range_defaults_and_bounds() {
        let r = date_range(&Some("2026-08-01".into()), &Some("2026-08-03".into())).unwrap();
        assert_eq!(r, vec!["2026-08-01", "2026-08-02", "2026-08-03"]);
        assert!(date_range(&Some("2026-08-05".into()), &Some("2026-08-03".into())).is_err());
        assert!(date_range(&Some("nope".into()), &None).is_err());
    }

    #[test]
    fn downsample_keeps_ends_under_cap() {
        let pts: Vec<(i64, f64)> = (0..10_000).map(|i| (i, i as f64)).collect();
        let (out, dropped) = downsample(pts.clone());
        assert!(out.len() <= MAX_POINTS + 1);
        assert_eq!(out.first(), pts.first().as_ref().copied());
        assert_eq!(out.last(), pts.last().as_ref().copied());
        assert_eq!(dropped, pts.len() - out.len());
        let (small, d0) = downsample(vec![(1, 1.0)]);
        assert_eq!((small.len(), d0), (1, 0));
    }

    #[test]
    fn instrument_split() {
        assert_eq!(
            split_instrument("btc-usdc.binance").unwrap(),
            ("BTC-USDC".into(), "binance".into())
        );
        assert!(split_instrument("nodot").is_err());
    }
}
