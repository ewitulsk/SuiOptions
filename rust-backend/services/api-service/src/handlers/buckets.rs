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
    pub bucket_id: String,
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
/// "YOUR PLACE IN THE QUEUE" tideline reads `exercise_cursor` (how far
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
    // tideline stops polling a stale id rather than rendering dead state.
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
        // TODO(cash-secured-puts): source this from the indexer's `optionKind`
        // once `indexer_graphql::Bucket` carries it; defaults to "call".
        option_kind: "call".to_string(),
        deepbook_pool_id: b.deepbook_pool_id.as_ref().map(|p| p.to_hex()),
        tradeable: is_tradeable(b.deepbook_pool_id.is_some(), b.cleaned, b.expiry_ms, now_ms),
    }
}

/// SO-153 tradeable gate. `invalidated` intentionally absent — it freezes
/// new mints, not secondary-market transfers of already-minted coins.
fn is_tradeable(has_pool: bool, cleaned: bool, expiry_ms: u64, now_ms: i64) -> bool {
    has_pool && !cleaned && (expiry_ms as i64) > now_ms
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
            // TODO(cash-secured-puts): once indexer-graphql's `Bucket` carries
            // `option_kind` (the indexer is being updated in parallel to expose
            // `optionKind`), read it here as `b.option_kind`. Until that field
            // lands the api-service defaults every bucket to "call", which is
            // back-compat-safe — calls keep working unchanged.
            option_kind: "call".to_string(),
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
        bucket_id,
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
    }
}

fn scale_u128(raw: u128, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

/// Convert an on-chain strike (`raw / 10^strike_scale` settlement-
/// smallest-units per underlying-smallest-unit) into USD. Mirrors the
/// bucket-creation math in `option-scheduler/src/strike_grid.rs`.
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
    fn strike_uses_both_decimals_so_btc_strike_lands_in_realistic_usd() {
        // Regression for SO-49: api-service was dividing the on-chain
        // strike by 10^settlement_decimals only, dropping a factor of
        // 10^under_dec. For TBTC(8)/TUSDC(6) that mapped a real
        // strike_raw=769 (the centre strike at ~$77k BTC the scheduler
        // produces today) into 0.000769 USD. Pin the conversion so the
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
    fn strike_scale_lets_tdeep_round_trip_through_dto() {
        // Regression for SO-55 — TDEEP/TUSDC at $0.15. Same decimals on
        // both sides, scheduler picks strike_scale=5 → strike_raw=15_000.
        // The api-service formula has to consume the scale; without it
        // the displayed strike collapses by 10^5.
        let cat = TokenCatalog::from_tokens(&[
            tok("TDEEP", "0xpkg::tdeep::TDEEP", 6),
            tok("TUSDC", "0xpkg::tusdc::TUSDC", 6),
        ]);
        let b = Bucket {
            asset_type: AssetType::new("0xpkg::tdeep::TDEEP"),
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
            deepbook_pool_id: None,
        }
    }

    #[test]
    fn queued_ahead_is_written_minus_cursor() {
        // TBTC(8): 4.2 written, 1.0 assigned → 3.2 still queued ahead of
        // the cursor. This is the number the tideline draws.
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
        let dto = detail_dto_from(&mk_idx_bucket(ObjectId::new([0xbb; 32]), 1, 5), &cat, NOW_MS);
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
}
