//! Shared client + wire types for the **oracle-service** (`services/oracle-service`).
//!
//! oracle-service is the single internal holder of the one Pyth Hermes SSE
//! subscription plus all Pyth caching (live prices + cached/paced realized
//! vol). Every other service reads prices and realized vol through this crate
//! instead of talking to Pyth directly — the one external Pyth connection (and
//! the API key) lives in oracle-service. The keeper keeps a separate direct
//! Hermes path only for the on-chain VAA, which a price cache can't serve.
//!
//! ## Two read paths
//!
//! - **REST** ([`OracleClient::prices`], [`OracleClient::realized_vol`], …) for
//!   per-tick pollers (api-service strike ladder, price-charting APY).
//! - **WS fanout** ([`OracleClient::subscribe`]) returns a push-fed
//!   [`PriceCache`] (the same `pyth-client` type) so a latency-sensitive
//!   consumer (mm-bot's per-RFQ hot path) reads spot locally with the exact
//!   `get_fresh` staleness check it used when it owned the SSE connection.
//!
//! ## Staleness across the hop
//!
//! Each WS price frame carries Pyth's `publish_time_ms` verbatim (the
//! publisher-age half of `get_fresh` stays exact) and the client re-stamps
//! `observed_at = Instant::now()` on arrival (the local-liveness half — a frame
//! arriving *is* a fresh local observation). If either hop (Pyth→oracle or
//! oracle→client) stalls, frames stop arriving and the cache ages out, so
//! `get_fresh` declines exactly as it would have on a direct outage.
//!
//! ## Hard cutover
//!
//! Like `token-info-client`, there is no direct-Pyth fallback in consumers.
//! [`OracleClient::fetch_blocking_until_ready`] retries for a bounded window at
//! boot and then errors so the caller crashes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

pub use pyth_client::{CachedPrice, PriceCache};
pub use protocol_types::PriceFeedId;

// ---- wire DTOs (shared by oracle-service handlers and this client) ---------

/// One feed's latest cached price. `age_ms` is publisher age
/// (`now - publish_time_ms`) — the only age meaningful to a remote caller.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PricePoint {
    pub feed_id: String,
    pub price: f64,
    pub conf: f64,
    pub publish_time_ms: i64,
    pub age_ms: i64,
}

/// `GET /prices` / `GET /snapshot` body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PricesResponse {
    pub as_of_ms: i64,
    pub upstream_healthy: bool,
    pub prices: Vec<PricePoint>,
}

