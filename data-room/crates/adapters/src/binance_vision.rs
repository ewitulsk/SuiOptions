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
    let mut out = Vec::new();
    let mut rejects = Vec::new();
    for_each_trade(
        reader,
        native_symbol,
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
    native_symbol: &str,
    src_file: &str,
    mut on_trade: impl FnMut(Trade),
    mut on_reject: impl FnMut(Reject),
) -> Result<(), anyhow::Error> {
    let instrument = instrument_id(native_symbol)
        .ok_or_else(|| anyhow::anyhow!("cannot split symbol {native_symbol}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn bad_rows_become_rejects_not_panics() {
        let csv = "0,notaprice,0.1,1.0,1544843507266,True,True\n";
        let (rows, rejects) = parse_trades_csv(csv.as_bytes(), "BTCUSDC", "f").unwrap();
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1);
    }
}
