//! Deribit options adapter (SO-397). Capture is REST chain snapshots:
//! `public/get_book_summary_by_currency?currency=X&kind=option` returns
//! every listed contract's bid/ask/mark/mark_iv/OI/underlying in one
//! response — a full vol-surface snapshot per poll. Fixture is a real
//! (trimmed) response.
//!
//! Prices are Deribit-native: options quoted in the base coin (a BTC
//! option's price is in BTC). `mark_iv` is percent annualized.

use chrono::NaiveDate;
use data_room_schema::OptionsQuote;
use serde::Deserialize;

use crate::Reject;

pub const EXCHANGE: &str = "deribit";

/// "BTC-25SEP26-84000-P" → (underlying, expiry ns, strike, "put"/"call").
/// Deribit options expire 08:00 UTC on the named day.
pub fn parse_instrument_name(name: &str) -> Option<(String, i64, f64, &'static str)> {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    let underlying = parts[0].to_string();
    let date = parse_deribit_date(parts[1])?;
    let expiry_ns = date.and_hms_opt(8, 0, 0)?.and_utc().timestamp_nanos_opt()?;
    let strike: f64 = parts[2].parse().ok()?;
    let opt_type = match parts[3] {
        "C" => "call",
        "P" => "put",
        _ => return None,
    };
    Some((underlying, expiry_ns, strike, opt_type))
}

/// "25SEP26" → NaiveDate.
fn parse_deribit_date(s: &str) -> Option<NaiveDate> {
    // Day may be 1 or 2 digits ("1AUG26" / "25SEP26").
    NaiveDate::parse_from_str(s, "%d%b%y")
        .or_else(|_| NaiveDate::parse_from_str(&format!("0{s}"), "%d%b%y"))
        .ok()
}

/// Our id for a Deribit contract: lowercase native name + venue suffix,
/// e.g. "btc-25sep26-84000-p.deribit".
pub fn instrument_id(native: &str) -> String {
    format!("{}.{}", native.to_lowercase(), EXCHANGE)
}

#[derive(Deserialize)]
struct Summary {
    instrument_name: String,
    #[serde(default)]
    bid_price: Option<f64>,
    #[serde(default)]
    ask_price: Option<f64>,
    #[serde(default)]
    mark_price: Option<f64>,
    #[serde(default)]
    mark_iv: Option<f64>,
    #[serde(default)]
    underlying_price: Option<f64>,
    #[serde(default)]
    open_interest: Option<f64>,
    /// ms epoch of the summary row.
    #[serde(default)]
    creation_timestamp: Option<i64>,
}

#[derive(Deserialize)]
struct Response {
    result: Vec<Summary>,
}

/// Parse one captured book-summary payload (a whole chain snapshot) into
/// per-contract quotes. Rows whose instrument name doesn't parse are
/// rejects; the rest of the snapshot still normalizes.
pub fn parse_book_summary(
    payload: &str,
    ts_recv: Option<i64>,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<OptionsQuote>, Reject> {
    let resp: Response = serde_json::from_str(payload).map_err(|e| Reject {
        src_file: src_file.into(),
        src_line,
        reason: e.to_string(),
    })?;
    Ok(resp
        .result
        .into_iter()
        .map(|s| OptionsQuote {
            ts_event: s.creation_timestamp.map(|ms| ms * 1_000_000),
            ts_recv,
            exchange: EXCHANGE.into(),
            instrument_id: instrument_id(&s.instrument_name),
            bid: s.bid_price,
            ask: s.ask_price,
            mark_price: s.mark_price,
            mark_iv: s.mark_iv,
            underlying_price: s.underlying_price,
            open_interest: s.open_interest,
            src_file: src_file.into(),
            src_line,
        })
        .collect())
}

/// The `underlying=` partition value for a contract id
/// ("btc-25sep26-84000-p.deribit" → "BTC").
pub fn underlying_of(instrument_id: &str) -> Option<String> {
    Some(instrument_id.split('-').next()?.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/deribit-book-summary.json");

    #[test]
    fn real_snapshot_parses() {
        let quotes = parse_book_summary(FIXTURE, Some(7), "f", 0).unwrap();
        assert_eq!(quotes.len(), 4);
        let q = &quotes[0];
        assert_eq!(q.instrument_id, "btc-25sep26-84000-p.deribit");
        assert_eq!(q.bid, Some(0.3165));
        assert_eq!(q.mark_iv, Some(38.02));
        assert!(q.underlying_price.unwrap() > 60_000.0);
        assert_eq!(q.ts_recv, Some(7));
        assert!(q.ts_event.unwrap() > 1_700_000_000_000_000_000);
    }

    #[test]
    fn instrument_names_parse() {
        let (u, exp, k, t) = parse_instrument_name("BTC-25SEP26-84000-P").unwrap();
        assert_eq!((u.as_str(), k, t), ("BTC", 84_000.0, "put"));
        // 2026-09-25 08:00 UTC
        assert_eq!(exp, 1_790_323_200_000_000_000);
        let (_, _, _, t2) = parse_instrument_name("BTC-1AUG26-60000-C").unwrap();
        assert_eq!(t2, "call");
        assert!(parse_instrument_name("BTC-PERPETUAL").is_none());
        assert!(parse_instrument_name("BTC-25SEP26-84000-X").is_none());
    }

    #[test]
    fn underlying_partition_value() {
        assert_eq!(
            underlying_of("btc-25sep26-84000-p.deribit").as_deref(),
            Some("BTC")
        );
    }
}
