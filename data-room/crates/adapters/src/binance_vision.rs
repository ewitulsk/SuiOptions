//! Binance `data.binance.vision` flat-file dump adapter (spec §6.6).
//!
//! Spot `trades` CSVs, no header row:
//! `id,price,qty,quote_qty,time,is_buyer_maker,is_best_match`
//! `time` is epoch ms in older files and epoch µs in newer ones — detected
//! by magnitude per row. `is_buyer_maker=True` means the taker sold.
//!
//! Only `kind=trades` normalizes into silver; aggTrades are archive-only
//! (same fills, would double count).

use std::io::Read;

use schema::Trade;

use crate::Reject;

pub const EXCHANGE: &str = "binance";

/// Binance native symbol ("BTCUSDC") assumed BASE+QUOTE with a known
/// quote suffix; extend the suffix list as symbols are onboarded.
pub fn split_symbol(native: &str) -> Option<(String, String)> {
    for quote in ["USDC", "USDT", "USD", "BTC", "ETH"] {
        if let Some(base) = native.strip_suffix(quote) {
            if !base.is_empty() {
                return Some((base.to_string(), quote.to_string()));
            }
        }
    }
    None
}

pub fn instrument_id(native: &str) -> Option<String> {
    let (base, quote) = split_symbol(native)?;
    Some(format!(
        "{}-{}.{}",
        base.to_lowercase(),
        quote.to_lowercase(),
        EXCHANGE
    ))
}

/// Perp instrument id for a um-futures symbol: "BTCUSDT" →
/// "btc-usdt-perp.binance". Dated futures ("BTCUSDT_240628") are not
/// perps and return None.
pub fn perp_instrument_id(native: &str) -> Option<String> {
    if native.contains('_') {
        return None;
    }
    let (base, quote) = split_symbol(native)?;
    Some(format!(
        "{}-{}-perp.{}",
        base.to_lowercase(),
        quote.to_lowercase(),
        EXCHANGE
    ))
}

/// Silver `symbol=` partition for a perp: "BTC-USDT-PERP".
pub fn perp_partition_symbol(native: &str) -> Option<String> {
    let (base, quote) = split_symbol(native)?;
    Some(format!("{base}-{quote}-PERP"))
}

/// Epoch of unknown resolution → nanoseconds. Dumps have used ms (13
/// digits) and µs (16 digits); accept seconds and ns too for robustness.
fn epoch_to_ns(v: i64) -> i64 {
    match v {
        v if v < 100_000_000_000 => v * 1_000_000_000, // seconds
        v if v < 100_000_000_000_000 => v * 1_000_000, // millis
        v if v < 100_000_000_000_000_000 => v * 1_000, // micros
        v => v,                                        // nanos
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "True" | "true" | "1" => Some(true),
        "False" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Parse a whole `trades` CSV into memory. Fine for daily files and
/// tests; for multi-GB monthly dumps use [`for_each_trade`], which is
/// what the normalizer streams through (a 2 GB host cannot hold a whole
/// 2026 month of rows).
pub fn parse_trades_csv<R: Read>(
    reader: R,
    native_symbol: &str,
    src_file: &str,
) -> Result<(Vec<Trade>, Vec<Reject>), anyhow::Error> {
    let instrument = instrument_id(native_symbol)
        .ok_or_else(|| anyhow::anyhow!("cannot split symbol {native_symbol}"))?;
    let mut out = Vec::new();
    let mut rejects = Vec::new();
    for_each_trade(
        reader,
        &instrument,
        src_file,
        |t| out.push(t),
        |r| rejects.push(r),
    )?;
    Ok((out, rejects))
}

/// Streaming variant: invoke `on_trade` / `on_reject` per row without
/// buffering the file. Row order is the file's order (dumps are
/// trade-id / time ordered).
pub fn for_each_trade<R: Read>(
    reader: R,
    instrument: &str,
    src_file: &str,
    mut on_trade: impl FnMut(Trade),
    mut on_reject: impl FnMut(Reject),
) -> Result<(), anyhow::Error> {
    let instrument = instrument.to_string();
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(reader);

    for (i, rec) in rdr.records().enumerate() {
        let line = i as i32;
        let mut reject = |reason: String| {
            on_reject(Reject {
                src_file: src_file.into(),
                src_line: line,
                reason,
            })
        };
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                reject(e.to_string());
                continue;
            }
        };
        // Some newer dumps ship a header row; skip it silently.
        if i == 0 && rec.get(0) == Some("id") {
            continue;
        }
        let parsed = (|| -> Option<Trade> {
            let trade_id = rec.get(0)?.to_string();
            let price: f64 = rec.get(1)?.parse().ok()?;
            let size: f64 = rec.get(2)?.parse().ok()?;
            let time: i64 = rec.get(4)?.parse().ok()?;
            let buyer_is_maker = parse_bool(rec.get(5)?)?;
            Some(Trade {
                ts_event: Some(epoch_to_ns(time)),
                ts_recv: None,
                exchange: EXCHANGE.into(),
                instrument_id: instrument.clone(),
                price,
                size,
                // buyer is maker → the aggressor sold.
                side: Some(if buyer_is_maker { "sell" } else { "buy" }.into()),
                trade_id,
                src_file: src_file.into(),
                src_line: line,
            })
        })();
        match parsed {
            Some(t) => on_trade(t),
            None => reject(format!("bad record: {rec:?}")),
        }
    }
    Ok(())
}

