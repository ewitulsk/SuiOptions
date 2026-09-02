//! Aftermath router bronze → silver `quote_ladder` (SO-446, S1c). Every
//! `route.*` poller stream captured on a day (one per rung and
//! direction) folds into one partition per pair; the direction lives in
//! the row, not the path, so both ladders of a pair sit side by side.

use std::collections::BTreeMap;

use tracing::info;

use crate::{for_each_bronze_payload, handle_rejects, list_keys, put_bytes, Store};

/// Normalize every `route.*` stream captured on `date`. Returns the
/// number of pair partitions written.
pub async fn normalize_day(store: &Store, date: &str) -> anyhow::Result<usize> {
    let keys = list_keys(store, "bronze/v1/exchange=aftermath/").await?;
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
        .filter(|s| s.starts_with("route."))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    streams.sort();

    // A day of ladder is ~3k rows across every rung — whole-day vectors
    // are fine here; this is the one table that is deliberately tiny.
    let mut by_pair: BTreeMap<String, Vec<schema::QuoteLadder>> = Default::default();
    let mut rejects = Vec::new();
    let mut lines_total = 0usize;
    for stream in &streams {
        let prefix = schema::bronze_ws_prefix("aftermath", stream, date);
        for key in list_keys(store, &prefix).await? {
            lines_total +=
                for_each_bronze_payload(store, &key, &mut rejects, |line_no, ts, payload| {
                    let q = adapters::aftermath::parse(payload, ts, &key, line_no)?;
                    by_pair.entry(q.pair.clone()).or_default().push(q);
                    Ok(())
                })
                .await?;
        }
    }

    let partitions = by_pair.len();
    let mut rows_total = 0usize;
    for (pair, rows) in by_pair {
        rows_total += rows.len();
        let bytes = schema::write_parquet(&schema::quote_ladder_batch(rows)?)?;
        put_bytes(
            store,
            &schema::quote_ladder_key("aftermath", &pair, date),
            bytes,
        )
        .await?;
    }
    info!(
        date,
        streams = streams.len(),
        partitions,
        rows = rows_total,
        lines = lines_total,
        "aftermath ladder normalized"
    );
    handle_rejects(
        store,
        &format!("exchange=aftermath/quote_ladder/date={date}"),
        &rejects,
        lines_total,
    )
    .await?;
    Ok(partitions)
}
