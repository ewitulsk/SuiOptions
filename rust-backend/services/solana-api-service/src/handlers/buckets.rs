//! `GET /buckets` — the bucket catalog the frontend renders from.
//!
//! # Response shape
//!
//! Buckets are grouped into **series** keyed by `(underlying_mint,
//! settlement_mint, expiry_ms, option_kind)`. Within a series, every
//! bucket is a distinct strike — that's the level a user picks from when
//! composing a trade. The series-level fields collapse what's redundant
//! across all buckets in the same expiry; the bucket-level fields are what
//! differ.
//!
//! Numeric fields exist in two flavors (same as the Sui twin):
//!
//! - **Scaled** (`f64`) — strike/written/cursor divided by the relevant
//!   token's decimals. Suitable for direct display.
//! - **Raw** (`string`) — the on-chain integer in atomic units, sent as a
//!   string so we never lose u64/u128 precision through JSON. Required
//!   when building a transaction off this data.
//!
//! Symbols and decimals are resolved from the solana-token-info catalog at
//! startup, keyed by mint. A bucket whose mint isn't in the catalog falls
//! back to the raw base58 mint as its `*_symbol`, with `*_decimals: null`
//! and `null` scaled fields, so the bucket is still visible but flagged as
//! un-renderable.
//!
//! Solana deltas vs the Sui twin: ids are base58 pubkeys; there is no
//! order book yet, so there are no `deepbook_pool_id` fields and
//! `tradeable = !cleaned && !invalidated && !expired`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::catalog::TokenCatalog;
use crate::ids;
use crate::state::{AppState, IndexerBucket};

#[derive(Serialize)]
pub struct BucketDto {
    pub bucket_id: String,
    /// Strike in USD-equivalent whole units. `null` if either decimals
    /// lookup failed. Real ratio is
    /// `strike_raw / 10^strike_scale × 10^(under_dec − settle_dec)` —
    /// see `strike_raw_to_usd`.
    pub strike: Option<f64>,
    /// Raw on-chain u128 strike. Real ratio = `strike_raw / 10^strike_scale`.
    pub strike_raw: String,
    /// SPL mint of this bucket's fungible option token (base58). The
    /// frontend uses it to match the user's owned option tokens to buckets
    /// and as the mint account when exercising.
    pub option_mint: String,
    /// On-chain `strike_scale` (0..=9). Exposed so frontends can recompute
    /// the USD strike independently if they want.
    pub strike_scale: u8,
    /// Total underlying written into the bucket, in underlying whole units.
    /// `null` if underlying decimals are unknown.
    pub total_written: Option<f64>,
    pub total_written_raw: String,
    /// Exercise cursor in underlying whole units. `null` if unknown decimals.
    pub exercise_cursor: Option<f64>,
    pub exercise_cursor_raw: String,
    /// `100 * exercise_cursor / total_written`. `0.0` when nothing's been
    /// written yet (avoids a NaN); `null` when underlying decimals are
    /// unknown so the math is unsafe.
    pub fill_pct: Option<f64>,
    /// Admin-set freeze on new writes. The writer screen filters these out
    /// entirely.
    pub invalidated: bool,
    /// Whether the bucket is actionable: not cleaned, not invalidated, not
    /// expired. No order-book condition — there is no Solana venue pool yet.
    pub tradeable: bool,
}

#[derive(Serialize)]
pub struct SeriesDto {
    /// Friendly symbol from solana-token-info (`"TBTC"`) — or the raw
    /// base58 mint when the mint isn't in the catalog.
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    /// Underlying SPL mint address (base58).
    pub asset_mint: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_mint: String,
    /// `"call"` | `"put"`. Series are grouped by `(asset, settlement,
    /// expiry, option_type)`, so every bucket within a series shares this.
    pub option_type: String,
    /// Unix millis. Sent as a number — Date.now()-style.
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
    /// Drop series whose expiry is already in the past. Opt-in (defaults
    /// to `false`) so admin/monitoring and dashboard views keep their full
    /// catalog; the trade picker passes `?exclude_expired=true`.
    #[serde(default)]
    pub exclude_expired: bool,
    /// Drop admin-invalidated buckets (and series left empty). Opt-in.
    #[serde(default)]
    pub exclude_invalidated: bool,
}

