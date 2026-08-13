//! Binance vision zips (bronze) → silver trades, partitioned per UTC day
//! of `ts_event` (spec §6.6). Only `kind=trades` normalizes; aggTrades
//! stays archive-only. Processed files get a `.done` state marker so the
//! daily timer only touches new arrivals; deleting markers forces replay.

use std::collections::BTreeMap;
use std::io::Read;

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

pub async fn normalize_zip(store: &Store, key: &str, symbol: &str) -> anyhow::Result<()> {
    let zip_bytes = get_bytes(store, key).await?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))?;
    anyhow::ensure!(
        archive.len() == 1,
        "expected single-entry zip, got {}",
        archive.len()
    );
    let mut csv = Vec::new();
    archive.by_index(0)?.read_to_end(&mut csv)?;

    let (rows, rejects) = adapters::binance_vision::parse_trades_csv(&csv[..], symbol, key)?;
    let total = rows.len() + rejects.len();

    // Partition by UTC day of ts_event.
    let mut by_day: BTreeMap<String, Vec<schema::Trade>> = BTreeMap::new();
    for t in rows {
        let ns = t.ts_event.expect("vision rows always carry ts_event");
        let day = DateTime::from_timestamp_nanos(ns)
            .format("%Y-%m-%d")
            .to_string();
        by_day.entry(day).or_default().push(t);
    }

    let instrument = adapters::binance_vision::instrument_id(symbol)
        .ok_or_else(|| anyhow::anyhow!("bad symbol {symbol}"))?;
    // silver symbol partition value: "BTC-USDC" from "btc-usdc.binance".
    let partition_symbol = instrument.split('.').next().unwrap().to_uppercase();

    let days = by_day.len();
    for (day, trades) in by_day {
        let batch = schema::trades_batch(trades)?;
        let bytes = schema::write_parquet(&batch)?;
        put_bytes(
            store,
            &schema::silver_key("trades", "binance", &partition_symbol, &day),
            bytes,
        )
        .await?;
    }
    info!(key, days, "vision zip normalized");

    let name = key.rsplit('/').next().unwrap();
    handle_rejects(store, &format!("vision/{name}"), &rejects, total).await
}
