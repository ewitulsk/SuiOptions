//! `GET /buckets` — the bucket catalog the frontend renders from.
//!
//! # Response shape
//!
//! Buckets are grouped into **series** keyed by `(asset_type,
//! settlement_type, expiry_ms)`. Within a series, every bucket is a
//! distinct strike — that's the level a user picks from when composing a
//! trade. The series-level fields collapse what's redundant across all
//! buckets in the same expiry; the bucket-level fields are what differ.
//!
//! Numeric fields exist in two flavors:
//!
//! - **Scaled** (`f64`) — strike/written/cursor divided by the relevant
//!   token's decimals. Suitable for direct display (`$85,000.00`,
//!   `4.2 BTC`). Resolution is fine enough for any realistic option
//!   market; consumers that need exact-integer arithmetic should rebuild
//!   from `*_raw`.
//! - **Raw** (`string`) — the on-chain integer in atomic units, sent as
//!   a string so we never lose u64/u128 precision through JSON. Required
//!   when building a transaction off this data.
//!
//! Symbols and decimals are resolved from `deployments.json` at api-service
//! startup. A bucket whose coin type isn't in the catalog falls back to
//! the raw Move type string as its `*_symbol`, with `*_decimals: null` and
//! `null` scaled fields, so the bucket is still visible but flagged as
//! un-renderable.
//!
//! # Example
//!
//! ```json
//! {
//!   "series": [
//!     {
//!       "asset_symbol": "TBTC",
//!       "asset_decimals": 8,
//!       "settlement_symbol": "TUSDC",
//!       "settlement_decimals": 6,
//!       "option_type": "call",
//!       "expiry_ms": 1782345600000,
//!       "expiry_iso": "2026-06-26T08:00:00Z",
//!       "buckets": [
//!         {
//!           "bucket_id": "0x9c2b…42a1",
//!           "strike": 85000.0,
//!           "strike_raw": "85000000000",
//!           "call_coin_type": "0x…::call_0::CALL_0",
//!           "option_coin_type": "0x…::call_0::CALL_0",
//!           "total_written": 4.2,
//!           "total_written_raw": "420000000",
//!           "exercise_cursor": 1.0,
//!           "exercise_cursor_raw": "100000000",
//!           "fill_pct": 23.8
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};

use protocol_types::ids::ObjectId;

use crate::bucket::Bucket;
use crate::catalog::TokenCatalog;
use crate::state::{AppState, IndexerBucket};

#[derive(Serialize)]
pub struct BucketDto {
    /// `null` for a **listed but not-yet-created** strike (SO-400): the board
    /// advertises the ladder around spot, and a bucket only becomes a real
    /// object when someone writes at that strike. Consumers that need an
    /// object id (PTB inputs, `/buckets/:id`) must skip these; the frontend
    /// routes them through `create_bucket_any_strike` first.
    pub bucket_id: Option<String>,
    /// Strike in USD-equivalent whole units. `null` if either decimals
    /// lookup failed. Post-SO-55: real ratio is
    /// `strike_raw / 10^strike_scale × 10^(under_dec − settle_dec)` —
    /// see `strike_raw_to_usd`.
    pub strike: Option<f64>,
    /// Raw on-chain u128 strike. Real ratio = `strike_raw / 10^strike_scale`.
    pub strike_raw: String,
    /// Fully-qualified type of this bucket's fungible option coin
    /// (`Coin<call_coin_type>`). The frontend uses it as the `Call` type arg
    /// when exercising, and to match the user's owned option coins to buckets.
    /// Kept populated for both calls and puts (back-compat); put consumers
    /// should prefer `option_coin_type`.
    pub call_coin_type: String,
    /// Generic per-bucket option coin type. Equals `call_coin_type` for calls
    /// and the per-bucket put coin for puts. Emitted alongside `call_coin_type`
    /// so kind-aware consumers don't have to special-case the field name.
    pub option_coin_type: String,
    /// On-chain `strike_scale` (0..=9). Exposed so frontends can recompute
    /// the USD strike independently if they want.
    pub strike_scale: u8,
    /// Total underlying written into the bucket, in underlying whole units.
    /// `null` if asset decimals are unknown.
    pub total_written: Option<f64>,
    pub total_written_raw: String,
    /// Exercise cursor in underlying whole units. `null` if unknown decimals.
    pub exercise_cursor: Option<f64>,
    pub exercise_cursor_raw: String,
    /// `100 * exercise_cursor / total_written`. `0.0` when nothing's been
    /// written yet (avoids a NaN); `null` when underlying decimals are
    /// unknown so the math is unsafe.
    pub fill_pct: Option<f64>,
    /// Admin-set freeze on new writes. The writer screen filters these
    /// out entirely; the future positions dashboard will badge owned
    /// positions in an invalidated bucket. See SO-69.
    pub invalidated: bool,
    /// DeepBook pool trading this bucket's call coin (SO-153). `null` until
    /// a venue is created on-chain.
    pub deepbook_pool_id: Option<String>,
    /// Whether the DeepBook trade panel should be live: a pool exists, the
    /// bucket isn't cleaned, and it hasn't expired. `invalidated` does NOT
    /// gate this — invalidation freezes new mints, not secondary trading.
    pub tradeable: bool,
    /// Write/RFQ liveness (SO-394): not cleaned, not expired, not
    /// invalidated. A pool-less any-strike bucket is RFQ-tradeable from
    /// birth — pools are a graduation, not a birthright.
    pub rfq_tradeable: bool,
    /// Alias of `tradeable` under the kind-aware split; prefer this in new
    /// consumers.
    pub pool_tradeable: bool,
}

#[derive(Serialize)]
pub struct SeriesDto {
    /// Friendly symbol from `deployments.json` (`"TBTC"`) — or the raw Move
    /// type string when the coin type isn't in the catalog.
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    /// Full Move coin type (e.g. `0xtp::tbtc::TBTC`). Used by the
    /// frontend as the `Underlying` type arg when building exercise PTBs.
    pub asset_coin_type: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_coin_type: String,
    /// `"call"` | `"put"`. Series are grouped by `(asset, settlement, expiry,
    /// option_type)`, so every bucket within a series shares this kind.
    pub option_type: String,
    /// Unix millis. Sent as a number — Date.now()-style. Safe in JS as
    /// long as expiries stay before year 2255.
    pub expiry_ms: i64,
    /// Pre-formatted ISO-8601 UTC string for direct display.
    pub expiry_iso: String,
    pub buckets: Vec<BucketDto>,
}

#[derive(Serialize)]
pub struct BucketsResponse {
    pub series: Vec<SeriesDto>,
}

#[derive(Deserialize, Default)]
pub struct ListBucketsParams {
    /// Drop series whose expiry is already in the past. Opt-in (defaults to
    /// `false`) so the admin/monitoring and dashboard views — which still need
    /// expired series — keep their full catalog; the trade picker passes
    /// `?exclude_expired=true`.
    #[serde(default)]
    pub exclude_expired: bool,
    /// Drop admin-invalidated buckets (and series left empty). Opt-in
    /// (defaults to `false`) so admin/monitoring views still see invalidated
    /// buckets; the trade picker passes `?exclude_invalidated=true` to hide
    /// frozen strikes server-side rather than filtering client-side.
    #[serde(default)]
    pub exclude_invalidated: bool,
    /// Serve the **full historical catalog** instead of the listed board
    /// (SO-400). By default `/buckets` returns only the board expiries —
    /// active week, next week, next two month-ends — plus the ladder around
    /// spot; admin and monitoring views that need every bucket ever created
    /// pass `?all=true` and get the pre-ladder behaviour verbatim.
    #[serde(default)]
    pub all: bool,
}