pub async fn list_buckets(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListBucketsParams>,
) -> Result<Json<BucketsResponse>, StatusCode> {
    // One fetch returns calls *and* puts; `group_into_series` splits them
    // into separate series by `option_type`.
    let active = state
        .indexer
        .buckets(true, None, None, None, None, None)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer buckets query failed");
            StatusCode::BAD_GATEWAY
        })?;
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
/// seconds without re-pulling the whole `/buckets` catalog.
#[derive(Serialize)]
pub struct BucketDetailDto {
    pub bucket_id: String,
    /// Friendly symbol from the catalog; raw base58 mint when unknown.
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    pub asset_mint: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_mint: String,
    /// Strike in USD whole units. `null` if either decimals lookup failed.
    pub strike: Option<f64>,
    pub strike_raw: String,
    pub strike_scale: u8,
    pub expiry_ms: i64,
    pub total_written: Option<f64>,
    pub total_written_raw: String,
    pub exercise_cursor: Option<f64>,
    pub exercise_cursor_raw: String,
    /// Underlying written but not yet assigned: `total_written −
    /// exercise_cursor`, in whole units. `null` if asset decimals unknown.
    pub queued_ahead: Option<f64>,
    pub queued_ahead_raw: String,
    pub fill_pct: Option<f64>,
    /// SPL mint of the bucket's fungible option token.
    pub option_mint: String,
    /// `"call"` | `"put"`.
    pub option_kind: String,
    /// Not cleaned, not invalidated, not expired (see `/buckets`).
    pub tradeable: bool,
}

pub async fn get_bucket(
    State(state): State<Arc<AppState>>,
    Path(bucket_id): Path<String>,
) -> Result<Json<BucketDetailDto>, StatusCode> {
    if !ids::is_pubkey(&bucket_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let bucket = state.indexer.bucket(&bucket_id).await.map_err(|e| {
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
    let asset_meta = catalog.lookup(&b.underlying_mint);
    let settle_meta = catalog.lookup(&b.settlement_mint);
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
        bucket_id: b.bucket_id.clone(),
        asset_symbol: asset_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| b.underlying_mint.clone()),
        asset_decimals,
        asset_mint: b.underlying_mint.clone(),
        settlement_symbol: settle_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| b.settlement_mint.clone()),
        settlement_decimals: settle_decimals,
        settlement_mint: b.settlement_mint.clone(),
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
        option_mint: b.option_mint.clone(),
        option_kind: b.option_kind.clone(),
        tradeable: is_tradeable(b.cleaned, b.invalidated, b.expiry_ms, now_ms),
    }
}

/// Solana tradeable gate: no order book yet, so no pool condition —
/// `!cleaned && !invalidated && !expired` (per doc 04; note `invalidated`
/// IS part of the gate here, unlike the Sui twin where secondary-market
/// transfers survived invalidation).
fn is_tradeable(cleaned: bool, invalidated: bool, expiry_ms: u64, now_ms: i64) -> bool {
    !cleaned && !invalidated && (expiry_ms as i64) > now_ms
}

/// `(underlying_mint, settlement_mint, expiry_ms, option_kind)`. Adding
/// the option kind keeps call and put strikes in separate series even when
/// they share an asset/settlement/expiry.
type SeriesKey = (String, String, u64, String);

/// Pure helper — split out so it's unit-testable without spinning up axum.
fn group_into_series(
    buckets: Vec<IndexerBucket>,
    catalog: &TokenCatalog,
    now_ms: i64,
) -> Vec<SeriesDto> {
    let mut grouped: BTreeMap<SeriesKey, Vec<IndexerBucket>> = BTreeMap::new();
    for b in buckets {
        let key = (
            b.underlying_mint.clone(),
            b.settlement_mint.clone(),
            b.expiry_ms,
            b.option_kind.clone(),
        );
        grouped.entry(key).or_default().push(b);
    }

    grouped
        .into_iter()
        .map(|((asset_mint, settle_mint, expiry_ms, option_kind), members)| {
            let asset_meta = catalog.lookup(&asset_mint);
            let settle_meta = catalog.lookup(&settle_mint);
            let asset_decimals = asset_meta.map(|m| m.decimals);
            let settle_decimals = settle_meta.map(|m| m.decimals);

            let mut bucket_dtos: Vec<BucketDto> = members
                .into_iter()
                .map(|b| dto_from(&b, asset_decimals, settle_decimals, now_ms))
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
                    .unwrap_or_else(|| asset_mint.clone()),
                asset_decimals,
                asset_mint,
                settlement_symbol: settle_meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| settle_mint.clone()),
                settlement_decimals: settle_decimals,
                settlement_mint: settle_mint,
                option_type: option_kind,
                expiry_ms: expiry_ms as i64,
                expiry_iso: iso_millis(expiry_ms as i64),
                buckets: bucket_dtos,
            }
        })
        .collect()
}

