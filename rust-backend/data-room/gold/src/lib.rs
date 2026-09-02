//! Gold derived products (spec §5.7/§8): bars, realized vol, gaps ledger.
//! Everything here is regenerable from silver + bronze at will.

pub mod bars;
pub mod gaps;
pub mod read;
pub mod rv;

use std::sync::Arc;

use object_store::ObjectStore;

pub type Store = Arc<dyn ObjectStore>;

/// (exchange, symbol) pairs with a silver trades partition on `date`.
pub async fn pairs_for_date(store: &Store, date: &str) -> anyhow::Result<Vec<(String, String)>> {
    let keys = crate::read::list_keys(store, "silver/v1/trades/").await?;
    let mut out: Vec<(String, String)> = keys
        .iter()
        .filter(|k| k.contains(&format!("/date={date}/")))
        .filter_map(|k| {
            let ex = k.split("/exchange=").nth(1)?.split('/').next()?;
            let sym = k.split("/symbol=").nth(1)?.split('/').next()?;
            Some((ex.to_string(), sym.to_string()))
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn instrument_id(exchange: &str, symbol: &str) -> String {
    format!("{}.{}", symbol.to_lowercase(), exchange)
}
