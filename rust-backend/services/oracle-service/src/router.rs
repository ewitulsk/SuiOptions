//! axum router: REST snapshot + realized-vol endpoints and the `/ws` fanout.
//! Everything here is read-only; the browser-facing subset (descriptor,
//! prices, legs) is nginx-proxied under exact paths and needs CORS.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use oracle_client::{PricePoint, PricesResponse, RealizedVolPoint, RealizedVolResponse, WsMessage};
use pyth_client::{benchmark_feed_id, PriceFeedId};
use serde::Deserialize;
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tracing::debug;

use crate::state::AppState;

pub fn router(state: Arc<AppState>, allowed_origins: &[String]) -> Result<Router> {
    let cors = build_cors(allowed_origins)?;
    Ok(Router::new()
        .route("/health", get(health))
        .route("/prices", get(get_prices))
        .route("/snapshot", get(get_prices))
        .route("/prices/:feed", get(get_price))
        .route("/prices/by-asset/:coin_type", get(get_price_by_asset))
        .route("/oracle/descriptor", get(get_oracle_descriptor))
        .route("/oracle/legs", get(get_oracle_legs))
        // Root aliases (SO-359): nginx exposes these WITHOUT the
        // `/oracle` nesting (`/{env}/oracle/descriptor` → here), so
        // browser clients compose base-relative paths — serving the same
        // shape locally keeps dev and deployed URLs identical.
        .route("/descriptor", get(get_oracle_descriptor))
        .route("/legs", get(get_oracle_legs))
        .route("/vol/realized", get(get_realized_vol))
        .route("/ws", get(ws_handler))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(observability::middleware::http_obs))
        .layer(cors))
}

/// Mirrors token-info's `build_cors` (SO-357): `"*"` anywhere in the list
/// short-circuits to a fully permissive layer; otherwise the exact
/// origins. All routes are read-only, so no credentials are involved.
fn build_cors(allowed_origins: &[String]) -> Result<CorsLayer> {
    if allowed_origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any));
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for o in allowed_origins {
        origins.push(o.parse()?);
    }
    Ok(CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(Any)
        .allow_headers(Any))
}

async fn health() -> &'static str {
    "ok"
}

/// `GET /oracle/descriptor` — **the switch, published.**
///
/// One place names the live provider and its on-chain identity. Both PTB
/// composers (Rust `sui_tx::tx::oracle`, browser `tx/appraisal.ts`) read
/// this instead of hardcoding an adapter, which is what lets the provider
/// change without redeploying either of them.
///
/// `feeds` is coin type → that asset's feed key under the live provider,
/// so a caller never has to know which catalog column to read.
#[derive(serde::Serialize)]
pub struct OracleDescriptor {
    pub provider: protocol_types::OracleProvider,
    /// The Move module `attest` lives in for this provider.
    pub adapter_module: &'static str,
    /// `None` when the live provider's adapter is not deployed on this
    /// network — the data plane still works, PTB composition does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<crate::state::AdapterIds>,
    pub feeds: std::collections::BTreeMap<String, String>,
}

async fn get_oracle_descriptor(State(state): State<Arc<AppState>>) -> Json<OracleDescriptor> {
    Json(OracleDescriptor {
        provider: state.provider,
        adapter_module: state.provider.adapter_module(),
        adapter: state.adapter,
        feeds: state
            .descriptor_feeds
            .iter()
            .map(|(asset, feed)| (asset.clone(), feed.to_hex()))
            .collect(),
    })
}

#[derive(Debug, Deserialize)]
struct LegsQuery {
    /// Comma-separated canonical coin types.
    assets: String,
}

