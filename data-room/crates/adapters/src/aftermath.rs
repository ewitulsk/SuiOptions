//! Aftermath router quote-ladder adapter (SO-446, S1c). Capture is one
//! `POST /api/router/trade-route` per rung (collector poller); each
//! response prices one fixed input size, so one bronze line is one
//! `QuoteLadder` row. Fixtures are real responses.
//!
//! Amounts are BigInt-style strings with a trailing `n` (JS BigInt
//! literal); coin decimals come from the coin type, so the row is in
//! human units either way. Direction is derived from which side is the
//! quote coin (USDC): base-in is `sell_base`, quote-in is `buy_base`.

use schema::QuoteLadder;
use serde::Deserialize;

use crate::Reject;

pub const EXCHANGE: &str = "aftermath";

/// Coin type → (symbol, decimals). Matched on the `::module::Name`
/// suffix so the address form (padded or not) is irrelevant. Only coins
/// the ladder is configured for; anything else is a reject, never a
/// guess.
fn coin(type_: &str) -> Option<(&'static str, u32)> {
    if type_.ends_with("::sui::SUI") {
        Some(("SUI", 9))
    } else if type_.ends_with("::usdc::USDC") {
        Some(("USDC", 6))
    } else {
        None
    }
}

#[derive(Deserialize)]
struct Coin {
    #[serde(rename = "type")]
    type_: String,
    amount: String,
}

#[derive(Deserialize)]
struct Path {
    #[serde(rename = "protocolName")]
    protocol_name: String,
}

#[derive(Deserialize)]
struct Route {
    paths: Vec<Path>,
}

#[derive(Deserialize)]
struct Response {
    #[serde(rename = "coinIn")]
    coin_in: Coin,
    #[serde(rename = "coinOut")]
    coin_out: Coin,
    #[serde(default)]
    routes: Vec<Route>,
}

fn reject(src_file: &str, src_line: i32, reason: impl ToString) -> Reject {
    Reject {
        src_file: src_file.into(),
        src_line,
        reason: reason.to_string(),
    }
}

/// `"14700000000000n"` with 9 decimals → 14700.0.
fn human(amount: &str, decimals: u32) -> Option<f64> {
    let raw: u128 = amount.strip_suffix('n').unwrap_or(amount).parse().ok()?;
    Some(raw as f64 / 10f64.powi(decimals as i32))
}

/// Parse one captured trade-route response into a ladder row.
pub fn parse(
    payload: &str,
    ts_recv: i64,
    src_file: &str,
    src_line: i32,
) -> Result<QuoteLadder, Reject> {
    let r: Response = serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
    let (in_sym, in_dec) = coin(&r.coin_in.type_).ok_or_else(|| {
        reject(
            src_file,
            src_line,
            format!("unknown coin {}", r.coin_in.type_),
        )
    })?;
    let (out_sym, out_dec) = coin(&r.coin_out.type_).ok_or_else(|| {
        reject(
            src_file,
            src_line,
            format!("unknown coin {}", r.coin_out.type_),
        )
    })?;
    let (pair, direction) = match (in_sym, out_sym) {
        (base, "USDC") => (format!("{base}-USDC"), "sell_base"),
        ("USDC", base) => (format!("{base}-USDC"), "buy_base"),
        _ => {
            return Err(reject(
                src_file,
                src_line,
                format!("no quote coin in {in_sym}->{out_sym}"),
            ))
        }
    };
    let amount_in = human(&r.coin_in.amount, in_dec).ok_or_else(|| {
        reject(
            src_file,
            src_line,
            format!("bad amount {}", r.coin_in.amount),
        )
    })?;
    let amount_out = human(&r.coin_out.amount, out_dec).ok_or_else(|| {
        reject(
            src_file,
            src_line,
            format!("bad amount {}", r.coin_out.amount),
        )
    })?;

    // Protocols in order of first appearance across all split routes.
    let mut protocols: Vec<String> = Vec::new();
    for p in r.routes.iter().flat_map(|r| r.paths.iter()) {
        if !protocols.contains(&p.protocol_name) {
            protocols.push(p.protocol_name.clone());
        }
    }
    let route = (!protocols.is_empty()).then(|| protocols.join(","));

    Ok(QuoteLadder {
        ts_recv,
        exchange: EXCHANGE.into(),
        pair,
        direction: direction.into(),
        amount_in,
        amount_out,
        route,
        src_file: src_file.into(),
        src_line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real responses, captured 2026-09-01 off the public endpoint.
    const SELL: &str = include_str!("../fixtures/aftermath-trade-route-sui-usdc.json");
    const BUY: &str = include_str!("../fixtures/aftermath-trade-route-usdc-sui.json");

    #[test]
    fn real_sell_base_quote_parses() {
        let q = parse(SELL, 7, "f", 3).unwrap();
        assert_eq!(q.pair, "SUI-USDC");
        assert_eq!(q.direction, "sell_base");
        assert_eq!(q.amount_in, 14_700.0); // 14700000000000n, 9 dp
        assert_eq!(q.amount_out, 10_597.298667); // 10597298667n, 6 dp
        assert_eq!(q.route.as_deref(), Some("Cetus,Bluefin"));
        assert_eq!((q.ts_recv, q.src_line), (7, 3));
    }

    #[test]
    fn real_buy_base_quote_parses() {
        let q = parse(BUY, 7, "f", 0).unwrap();
        assert_eq!(q.pair, "SUI-USDC");
        assert_eq!(q.direction, "buy_base");
        assert_eq!(q.amount_in, 10_000.0); // 10000000000n USDC, 6 dp
        assert_eq!(q.amount_out, 13_810.569308272); // 13810569308272n SUI, 9 dp
        assert_eq!(q.route.as_deref(), Some("Cetus,Bluefin"));
    }

    #[test]
    fn unknown_coin_and_garbage_are_rejects() {
        let bad_coin = r#"{"coinIn":{"type":"0x1::deep::DEEP","amount":"1n"},"coinOut":{"type":"0x2::sui::SUI","amount":"1n"},"routes":[]}"#;
        assert!(parse(bad_coin, 1, "f", 0)
            .unwrap_err()
            .reason
            .contains("unknown coin"));
        let no_quote = r#"{"coinIn":{"type":"0x2::sui::SUI","amount":"1n"},"coinOut":{"type":"0x2::sui::SUI","amount":"1n"},"routes":[]}"#;
        assert!(parse(no_quote, 1, "f", 0)
            .unwrap_err()
            .reason
            .contains("no quote coin"));
        assert!(parse("not json", 1, "f", 0).is_err());
    }

    #[test]
    fn amounts_without_bigint_suffix_still_parse() {
        let plain = r#"{"coinIn":{"type":"0x2::sui::SUI","amount":"1000000000"},"coinOut":{"type":"0xd::usdc::USDC","amount":"700000"}}"#;
        let q = parse(plain, 1, "f", 0).unwrap();
        assert_eq!((q.amount_in, q.amount_out), (1.0, 0.7));
        assert_eq!(q.route, None);
    }
}
