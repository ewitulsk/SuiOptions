//! Settled funding via venue REST (SO-391). Hyperliquid keeps full
//! funding history queryable, so settled rates are *repairable* — any
//! capture gap self-heals on the next run. Rows land in the same
//! partition dir as the streamed predicted rates but in their own
//! `part-settled.parquet`, so the two jobs overwrite independently.

use chrono::{Duration, NaiveDate};
use data_room_schema::FundingRate;
use serde::Deserialize;
use tracing::info;

use crate::{put_bytes, Store};

#[derive(Deserialize)]
struct HlFunding {
    coin: String,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    /// Settlement time, ms epoch.
    time: i64,
}

/// Fetch settled Hyperliquid funding for [from, to] (inclusive UTC days)
/// and write one part-settled partition per day. Idempotent.
pub async fn hyperliquid_settled(
    store: &Store,
    coins: &[String],
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<usize> {
    let http = reqwest::Client::builder()
        .user_agent("data-room-funding")
        .build()?;
    let mut partitions = 0usize;
    for coin in coins {
        let start_ms = from
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        let end_ms = (to + Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        // One page covers ~500 settlements (~20 days); loop the cursor.
        let mut cursor = start_ms;
        let mut rows: Vec<HlFunding> = Vec::new();
        loop {
            let page: Vec<HlFunding> = http
                .post("https://api.hyperliquid.xyz/info")
                .json(&serde_json::json!({
                    "type": "fundingHistory",
                    "coin": coin,
                    "startTime": cursor,
                    "endTime": end_ms,
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let Some(last) = page.last() else { break };
            let next_cursor = last.time + 1;
            rows.extend(page);
            if next_cursor >= end_ms || rows.len() > 100_000 {
                break;
            }
            cursor = next_cursor;
        }

        // Group per UTC day and write.
        let mut by_day: std::collections::BTreeMap<String, Vec<FundingRate>> = Default::default();
        for (i, r) in rows.iter().enumerate() {
            let day = chrono::DateTime::from_timestamp_millis(r.time)
                .ok_or_else(|| anyhow::anyhow!("bad time {}", r.time))?
                .format("%Y-%m-%d")
                .to_string();
            by_day.entry(day).or_default().push(FundingRate {
                ts_event: Some(r.time * 1_000_000),
                ts_recv: None, // REST-sourced, not observed live
                exchange: data_room_adapters::hyperliquid::EXCHANGE.into(),
                instrument_id: data_room_adapters::hyperliquid::instrument_id(&r.coin),
                rate: r.funding_rate.parse()?,
                interval_hours: data_room_adapters::hyperliquid::FUNDING_INTERVAL_HOURS,
                kind: "settled".into(),
                mark_price: None,
                index_price: None,
                src_file: "rest:fundingHistory".into(),
                src_line: i as i32,
            });
        }
        let symbol = data_room_adapters::hyperliquid::partition_symbol(coin);
        for (day, rows) in by_day {
            let bytes =
                data_room_schema::write_parquet(&data_room_schema::funding_rates_batch(rows)?)?;
            put_bytes(
                store,
                &data_room_schema::funding_silver_key("hyperliquid", &symbol, &day, "settled"),
                bytes,
            )
            .await?;
            partitions += 1;
        }
    }
    info!(partitions, "settled funding written");
    Ok(partitions)
}
