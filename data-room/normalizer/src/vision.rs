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

use crate::{handle_rejects, list_keys, put_bytes, Store};

fn done_key(zip_name: &str) -> String {
    format!("silver/v1/_state/vision/{zip_name}.done")
}

/// Instrument mapping for a vision market label.
fn market_ids(market: &str, symbol: &str) -> anyhow::Result<(String, String)> {
    match market {
        "spot" => {
            let instrument = adapters::binance_vision::instrument_id(symbol)
                .ok_or_else(|| anyhow::anyhow!("bad symbol {symbol}"))?;
            let partition = instrument.split('.').next().unwrap().to_uppercase();
            Ok((instrument, partition))
        }
        "um-futures" => Ok((
            adapters::binance_vision::perp_instrument_id(symbol)
                .ok_or_else(|| anyhow::anyhow!("not a perp symbol {symbol}"))?,
            adapters::binance_vision::perp_partition_symbol(symbol)
                .ok_or_else(|| anyhow::anyhow!("bad symbol {symbol}"))?,
        )),
        other => anyhow::bail!("unsupported market label {other}"),
    }
}

/// Normalize all pending zips for `symbol` on a market: `trades` always,
/// plus `fundingRate` on futures. Files process in sorted name order
/// (monthly before the dailies of the same month), so overlapping
/// coverage resolves deterministically. Returns zips processed.
pub async fn normalize_pending(store: &Store, market: &str, symbol: &str) -> anyhow::Result<usize> {
    let done = list_keys(store, "silver/v1/_state/vision/").await?;
    let done: std::collections::HashSet<&str> =
        done.iter().filter_map(|k| k.rsplit('/').next()).collect();

    let kinds: &[&str] = if market == "um-futures" {
        &["trades", "fundingRate", "bookTicker"]
    } else {
        &["trades"]
    };
    let mut processed = 0;
    for kind in kinds {
        let prefix = schema::bronze_vision_prefix(market, kind, symbol);
        let zips = list_keys(store, &prefix).await?;
        for key in zips.iter().filter(|k| k.ends_with(".zip")) {
            let name = key.rsplit('/').next().unwrap();
            if done.contains(format!("{name}.done").as_str()) {
                continue;
            }
            match *kind {
                "trades" => normalize_zip(store, key, market, symbol).await?,
                "fundingRate" => normalize_funding_zip(store, key, market, symbol).await?,
                "bookTicker" => normalize_book_ticker_zip(store, key, market, symbol).await?,
                _ => unreachable!(),
            }
            put_bytes(store, &done_key(name), Vec::new()).await?;
            processed += 1;
        }
    }
    Ok(processed)
}

/// fundingRate zips are tiny (a few hundred rows/month): parse whole,
/// group per settlement day, write part-settled partitions.
pub async fn normalize_funding_zip(
    store: &Store,
    key: &str,
    market: &str,
    symbol: &str,
) -> anyhow::Result<()> {
    let (_, partition_symbol) = market_ids(market, symbol)?;
    let tmp = crate::get_to_tempfile(store, key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing zip {key}"))?;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(tmp.path())?)?;
    let entry = csv_entry_index(&mut archive, key)?;
    let mut csv = Vec::new();
    std::io::Read::read_to_end(&mut archive.by_index(entry)?, &mut csv)?;

    let (rows, rejects) = adapters::binance_vision::parse_funding_csv(&csv[..], symbol, key)?;
    let total = rows.len() + rejects.len();

    let mut by_day: std::collections::BTreeMap<String, Vec<schema::FundingRate>> =
        Default::default();
    for r in rows {
        let day = DateTime::from_timestamp_nanos(r.ts_event.unwrap())
            .format("%Y-%m-%d")
            .to_string();
        by_day.entry(day).or_default().push(r);
    }
    let days = by_day.len();
    for (day, rows) in by_day {
        let bytes = schema::write_parquet(&schema::funding_rates_batch(rows)?)?;
        put_bytes(
            store,
            &schema::funding_silver_key("binance", &partition_symbol, &day, "settled"),
            bytes,
        )
        .await?;
    }
    info!(key, days, "vision funding zip normalized");
    let name = key.rsplit('/').next().unwrap();
    handle_rejects(store, &format!("vision/{name}"), &rejects, total).await
}

fn day_of(ns: i64) -> String {
    DateTime::from_timestamp_nanos(ns)
        .format("%Y-%m-%d")
        .to_string()
}

/// Index of the canonical CSV entry: top-level (no directory) `.csv`.
/// Some vision zips ship a duplicate of the CSV under an internal
/// `fsx-data/…` path (e.g. BTCUSDC-trades-2021-04.zip) — pick the bare
/// entry, error if that isn't unambiguous.
fn csv_entry_index<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    key: &str,
) -> anyhow::Result<usize> {
    let mut hits = Vec::new();
    for i in 0..archive.len() {
        let name = archive.by_index(i)?.name().to_string();
        if !name.contains('/') && name.ends_with(".csv") {
            hits.push(i);
        }
    }
    anyhow::ensure!(
        hits.len() == 1,
        "expected exactly one top-level csv in {key}, found {}",
        hits.len()
    );
    Ok(hits[0])
}