/// Parse a um-futures `bookTicker` CSV whole. Convenient for tests and
/// daily files; monthly dumps must use [`for_each_book_ticker`] — a busy
/// month is >100M rows and will not fit in memory.
pub fn parse_book_ticker_csv<R: Read>(
    reader: R,
    native_symbol: &str,
    src_file: &str,
) -> Result<(Vec<schema::BookTop>, Vec<Reject>), anyhow::Error> {
    let instrument = perp_instrument_id(native_symbol)
        .ok_or_else(|| anyhow::anyhow!("not a perp symbol: {native_symbol}"))?;
    let mut out = Vec::new();
    let mut rejects = Vec::new();
    for_each_book_ticker(
        reader,
        &instrument,
        src_file,
        |b| out.push(b),
        |r| rejects.push(r),
    )?;
    Ok((out, rejects))
}

/// Streaming `bookTicker` parse — the only historical quote data that
/// exists for SUI (2023-05-16 -> 2024-03-30, then Binance discontinued the
/// dump). Header:
/// `update_id,best_bid_price,best_bid_qty,best_ask_price,best_ask_qty,transaction_time,event_time`
///
/// `ts_event` is `transaction_time` (when the book changed), not
/// `event_time` (when Binance published it); `ts_recv` stays None because
/// archive rows have no capture clock — the spec is explicit that
/// archive-only backtests must model latency rather than inherit a
/// latency-free one.
///
/// Rows with a crossed or zero book are rejected rather than dropped: at
/// this cadence a crossed quote is a data defect worth counting, and the
/// rejects file is where the spec puts those.
pub fn for_each_book_ticker<R: Read>(
    reader: R,
    instrument: &str,
    src_file: &str,
    mut on_row: impl FnMut(schema::BookTop),
    mut on_reject: impl FnMut(Reject),
) -> Result<(), anyhow::Error> {
    let instrument = instrument.to_string();
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(reader);

    for (i, rec) in rdr.records().enumerate() {
        let line = i as i32;
        let mut reject = |reason: String| {
            on_reject(Reject {
                src_file: src_file.into(),
                src_line: line,
                reason,
            })
        };
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                reject(e.to_string());
                continue;
            }
        };
        // Futures dumps carry a header row.
        if i == 0 && rec.get(0) == Some("update_id") {
            continue;
        }
        let parsed = (|| -> Option<schema::BookTop> {
            let update_id: i64 = rec.get(0)?.parse().ok()?;
            let bid_px: f64 = rec.get(1)?.parse().ok()?;
            let bid_sz: f64 = rec.get(2)?.parse().ok()?;
            let ask_px: f64 = rec.get(3)?.parse().ok()?;
            let ask_sz: f64 = rec.get(4)?.parse().ok()?;
            let transaction_time: i64 = rec.get(5)?.parse().ok()?;
            Some(schema::BookTop {
                ts_event: Some(epoch_to_ns(transaction_time)),
                ts_recv: None,
                exchange: EXCHANGE.into(),
                instrument_id: instrument.clone(),
                update_id,
                bid_px,
                bid_sz,
                ask_px,
                ask_sz,
                src_file: src_file.into(),
                src_line: line,
            })
        })();
        match parsed {
            Some(b) if b.bid_px <= 0.0 || b.ask_px <= 0.0 => {
                reject(format!("non-positive quote: {rec:?}"))
            }
            Some(b) if b.bid_px >= b.ask_px => reject(format!("crossed book: {rec:?}")),
            Some(b) => on_row(b),
            None => reject(format!("bad record: {rec:?}")),
        }
    }
    Ok(())
}

