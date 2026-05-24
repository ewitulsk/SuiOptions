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
//!       "expiry_ms": 1782345600000,
//!       "expiry_iso": "2026-06-26T08:00:00Z",
//!       "buckets": [
//!         {
//!           "bucket_id": "0x9c2b…42a1",
//!           "strike": 85000.0,
//!           "strike_raw": "85000000000",
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

use axum::{extract::State, Json};
use chrono::{TimeZone, Utc};
use serde::Serialize;

use crate::bucket::Bucket;
use crate::catalog::TokenCatalog;
use crate::state::AppState;

#[derive(Serialize)]
pub struct BucketDto {
    pub bucket_id: String,
    /// Strike in settlement-token whole units (`raw / 10^settlement_decimals`).
    /// `null` if settlement decimals are unknown.
    pub strike: Option<f64>,
    /// Raw on-chain strike in settlement atomic units.
    pub strike_raw: String,
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
}

#[derive(Serialize)]
pub struct SeriesDto {
    /// Friendly symbol from `deployments.json` (`"TBTC"`) — or the raw Move
    /// type string when the coin type isn't in the catalog.
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
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

pub async fn list_buckets(State(state): State<Arc<AppState>>) -> Json<BucketsResponse> {
    let active = state.active_buckets();
    Json(BucketsResponse {
        series: group_into_series(active, &state.catalog),
    })
}

type SeriesKey = (String, String, u64);

/// Pure helper — split out so it's unit-testable without spinning up axum.
fn group_into_series(
    buckets: Vec<(shared::protocol_types::ids::ObjectId, Bucket)>,
    catalog: &TokenCatalog,
) -> Vec<SeriesDto> {
    let mut grouped: BTreeMap<SeriesKey, Vec<(String, Bucket)>> = BTreeMap::new();
    for (id, b) in buckets {
        let key = (
            b.asset_type.as_str().to_string(),
            b.settlement_type.as_str().to_string(),
            b.expiry_ms,
        );
        grouped.entry(key).or_default().push((id.to_hex(), b));
    }

    grouped
        .into_iter()
        .map(|((asset_ct, settle_ct, expiry_ms), members)| {
            let asset_meta = catalog.lookup(&asset_ct);
            let settle_meta = catalog.lookup(&settle_ct);
            let asset_decimals = asset_meta.map(|m| m.decimals);
            let settle_decimals = settle_meta.map(|m| m.decimals);

            let mut bucket_dtos: Vec<BucketDto> = members
                .into_iter()
                .map(|(id_hex, b)| dto_from(id_hex, &b, asset_decimals, settle_decimals))
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
                asset_symbol: asset_meta.map(|m| m.symbol.clone()).unwrap_or(asset_ct),
                asset_decimals,
                settlement_symbol: settle_meta.map(|m| m.symbol.clone()).unwrap_or(settle_ct),
                settlement_decimals: settle_decimals,
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
) -> BucketDto {
    let strike = settle_decimals.map(|d| scale_u64(b.strike, d));
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
        total_written,
        total_written_raw: b.total_written.to_string(),
        exercise_cursor,
        exercise_cursor_raw: b.exercise_cursor.to_string(),
        fill_pct,
    }
}

fn scale_u64(raw: u64, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

fn scale_u128(raw: u128, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

fn iso_millis(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bucket::Bucket;
    use shared::deployments::{
        Deployments, NetworkDeployment, PackageInfo, TestTokens, TokenInfo, TokenSpec,
    };
    use shared::protocol_types::asset::AssetType;
    use shared::protocol_types::ids::ObjectId;
    use std::collections::BTreeMap;

    fn fixture_catalog() -> TokenCatalog {
        let mut tokens = BTreeMap::new();
        tokens.insert(
            "TBTC".into(),
            TokenInfo {
                coin_type: "0xpkg::tbtc::TBTC".into(),
                faucet_id: "0x1".into(),
                decimals: 8,
            },
        );
        tokens.insert(
            "TUSDC".into(),
            TokenInfo {
                coin_type: "0xpkg::tusdc::TUSDC".into(),
                faucet_id: "0x2".into(),
                decimals: 6,
            },
        );
        let token_info: BTreeMap<String, TokenSpec> = BTreeMap::new();
        let deps = Deployments {
            mainnet: None,
            devnet: None,
            testnet: Some(NetworkDeployment {
                package_info: PackageInfo {
                    package_id: "0xp".into(),
                    admin_cap_id: "0xa".into(),
                    protocol_config_id: "0xc".into(),
                    upgrade_cap_id: "0xu".into(),
                    treasury_id: None,
                    publish_digest: "x".into(),
                    init_digest: None,
                    deployer: "0xd".into(),
                    deployed_at: "".into(),
                    network: "testnet".into(),
                    test_tokens: Some(TestTokens {
                        package_id: "0xtp".into(),
                        upgrade_cap_id: "0xtu".into(),
                        publish_digest: "y".into(),
                        deployed_at: "".into(),
                        tokens,
                    }),
                },
                token_info,
            }),
        };
        TokenCatalog::from_deployments(&deps, "testnet").unwrap()
    }

    fn mk_bucket(strike: u64, written: u128, cursor: u128) -> Bucket {
        Bucket {
            asset_type: AssetType::new("0xpkg::tbtc::TBTC"),
            settlement_type: AssetType::new("0xpkg::tusdc::TUSDC"),
            strike,
            expiry_ms: 1_782_345_600_000,
            total_written: written,
            exercise_cursor: cursor,
            cleaned: false,
        }
    }

    #[test]
    fn groups_buckets_into_one_series_by_expiry_and_assets() {
        let cat = fixture_catalog();
        let buckets = vec![
            (
                ObjectId::new([0xaa; 32]),
                mk_bucket(85_000_000_000, 420_000_000, 100_000_000),
            ),
            (ObjectId::new([0xbb; 32]), mk_bucket(90_000_000_000, 0, 0)),
        ];
        let series = group_into_series(buckets, &cat);
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
        assert_eq!(b.total_written, Some(4.2));
        assert_eq!(b.exercise_cursor, Some(1.0));
        assert!((b.fill_pct.unwrap() - 100.0 * 1.0 / 4.2).abs() < 1e-9);
    }

    #[test]
    fn unknown_coin_type_falls_back_to_raw_string() {
        let cat = TokenCatalog::default();
        let buckets = vec![(ObjectId::new([0xcc; 32]), mk_bucket(1, 0, 0))];
        let series = group_into_series(buckets, &cat);
        assert_eq!(series[0].asset_symbol, "0xpkg::tbtc::TBTC");
        assert_eq!(series[0].asset_decimals, None);
        assert_eq!(series[0].buckets[0].strike, None);
        assert_eq!(series[0].buckets[0].strike_raw, "1");
    }

    #[test]
    fn empty_bucket_has_zero_fill_not_nan() {
        let cat = fixture_catalog();
        let buckets = vec![(
            ObjectId::new([0xdd; 32]),
            mk_bucket(85_000_000_000, 0, 0),
        )];
        let s = group_into_series(buckets, &cat);
        assert_eq!(s[0].buckets[0].fill_pct, Some(0.0));
    }
}
