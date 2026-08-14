//! Deribit bronze → silver (SO-397): chain snapshots → options_quotes,
//! and DVOL index candles via REST (deep history is free, so the series
//! is repairable like Hyperliquid funding).

use std::io::Read;

use chrono::{Duration, NaiveDate};
use serde::Deserialize;
use tracing::info;

use crate::{get_bytes, handle_rejects, list_keys, put_bytes, Store};

#[derive(Deserialize)]
struct BronzeLine {
    ts_recv_ns: i64,
    payload: Option<String>,
    marker: Option<String>,
}

/// Normalize every chain.* stream captured on `date`.
pub async fn normalize_day(store: &Store, date: &str) -> anyhow::Result<usize> {
    let keys = list_keys(store, "bronze/v1/exchange=deribit/").await?;
    let mut streams: Vec<String> = keys
        .iter()
        .filter(|k| k.contains(&format!("/date={date}/")))
        .filter_map(|k| {
            k.split("/stream=")
                .nth(1)?
                .split('/')
                .next()
                .map(str::to_string)
        })
        .filter(|s| s.starts_with("chain."))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    streams.sort();

    for stream in &streams {
        normalize_stream_day(store, stream, date).await?;
    }
    Ok(streams.len())
}

async fn normalize_stream_day(store: &Store, stream: &str, date: &str) -> anyhow::Result<()> {
    let prefix = schema::bronze_ws_prefix("deribit", stream, date);
    let keys = list_keys(store, &prefix).await?;

    // underlying → quotes. One currency's day ≈ 1,440 snapshots × ~800
    // contracts ≈ 1.2M rows (~200 MB peak) — fine on the 4 GB host;
    // revisit with the chunked-writer pattern if chains grow.
    let mut by_underlying: std::collections::BTreeMap<String, Vec<schema::OptionsQuote>> =
        Default::default();
    let mut rejects = Vec::new();
    let mut lines_total = 0usize;
    for key in &keys {
        let gz = get_bytes(store, key).await?;
        let mut raw = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut raw)?;
        for (i, line) in raw.lines().enumerate() {
            lines_total += 1;
            let Ok(bl) = serde_json::from_str::<BronzeLine>(line) else {
                rejects.push(adapters::Reject {
                    src_file: key.clone(),
                    src_line: i as i32,
                    reason: "bad bronze envelope".into(),
                });
                continue;
            };
            if bl.marker.is_some() {
                continue;
            }
            let Some(payload) = bl.payload else { continue };
            match adapters::deribit::parse_book_summary(
                &payload,
                Some(bl.ts_recv_ns),
                key,
                i as i32,
            ) {
                Ok(quotes) => {
                    for q in quotes {
                        let u = adapters::deribit::underlying_of(&q.instrument_id)
                            .unwrap_or_else(|| "UNKNOWN".into());
                        by_underlying.entry(u).or_default().push(q);
                    }
                }
                Err(r) => rejects.push(r),
            }
        }
    }

    let mut rows_total = 0usize;
    for (underlying, rows) in by_underlying {
        rows_total += rows.len();
        let bytes = schema::write_parquet(&schema::options_quotes_batch(rows)?)?;
        put_bytes(
            store,
            &schema::options_silver_key("deribit", &underlying, date),
            bytes,
        )
        .await?;
    }
    info!(
        stream,
        date,
        rows = rows_total,
        lines = lines_total,
        "deribit chain normalized"
    );
    handle_rejects(
        store,
        &format!("exchange=deribit/stream={stream}/date={date}"),
        &rejects,
        lines_total.max(rows_total),
    )
    .await
}

#[derive(Deserialize)]
struct DvolResp {
    result: DvolData,
}

#[derive(Deserialize)]
struct DvolData {
    /// [ts_ms, open, high, low, close]
    data: Vec<[f64; 5]>,
}

/// (ts ns, open, high, low, close) DVOL candle.
type Candle = (i64, f64, f64, f64, f64);

/// Fetch DVOL hourly candles for [from, to] and write one vol_index
/// partition per day. Deribit serves full history, so re-runs repair
/// any gap. Chunked ~40 days per request to stay under response caps.
pub async fn dvol(
    store: &Store,
    currencies: &[String],
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<usize> {
    use arrow::array::{Float64Array, Int64Array, RecordBatch};
    use std::sync::Arc;

    let http = reqwest::Client::builder()
        .user_agent("data-room-dvol")
        .build()?;
    let mut partitions = 0usize;
    for currency in currencies {
        let mut candles: Vec<Candle> = Vec::new();
        let mut chunk_start = from;
        while chunk_start <= to {
            let chunk_end = (chunk_start + Duration::days(40)).min(to + Duration::days(1));
            let start_ms = chunk_start
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis();
            let end_ms = chunk_end
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis();
            let resp: DvolResp = http
                .get("https://www.deribit.com/api/v2/public/get_volatility_index_data")
                .query(&[
                    ("currency", currency.as_str()),
                    ("start_timestamp", &start_ms.to_string()),
                    ("end_timestamp", &end_ms.to_string()),
                    ("resolution", "3600"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            candles.extend(
                resp.result
                    .data
                    .iter()
                    .map(|c| (c[0] as i64 * 1_000_000, c[1], c[2], c[3], c[4])),
            );
            chunk_start = chunk_end;
        }

        let mut by_day: std::collections::BTreeMap<String, Vec<Candle>> = Default::default();
        for c in candles {
            let day = chrono::DateTime::from_timestamp_nanos(c.0)
                .format("%Y-%m-%d")
                .to_string();
            by_day.entry(day).or_default().push(c);
        }
        let symbol = format!("{}-DVOL", currency.to_uppercase());
        for (day, rows) in by_day {
            let batch = RecordBatch::try_new(
                Arc::new(schema::vol_index_schema()),
                vec![
                    Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.0))),
                    Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.1))),
                    Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.2))),
                    Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.3))),
                    Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.4))),
                ],
            )?;
            let bytes = schema::write_parquet(&batch)?;
            put_bytes(
                store,
                &schema::vol_index_key("deribit", &symbol, &day),
                bytes,
            )
            .await?;
            partitions += 1;
        }
    }
    info!(partitions, "dvol written");
    Ok(partitions)
}

#[derive(Deserialize)]
struct InstrumentsResp {
    result: Vec<DeribitInstrument>,
}

#[derive(Deserialize)]
struct DeribitInstrument {
    instrument_name: String,
    tick_size: f64,
    contract_size: f64,
}

/// Live option instruments for a currency → instrument-master rows.
pub async fn fetch_instruments(currency: &str) -> anyhow::Result<Vec<schema::Instrument>> {
    let resp: InstrumentsResp = reqwest::Client::builder()
        .user_agent("data-room-instruments")
        .build()?
        .get("https://www.deribit.com/api/v2/public/get_instruments")
        .query(&[("currency", currency), ("kind", "option")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp
        .result
        .into_iter()
        .filter_map(|i| {
            let (underlying, expiry, strike, opt_type) =
                adapters::deribit::parse_instrument_name(&i.instrument_name)?;
            Some(schema::Instrument {
                instrument_id: adapters::deribit::instrument_id(&i.instrument_name),
                exchange: "deribit".into(),
                native_symbol: i.instrument_name,
                asset_class: "option".into(),
                base: underlying.clone(),
                quote: underlying,
                tick_size: Some(i.tick_size),
                lot_size: Some(i.contract_size),
                strike: Some(strike),
                expiry: Some(expiry),
                opt_type: Some(opt_type.into()),
            })
        })
        .collect())
}