/// Parse a um-futures `fundingRate` CSV (header:
/// `calc_time,funding_interval_hours,last_funding_rate`, ms epochs).
/// Rows are settled rates; `ts_recv` is None (archive-sourced).
pub fn parse_funding_csv<R: Read>(
    reader: R,
    native_symbol: &str,
    src_file: &str,
) -> Result<(Vec<schema::FundingRate>, Vec<Reject>), anyhow::Error> {
    let instrument = perp_instrument_id(native_symbol)
        .ok_or_else(|| anyhow::anyhow!("not a perp symbol: {native_symbol}"))?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(reader);

    let mut out = Vec::new();
    let mut rejects = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        let line = i as i32;
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                rejects.push(Reject {
                    src_file: src_file.into(),
                    src_line: line,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if i == 0 && rec.get(0) == Some("calc_time") {
            continue;
        }
        let parsed = (|| -> Option<schema::FundingRate> {
            Some(schema::FundingRate {
                ts_event: Some(epoch_to_ns(rec.get(0)?.parse().ok()?)),
                ts_recv: None,
                exchange: EXCHANGE.into(),
                instrument_id: instrument.clone(),
                rate: rec.get(2)?.parse().ok()?,
                interval_hours: rec.get(1)?.parse().ok()?,
                kind: "settled".into(),
                mark_price: None,
                index_price: None,
                src_file: src_file.into(),
                src_line: line,
            })
        })();
        match parsed {
            Some(f) => out.push(f),
            None => rejects.push(Reject {
                src_file: src_file.into(),
                src_line: line,
                reason: format!("bad funding record: {rec:?}"),
            }),
        }
    }
    Ok((out, rejects))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOK_TICKER: &str = include_str!("../fixtures/SUIUSDT-bookTicker-head.csv");

    /// Real head of `SUIUSDT-bookTicker-2024-03-30.zip`, header included.
    #[test]
    fn real_book_ticker_csv_parses() {
        let (rows, rejects) =
            parse_book_ticker_csv(BOOK_TICKER.as_bytes(), "SUIUSDT", "f").unwrap();
        assert!(rejects.is_empty(), "unexpected rejects: {rejects:?}");
        assert_eq!(rows.len(), 5, "header must be skipped");

        let r = &rows[0];
        assert_eq!(r.instrument_id, "sui-usdt-perp.binance");
        assert_eq!(r.exchange, "binance");
        assert_eq!(r.update_id, 4_307_082_984_787);
        assert_eq!(r.bid_px, 1.9056);
        assert_eq!(r.bid_sz, 33.7);
        assert_eq!(r.ask_px, 1.9057);
        assert_eq!(r.ask_sz, 511.0);
        // transaction_time (ms) -> ns, NOT event_time.
        assert_eq!(r.ts_event, Some(1_711_756_800_059 * 1_000_000));
        // Archive rows have no capture clock.
        assert_eq!(r.ts_recv, None);
        assert_eq!(r.src_line, 1);

        assert!(rows.iter().all(|b| b.bid_px < b.ask_px));
        let ids: Vec<i64> = rows.iter().map(|b| b.update_id).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "update_id monotonic");
    }

    #[test]
    fn book_ticker_rejects_bad_books_rather_than_dropping_them() {
        let csv = "update_id,best_bid_price,best_bid_qty,best_ask_price,best_ask_qty,transaction_time,event_time\n\
                   1,2.0,1.0,1.0,1.0,1711756800000,1711756800001\n\
                   2,0,1.0,1.0,1.0,1711756800000,1711756800001\n\
                   3,1.0,1.0,2.0,1.0,1711756800000,1711756800001\n";
        let (rows, rejects) = parse_book_ticker_csv(csv.as_bytes(), "SUIUSDT", "f").unwrap();
        assert_eq!(rows.len(), 1, "only the sane row survives");
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].reason.contains("crossed"));
        assert!(rejects[1].reason.contains("non-positive"));
    }

    #[test]
    fn book_ticker_needs_a_perp_symbol() {
        assert!(parse_book_ticker_csv(BOOK_TICKER.as_bytes(), "SUIUSDT_240628", "f").is_err());
    }

    #[test]
    fn splits_known_quotes() {
        assert_eq!(split_symbol("BTCUSDC"), Some(("BTC".into(), "USDC".into())));
        assert_eq!(split_symbol("SUIUSDT"), Some(("SUI".into(), "USDT".into())));
        assert_eq!(split_symbol("USDC"), None);
    }

    #[test]
    fn epoch_detection_covers_ms_and_us() {
        assert_eq!(epoch_to_ns(1_544_843_507_266), 1_544_843_507_266_000_000); // 2018 ms
        assert_eq!(
            epoch_to_ns(1_786_320_000_011_522),
            1_786_320_000_011_522_000
        ); // 2026 µs
    }

    #[test]
    fn parses_old_format_lines() {
        let csv = "0,3200.00000000,0.00312500,10.00000000,1544843507266,True,True\n\
                   1,3000.00000000,0.00333300,9.99900000,1544843521910,False,True\n";
        let (rows, rejects) = parse_trades_csv(csv.as_bytes(), "BTCUSDC", "f.zip").unwrap();
        assert!(rejects.is_empty());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].price, 3200.0);
        assert_eq!(rows[0].side.as_deref(), Some("sell")); // buyer maker → taker sold
        assert_eq!(rows[1].side.as_deref(), Some("buy"));
        assert_eq!(rows[0].ts_event, Some(1_544_843_507_266_000_000));
        assert_eq!(rows[0].ts_recv, None);
        assert_eq!(rows[0].instrument_id, "btc-usdc.binance");
    }

    #[test]
    fn real_2018_dump_fixture_parses_clean() {
        let zip_bytes = include_bytes!("../fixtures/BTCUSDC-trades-2018-12-15.zip");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes.as_slice())).unwrap();
        let file = archive.by_index(0).unwrap();
        let (rows, rejects) = parse_trades_csv(file, "BTCUSDC", "fixture.zip").unwrap();
        assert!(rejects.is_empty(), "rejects: {rejects:?}");
        assert_eq!(rows.len(), 1050, "fixture row count pinned");
        assert_eq!(rows[0].trade_id, "0");
        assert_eq!(rows[0].price, 3200.0);
        // All timestamps normalized to ns and within 2018-12-15 UTC.
        for r in &rows {
            let ns = r.ts_event.unwrap();
            assert!((1_544_832_000_000_000_000..1_544_918_400_000_000_000).contains(&ns));
        }
    }

    #[test]
    fn header_row_is_skipped() {
        let csv = "id,price,qty,quote_qty,time,is_buyer_maker,is_best_match\n\
                   5,100.0,1.0,100.0,1544843507266,True,True\n";
        let (rows, rejects) = parse_trades_csv(csv.as_bytes(), "BTCUSDC", "f").unwrap();
        assert!(rejects.is_empty());
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn perp_ids_and_dated_futures_guard() {
        assert_eq!(
            perp_instrument_id("BTCUSDT").as_deref(),
            Some("btc-usdt-perp.binance")
        );
        assert_eq!(
            perp_partition_symbol("BTCUSDT").as_deref(),
            Some("BTC-USDT-PERP")
        );
        assert_eq!(perp_instrument_id("BTCUSDT_240628"), None);
    }

    #[test]
    fn real_funding_fixture_parses() {
        let zip_bytes = include_bytes!("../fixtures/BTCUSDT-fundingRate-2020-01.zip");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes.as_slice())).unwrap();
        let file = archive.by_index(0).unwrap();
        let (rows, rejects) = parse_funding_csv(file, "BTCUSDT", "fixture.zip").unwrap();
        assert!(rejects.is_empty(), "rejects: {rejects:?}");
        // Jan 2020, 8h interval → ~93 settlements.
        assert!((85..=96).contains(&rows.len()), "got {}", rows.len());
        let r = &rows[0];
        assert_eq!(r.ts_event, Some(1_577_836_800_000_000_000)); // 2020-01-01T00:00Z
        assert_eq!(r.rate, -0.00012359);
        assert_eq!(r.interval_hours, 8.0);
        assert_eq!(r.kind, "settled");
        assert_eq!(r.instrument_id, "btc-usdt-perp.binance");
    }

    #[test]
    fn futures_trades_header_and_lowercase_bools_parse() {
        // Real um-futures format: header row + lowercase true/false, 6 cols.
        let csv = "id,price,qty,quote_qty,time,is_buyer_maker\n\
                   7962217185,64867.8,0.013,843.2814,1786320000011,true\n";
        let (rows, rejects) = parse_trades_csv(csv.as_bytes(), "BTCUSDT", "f").unwrap();
        assert!(rejects.is_empty());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].side.as_deref(), Some("sell"));
        assert_eq!(rows[0].ts_event, Some(1_786_320_000_011_000_000));
    }

    #[test]
    fn bad_rows_become_rejects_not_panics() {
        let csv = "0,notaprice,0.1,1.0,1544843507266,True,True\n";
        let (rows, rejects) = parse_trades_csv(csv.as_bytes(), "BTCUSDC", "f").unwrap();
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1);
    }
}
