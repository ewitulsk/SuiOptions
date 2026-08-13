//! Binance vision zips (bronze) → silver trades, partitioned per UTC day
//! of `ts_event` (spec §6.6). Only `kind=trades` normalizes; aggTrades
//! stays archive-only. Processed files get a `.done` state marker so the
//! daily timer only touches new arrivals; deleting markers forces replay.
//!
//! Memory: rows stream through a one-day buffer — dumps are time-ordered,
//! so each UTC day is serialized to (zstd) parquet bytes as soon as the
//! next day begins. Peak usage is one day of rows plus the compressed
//! outputs, not one zip of rows (a 2026 monthly file holds tens of
//! millions of rows; the host has 2 GB).

use chrono::DateTime;
use tracing::info;

use crate::{get_bytes, handle_rejects, list_keys, put_bytes, Store};

fn done_key(zip_name: &str) -> String {
    format!("silver/v1/_state/vision/{zip_name}.done")
}

/// Normalize all pending trades zips for `symbol` (Binance native, e.g.
/// "BTCUSDC"). Files process in sorted name order (monthly before the
/// dailies of the same month), so overlapping coverage resolves
/// deterministically. Returns the number of zips processed.
pub async fn normalize_pending(store: &Store, market: &str, symbol: &str) -> anyhow::Result<usize> {
    let prefix = schema::bronze_vision_prefix(market, "trades", symbol);
    let zips = list_keys(store, &prefix).await?;
    let done = list_keys(store, "silver/v1/_state/vision/").await?;
    let done: std::collections::HashSet<&str> =
        done.iter().filter_map(|k| k.rsplit('/').next()).collect();

    let mut processed = 0;
    for key in zips.iter().filter(|k| k.ends_with(".zip")) {
        let name = key.rsplit('/').next().unwrap();
        if done.contains(format!("{name}.done").as_str()) {
            continue;
        }
        normalize_zip(store, key, symbol).await?;
        put_bytes(store, &done_key(name), Vec::new()).await?;
        processed += 1;
    }
    Ok(processed)
}

fn day_of(ns: i64) -> String {
    DateTime::from_timestamp_nanos(ns)
        .format("%Y-%m-%d")
        .to_string()
}

/// One UTC day of rows → deterministic parquet bytes (sync, so it can
/// run inside the streaming callback).
fn day_to_parquet(rows: Vec<schema::Trade>) -> anyhow::Result<Vec<u8>> {
    schema::write_parquet(&schema::trades_batch(rows)?)
}

pub async fn normalize_zip(store: &Store, key: &str, symbol: &str) -> anyhow::Result<()> {
    let zip_bytes = get_bytes(store, key).await?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))?;
    anyhow::ensure!(
        archive.len() == 1,
        "expected single-entry zip, got {}",
        archive.len()
    );

    let instrument = adapters::binance_vision::instrument_id(symbol)
        .ok_or_else(|| anyhow::anyhow!("bad symbol {symbol}"))?;
    // silver symbol partition value: "BTC-USDC" from "btc-usdc.binance".
    let partition_symbol = instrument.split('.').next().unwrap().to_uppercase();

    // Stream rows; when the UTC day advances, compress the finished day
    // to parquet immediately and drop its rows. Dumps are time-ordered;
    // an out-of-order day would clobber an already-serialized partition,
    // so it is a hard error (bronze stays intact, nothing written).
    let mut current_day: Option<String> = None;
    let mut day_rows: Vec<schema::Trade> = Vec::new();
    let mut outputs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rejects = Vec::new();
    let mut total_rows = 0usize;
    let mut stream_err: Option<anyhow::Error> = None;

    {
        let reader = archive.by_index(0)?;
        adapters::binance_vision::for_each_trade(
            reader,
            symbol,
            key,
            |t| {
                if stream_err.is_some() {
                    return;
                }
                let day = day_of(t.ts_event.expect("vision rows always carry ts_event"));
                match &current_day {
                    Some(d) if *d == day => {}
                    Some(d) if *d < day => {
                        total_rows += day_rows.len();
                        match day_to_parquet(std::mem::take(&mut day_rows)) {
                            Ok(bytes) => outputs.push((d.clone(), bytes)),
                            Err(e) => {
                                stream_err = Some(e);
                                return;
                            }
                        }
                        current_day = Some(day);
                    }
                    Some(d) => {
                        stream_err = Some(anyhow::anyhow!(
                            "out-of-order timestamps in {key}: {day} after {d}"
                        ));
                        return;
                    }
                    None => current_day = Some(day),
                }
                day_rows.push(t);
            },
            |r| rejects.push(r),
        )?;
    }
    if let Some(e) = stream_err {
        return Err(e);
    }
    if let Some(d) = current_day.take() {
        total_rows += day_rows.len();
        outputs.push((d, day_to_parquet(std::mem::take(&mut day_rows))?));
    }

    let days = outputs.len();
    for (day, bytes) in outputs {
        put_bytes(
            store,
            &schema::silver_key("trades", "binance", &partition_symbol, &day),
            bytes,
        )
        .await?;
    }
    info!(key, days, rows = total_rows, "vision zip normalized");

    let name = key.rsplit('/').next().unwrap();
    handle_rejects(
        store,
        &format!("vision/{name}"),
        &rejects,
        total_rows + rejects.len(),
    )
    .await
}