/// `bookTicker` zips -> silver `book_top`. Same per-day chunked-writer
/// shape as [`normalize_zip`], and for the same reason only more so: a
/// single SUIUSDT day is ~1.9M quotes and 2024-01 is a 1.76 GB zip, so
/// nothing here may hold a day of rows.
///
/// Futures-only, and Binance discontinued the dump on 2024-03-30 — for SUI
/// this is the entire historical quote record that will ever exist.
pub async fn normalize_book_ticker_zip(
    store: &Store,
    key: &str,
    market: &str,
    symbol: &str,
) -> anyhow::Result<()> {
    let tmp = crate::get_to_tempfile(store, key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing zip {key}"))?;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(tmp.path())?)?;
    let entry = csv_entry_index(&mut archive, key)?;

    let (instrument, partition_symbol) = market_ids(market, symbol)?;

    const CHUNK_ROWS: usize = 100_000;
    struct DayAcc {
        writer: schema::BookTopWriter,
        chunk: Vec<schema::BookTop>,
    }
    let mut days_acc: std::collections::BTreeMap<String, DayAcc> = Default::default();
    let mut rejects = Vec::new();
    let mut total_rows = 0usize;
    let mut stream_err: Option<anyhow::Error> = None;

    {
        let reader = archive.by_index(entry)?;
        adapters::binance_vision::for_each_book_ticker(
            reader,
            &instrument,
            key,
            |b| {
                if stream_err.is_some() {
                    return;
                }
                let day = day_of(b.ts_event.expect("vision rows always carry ts_event"));
                let acc = match days_acc.entry(day) {
                    std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::btree_map::Entry::Vacant(v) => {
                        match schema::BookTopWriter::new() {
                            Ok(w) => v.insert(DayAcc {
                                writer: w,
                                chunk: Vec::new(),
                            }),
                            Err(e) => {
                                stream_err = Some(e);
                                return;
                            }
                        }
                    }
                };
                acc.chunk.push(b);
                total_rows += 1;
                if acc.chunk.len() >= CHUNK_ROWS {
                    if let Err(e) = acc.writer.write_chunk(std::mem::take(&mut acc.chunk)) {
                        stream_err = Some(e);
                    }
                }
            },
            |r| rejects.push(r),
        )?;
    }
    if let Some(e) = stream_err {
        return Err(e);
    }

    let days = days_acc.len();
    for (day, mut acc) in days_acc {
        acc.writer.write_chunk(std::mem::take(&mut acc.chunk))?;
        let tmp = acc.writer.finish()?;
        let bytes = std::fs::read(tmp.path())?;
        put_bytes(
            store,
            &schema::silver_key("book_top", "binance", &partition_symbol, &day),
            bytes,
        )
        .await?;
    }
    info!(key, days, rows = total_rows, "vision bookTicker normalized");

    let name = key.rsplit('/').next().unwrap();
    handle_rejects(
        store,
        &format!("vision/{name}"),
        &rejects,
        total_rows + rejects.len(),
    )
    .await
}

pub async fn normalize_zip(
    store: &Store,
    key: &str,
    market: &str,
    symbol: &str,
) -> anyhow::Result<()> {
    // Spool the zip to disk: perp monthlies are multi-GB and the zip
    // reader needs a Seek source anyway.
    let tmp = crate::get_to_tempfile(store, key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing zip {key}"))?;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(tmp.path())?)?;
    let entry = csv_entry_index(&mut archive, key)?;

    let (instrument, partition_symbol) = market_ids(market, symbol)?;

    // Stream rows through PER-DAY incremental writers. Dumps are mostly
    // time-ordered but not always (BTCUSDT-trades-2023-01.zip interleaves
    // late-January rows before January 1st), so no ordering is assumed:
    // each UTC day owns a chunked parquet writer, and a day's memory
    // footprint is one chunk of rows plus its compressed output buffer.
    const CHUNK_ROWS: usize = 100_000;
    struct DayAcc {
        writer: schema::TradesWriter,
        chunk: Vec<schema::Trade>,
    }
    let mut days_acc: std::collections::BTreeMap<String, DayAcc> = Default::default();
    let mut rejects = Vec::new();
    let mut total_rows = 0usize;
    let mut stream_err: Option<anyhow::Error> = None;

    {
        let reader = archive.by_index(entry)?;
        adapters::binance_vision::for_each_trade(
            reader,
            &instrument,
            key,
            |t| {
                if stream_err.is_some() {
                    return;
                }
                let day = day_of(t.ts_event.expect("vision rows always carry ts_event"));
                let acc = match days_acc.entry(day) {
                    std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::btree_map::Entry::Vacant(v) => {
                        match schema::TradesWriter::new() {
                            Ok(w) => v.insert(DayAcc {
                                writer: w,
                                chunk: Vec::new(),
                            }),
                            Err(e) => {
                                stream_err = Some(e);
                                return;
                            }
                        }
                    }
                };
                acc.chunk.push(t);
                total_rows += 1;
                if acc.chunk.len() >= CHUNK_ROWS {
                    if let Err(e) = acc.writer.write_chunk(std::mem::take(&mut acc.chunk)) {
                        stream_err = Some(e);
                    }
                }
            },
            |r| rejects.push(r),
        )?;
    }
    if let Some(e) = stream_err {
        return Err(e);
    }
    // Finish + upload one day at a time: each finished parquet sits on
    // disk, and only the day currently uploading is read into memory.
    let days = days_acc.len();
    for (day, mut acc) in days_acc {
        acc.writer.write_chunk(std::mem::take(&mut acc.chunk))?;
        let tmp = acc.writer.finish()?;
        let bytes = std::fs::read(tmp.path())?;
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