/// `GET /oracle/legs?assets=…` — the live provider's off-chain payload
/// for one PTB's price legs (SO-346).
///
/// The descriptor names the provider; this hands a composer the actual
/// oracle data: a Hermes accumulator update under Pyth, a signed
/// Crossbar quote bundle (plus queue + `on_demand` ids) under
/// Switchboard. Assets with no feed under the live provider are absent
/// from the response's coverage map — the caller's `none`-leg posture,
/// not an error here.
async fn get_oracle_legs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LegsQuery>,
) -> Result<Json<oracle_client::OracleLegsResponse>, (StatusCode, String)> {
    let assets: Vec<String> = q
        .assets
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| protocol_types::asset::canonicalize_move_type(s.trim()))
        .collect();
    if assets.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no assets requested".into()));
    }
    let (coverage, feeds) = resolve_feeds(&state.descriptor_feeds, &assets);
    if feeds.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no {} feed for any requested asset", state.provider),
        ));
    }

    match &state.legs {
        crate::state::LegsBackend::Pyth { http, hermes_url } => {
            let (payloads, _) = pyth_client::latest_with_update_data(http, hermes_url, &feeds)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("hermes update: {e:#}")))?;
            let update = payloads.first().ok_or((
                StatusCode::BAD_GATEWAY,
                "hermes returned no update payloads".to_string(),
            ))?;
            if payloads.len() > 1 {
                tracing::warn!(payloads = payloads.len(), "hermes returned multiple payloads; serving the first");
            }
            use base64::Engine;
            Ok(Json(oracle_client::OracleLegsResponse::Pyth(
                oracle_client::PythLegsPayload {
                    accumulator_update_b64: base64::engine::general_purpose::STANDARD
                        .encode(update),
                    feeds: coverage,
                },
            )))
        }
        crate::state::LegsBackend::Switchboard {
            crossbar,
            oracles,
            sui_rpc_url,
            queue_id,
            queue_key,
            switchboard_package_id,
        } => {
            let hashes: Vec<String> = feeds.iter().map(|f| f.to_hex()).collect();
            // Crossbar rotates signers per request, and on this network
            // MOST registered oracles carry zero/stale on-chain secp
            // keys (their quotes can never verify — observed live,
            // SO-346). Each retry is a fresh draw, so keep pulling until
            // crossbar picks an attested signer; the map is re-resolved
            // once mid-way in case the failure is genuine key turnover.
            let bundle = {
                const DRAWS: usize = 6;
                let mut map = oracles.read().await.clone();
                let mut last: Option<anyhow::Error> = None;
                let mut won = None;
                for attempt in 1..=DRAWS {
                    match crossbar.fetch_quotes(&hashes, &map).await {
                        Ok(b) => {
                            won = Some(b);
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(attempt, error = %format!("{e:#}"), "quote draw failed; retrying");
                            last = Some(e);
                            if attempt == DRAWS / 2 {
                                match switchboard_client::oracles_from_queue(sui_rpc_url, *queue_id)
                                    .await
                                {
                                    Ok(fresh) => {
                                        *oracles.write().await = fresh.clone();
                                        map = fresh;
                                    }
                                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "oracle map refresh failed"),
                                }
                            }
                        }
                    }
                }
                won.ok_or_else(|| {
                    (
                        StatusCode::BAD_GATEWAY,
                        format!(
                            "crossbar quotes: no attested signer in {DRAWS} draws: {:#}",
                            last.unwrap()
                        ),
                    )
                })?
            };
            bundle
                .require_queue(queue_key)
                .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
            Ok(Json(oracle_client::OracleLegsResponse::Switchboard(
                oracle_client::SwitchboardLegsPayload {
                    switchboard_package_id: switchboard_package_id.clone(),
                    queue_id: queue_id.to_hex_literal(),
                    feed_hashes: coverage,
                    quote: quote_wire(&bundle),
                },
            )))
        }
    }
}

/// Coverage map (asset → feed key hex) + deduped feed list for the
/// requested assets, skipping ones the live provider has no feed for.
fn resolve_feeds(
    feed_by_asset: &std::collections::BTreeMap<String, PriceFeedId>,
    assets: &[String],
) -> (std::collections::BTreeMap<String, String>, Vec<PriceFeedId>) {
    let mut coverage = std::collections::BTreeMap::new();
    let mut feeds: Vec<PriceFeedId> = Vec::new();
    for a in assets {
        let Some(feed) = feed_by_asset.get(a) else {
            continue;
        };
        coverage.insert(a.clone(), feed.to_hex());
        if !feeds.contains(feed) {
            feeds.push(*feed);
        }
    }
    (coverage, feeds)
}