pub async fn list_buckets(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListBucketsParams>,
) -> Result<Json<BucketsResponse>, StatusCode> {
    // One fetch returns calls *and* puts (the indexer's `buckets` query has no
    // kind filter); `group_into_series` then splits them into separate series by
    // `option_type`, so both kinds surface from this single unified endpoint.
    let active = state
        .indexer
        .buckets(true, None, None, None)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer buckets query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let active = active.into_iter().map(into_local_bucket).collect();
    let now_ms = Utc::now().timestamp_millis();
    let mut series = group_into_series(active, &state.catalog, now_ms);
    if !params.all {
        series = build_board(&state, series, now_ms).await;
    }
    if params.exclude_expired {
        series.retain(|s| s.expiry_ms > now_ms);
    }
    if params.exclude_invalidated {
        retain_non_invalidated(&mut series);
    }
    Ok(Json(BucketsResponse { series }))
}

/// Drop admin-invalidated buckets from every series and remove series left
/// with no buckets. Used by `?exclude_invalidated=true`.
fn retain_non_invalidated(series: &mut Vec<SeriesDto>) {
    for s in series.iter_mut() {
        s.buckets.retain(|b| !b.invalidated);
    }
    series.retain(|s| !s.buckets.is_empty());
}

/// `GET /buckets/:bucket_id` — one bucket's cursor/queue state.
///
/// A focused, cheaply-pollable single-row view: the writer composer's
/// "YOUR PLACE IN THE QUEUE" queue wave reads `exercise_cursor` (how far
/// FIFO assignment has eaten into the bucket) and `queued_ahead` (written
/// underlying sitting ahead of the cursor, still unassigned) every few
/// seconds without re-pulling the whole `/buckets` catalog. The numbers
/// are the same ones `/buckets` exposes per-bucket; this is just the
/// narrow, frequently-refreshed projection.
#[derive(Serialize)]
pub struct BucketDetailDto {
    pub bucket_id: String,
    /// Friendly symbol from the catalog; raw Move type when unknown.
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    pub asset_coin_type: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_coin_type: String,
    /// Strike in USD whole units. `null` if either decimals lookup failed.
    pub strike: Option<f64>,
    pub strike_raw: String,
    pub strike_scale: u8,
    pub expiry_ms: i64,
    /// Total underlying written, in whole units. `null` if asset decimals
    /// are unknown.
    pub total_written: Option<f64>,
    pub total_written_raw: String,
    /// Exercise cursor in whole units. `null` if asset decimals unknown.
    pub exercise_cursor: Option<f64>,
    pub exercise_cursor_raw: String,
    /// Underlying written but not yet assigned: `total_written -
    /// exercise_cursor`, in whole units. `null` if asset decimals unknown.
    pub queued_ahead: Option<f64>,
    pub queued_ahead_raw: String,
    /// `100 * exercise_cursor / total_written`. `0.0` when nothing's been
    /// written; `null` when underlying decimals are unknown.
    pub fill_pct: Option<f64>,
    /// Fully-qualified type of the bucket's fungible option coin — the
    /// `Call` type argument for write/bid/exercise PTBs. Populated for both
    /// calls and puts (back-compat).
    pub call_coin_type: String,
    /// Generic per-bucket option coin type (equals `call_coin_type` for calls,
    /// the put coin for puts). The api-service-client's `BucketDetailWire`
    /// prefers this field.
    pub option_coin_type: String,
    /// `"call"` | `"put"`. The api-service-client reads this to set `is_put`.
    pub option_kind: String,
    /// DeepBook pool for this bucket (SO-153); `null` if none.
    pub deepbook_pool_id: Option<String>,
    /// Pool exists, bucket not cleaned, not expired (see `/buckets`).
    pub tradeable: bool,
    /// See `/buckets`: write/RFQ liveness (pool-less buckets included).
    pub rfq_tradeable: bool,
    /// Alias of `tradeable` (kind-aware split).
    pub pool_tradeable: bool,
}

pub async fn get_bucket(
    State(state): State<Arc<AppState>>,
    Path(bucket_id): Path<String>,
) -> Result<Json<BucketDetailDto>, StatusCode> {
    let id = ObjectId::from_hex(&bucket_id).map_err(|_| StatusCode::NOT_FOUND)?;
    let bucket = state.indexer.bucket(id).await.map_err(|e| {
        tracing::warn!(error = %e, "indexer bucket query failed");
        StatusCode::BAD_GATEWAY
    })?;
    // Cleaned buckets are settled-and-gone — treat them as absent so the
    // queue wave stops polling a stale id rather than rendering dead state.
    let bucket = bucket.filter(|b| !b.cleaned).ok_or(StatusCode::NOT_FOUND)?;
    let now_ms = Utc::now().timestamp_millis();
    Ok(Json(detail_dto_from(&bucket, &state.catalog, now_ms)))
}

/// Pure projection — split out so the queued-ahead math is unit-testable
/// without an indexer.
fn detail_dto_from(b: &IndexerBucket, catalog: &TokenCatalog, now_ms: i64) -> BucketDetailDto {
    let asset_meta = catalog.lookup(b.asset_type.as_str());
    let settle_meta = catalog.lookup(b.settlement_type.as_str());
    let asset_decimals = asset_meta.map(|m| m.decimals);
    let settle_decimals = settle_meta.map(|m| m.decimals);

    let strike = match (asset_decimals, settle_decimals) {
        (Some(u), Some(s)) => Some(strike_raw_to_usd(b.strike, b.strike_scale, u, s)),
        _ => None,
    };
    let total_written = asset_decimals.map(|d| scale_u128(b.total_written, d));
    let exercise_cursor = asset_decimals.map(|d| scale_u128(b.exercise_cursor, d));
    // Cursor should never run past written, but saturate so a transiently
    // inconsistent indexer read can't underflow-panic the poller.
    let queued_ahead_raw = b.total_written.saturating_sub(b.exercise_cursor);
    let queued_ahead = asset_decimals.map(|d| scale_u128(queued_ahead_raw, d));
    let fill_pct = match (total_written, exercise_cursor) {
        (Some(w), Some(c)) if w > 0.0 => Some(100.0 * c / w),
        (Some(_), Some(_)) => Some(0.0),
        _ => None,
    };

    BucketDetailDto {
        bucket_id: b.bucket_id.to_hex(),
        asset_symbol: asset_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| b.asset_type.as_str().to_string()),
        asset_decimals,
        asset_coin_type: b.asset_type.to_canonical(),
        settlement_symbol: settle_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| b.settlement_type.as_str().to_string()),
        settlement_decimals: settle_decimals,
        settlement_coin_type: b.settlement_type.to_canonical(),
        strike,
        strike_raw: b.strike.to_string(),
        strike_scale: b.strike_scale,
        expiry_ms: b.expiry_ms as i64,
        total_written,
        total_written_raw: b.total_written.to_string(),
        exercise_cursor,
        exercise_cursor_raw: b.exercise_cursor.to_string(),
        queued_ahead,
        queued_ahead_raw: queued_ahead_raw.to_string(),
        fill_pct,
        call_coin_type: b.call_type.to_canonical(),
        option_coin_type: b.call_type.to_canonical(),
        option_kind: b.option_kind.clone(),
        deepbook_pool_id: b.deepbook_pool_id.as_ref().map(|p| p.to_hex()),
        tradeable: is_tradeable(b.deepbook_pool_id.is_some(), b.cleaned, b.expiry_ms, now_ms),
        rfq_tradeable: is_rfq_tradeable(b.cleaned, b.invalidated, b.expiry_ms, now_ms),
        pool_tradeable: is_tradeable(b.deepbook_pool_id.is_some(), b.cleaned, b.expiry_ms, now_ms),
    }
}