/// One feed's realized-vol result. `sigma` is set on success; `error` carries a
/// reason on failure (insufficient closes, benchmark outage). Consumers apply
/// their own `sigma_fallback` — the oracle never injects one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealizedVolPoint {
    pub feed_id: String,
    pub stable_feed_id: String,
    pub window_days: u32,
    #[serde(default)]
    pub sigma: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /vol/realized` body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealizedVolResponse {
    pub results: Vec<RealizedVolPoint>,
}

/// A WS fanout frame. `price` frames carry Pyth's `publish_time_ms` verbatim;
/// `sent_at_ms` (server wall clock at send) is diagnostics-only and never used
/// for the staleness decision. `status` frames mark upstream connect-state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WsMessage {
    Price {
        feed_id: String,
        price: f64,
        conf: f64,
        publish_time_ms: i64,
        sent_at_ms: i64,
    },
    Status {
        upstream_healthy: bool,
        #[serde(default)]
        reason: Option<String>,
    },
}

// ---- client ----------------------------------------------------------------

/// On-chain identity of the live provider's adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterIds {
    pub adapter_package_id: String,
    pub feed_registry_id: String,
    pub oracle_registry_id: String,
}

/// `GET /oracle/descriptor` — the published form of the oracle switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleDescriptor {
    pub provider: protocol_types::OracleProvider,
    pub adapter_module: String,
    /// Absent when the live provider's adapter is not deployed here.
    #[serde(default)]
    pub adapter: Option<AdapterIds>,
    /// canonical coin type → feed key under the live provider.
    #[serde(default)]
    pub feeds: std::collections::BTreeMap<String, String>,
}

/// `GET /oracle/legs` — the live provider's off-chain payload for one
/// PTB's price legs (SO-346).
///
/// The descriptor says WHICH provider is live; this says WHAT to lay
/// into the transaction: the Hermes accumulator update under Pyth, the
/// signed Crossbar quote bundle under Switchboard. A composer that reads
/// both never names a provider itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum OracleLegsResponse {
    Pyth(PythLegsPayload),
    Switchboard(SwitchboardLegsPayload),
}

/// Pyth's off-chain half: one accumulator update covering every
/// requested feed. The caller still resolves the on-chain
/// `PriceInfoObject`s — those are chain state, not oracle data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythLegsPayload {
    /// Hermes accumulator update, base64.
    pub accumulator_update_b64: String,
    /// canonical coin type → Pyth feed id (hex) covered by the update.
    /// Requested assets with no feed under the live provider are simply
    /// absent (the caller passes `none` legs for them).
    pub feeds: std::collections::BTreeMap<String, String>,
}

/// Switchboard's off-chain half: everything `run_N` + `attest` need
/// beyond chain-resolved object references.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchboardLegsPayload {
    /// Switchboard's own `on_demand` package id (theirs, not our adapter).
    pub switchboard_package_id: String,
    /// The Sui `Queue` object `run_N` validates signing oracles against.
    pub queue_id: String,
    /// canonical coin type → 32-byte feed hash (hex) covered by `quote`.
    pub feed_hashes: std::collections::BTreeMap<String, String>,
    pub quote: SwitchboardQuoteWire,
}

/// One signed Crossbar consensus payload, JSON-safe: byte vectors are
/// hex/base64 and the 18-decimal values are decimal STRINGS — they sit
/// far past 2^53, so a JSON number would silently lose precision in any
/// JS consumer (the browser composer is a planned reader of this).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchboardQuoteWire {
    /// 32-byte feed hashes, hex, parallel with `values` / `values_neg` /
    /// `min_oracle_samples`.
    pub feed_ids: Vec<String>,
    /// 18-decimal fixed-point magnitudes as decimal strings (u128 range).
    pub values: Vec<String>,
    pub values_neg: Vec<bool>,
    pub min_oracle_samples: Vec<u8>,
    /// Per-oracle signatures, base64, parallel with `oracle_ids`.
    pub signatures_b64: Vec<String>,
    pub slot: u64,
    pub timestamp_seconds: u64,
    /// Sui `Oracle` object ids, in signature order.
    pub oracle_ids: Vec<String>,
}

impl SwitchboardQuoteWire {
    /// Decode `feed_ids` to raw 32-byte vectors.
    pub fn feed_id_bytes(&self) -> Result<Vec<Vec<u8>>> {
        self.feed_ids
            .iter()
            .map(|h| {
                let bytes = hex::decode(h.trim().trim_start_matches("0x"))
                    .with_context(|| format!("feed id {h:?} is not hex"))?;
                if bytes.len() != 32 {
                    return Err(anyhow!("feed id {h:?} is {} bytes; expected 32", bytes.len()));
                }
                Ok(bytes)
            })
            .collect()
    }

    /// Parse `values` back to u128 magnitudes.
    pub fn values_u128(&self) -> Result<Vec<u128>> {
        self.values
            .iter()
            .map(|v| v.parse::<u128>().with_context(|| format!("parsing quote value {v:?}")))
            .collect()
    }

    /// Decode `signatures_b64` to raw bytes.
    pub fn signature_bytes(&self) -> Result<Vec<Vec<u8>>> {
        use base64::Engine;
        self.signatures_b64
            .iter()
            .map(|s| {
                base64::engine::general_purpose::STANDARD
                    .decode(s.trim())
                    .context("decoding base64 quote signature")
            })
            .collect()
    }
}

/// Async client over oracle-service's internal REST + WS API.
#[derive(Debug, Clone)]
pub struct OracleClient {
    base_url: String,
    http: reqwest::Client,
}

impl OracleClient {
    /// `base_url` is the service's internal base, e.g. `http://oracle-service:9013`.
    /// A trailing slash is fine.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// All cached feeds.
    pub async fn prices(&self) -> Result<Vec<PricePoint>> {
        Ok(self
            .get_json::<PricesResponse>("/prices", "GET /prices")
            .await?
            .prices)
    }

    /// One feed's latest cached price. Errors (404) if never observed.
    /// The live oracle provider and its on-chain adapter identity
    /// (SO-335). This is the single read that tells a PTB composer which
    /// provider's price legs to build.
    pub async fn descriptor(&self) -> Result<OracleDescriptor> {
        let url = format!("{}/oracle/descriptor", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        resp.json().await.context("decoding /oracle/descriptor")
    }

    /// The live provider's off-chain payload for `assets`' price legs
    /// (SO-346). `assets` are canonical coin types; ones without a feed
    /// under the live provider are absent from the response's coverage
    /// map rather than an error (the caller passes `none` legs).
    pub async fn legs(&self, assets: &[String]) -> Result<OracleLegsResponse> {
        if assets.is_empty() {
            return Err(anyhow!("legs() called with no assets"));
        }
        self.get_json::<OracleLegsResponse>(
            &format!("/oracle/legs?assets={}", assets.join(",")),
            "GET /oracle/legs",
        )
        .await
    }

    /// Spot for a coin type, without the caller ever naming a provider's
    /// feed key. Prefer this over [`OracleClient::price`] in new code —
    /// it is what makes a consumer survive a provider switch untouched.
    pub async fn price_for_asset(&self, coin_type: &str) -> Result<PricePoint> {
        let url = format!("{}/prices/by-asset/{}", self.base_url, coin_type);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        resp.json().await.context("decoding /prices/by-asset")
    }

    pub async fn price(&self, feed: PriceFeedId) -> Result<PricePoint> {
        self.get_json::<PricePoint>(
            &format!("/prices/{}", feed.to_hex()),
            "GET /prices/:feed",
        )
        .await
    }

    /// Annualized realized vol for one feed over `window_days`. Errors if the
    /// service couldn't compute it (caller applies its own fallback).
    pub async fn realized_vol(&self, feed: PriceFeedId, window_days: u32) -> Result<f64> {
        let mut out = self.realized_vol_bulk(&[feed], window_days).await?;
        out.remove(&feed)
            .unwrap_or_else(|| Err(anyhow!("no sigma returned for {feed}")))
    }

    /// Realized vol for many feeds in one request (mirrors
    /// `pyth_client::BenchmarkVol::realized_sigma_bulk`'s per-feed result so one
    /// feed's gap doesn't sink the others).
    pub async fn realized_vol_bulk(
        &self,
        feeds: &[PriceFeedId],
        window_days: u32,
    ) -> Result<HashMap<PriceFeedId, Result<f64>>> {
        if feeds.is_empty() {
            return Ok(HashMap::new());
        }
        let feeds_param = feeds
            .iter()
            .map(|f| f.to_hex())
            .collect::<Vec<_>>()
            .join(",");
        let path = format!("/vol/realized?feeds={feeds_param}&window_days={window_days}");
        let resp = self
            .get_json::<RealizedVolResponse>(&path, "GET /vol/realized")
            .await?;
        let mut out = HashMap::new();
        for point in resp.results {
            let Ok(feed) = PriceFeedId::from_hex(&point.feed_id) else {
                continue;
            };
            let val = match (point.sigma, point.error) {
                (Some(s), _) => Ok(s),
                (None, Some(e)) => Err(anyhow!("{e}")),
                (None, None) => Err(anyhow!("oracle returned no sigma for {feed}")),
            };
            out.insert(feed, val);
        }
        Ok(out)
    }

    /// Combined prices snapshot (boot-readiness probe).
    pub async fn snapshot(&self) -> Result<PricesResponse> {
        self.get_json::<PricesResponse>("/snapshot", "GET /snapshot")
            .await
    }

    /// Like [`snapshot`](Self::snapshot) but retries for a bounded window —
    /// oracle-service may still be warming its cache at boot. After the window
    /// the last error is returned so the caller crashes (no Pyth fallback).
    pub async fn fetch_blocking_until_ready(
        &self,
        max_attempts: u32,
        delay: Duration,
    ) -> Result<PricesResponse> {
        let mut last_err = None;
        for attempt in 1..=max_attempts {
            match self.snapshot().await {
                Ok(snap) => return Ok(snap),
                Err(e) => {
                    warn!(base = %self.base_url, attempt, max_attempts, error = %e, "oracle-service not ready; retrying");
                    last_err = Some(e);
                    if attempt < max_attempts {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("oracle-service unreachable")))
            .with_context(|| {
                format!(
                    "oracle-service at {} unreachable after {max_attempts} attempts",
                    self.base_url
                )
            })
    }

    /// Open a WS subscription to the fanout and return a [`PriceCache`] that a
    /// background task keeps current from the stream, plus the task handle.
    /// Reading the cache via `get_fresh` is identical to owning the SSE
    /// connection directly. The task reconnects with backoff for the cache's
    /// lifetime; drop the handle's cache (all clones) to stop it.
    pub fn subscribe(&self) -> (PriceCache, JoinHandle<()>) {
        let cache = PriceCache::new();
        let ws_url = http_to_ws(&self.base_url);
        let handle = tokio::spawn(run_ws(ws_url, cache.clone()));
        (cache, handle)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        op: &'static str,
    ) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let resp = observability::client::instrumented("oracle-service", op, |h| {
            self.http.get(&url).headers(h).send()
        })
        .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GET {url} -> {status}: {body}"));
        }
        resp.json::<T>().await.with_context(|| format!("decoding {url}"))
    }
}