/// [`switchboard_client::QuoteBundle`] → the JSON-safe wire form.
fn quote_wire(bundle: &switchboard_client::QuoteBundle) -> oracle_client::SwitchboardQuoteWire {
    use base64::Engine;
    oracle_client::SwitchboardQuoteWire {
        feed_ids: bundle.feed_ids.iter().map(hex::encode).collect(),
        values: bundle.values.iter().map(|v| v.to_string()).collect(),
        values_neg: bundle.values_neg.clone(),
        min_oracle_samples: bundle.min_oracle_samples.clone(),
        signatures_b64: bundle
            .signatures
            .iter()
            .map(|s| base64::engine::general_purpose::STANDARD.encode(s))
            .collect(),
        slot: bundle.slot,
        timestamp_seconds: bundle.timestamp_seconds,
        oracle_ids: bundle.oracle_ids.iter().map(|o| o.to_hex_literal()).collect(),
    }
}

/// `GET /prices/by-asset/:coin_type` — spot keyed by ASSET, not by a
/// provider-specific feed id.
///
/// This is the data-plane half of the switch: a consumer asks for "the
/// price of this coin type" and never learns which provider answered, so
/// flipping providers needs no consumer change.
async fn get_price_by_asset(
    State(state): State<Arc<AppState>>,
    Path(coin_type): Path<String>,
) -> Result<Json<PricePoint>, StatusCode> {
    let canonical = protocol_types::asset::canonicalize_move_type(&coin_type);
    let feed = state
        .feed_by_asset
        .get(&canonical)
        .copied()
        .ok_or(StatusCode::NOT_FOUND)?;
    let cp = state.price_cache.peek(feed).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(point(feed, &cp, now_ms())))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn point(feed: PriceFeedId, cp: &pyth_client::CachedPrice, now: i64) -> PricePoint {
    PricePoint {
        feed_id: feed.to_hex(),
        price: cp.price,
        conf: cp.conf,
        publish_time_ms: cp.publish_time_ms,
        age_ms: now - cp.publish_time_ms,
    }
}

async fn get_prices(State(state): State<Arc<AppState>>) -> Json<PricesResponse> {
    let now = now_ms();
    let prices = state
        .feeds
        .iter()
        .filter_map(|f| state.price_cache.peek(*f).map(|cp| point(*f, &cp, now)))
        .collect();
    Json(PricesResponse {
        as_of_ms: now,
        upstream_healthy: state.upstream_healthy.load(Ordering::Relaxed),
        prices,
    })
}

async fn get_price(
    State(state): State<Arc<AppState>>,
    Path(feed): Path<String>,
) -> Result<Json<PricePoint>, StatusCode> {
    let id = PriceFeedId::from_hex(&feed).map_err(|_| StatusCode::BAD_REQUEST)?;
    let cp = state.price_cache.peek(id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(point(id, &cp, now_ms())))
}

#[derive(Debug, Deserialize)]
struct VolQuery {
    /// Comma-separated 64-hex feed ids.
    feeds: String,
    window_days: u32,
}