/// SO-153 tradeable gate. `invalidated` intentionally absent — it freezes
/// new mints, not secondary-market transfers of already-minted coins.
fn is_tradeable(has_pool: bool, cleaned: bool, expiry_ms: u64, now_ms: i64) -> bool {
    has_pool && !cleaned && (expiry_ms as i64) > now_ms
}

/// Write/RFQ liveness (SO-394): the mint path is open — no pool required.
fn is_rfq_tradeable(cleaned: bool, invalidated: bool, expiry_ms: u64, now_ms: i64) -> bool {
    !cleaned && !invalidated && (expiry_ms as i64) > now_ms
}

// ─────────────────────────── /buckets/spec ───────────────────────────

#[derive(Deserialize)]
pub struct SpecQuery {
    /// Catalog symbol (`TBTC`) or full coin type.
    pub underlying: String,
    pub settlement: String,
    pub expiry_ms: u64,
    /// Raw u128 strike (scaled by `strike_scale`), decimal string.
    pub strike_raw: String,
    pub strike_scale: u8,
    /// `"call"` (default) | `"put"`.
    #[serde(default)]
    pub option_type: Option<String>,
}

#[derive(Serialize)]
pub struct SpecDto {
    /// Whether a bucket for this normalized spec already exists on-chain
    /// (per the indexer). `false` ⇒ the frontend prepends the sponsored
    /// `create_bucket_any_strike` leg (or runs the create-first two-step).
    pub exists: bool,
    pub bucket_id: Option<String>,
    /// The spec's option coin type — a pure function of the spec
    /// (`OptionCall<U, S, D0..D9>` under the byte-marker encoding), valid
    /// whether or not the bucket exists yet. `null` when the options
    /// package id is unknown to this deployment.
    pub option_coin_type: Option<String>,
    /// Canonical strike: `sig / 10^exp` (trailing zeros stripped).
    pub normalized_sig: String,
    pub normalized_exp: u8,
    /// Creation requires minute-aligned expiries.
    pub expiry_aligned: bool,
    pub underlying_coin_type: String,
    pub settlement_coin_type: String,
}

/// Resolve an economic spec to its (maybe not-yet-created) bucket: the
/// any-strike UI asks this before deciding between write-to-existing and
/// create-on-write.
pub async fn bucket_spec(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SpecQuery>,
) -> Result<Json<SpecDto>, StatusCode> {
    let is_put = match q.option_type.as_deref() {
        None | Some("call") => false,
        Some("put") => true,
        Some(_) => return Err(StatusCode::BAD_REQUEST),
    };
    let strike: u128 = q.strike_raw.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let (sig, exp) = normalize_strike(strike, q.strike_scale).ok_or(StatusCode::BAD_REQUEST)?;
    let expiry_aligned = q.expiry_ms % 60_000 == 0 && q.expiry_ms / 60_000 <= u32::MAX as u64;

    let resolve = |input: &str| -> String {
        state
            .catalog
            .by_symbol(input)
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string())
    };
    let u_type = protocol_types::asset::canonicalize_move_type(&resolve(&q.underlying));
    let s_type = protocol_types::asset::canonicalize_move_type(&resolve(&q.settlement));

    // Narrow indexer query: same pair + expiry, then match normalized strike
    // + kind locally. The indexer stores chain-form `TypeName`s (bare
    // addresses) and its filters string-match verbatim — so the filter args
    // must be chain-form, NOT the canonical 0x-form we emit to clients
    // (the /buckets/spec `exists:false`-for-existing-bucket bug).
    let chain_form = |t: &str| t.strip_prefix("0x").unwrap_or(t).to_string();
    let candidates = state
        .indexer
        .buckets(
            /* active_only */ false,
            Some(&protocol_types::asset::AssetType::new(chain_form(&u_type))),
            Some(&protocol_types::asset::AssetType::new(chain_form(&s_type))),
            Some(q.expiry_ms),
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer spec query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let hit = candidates.into_iter().find(|b| {
        let kind_matches = (b.option_kind == "put") == is_put;
        kind_matches && !b.cleaned && normalize_strike(b.strike, b.strike_scale) == Some((sig, exp))
    });

    let option_coin_type = state
        .options_package
        .as_deref()
        .map(|pkg| option_coin_type_str(pkg, is_put, &u_type, &s_type, q.expiry_ms, sig, exp));
    Ok(Json(SpecDto {
        exists: hit.is_some(),
        bucket_id: hit.map(|b| b.bucket_id.to_hex()),
        option_coin_type,
        normalized_sig: sig.to_string(),
        normalized_exp: exp,
        expiry_aligned,
        underlying_coin_type: u_type,
        settlement_coin_type: s_type,
    }))
}

/// Mirror of `option_coin::normalize_strike`: strip trailing zeros; the
/// significand must fit the encoding's u40 field.
fn normalize_strike(strike: u128, strike_scale: u8) -> Option<(u64, u8)> {
    if strike == 0 {
        return None;
    }
    let (mut sig, mut exp) = (strike, strike_scale);
    while sig % 10 == 0 && exp > 0 {
        sig /= 10;
        exp -= 1;
    }
    (sig <= 0xFF_FFFF_FFFF).then_some((sig as u64, exp))
}

/// Canonical `OptionCall<U, S, D0..D9>` (or put) type literal for a spec —
/// byte-compatible with `sui_tx::tx::option_coin` and the on-chain builder:
/// minutes u32 ‖ sig u40 ‖ exp u8 as byte markers from `enc0`/`enc1`.
fn option_coin_type_str(
    package: &str,
    is_put: bool,
    u_type: &str,
    s_type: &str,
    expiry_ms: u64,
    sig: u64,
    exp: u8,
) -> String {
    let pkg = protocol_types::asset::canonicalize_move_type(&format!("{package}::x::X"));
    let pkg = pkg
        .split_once("::")
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| package.into());
    let minutes = (expiry_ms / 60_000) as u32;
    let mut bytes = minutes.to_be_bytes().to_vec();
    bytes.extend_from_slice(&sig.to_be_bytes()[3..]);
    bytes.push(exp);
    let markers: Vec<String> = bytes
        .iter()
        .map(|b| {
            let module = if *b < 0x80 { "enc0" } else { "enc1" };
            format!("{pkg}::{module}::B{b:02X}")
        })
        .collect();
    let root = if is_put { "OptionPut" } else { "OptionCall" };
    format!(
        "{pkg}::option_coin::{root}<{u_type},{s_type},{}>",
        markers.join(",")
    )
}

// ─────────────────────────── strike ladder (SO-400) ───────────────────────────

/// Spot + σ for one configured pair, resolved once and reused across every
/// expiry on the board.
struct LadderInputs {
    asset_ct: String,
    settlement_ct: String,
    asset_symbol: String,
    settlement_symbol: String,
    asset_decimals: u8,
    settlement_decimals: u8,
    /// Settlement-per-underlying cross, not the raw USD price: a series
    /// settles in TUSDC, so the strike axis is denominated in it.
    spot: f64,
    sigma: f64,
}

