//! Instrument-master snapshot (spec §5.5): Coinbase products via public
//! REST + static rows for vision-only Binance symbols.

use schema::instrument::{instruments_batch, instruments_key, Instrument};
use serde::Deserialize;
use tracing::info;

use crate::{put_bytes, Store};

#[derive(Deserialize)]
struct CoinbaseProduct {
    id: String,
    base_currency: String,
    quote_currency: String,
    quote_increment: String,
    base_increment: String,
}

pub async fn fetch_coinbase(product_id: &str) -> anyhow::Result<Instrument> {
    let p: CoinbaseProduct = reqwest::Client::builder()
        .user_agent("data-room-instruments")
        .build()?
        .get(format!(
            "https://api.exchange.coinbase.com/products/{product_id}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(Instrument {
        instrument_id: Instrument::make_id(&p.base_currency, &p.quote_currency, "coinbase"),
        exchange: "coinbase".into(),
        native_symbol: p.id,
        asset_class: "spot".into(),
        base: p.base_currency,
        quote: p.quote_currency,
        tick_size: p.quote_increment.parse().ok(),
        lot_size: p.base_increment.parse().ok(),
    })
}

pub fn binance_static(native: &str) -> anyhow::Result<Instrument> {
    let (base, quote) = adapters::binance_vision::split_symbol(native)
        .ok_or_else(|| anyhow::anyhow!("bad binance symbol {native}"))?;
    Ok(Instrument {
        instrument_id: Instrument::make_id(&base, &quote, "binance"),
        exchange: "binance".into(),
        native_symbol: native.into(),
        asset_class: "spot".into(),
        base,
        quote,
        // No live API access (geo-blocked); sizes unknown until sourced
        // from exchangeInfo dumps.
        tick_size: None,
        lot_size: None,
    })
}

pub async fn snapshot(
    store: &Store,
    coinbase_products: &[String],
    binance_symbols: &[String],
    snapshot_date: &str,
) -> anyhow::Result<usize> {
    let mut rows = Vec::new();
    for p in coinbase_products {
        rows.push(fetch_coinbase(p).await?);
    }
    for s in binance_symbols {
        rows.push(binance_static(s)?);
    }
    let n = rows.len();
    let bytes = schema::write_parquet(&instruments_batch(rows)?)?;
    put_bytes(store, &instruments_key(snapshot_date), bytes).await?;
    info!(
        snapshot_date,
        instruments = n,
        "instrument snapshot written"
    );
    Ok(n)
}