async fn get_realized_vol(
    State(state): State<Arc<AppState>>,
    Query(q): Query<VolQuery>,
) -> Result<Json<RealizedVolResponse>, StatusCode> {
    // Map each requested (beta) feed to its stable Benchmarks feed id here, so
    // consumers never have to know about the beta→stable mapping.
    let pairs: Vec<(PriceFeedId, PriceFeedId)> = q
        .feeds
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|tok| PriceFeedId::from_hex(tok).ok())
        .map(|orig| (orig, benchmark_feed_id(orig)))
        .collect();
    if pairs.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let stable_feeds: Vec<PriceFeedId> = pairs.iter().map(|(_, s)| *s).collect();
    let now_secs = now_ms() / 1000;
    // Hard time budget on the cache walk. A cold cache fills one paced
    // Benchmarks request per missing day, and since Pyth started 401-ing
    // anonymous + 429-ing keyed requests (observed 2026-09-01), that walk
    // can run for minutes — long enough that every caller upstack (api's
    // /buckets, mm-bot vol pulls) rode it into a gateway timeout. Cutting
    // it off returns per-feed "no result" errors, which every consumer
    // already maps to its fallback sigma; the closes fetched before the
    // deadline stay cached, so successive calls warm the cache
    // progressively instead of never answering.
    let bulk = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state
            .benchmark_vol
            .realized_sigma_bulk(&stable_feeds, q.window_days, now_secs),
    )
    .await
    {
        Ok(bulk) => bulk,
        Err(_elapsed) => {
            tracing::warn!(
                feeds = stable_feeds.len(),
                window_days = q.window_days,
                "benchmark vol walk exceeded its time budget; serving fallback errors"
            );
            Default::default()
        }
    };

    let results = pairs
        .into_iter()
        .map(|(orig, stable)| {
            let (sigma, error) = match bulk.get(&stable) {
                Some(Ok(s)) => (Some(*s), None),
                Some(Err(e)) => (None, Some(format!("{e:#}"))),
                None => (None, Some("benchmark returned no result".to_string())),
            };
            RealizedVolPoint {
                feed_id: orig.to_hex(),
                stable_feed_id: stable.to_hex(),
                window_days: q.window_days,
                sigma,
                error,
            }
        })
        .collect();

    Ok(Json(RealizedVolResponse { results }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::base_types::ObjectID;

    #[test]
    fn resolve_feeds_skips_uncovered_and_dedupes() {
        let feed = PriceFeedId::from_hex(&"ab".repeat(32)).unwrap();
        let map: std::collections::BTreeMap<String, PriceFeedId> = [
            ("0x1::a::A".to_string(), feed),
            ("0x1::b::B".to_string(), feed), // same feed, two assets
        ]
        .into();
        let (coverage, feeds) = resolve_feeds(
            &map,
            &[
                "0x1::a::A".to_string(),
                "0x1::b::B".to_string(),
                "0x1::c::C".to_string(), // no feed → none-leg, not an error
            ],
        );
        assert_eq!(coverage.len(), 2);
        assert_eq!(feeds, vec![feed]);
        assert!(!coverage.contains_key("0x1::c::C"));
    }

    #[test]
    fn quote_wire_is_lossless() {
        let bundle = switchboard_client::QuoteBundle {
            feed_ids: vec![vec![0xab; 32]],
            values: vec![63_456_010_000_000_000_000_000u128],
            values_neg: vec![false],
            min_oracle_samples: vec![1],
            signatures: vec![vec![7u8; 64]],
            slot: 42,
            timestamp_seconds: 1_785_700_471,
            oracle_ids: vec![ObjectID::from_hex_literal("0x11").unwrap()],
            queue_key: "c9".repeat(32),
        };
        let wire = quote_wire(&bundle);
        assert_eq!(wire.feed_id_bytes().unwrap(), bundle.feed_ids);
        assert_eq!(wire.values_u128().unwrap(), bundle.values);
        assert_eq!(wire.signature_bytes().unwrap(), bundle.signatures);
        assert_eq!(wire.oracle_ids, vec!["0x11"]);
        // u128 values must be strings on the wire — JS consumers cannot
        // hold 1e22 in a JSON number.
        let json = serde_json::to_value(&wire).unwrap();
        assert!(json["values"][0].is_string());
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.fanout.subscribe();

    // Lead with the current upstream status so a fresh subscriber knows whether
    // the prices it's about to receive are live.
    let initial = WsMessage::Status {
        upstream_healthy: state.upstream_healthy.load(Ordering::Relaxed),
        reason: None,
    };
    if let Ok(txt) = serde_json::to_string(&initial) {
        if socket.send(Message::Text(txt)).await.is_err() {
            return;
        }
    }

    loop {
        match rx.recv().await {
            Ok(msg) => {
                let Ok(txt) = serde_json::to_string(&msg) else {
                    continue;
                };
                if socket.send(Message::Text(txt)).await.is_err() {
                    break; // client gone
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // A slow client missed some frames; the next price frames repopulate
                // its cache, so just keep going.
                debug!(skipped, "oracle ws subscriber lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