/// Resolve a configured pair against the catalog and oracle-service.
///
/// Returns `None` when the pair can't be listed at all (unknown token, no
/// spot). A missing *vol* is not fatal — the pair's `fallback_sigma` keeps
/// the board up through an oracle vol outage.
async fn ladder_inputs(state: &AppState, pair: &crate::ladder::LadderPair) -> Option<LadderInputs> {
    let oracle = state.oracle.as_ref()?;
    let asset_ct = state.catalog.by_symbol(&pair.underlying)?.to_string();
    let settlement_ct = state.catalog.by_symbol(&pair.settlement)?.to_string();
    let asset_meta = state.catalog.lookup(&asset_ct)?.clone();
    let settle_meta = state.catalog.lookup(&settlement_ct)?.clone();

    // Cross = underlying/USD ÷ settlement/USD. For a stablecoin settlement
    // this is within a few bps of the raw USD price, but taking the ratio
    // keeps the ladder honest if the settlement asset ever depegs.
    let under_px = oracle.price_for_asset(&asset_ct).await.ok()?;
    let settle_px = oracle.price_for_asset(&settlement_ct).await.ok()?;
    if !(settle_px.price.is_finite() && settle_px.price > 0.0) {
        return None;
    }
    let spot = under_px.price / settle_px.price;

    let sigma = match asset_meta.pyth_feed_id.as_deref() {
        Some(hex) => match protocol_types::PriceFeedId::from_hex(hex) {
            Ok(feed) => oracle
                .realized_vol(feed, pair.vol_window_days)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        pair = %pair.underlying,
                        error = %e,
                        "realized vol unavailable; using fallback sigma"
                    );
                    pair.fallback_sigma
                }),
            Err(_) => pair.fallback_sigma,
        },
        None => pair.fallback_sigma,
    };

    Some(LadderInputs {
        asset_ct,
        settlement_ct,
        asset_symbol: asset_meta.symbol,
        settlement_symbol: settle_meta.symbol,
        asset_decimals: asset_meta.decimals,
        settlement_decimals: settle_meta.decimals,
        spot,
        sigma: if sigma.is_finite() && sigma > 0.0 {
            sigma
        } else {
            pair.fallback_sigma
        },
    })
}

/// Build the listed board: the configured pairs × [`crate::ladder::expiry_board`],
/// with every real bucket on those expiries merged in.
///
/// Real buckets always win: a strike that exists on-chain is emitted with its
/// object id and true written/cursor state even when it sits off the lattice
/// (the manually-created any-strike buckets are exactly this case), and the
/// synthetic entry for that strike is suppressed so a strike never appears
/// twice.
///
/// Real series *off* the board are dropped — that's the point of the board,
/// and `?all=true` is the escape hatch for admin and monitoring views.
async fn build_board(
    state: &AppState,
    real: Vec<SeriesDto>,
    now_ms: i64,
) -> Vec<SeriesDto> {
    let board = crate::ladder::expiry_board(now_ms);
    // Index the real series so each board slot can claim its own.
    let mut by_key: BTreeMap<SeriesKey, SeriesDto> = real
        .into_iter()
        .map(|s| {
            let key = (
                strip_0x(&s.asset_coin_type),
                strip_0x(&s.settlement_coin_type),
                s.expiry_ms.max(0) as u64,
                s.option_type.clone(),
            );
            (key, s)
        })
        .collect();

    let Some(package) = state.options_package.as_deref() else {
        // No options package ⇒ we can't name the option coin of a strike that
        // doesn't exist yet, so there is nothing to synthesize.
        return by_key.into_values().collect();
    };

    let mut out: Vec<SeriesDto> = Vec::new();
    for pair in &state.ladder_pairs {
        let Some(inputs) = ladder_inputs(state, pair).await else {
            tracing::warn!(
                underlying = %pair.underlying,
                settlement = %pair.settlement,
                "ladder inputs unavailable; listing existing buckets only"
            );
            continue;
        };
        let option_type = if pair.is_put() { "put" } else { "call" };

        for expiry_ms in &board {
            let expiry = *expiry_ms as u64;
            let key = (
                strip_0x(&inputs.asset_ct),
                strip_0x(&inputs.settlement_ct),
                expiry,
                option_type.to_string(),
            );
            let mut series = by_key.remove(&key).unwrap_or_else(|| SeriesDto {
                asset_symbol: inputs.asset_symbol.clone(),
                asset_decimals: Some(inputs.asset_decimals),
                asset_coin_type: protocol_types::asset::canonicalize_move_type(&inputs.asset_ct),
                settlement_symbol: inputs.settlement_symbol.clone(),
                settlement_decimals: Some(inputs.settlement_decimals),
                settlement_coin_type: protocol_types::asset::canonicalize_move_type(
                    &inputs.settlement_ct,
                ),
                option_type: option_type.to_string(),
                expiry_ms: *expiry_ms,
                expiry_iso: iso_millis(*expiry_ms),
                buckets: Vec::new(),
            });

            let tau = crate::ladder::tau_years(now_ms, *expiry_ms);
            let strikes = crate::ladder::ladder_strikes(pair, inputs.spot, inputs.sigma, tau);
            append_synthetic_strikes(&mut series, &strikes, &inputs, pair, package, expiry);
            sort_by_strike(&mut series.buckets);
            out.push(series);
        }
    }

    // Real series that landed on a board expiry but whose pair isn't
    // configured still belong on the board — dropping them would hide live
    // open interest behind a config omission.
    for (_, s) in by_key {
        if board.contains(&s.expiry_ms) {
            out.push(s);
        }
    }
    out.sort_by(|a, b| {
        (&a.asset_symbol, a.expiry_ms, &a.option_type).cmp(&(
            &b.asset_symbol,
            b.expiry_ms,
            &b.option_type,
        ))
    });
    out
}

/// Push a synthetic [`BucketDto`] for every ladder strike the series doesn't
/// already carry, matching on the *normalized* strike so the two encodings of
/// one economic strike can't both be listed.
fn append_synthetic_strikes(
    series: &mut SeriesDto,
    strikes: &[f64],
    inputs: &LadderInputs,
    pair: &crate::ladder::LadderPair,
    package: &str,
    expiry_ms: u64,
) {
    let existing: std::collections::HashSet<(u64, u8)> = series
        .buckets
        .iter()
        .filter_map(|b| {
            let raw = b.strike_raw.parse::<u128>().ok()?;
            normalize_strike(raw, b.strike_scale)
        })
        .collect();

    for strike in strikes {
        let Some((raw, scale)) = crate::ladder::strike_to_raw(
            *strike,
            inputs.asset_decimals,
            inputs.settlement_decimals,
        ) else {
            continue;
        };
        let Some((sig, exp)) = normalize_strike(raw, scale) else {
            continue;
        };
        if existing.contains(&(sig, exp)) {
            continue;
        }
        let coin_type = option_coin_type_str(
            package,
            pair.is_put(),
            &protocol_types::asset::canonicalize_move_type(&inputs.asset_ct),
            &protocol_types::asset::canonicalize_move_type(&inputs.settlement_ct),
            expiry_ms,
            sig,
            exp,
        );
        series.buckets.push(BucketDto {
            bucket_id: None,
            strike: Some(*strike),
            strike_raw: raw.to_string(),
            call_coin_type: coin_type.clone(),
            option_coin_type: coin_type,
            strike_scale: scale,
            total_written: Some(0.0),
            total_written_raw: "0".to_string(),
            exercise_cursor: Some(0.0),
            exercise_cursor_raw: "0".to_string(),
            fill_pct: Some(0.0),
            invalidated: false,
            deepbook_pool_id: None,
            // A strike that doesn't exist yet has no pool and no secondary
            // market, but the mint path is open the moment it's created —
            // which is exactly what `rfq_tradeable` gates.
            tradeable: false,
            rfq_tradeable: true,
            pool_tradeable: false,
        });
    }
}