/// Map an `http(s)://host[:port]` base to the `ws(s)://host[:port]/ws` endpoint.
fn http_to_ws(base: &str) -> String {
    let stripped = base.trim_end_matches('/');
    let with_scheme = if let Some(rest) = stripped.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = stripped.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{stripped}")
    };
    format!("{with_scheme}/ws")
}

const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const HEALTHY_DURATION: Duration = Duration::from_secs(5);

/// Hold the WS connection open, re-stamping each price frame into `cache`.
/// Reconnects with exponential backoff (reset after a healthy session),
/// mirroring `pyth_client::stream`.
async fn run_ws(ws_url: String, cache: PriceCache) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        match pump(&ws_url, &cache).await {
            Ok(()) => {
                debug!(url = %ws_url, "oracle ws closed cleanly; reconnecting");
                backoff = MIN_BACKOFF;
            }
            Err(e) => {
                if started.elapsed() >= HEALTHY_DURATION {
                    info!(url = %ws_url, error = %format!("{e:#}"), "oracle ws interrupted after healthy session; reconnecting");
                    backoff = MIN_BACKOFF;
                } else {
                    warn!(url = %ws_url, error = %format!("{e:#}"), "oracle ws errored; reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

async fn pump(ws_url: &str, cache: &PriceCache) -> Result<()> {
    info!(url = %ws_url, "opening oracle ws subscription");
    let (mut stream, _resp) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("connecting oracle ws")?;
    while let Some(msg) = stream.next().await {
        use tokio_tungstenite::tungstenite::Message;
        let msg = msg.context("reading oracle ws frame")?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        match serde_json::from_str::<WsMessage>(&text) {
            Ok(WsMessage::Price {
                feed_id,
                price,
                conf,
                publish_time_ms,
                ..
            }) => {
                let Ok(id) = PriceFeedId::from_hex(&feed_id) else {
                    continue;
                };
                cache.insert(
                    id,
                    CachedPrice {
                        price,
                        conf,
                        publish_time_ms,
                        observed_at: Instant::now(),
                    },
                );
            }
            // Upstream went stale: stop re-stamping so existing entries age out
            // of `get_fresh` naturally. (We simply don't insert anything.)
            Ok(WsMessage::Status {
                upstream_healthy, ..
            }) => {
                debug!(upstream_healthy, "oracle ws status frame");
            }
            Err(e) => debug!(error = %e, "oracle ws: unparseable frame"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_derivation() {
        assert_eq!(http_to_ws("http://oracle-service:9013"), "ws://oracle-service:9013/ws");
        assert_eq!(http_to_ws("https://x.com/prod/oracle/"), "wss://x.com/prod/oracle/ws");
    }

    #[test]
    fn legs_response_roundtrips_both_providers() {
        // The tag is the config/descriptor spelling, so the two halves of
        // the switch can never disagree on naming.
        let sw = OracleLegsResponse::Switchboard(SwitchboardLegsPayload {
            switchboard_package_id: "0xea".into(),
            queue_id: "0xe6".into(),
            feed_hashes: [("0x1::a::A".to_string(), "ab".repeat(32))].into(),
            quote: SwitchboardQuoteWire {
                feed_ids: vec!["ab".repeat(32)],
                values: vec!["63456010000000000000000".into()],
                values_neg: vec![false],
                min_oracle_samples: vec![1],
                signatures_b64: vec!["NMbd".into()],
                slot: 42,
                timestamp_seconds: 1_785_700_471,
                oracle_ids: vec!["0x11".into()],
            },
        });
        let j = serde_json::to_string(&sw).unwrap();
        assert!(j.contains("\"provider\":\"switchboard\""), "{j}");
        let back: OracleLegsResponse = serde_json::from_str(&j).unwrap();
        assert!(matches!(back, OracleLegsResponse::Switchboard(_)));

        let py = OracleLegsResponse::Pyth(PythLegsPayload {
            accumulator_update_b64: "UE5BVQ==".into(),
            feeds: Default::default(),
        });
        let j = serde_json::to_string(&py).unwrap();
        assert!(j.contains("\"provider\":\"pyth\""), "{j}");
    }

    #[test]
    fn quote_wire_decodes_to_submit_shapes() {
        let wire = SwitchboardQuoteWire {
            feed_ids: vec!["ab".repeat(32), format!("0x{}", "cd".repeat(32))],
            values: vec!["63456010000000000000000".into(), "1".into()],
            values_neg: vec![false, false],
            min_oracle_samples: vec![1, 1],
            signatures_b64: vec![base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                [7u8; 64],
            )],
            slot: 1,
            timestamp_seconds: 2,
            oracle_ids: vec!["0x11".into()],
        };
        let feeds = wire.feed_id_bytes().unwrap();
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0], vec![0xab; 32]);
        assert_eq!(feeds[1], vec![0xcd; 32]); // 0x prefix tolerated
        assert_eq!(
            wire.values_u128().unwrap(),
            vec![63_456_010_000_000_000_000_000u128, 1]
        );
        assert_eq!(wire.signature_bytes().unwrap(), vec![vec![7u8; 64]]);

        // A truncated hash is an error, not a silently short vector —
        // run_N would abort on chain with nothing useful.
        let mut bad = wire.clone();
        bad.feed_ids = vec!["abcd".into()];
        assert!(bad.feed_id_bytes().unwrap_err().to_string().contains("expected 32"));
    }

    #[test]
    fn ws_message_roundtrips() {
        let m = WsMessage::Price {
            feed_id: "ab".repeat(32),
            price: 1.5,
            conf: 0.01,
            publish_time_ms: 1700,
            sent_at_ms: 1701,
        };
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"t\":\"price\""));
        let _back: WsMessage = serde_json::from_str(&j).unwrap();
        let s: WsMessage = serde_json::from_str(r#"{"t":"status","upstream_healthy":false}"#).unwrap();
        matches!(s, WsMessage::Status { upstream_healthy: false, .. });
    }
}