fn dto_from(
    b: &IndexerBucket,
    asset_decimals: Option<u8>,
    settle_decimals: Option<u8>,
    now_ms: i64,
) -> BucketDto {
    // On-chain strike is `strike_raw / 10^strike_scale` settlement-
    // smallest-units per underlying-smallest-unit, so USD conversion needs
    // both decimals AND the per-bucket scale (same math as the Sui twin).
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
        bucket_id: b.bucket_id.clone(),
        strike,
        strike_raw: b.strike.to_string(),
        option_mint: b.option_mint.clone(),
        strike_scale: b.strike_scale,
        total_written,
        total_written_raw: b.total_written.to_string(),
        exercise_cursor,
        exercise_cursor_raw: b.exercise_cursor.to_string(),
        fill_pct,
        invalidated: b.invalidated,
        tradeable: is_tradeable(b.cleaned, b.invalidated, b.expiry_ms, now_ms),
    }
}

pub(crate) fn scale_u128(raw: u128, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

/// Convert an on-chain strike (`raw / 10^strike_scale` settlement-
/// smallest-units per underlying-smallest-unit) into USD. Same formula as
/// the Sui twin (`option-scheduler/src/strike_grid.rs`).
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
    use solana_token_info_client::SupportedToken;

    /// Fixed test clock — comfortably before the fixture expiry.
    const NOW_MS: i64 = 1_700_000_000_000;
    const EXPIRY_MS: u64 = 1_782_345_600_000;

    const MINT_TBTC: &str = "So11111111111111111111111111111111111111112";
    const MINT_TUSDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const MINT_OPT: &str = "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin";

    fn tok(ticker: &str, mint: &str, decimals: u8) -> SupportedToken {
        SupportedToken {
            mint: mint.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals,
            pyth_feed_id: None,
            enabled: true,
        }
    }

    fn fixture_catalog() -> TokenCatalog {
        TokenCatalog::from_tokens(&[tok("TBTC", MINT_TBTC, 8), tok("TUSDC", MINT_TUSDC, 6)])
    }

    fn mk_bucket(id: &str, strike: u128, strike_scale: u8, written: u128, cursor: u128) -> IndexerBucket {
        IndexerBucket {
            bucket_id: id.to_string(),
            underlying_mint: MINT_TBTC.to_string(),
            settlement_mint: MINT_TUSDC.to_string(),
            option_mint: MINT_OPT.to_string(),
            option_kind: "call".to_string(),
            strike,
            strike_scale,
            expiry_ms: EXPIRY_MS,
            total_written: written,
            exercise_cursor: cursor,
            cleaned: false,
            invalidated: false,
        }
    }

    #[test]
    fn groups_buckets_into_one_series_by_expiry_and_mints() {
        let cat = fixture_catalog();
        // Realistic chain units for TBTC(8)/TUSDC(6):
        //   strike_raw 850 → $85,000, strike_raw 900 → $90,000.
        let buckets = vec![
            mk_bucket("bktA", 850, 0, 420_000_000, 100_000_000),
            mk_bucket("bktB", 900, 0, 0, 0),
        ];
        let series = group_into_series(buckets, &cat, NOW_MS);
        assert_eq!(series.len(), 1);
        let s = &series[0];
        assert_eq!(s.asset_symbol, "TBTC");
        assert_eq!(s.asset_decimals, Some(8));
        assert_eq!(s.asset_mint, MINT_TBTC);
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
        assert_eq!(b.option_mint, MINT_OPT);
    }

    #[test]
    fn call_and_put_buckets_split_into_separate_series() {
        // Same asset/settlement/expiry but different option_kind must land
        // in two distinct series, each tagged with its option_type.
        let cat = fixture_catalog();
        let mut put = mk_bucket("bktP", 900, 0, 0, 0);
        put.option_kind = "put".to_string();
        let series = group_into_series(
            vec![mk_bucket("bktC", 850, 0, 0, 0), put],
            &cat,
            NOW_MS,
        );
        assert_eq!(series.len(), 2);
        let call = series.iter().find(|s| s.option_type == "call").unwrap();
        let put = series.iter().find(|s| s.option_type == "put").unwrap();
        assert_eq!(call.buckets.len(), 1);
        assert_eq!(put.buckets.len(), 1);
    }

    #[test]
    fn strike_scale_round_trips_through_dto() {
        // Same decimals both sides, strike_scale=5 → strike_raw=15_000 is
        // $0.15. The formula must consume the scale.
        let cat = TokenCatalog::from_tokens(&[
            tok("TDEEP", MINT_TBTC, 6),
            tok("TUSDC", MINT_TUSDC, 6),
        ]);
        let b = mk_bucket("bktS", 15_000, 5, 0, 0);
        let s = group_into_series(vec![b], &cat, NOW_MS);
        assert!((s[0].buckets[0].strike.unwrap() - 0.15).abs() < 1e-12);
        assert_eq!(s[0].buckets[0].strike_scale, 5);
        assert_eq!(s[0].buckets[0].strike_raw, "15000");
    }

    #[test]
    fn unknown_mint_falls_back_to_raw_base58() {
        let cat = TokenCatalog::default();
        let series = group_into_series(vec![mk_bucket("bktU", 1, 0, 0, 0)], &cat, NOW_MS);
        assert_eq!(series[0].asset_symbol, MINT_TBTC);
        assert_eq!(series[0].asset_decimals, None);
        assert_eq!(series[0].buckets[0].strike, None);
        assert_eq!(series[0].buckets[0].strike_raw, "1");
    }

    #[test]
    fn empty_bucket_has_zero_fill_not_nan() {
        let cat = fixture_catalog();
        let s = group_into_series(vec![mk_bucket("bktZ", 850, 0, 0, 0)], &cat, NOW_MS);
        assert_eq!(s[0].buckets[0].fill_pct, Some(0.0));
    }

    #[test]
    fn exclude_invalidated_drops_buckets_and_empty_series() {
        let cat = fixture_catalog();
        let mut inv = mk_bucket("bktI", 860, 0, 0, 0);
        inv.invalidated = true;
        let mut series = group_into_series(
            vec![
                mk_bucket("bkt1", 850, 0, 0, 0),
                inv,
                mk_bucket("bkt3", 870, 0, 0, 0),
            ],
            &cat,
            NOW_MS,
        );
        assert_eq!(series[0].buckets.len(), 3);
        retain_non_invalidated(&mut series);
        assert_eq!(series[0].buckets.len(), 2);
        assert!(series[0].buckets.iter().all(|b| !b.invalidated));

        // A series whose every bucket is invalidated is removed entirely.
        let mut only_inv = mk_bucket("bkt4", 900, 0, 0, 0);
        only_inv.invalidated = true;
        let mut s2 = group_into_series(vec![only_inv], &cat, NOW_MS);
        retain_non_invalidated(&mut s2);
        assert!(s2.is_empty());
    }

    #[test]
    fn queued_ahead_is_written_minus_cursor() {
        // TBTC(8): 4.2 written, 1.0 assigned → 3.2 still queued ahead.
        let cat = fixture_catalog();
        let dto = detail_dto_from(&mk_bucket("bktQ", 850, 0, 420_000_000, 100_000_000), &cat, NOW_MS);
        assert_eq!(dto.total_written, Some(4.2));
        assert_eq!(dto.exercise_cursor, Some(1.0));
        assert_eq!(dto.queued_ahead, Some(3.2));
        assert_eq!(dto.queued_ahead_raw, "320000000");
        assert!((dto.fill_pct.unwrap() - 100.0 * 1.0 / 4.2).abs() < 1e-9);
        assert_eq!(dto.option_kind, "call");
    }

    #[test]
    fn queued_ahead_saturates_when_cursor_exceeds_written() {
        // Defensive: an inconsistent indexer read where cursor > written
        // must clamp to 0, not underflow-panic the poller.
        let cat = fixture_catalog();
        let dto = detail_dto_from(&mk_bucket("bktX", 850, 0, 1, 5), &cat, NOW_MS);
        assert_eq!(dto.queued_ahead_raw, "0");
        assert_eq!(dto.queued_ahead, Some(0.0));
    }

    #[test]
    fn unknown_decimals_null_the_scaled_fields() {
        let cat = TokenCatalog::default();
        let dto = detail_dto_from(&mk_bucket("bktN", 850, 0, 420_000_000, 100_000_000), &cat, NOW_MS);
        assert_eq!(dto.asset_decimals, None);
        assert_eq!(dto.queued_ahead, None);
        assert_eq!(dto.queued_ahead_raw, "320000000");
        assert_eq!(dto.fill_pct, None);
    }

    #[test]
    fn tradeable_gate_matrix() {
        // tradeable = !cleaned && !invalidated && !expired (no pool
        // condition on Solana; invalidated IS part of the gate).
        assert!(is_tradeable(false, false, EXPIRY_MS, NOW_MS));
        assert!(!is_tradeable(true, false, EXPIRY_MS, NOW_MS)); // cleaned
        assert!(!is_tradeable(false, true, EXPIRY_MS, NOW_MS)); // invalidated
        assert!(!is_tradeable(false, false, 1_000, NOW_MS)); // expired
    }

    #[test]
    fn malformed_bucket_id_is_a_404_guard() {
        // The handler's 404-on-unknown path keys off base58 validation
        // rejecting garbage before any indexer round-trip.
        assert!(!crate::ids::is_pubkey("not-a-real-pubkey"));
        assert!(!crate::ids::is_pubkey("0x9c2b42a1"));
    }
}