fn sort_by_strike(buckets: &mut [BucketDto]) {
    buckets.sort_by(|a, b| {
        a.strike
            .partial_cmp(&b.strike)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Series keys are compared in chain form (bare address, no `0x`) because
/// that's what the indexer stores; the DTOs carry the canonical `0x` form.
fn strip_0x(t: &str) -> String {
    t.strip_prefix("0x").unwrap_or(t).to_string()
}

/// Map the JIT client's bucket into the local `(id, Bucket)` shape that the
/// pure `group_into_series` helper (and its tests) work against.
fn into_local_bucket(b: indexer_graphql::Bucket) -> (protocol_types::ids::ObjectId, Bucket) {
    (
        b.bucket_id,
        Bucket {
            asset_type: b.asset_type,
            settlement_type: b.settlement_type,
            call_type: b.call_type,
            strike: b.strike,
            strike_scale: b.strike_scale,
            expiry_ms: b.expiry_ms,
            total_written: b.total_written,
            exercise_cursor: b.exercise_cursor,
            cleaned: b.cleaned,
            invalidated: b.invalidated,
            option_kind: b.option_kind,
            deepbook_pool_id: b.deepbook_pool_id.map(|p| p.to_hex()),
        },
    )
}

/// `(asset_type, settlement_type, expiry_ms, option_kind)`. Adding the option
/// kind to the key keeps call and put strikes in separate series even when they
/// share an asset/settlement/expiry.
type SeriesKey = (String, String, u64, String);

/// Pure helper — split out so it's unit-testable without spinning up axum.
fn group_into_series(
    buckets: Vec<(protocol_types::ids::ObjectId, Bucket)>,
    catalog: &TokenCatalog,
    now_ms: i64,
) -> Vec<SeriesDto> {
    let mut grouped: BTreeMap<SeriesKey, Vec<(String, Bucket)>> = BTreeMap::new();
    for (id, b) in buckets {
        let key = (
            b.asset_type.as_str().to_string(),
            b.settlement_type.as_str().to_string(),
            b.expiry_ms,
            b.option_kind.clone(),
        );
        grouped.entry(key).or_default().push((id.to_hex(), b));
    }

    grouped
        .into_iter()
        .map(|((asset_ct, settle_ct, expiry_ms, option_kind), members)| {
            let asset_meta = catalog.lookup(&asset_ct);
            let settle_meta = catalog.lookup(&settle_ct);
            let asset_decimals = asset_meta.map(|m| m.decimals);
            let settle_decimals = settle_meta.map(|m| m.decimals);

            let mut bucket_dtos: Vec<BucketDto> = members
                .into_iter()
                .map(|(id_hex, b)| dto_from(id_hex, &b, asset_decimals, settle_decimals, now_ms))
                .collect();
            // Sort strikes ascending for stable UI ordering. Buckets
            // without a known strike (decimals lookup failed) sink to the
            // end deterministically.
            bucket_dtos.sort_by(|a, b| {
                a.strike
                    .partial_cmp(&b.strike)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            SeriesDto {
                asset_symbol: asset_meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| asset_ct.clone()),
                asset_decimals,
                asset_coin_type: protocol_types::asset::canonicalize_move_type(&asset_ct),
                settlement_symbol: settle_meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| settle_ct.clone()),
                settlement_decimals: settle_decimals,
                settlement_coin_type: protocol_types::asset::canonicalize_move_type(&settle_ct),
                option_type: option_kind,
                expiry_ms: expiry_ms as i64,
                expiry_iso: iso_millis(expiry_ms as i64),
                buckets: bucket_dtos,
            }
        })
        .collect()
}

fn dto_from(
    bucket_id: String,
    b: &Bucket,
    asset_decimals: Option<u8>,
    settle_decimals: Option<u8>,
    now_ms: i64,
) -> BucketDto {
    // On-chain strike (post-SO-55) is `strike_raw / 10^strike_scale`
    // settlement-smallest-units per underlying-smallest-unit, so USD
    // conversion needs both decimals AND the per-bucket scale.
    let strike = match (asset_decimals, settle_decimals) {
        (Some(u), Some(s)) => Some(strike_raw_to_usd(b.strike, b.strike_scale, u, s)),
        _ => None,
    };
    let total_written = asset_decimals.map(|d| scale_u128(b.total_written, d));
    let exercise_cursor = asset_decimals.map(|d| scale_u128(b.exercise_cursor, d));
    let fill_pct = match (total_written, exercise_cursor) {
        (Some(w), Some(c)) if w > 0.0 => Some(100.0 * c / w),
        (Some(_), Some(_)) => Some(0.0),
        _ => None,
    };
    BucketDto {
        bucket_id: Some(bucket_id),
        strike,
        strike_raw: b.strike.to_string(),
        call_coin_type: protocol_types::asset::canonicalize_move_type(b.call_type.as_str()),
        option_coin_type: protocol_types::asset::canonicalize_move_type(b.call_type.as_str()),
        strike_scale: b.strike_scale,
        total_written,
        total_written_raw: b.total_written.to_string(),
        exercise_cursor,
        exercise_cursor_raw: b.exercise_cursor.to_string(),
        fill_pct,
        invalidated: b.invalidated,
        deepbook_pool_id: b.deepbook_pool_id.clone(),
        tradeable: is_tradeable(b.deepbook_pool_id.is_some(), b.cleaned, b.expiry_ms, now_ms),
        rfq_tradeable: is_rfq_tradeable(b.cleaned, b.invalidated, b.expiry_ms, now_ms),
        pool_tradeable: is_tradeable(b.deepbook_pool_id.is_some(), b.cleaned, b.expiry_ms, now_ms),
    }
}

fn scale_u128(raw: u128, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

/// Convert an on-chain strike (`raw / 10^strike_scale` settlement-
/// smallest-units per underlying-smallest-unit) into USD. Inverse of
/// `crate::ladder::strike_to_raw`.
pub(crate) fn strike_raw_to_usd(raw: u128, strike_scale: u8, under_dec: u8, settle_dec: u8) -> f64 {
    raw as f64 * 10f64.powi(under_dec as i32 - settle_dec as i32 - strike_scale as i32)
}

pub(crate) fn iso_millis(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::Bucket;
    use protocol_types::asset::AssetType;
    use protocol_types::ids::ObjectId;
    use token_info_client::SupportedToken;

    /// Fixed test clock — comfortably before the fixture expiry.
    const NOW_MS: i64 = 1_700_000_000_000;

    fn tok(ticker: &str, coin_type: &str, decimals: u8) -> SupportedToken {
        SupportedToken {
            coin_type: coin_type.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals,
            pyth_feed_id: None,
            switchboard_feed_id: None,
            enabled: true,
        }
    }

    fn fixture_catalog() -> TokenCatalog {
        TokenCatalog::from_tokens(&[
            tok("TBTC", "0xpkg::tbtc::TBTC", 8),
            tok("TUSDC", "0xpkg::tusdc::TUSDC", 6),
        ])
    }

    fn mk_bucket(strike: u128, strike_scale: u8, written: u128, cursor: u128) -> Bucket {
        Bucket {
            asset_type: AssetType::new("0xpkg::tbtc::TBTC"),
            settlement_type: AssetType::new("0xpkg::tusdc::TUSDC"),
            call_type: AssetType::new("0xpkg::call_0::CALL_0"),
            strike,
            strike_scale,
            expiry_ms: 1_782_345_600_000,
            total_written: written,
            exercise_cursor: cursor,
            cleaned: false,
            invalidated: false,
            option_kind: "call".to_string(),
            deepbook_pool_id: None,
        }
    }

    #[test]
    fn groups_buckets_into_one_series_by_expiry_and_assets() {
        let cat = fixture_catalog();
        // Realistic chain units for TBTC(8)/TUSDC(6):
        //   strike_raw 850 → $85,000, strike_raw 900 → $90,000.
        // The pre-fix code happened to assert against 85_000_000_000 raw
        // → 85_000.0 USD; both numbers were 10^under_dec off in opposite
        // directions and cancelled, masking the bug. See SO-49.
        let buckets = vec![
            (
                ObjectId::new([0xaa; 32]),
                mk_bucket(850, 0, 420_000_000, 100_000_000),
            ),
            (ObjectId::new([0xbb; 32]), mk_bucket(900, 0, 0, 0)),
        ];
        let series = group_into_series(buckets, &cat, NOW_MS);
        assert_eq!(series.len(), 1);
        let s = &series[0];
        assert_eq!(s.asset_symbol, "TBTC");
        assert_eq!(s.asset_decimals, Some(8));
        assert_eq!(s.settlement_symbol, "TUSDC");
        assert_eq!(s.settlement_decimals, Some(6));
        assert_eq!(s.buckets.len(), 2);
        // Sorted ascending by strike.
        assert!(s.buckets[0].strike.unwrap() < s.buckets[1].strike.unwrap());
        let b = &s.buckets[0];
        assert_eq!(b.strike, Some(85_000.0));
        assert_eq!(b.strike_raw, "850");
        assert_eq!(b.total_written, Some(4.2));
        assert_eq!(b.exercise_cursor, Some(1.0));
        assert!((b.fill_pct.unwrap() - 100.0 * 1.0 / 4.2).abs() < 1e-9);
    }

    #[test]
    fn call_and_put_buckets_split_into_separate_series() {
        // Same asset/settlement/expiry but different option_kind must land in
        // two distinct series, each tagged with its option_type.
        let cat = fixture_catalog();
        let mut put = mk_bucket(900, 0, 0, 0);
        put.option_kind = "put".to_string();
        let series = group_into_series(
            vec![
                (ObjectId::new([0x0a; 32]), mk_bucket(850, 0, 0, 0)),
                (ObjectId::new([0x0b; 32]), put),
            ],
            &cat,
            NOW_MS,
        );
        assert_eq!(series.len(), 2);
        let call = series.iter().find(|s| s.option_type == "call").unwrap();
        let put = series.iter().find(|s| s.option_type == "put").unwrap();
        assert_eq!(call.buckets.len(), 1);
        assert_eq!(put.buckets.len(), 1);
        // option_coin_type is emitted alongside the legacy call_coin_type.
        assert_eq!(
            call.buckets[0].option_coin_type,
            call.buckets[0].call_coin_type
        );
    }

    #[test]
    fn strike_uses_both_decimals_so_btc_strike_lands_in_realistic_usd() {
        // Regression for SO-49: api-service was dividing the on-chain
        // strike by 10^settlement_decimals only, dropping a factor of
        // 10^under_dec. For TBTC(8)/TUSDC(6) that mapped a real
        // strike_raw=769 (a centre strike at ~$77k BTC) into
        // 0.000769 USD. Pin the conversion so the
        // regression can't sneak back in.
        let cat = fixture_catalog();
        let buckets = vec![(ObjectId::new([0xee; 32]), mk_bucket(769, 0, 0, 0))];
        let s = group_into_series(buckets, &cat, NOW_MS);
        assert_eq!(s[0].buckets[0].strike, Some(76_900.0));
        assert_eq!(s[0].buckets[0].strike_raw, "769");
    }

    #[test]
    fn strike_handles_under_dec_below_settle_dec() {
        // Inverse of the BTC case: when under_dec < settle_dec, the
        // exponent is negative and the strike comes out sub-1. Locked
        // here so the formula doesn't silently break when a future
        // deployment lists a sub-dollar asset against a higher-precision
        // settlement (the very case strike_grid.rs §3.4 warns about).
        // DEEP(6)/TUSDC(9) at $0.15 → strike_raw 150 → $0.15.
        let cat = TokenCatalog::from_tokens(&[
            tok("DEEP", "0xpkg::deep::DEEP", 6),
            tok("TUSDC9", "0xpkg::tusdc::TUSDC", 9),
        ]);
        let b = Bucket {
            asset_type: AssetType::new("0xpkg::deep::DEEP"),
            settlement_type: AssetType::new("0xpkg::tusdc::TUSDC"),
            call_type: AssetType::new("0xpkg::call_0::CALL_0"),
            strike: 150,
            strike_scale: 0,
            expiry_ms: 1_782_345_600_000,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
            option_kind: "call".to_string(),
            deepbook_pool_id: None,
        };
        let s = group_into_series(vec![(ObjectId::new([0xff; 32]), b)], &cat, NOW_MS);
        // 150 * 10^(6-9-0) = 0.15
        assert!((s[0].buckets[0].strike.unwrap() - 0.15).abs() < 1e-12);
    }

    #[test]
    fn strike_scale_lets_sub_dollar_round_trip_through_dto() {
        // Regression for SO-55 — a sub-dollar asset at $0.15 against a
        // same-decimals stablecoin (TMICRO is a 6-dec stand-in, not a real
        // token) at strike_scale=5 → strike_raw=15_000.
        // The api-service formula has to consume the scale; without it
        // the displayed strike collapses by 10^5.
        let cat = TokenCatalog::from_tokens(&[
            tok("TMICRO", "0xpkg::tmicro::TMICRO", 6),
            tok("TUSDC", "0xpkg::tusdc::TUSDC", 6),
        ]);
        let b = Bucket {
            asset_type: AssetType::new("0xpkg::tmicro::TMICRO"),
            settlement_type: AssetType::new("0xpkg::tusdc::TUSDC"),
            call_type: AssetType::new("0xpkg::call_0::CALL_0"),
            strike: 15_000,
            strike_scale: 5,
            expiry_ms: 1_782_345_600_000,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
            option_kind: "call".to_string(),
            deepbook_pool_id: None,
        };
        let s = group_into_series(vec![(ObjectId::new([0xfe; 32]), b)], &cat, NOW_MS);
        // 15_000 * 10^(6 - 6 - 5) = 0.15 USD
        assert!((s[0].buckets[0].strike.unwrap() - 0.15).abs() < 1e-12);
        assert_eq!(s[0].buckets[0].strike_scale, 5);
        assert_eq!(s[0].buckets[0].strike_raw, "15000");
    }

    #[test]
    fn unknown_coin_type_falls_back_to_raw_string() {
        let cat = TokenCatalog::default();
        let buckets = vec![(ObjectId::new([0xcc; 32]), mk_bucket(1, 0, 0, 0))];
        let series = group_into_series(buckets, &cat, NOW_MS);
        assert_eq!(series[0].asset_symbol, "0xpkg::tbtc::TBTC");
        assert_eq!(series[0].asset_decimals, None);
        assert_eq!(series[0].buckets[0].strike, None);
        assert_eq!(series[0].buckets[0].strike_raw, "1");
    }

    #[test]
    fn emits_canonical_0x_coin_types() {
        // Regression for the balance bug (SO): chain events carry the
        // `TypeName` form with no `0x` prefix, which `suix_getBalance`
        // rejects. The handler must emit a valid `0x` Move literal.
        let cat = fixture_catalog();
        let raw = "9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86";
        let b = Bucket {
            asset_type: AssetType::new(format!("{raw}::tbtc::TBTC")),
            settlement_type: AssetType::new(format!("{raw}::tusdc::TUSDC")),
            call_type: AssetType::new(format!("{raw}::call_0::CALL_0")),
            strike: 850,
            strike_scale: 0,
            expiry_ms: 1_782_345_600_000,
            total_written: 0,
            exercise_cursor: 0,
            cleaned: false,
            invalidated: false,
            option_kind: "call".to_string(),
            deepbook_pool_id: None,
        };
        let s = group_into_series(vec![(ObjectId::new([0x11; 32]), b)], &cat, NOW_MS);
        assert_eq!(s[0].asset_coin_type, format!("0x{raw}::tbtc::TBTC"));
        assert_eq!(s[0].settlement_coin_type, format!("0x{raw}::tusdc::TUSDC"));
    }

    #[test]
    fn empty_bucket_has_zero_fill_not_nan() {
        let cat = fixture_catalog();
        let buckets = vec![(
            ObjectId::new([0xdd; 32]),
            mk_bucket(85_000_000_000, 0, 0, 0),
        )];
        let s = group_into_series(buckets, &cat, NOW_MS);
        assert_eq!(s[0].buckets[0].fill_pct, Some(0.0));
    }

    #[test]
    fn exclude_invalidated_drops_buckets_and_empty_series() {
        let cat = fixture_catalog();
        // One series: two valid strikes + one invalidated.
        let mut inv = mk_bucket(860, 0, 0, 0);
        inv.invalidated = true;
        let mut series = group_into_series(
            vec![
                (ObjectId::new([0x01; 32]), mk_bucket(850, 0, 0, 0)),
                (ObjectId::new([0x02; 32]), inv),
                (ObjectId::new([0x03; 32]), mk_bucket(870, 0, 0, 0)),
            ],
            &cat,
            NOW_MS,
        );
        assert_eq!(series[0].buckets.len(), 3);
        retain_non_invalidated(&mut series);
        assert_eq!(series[0].buckets.len(), 2);
        assert!(series[0].buckets.iter().all(|b| !b.invalidated));

        // A series whose every bucket is invalidated is removed entirely.
        let mut only_inv = mk_bucket(900, 0, 0, 0);
        only_inv.invalidated = true;
        let mut s2 = group_into_series(vec![(ObjectId::new([0x04; 32]), only_inv)], &cat, NOW_MS);
        retain_non_invalidated(&mut s2);
        assert!(s2.is_empty());
    }

    fn mk_idx_bucket(id: ObjectId, written: u128, cursor: u128) -> IndexerBucket {
        IndexerBucket {
            bucket_id: id,
            asset_type: AssetType::new("0xpkg::tbtc::TBTC"),
            settlement_type: AssetType::new("0xpkg::tusdc::TUSDC"),
            call_type: AssetType::new("0xpkg::call_0::CALL_0"),
            strike: 850,
            strike_scale: 0,
            expiry_ms: 1_782_345_600_000,
            total_written: written,
            exercise_cursor: cursor,
            cleaned: false,
            invalidated: false,
            option_kind: "call".to_string(),
            deepbook_pool_id: None,
        }
    }

    #[test]
    fn queued_ahead_is_written_minus_cursor() {
        // TBTC(8): 4.2 written, 1.0 assigned → 3.2 still queued ahead of
        // the cursor. This is the number the queue wave draws.
        let cat = fixture_catalog();
        let dto = detail_dto_from(
            &mk_idx_bucket(ObjectId::new([0xaa; 32]), 420_000_000, 100_000_000),
            &cat,
            NOW_MS,
        );
        assert_eq!(dto.total_written, Some(4.2));
        assert_eq!(dto.exercise_cursor, Some(1.0));
        assert_eq!(dto.queued_ahead, Some(3.2));
        assert_eq!(dto.queued_ahead_raw, "320000000");
        assert!((dto.fill_pct.unwrap() - 100.0 * 1.0 / 4.2).abs() < 1e-9);
    }

    #[test]
    fn queued_ahead_saturates_when_cursor_exceeds_written() {
        // Defensive: an inconsistent indexer read where cursor > written
        // must clamp to 0, not underflow-panic the poller.
        let cat = fixture_catalog();
        let dto = detail_dto_from(
            &mk_idx_bucket(ObjectId::new([0xbb; 32]), 1, 5),
            &cat,
            NOW_MS,
        );
        assert_eq!(dto.queued_ahead_raw, "0");
        assert_eq!(dto.queued_ahead, Some(0.0));
    }

    #[test]
    fn unknown_decimals_null_the_scaled_fields() {
        // Coin type absent from the catalog → no decimals, so every scaled
        // field (including queued_ahead) is null but raw values survive.
        let cat = TokenCatalog::default();
        let dto = detail_dto_from(
            &mk_idx_bucket(ObjectId::new([0xcc; 32]), 420_000_000, 100_000_000),
            &cat,
            NOW_MS,
        );
        assert_eq!(dto.asset_decimals, None);
        assert_eq!(dto.queued_ahead, None);
        assert_eq!(dto.queued_ahead_raw, "320000000");
        assert_eq!(dto.fill_pct, None);
    }

    #[test]
    fn tradeable_gate_matrix() {
        let expiry = 1_782_345_600_000u64; // after NOW_MS
        assert!(is_tradeable(true, false, expiry, NOW_MS));
        // RFQ gate: pool-less any-strike buckets are live; invalidation kills.
        assert!(is_rfq_tradeable(false, false, expiry, NOW_MS));
        assert!(!is_rfq_tradeable(true, false, expiry, NOW_MS)); // cleaned
        assert!(!is_rfq_tradeable(false, true, expiry, NOW_MS)); // invalidated
        assert!(!is_rfq_tradeable(false, false, 1_000, NOW_MS)); // expired
        assert!(!is_tradeable(false, false, expiry, NOW_MS)); // no pool
        assert!(!is_tradeable(true, true, expiry, NOW_MS)); // cleaned
        assert!(!is_tradeable(true, false, 1_000, NOW_MS)); // expired
                                                            // invalidated intentionally not part of the gate (mint freeze only).
    }

    #[test]
    fn dto_carries_deepbook_pool_and_tradeable() {
        let cat = fixture_catalog();
        let pool_hex = ObjectId::new([0xee; 32]).to_hex();
        let mut b = mk_bucket(850, 0, 0, 0);
        b.deepbook_pool_id = Some(pool_hex.clone());
        let s = group_into_series(vec![(ObjectId::new([0x33; 32]), b)], &cat, NOW_MS);
        let dto = &s[0].buckets[0];
        assert_eq!(dto.deepbook_pool_id.as_deref(), Some(pool_hex.as_str()));
        assert!(dto.tradeable);

        // No pool → not tradeable, null pool id.
        let s = group_into_series(
            vec![(ObjectId::new([0x34; 32]), mk_bucket(850, 0, 0, 0))],
            &cat,
            NOW_MS,
        );
        let dto = &s[0].buckets[0];
        assert_eq!(dto.deepbook_pool_id, None);
        assert!(!dto.tradeable);
    }

    #[test]
    fn malformed_bucket_id_is_a_404_guard() {
        // The handler's 404-on-unknown path keys off `ObjectId::from_hex`
        // rejecting garbage before any indexer round-trip.
        assert!(ObjectId::from_hex("not-a-real-object-id").is_err());
    }

    #[test]
    fn spec_normalize_matches_onchain_rules() {
        assert_eq!(normalize_strike(257100, 4), Some((2571, 2)));
        assert_eq!(normalize_strike(1500, 1), Some((150, 0)));
        assert_eq!(normalize_strike(0, 0), None);
        assert_eq!(normalize_strike(0x1_00_0000_0000u128, 0), None); // > u40
    }

    #[test]
    fn spec_option_coin_type_matches_encoding() {
        // Cross-checked with sui_tx::tx::option_coin::tests and the Move
        // builder: minutes 50_000_000 = 0x02FAF080, sig 2571 = 0x…0A0B,
        // exp 2 — markers B02,BFA,BF0,B80,B00,B00,B00,B0A,B0B,B02 with the
        // high-bit bytes in enc1.
        let t = option_coin_type_str(
            "0xabc",
            false,
            "0xU::tbtc::TBTC",
            "0xS::tusdc::TUSDC",
            50_000_000u64 * 60_000,
            2571,
            2,
        );
        let pkg = format!("0x{:0>64}", "abc");
        assert_eq!(
            t,
            format!(
                "{pkg}::option_coin::OptionCall<0xU::tbtc::TBTC,0xS::tusdc::TUSDC,\
{pkg}::enc0::B02,{pkg}::enc1::BFA,{pkg}::enc1::BF0,{pkg}::enc1::B80,\
{pkg}::enc0::B00,{pkg}::enc0::B00,{pkg}::enc0::B00,{pkg}::enc0::B0A,\
{pkg}::enc0::B0B,{pkg}::enc0::B02>"
            )
        );
    }

    // ─────────────────────── strike ladder (SO-400) ───────────────────────

    fn ladder_pair() -> crate::ladder::LadderPair {
        crate::ladder::LadderPair {
            underlying: "TBTC".into(),
            settlement: "TUSDC".into(),
            option_type: "call".into(),
            tick_pct: 0.025,
            z_width: 2.5,
            vol_window_days: 30,
            fallback_sigma: 0.6,
        }
    }

    fn ladder_inputs_fixture() -> LadderInputs {
        LadderInputs {
            asset_ct: "0xpkg::tbtc::TBTC".into(),
            settlement_ct: "0xpkg::tusdc::TUSDC".into(),
            asset_symbol: "TBTC".into(),
            settlement_symbol: "TUSDC".into(),
            asset_decimals: 8,
            settlement_decimals: 6,
            spot: 63_090.2,
            sigma: 0.35,
        }
    }

    fn empty_series() -> SeriesDto {
        SeriesDto {
            asset_symbol: "TBTC".into(),
            asset_decimals: Some(8),
            asset_coin_type: "0xpkg::tbtc::TBTC".into(),
            settlement_symbol: "TUSDC".into(),
            settlement_decimals: Some(6),
            settlement_coin_type: "0xpkg::tusdc::TUSDC".into(),
            option_type: "call".into(),
            expiry_ms: 1_782_345_600_000,
            expiry_iso: iso_millis(1_782_345_600_000),
            buckets: Vec::new(),
        }
    }

    const EXPIRY: u64 = 1_782_345_600_000;

    #[test]
    fn synthetic_strikes_are_listed_without_an_object_id() {
        let mut s = empty_series();
        let inputs = ladder_inputs_fixture();
        let pair = ladder_pair();
        let strikes = crate::ladder::ladder_strikes(&pair, inputs.spot, inputs.sigma, 7.0 / 365.0);
        append_synthetic_strikes(&mut s, &strikes, &inputs, &pair, "0xopt", EXPIRY);

        assert!(!s.buckets.is_empty());
        for b in &s.buckets {
            assert!(b.bucket_id.is_none(), "synthetic strike carries an id");
            assert!(b.rfq_tradeable, "a listed strike must be writable");
            assert!(!b.pool_tradeable, "a strike with no bucket has no pool");
            assert_eq!(b.total_written_raw, "0");
            assert!(b.option_coin_type.contains("OptionCall"));
        }
    }

    /// The core merge rule: a strike that already exists on-chain keeps its
    /// object id and real state, and must not also appear as a synthetic
    /// entry — even though the lattice lists that same strike.
    #[test]
    fn real_bucket_suppresses_the_synthetic_entry_for_its_strike() {
        let mut s = empty_series();
        // 63 000 encodes as (63000, 2) → normalized (630, 0).
        let (raw, scale) = crate::ladder::strike_to_raw(63_000.0, 8, 6).unwrap();
        s.buckets.push(dto_from(
            "0xreal".into(),
            &mk_bucket(raw, scale, 500, 0),
            Some(8),
            Some(6),
            NOW_MS,
        ));

        let inputs = ladder_inputs_fixture();
        let pair = ladder_pair();
        let strikes = crate::ladder::ladder_strikes(&pair, inputs.spot, inputs.sigma, 7.0 / 365.0);
        assert!(strikes.contains(&63_000.0), "fixture assumes 63000 is on-lattice");
        append_synthetic_strikes(&mut s, &strikes, &inputs, &pair, "0xopt", EXPIRY);

        let at_63k: Vec<&BucketDto> = s
            .buckets
            .iter()
            .filter(|b| b.strike.map(|k| (k - 63_000.0).abs() < 1e-6).unwrap_or(false))
            .collect();
        assert_eq!(at_63k.len(), 1, "63000 listed twice");
        assert_eq!(at_63k[0].bucket_id.as_deref(), Some("0xreal"));
        assert_eq!(at_63k[0].total_written_raw, "500");
    }

    /// An off-lattice bucket (the manually-created any-strike case) survives
    /// the merge — dropping it would strand real open interest.
    #[test]
    fn off_lattice_real_bucket_is_preserved() {
        let mut s = empty_series();
        let (raw, scale) = crate::ladder::strike_to_raw(98_765.43, 8, 6).unwrap();
        s.buckets.push(dto_from(
            "0xodd".into(),
            &mk_bucket(raw, scale, 42, 0),
            Some(8),
            Some(6),
            NOW_MS,
        ));

        let inputs = ladder_inputs_fixture();
        let pair = ladder_pair();
        let strikes = crate::ladder::ladder_strikes(&pair, inputs.spot, inputs.sigma, 7.0 / 365.0);
        assert!(!strikes.contains(&98_765.43), "fixture assumes it is off-lattice");
        append_synthetic_strikes(&mut s, &strikes, &inputs, &pair, "0xopt", EXPIRY);
        sort_by_strike(&mut s.buckets);

        let odd = s
            .buckets
            .iter()
            .find(|b| b.bucket_id.as_deref() == Some("0xodd"))
            .expect("off-lattice bucket dropped");
        assert_eq!(odd.total_written_raw, "42");
    }

    /// Two encodings of one strike normalize to the same spec; matching on
    /// the raw pair instead would list the strike twice.
    #[test]
    fn dedup_matches_on_the_normalized_strike_not_the_raw_pair() {
        let mut s = empty_series();
        // (6_300_000, 4) is 63 000 written at a deeper scale than the
        // ladder's own (63_000, 2) encoding — same economics.
        s.buckets.push(dto_from(
            "0xdeep".into(),
            &mk_bucket(6_300_000, 4, 1, 0),
            Some(8),
            Some(6),
            NOW_MS,
        ));

        let inputs = ladder_inputs_fixture();
        let pair = ladder_pair();
        append_synthetic_strikes(&mut s, &[63_000.0], &inputs, &pair, "0xopt", EXPIRY);
        assert_eq!(s.buckets.len(), 1, "same strike listed twice: {:?}", s.buckets.iter().map(|b| b.strike).collect::<Vec<_>>());
        assert_eq!(s.buckets[0].bucket_id.as_deref(), Some("0xdeep"));
    }

    #[test]
    fn synthetic_option_coin_type_matches_the_spec_endpoint() {
        let mut s = empty_series();
        let inputs = ladder_inputs_fixture();
        let pair = ladder_pair();
        append_synthetic_strikes(&mut s, &[63_000.0], &inputs, &pair, "0xopt", EXPIRY);

        let (sig, exp) = normalize_strike(63_000, 2).unwrap();
        let expected = option_coin_type_str(
            "0xopt",
            false,
            &protocol_types::asset::canonicalize_move_type("0xpkg::tbtc::TBTC"),
            &protocol_types::asset::canonicalize_move_type("0xpkg::tusdc::TUSDC"),
            EXPIRY,
            sig,
            exp,
        );
        assert_eq!(s.buckets[0].option_coin_type, expected);
        assert_eq!(s.buckets[0].call_coin_type, expected);
    }
}
