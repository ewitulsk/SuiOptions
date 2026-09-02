//! L2 depth bronze → silver `book_l2` (SO-446, S5; plan
//! `docs/l2-silver-schema-plan.md` §3). Two venues, one table:
//!
//! - **bluefin**: `book.{sym}` (WS `OrderbookDiffDepthUpdate`, absolute
//!   sizes at `firstUpdateId..lastUpdateId`) merged with `depth.{sym}`
//!   (REST `/v1/exchange/depth` poller → full snapshots at `lastUpdateId`,
//!   the venue's documented sync point for the diff stream).
//! - **deepbook**: `book.{POOL}` — every 30 s poll is a snapshot.
//!
//! Bounded memory, by construction: the day is processed one bronze
//! **hour** at a time (the spool rotates on `ts_recv` hour, so an hour
//! directory is a `ts_recv` boundary and hour order is global order).
//! Each hour's rows are sorted and flushed as one row group through
//! `data_room_schema::BookL2Writer`; nothing holds a day. At 200 ms diff cadence
//! that is ≤ 18k frames/hour — tens of MB, not GB — on the 2 GB host.
//! Frames are deduped on `(is_snapshot, seq)` across the day (one small
//! set of ids, ~2M entries worst case).

use std::collections::{BTreeMap, HashSet};

use tracing::info;

use crate::{for_each_bronze_payload, handle_rejects, list_keys, put_bytes, Store};

/// Bronze stream kinds feeding `book_l2` per exchange.
fn stream_kinds(exchange: &str) -> &'static [&'static str] {
    match exchange {
        "bluefin" => &["book.", "depth."],
        "deepbook" => &["book."],
        _ => &[],
    }
}

fn stream_of(key: &str) -> Option<&str> {
    key.split("/stream=").nth(1)?.split('/').next()
}

fn hour_of(key: &str) -> Option<&str> {
    key.split("/hour=").nth(1)?.split('/').next()
}

/// Normalize every depth symbol captured on `date` for `exchange`.
/// Returns the number of symbol partitions written.
pub async fn normalize_day(store: &Store, exchange: &str, date: &str) -> anyhow::Result<usize> {
    let kinds = stream_kinds(exchange);
    anyhow::ensure!(!kinds.is_empty(), "no book_l2 streams for {exchange}");
    let keys = list_keys(store, &format!("bronze/v1/exchange={exchange}/")).await?;
    let day_keys: Vec<&String> = keys
        .iter()
        .filter(|k| k.contains(&format!("/date={date}/")))
        .collect();

    let mut symbols: Vec<String> = day_keys
        .iter()
        .filter_map(|k| {
            let stream = stream_of(k)?;
            kinds
                .iter()
                .find_map(|kind| stream.strip_prefix(kind))
                .map(str::to_string)
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    symbols.sort();

    let mut partitions = 0usize;
    for symbol in &symbols {
        // hour → bronze keys of every feeding stream, in sorted key order.
        let mut by_hour: BTreeMap<String, Vec<&String>> = BTreeMap::new();
        for k in &day_keys {
            let Some(stream) = stream_of(k) else { continue };
            if kinds.iter().any(|kind| stream == format!("{kind}{symbol}")) {
                if let Some(h) = hour_of(k) {
                    by_hour.entry(h.to_string()).or_default().push(k);
                }
            }
        }

        let mut writer = data_room_schema::BookL2Writer::new()?;
        let mut seen: HashSet<(bool, i64)> = HashSet::new();
        let mut rejects = Vec::new();
        let mut lines_total = 0usize;
        let mut frames = 0usize;
        for hour_keys in by_hour.values() {
            let mut rows: Vec<data_room_schema::BookL2> = Vec::new();
            for key in hour_keys {
                let stream = stream_of(key).unwrap_or_default();
                lines_total +=
                    for_each_bronze_payload(store, key, &mut rejects, |line_no, ts, payload| {
                        let frame = match (exchange, stream.split_once('.').map(|s| s.0)) {
                            ("bluefin", Some("book")) => {
                                data_room_adapters::bluefin::parse_book(payload, ts, key, line_no)?
                            }
                            ("bluefin", Some("depth")) => {
                                data_room_adapters::bluefin::parse_depth_rest(
                                    payload, ts, key, line_no,
                                )?
                            }
                            ("deepbook", Some("book")) => data_room_adapters::deepbook::parse_book(
                                payload, symbol, ts, key, line_no,
                            )?,
                            _ => return Ok(()),
                        };
                        if let Some(first) = frame.first() {
                            if seen.insert((first.is_snapshot, first.seq)) {
                                frames += 1;
                                rows.extend(frame);
                            }
                        }
                        Ok(())
                    })
                    .await?;
            }
            writer.write_chunk(rows)?;
        }

        let rows_total = writer.rows();
        if rows_total > 0 {
            let tmp = writer.finish()?;
            let bytes = std::fs::read(tmp.path())?;
            let partition = match exchange {
                "bluefin" => data_room_adapters::bluefin::partition_symbol(symbol),
                _ => data_room_adapters::deepbook::partition_symbol(symbol),
            };
            put_bytes(
                store,
                &data_room_schema::silver_key("book_l2", exchange, &partition, date),
                bytes,
            )
            .await?;
            partitions += 1;
        }
        info!(
            exchange,
            symbol,
            date,
            hours = by_hour.len(),
            frames,
            rows = rows_total,
            lines = lines_total,
            "book_l2 normalized"
        );
        handle_rejects(
            store,
            &format!("exchange={exchange}/book_l2.{symbol}/date={date}"),
            &rejects,
            lines_total,
        )
        .await?;
    }
    Ok(partitions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const DIFF: &str = include_str!("../../crates/adapters/fixtures/bluefin-diffdepth.json");
    const DIFF_MULTI: &str =
        include_str!("../../crates/adapters/fixtures/bluefin-diffdepth-multi.json");
    const PARTIAL: &str = include_str!("../../crates/adapters/fixtures/bluefin-partialdepth.json");
    const DEPTH_REST: &str = include_str!("../../crates/adapters/fixtures/bluefin-depth-rest.json");
    const DEEPBOOK: &str = include_str!("../../crates/adapters/fixtures/deepbook-orderbook.json");

    fn bronze(
        root: &std::path::Path,
        exchange: &str,
        stream: &str,
        date: &str,
        hour: &str,
        file: &str,
        lines: &[(i64, &str)],
    ) {
        let dir = root.join(format!(
            "bronze/v1/exchange={exchange}/stream={stream}/date={date}/hour={hour}"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(dir.join(file)).unwrap(),
            flate2::Compression::default(),
        );
        for (i, (ts, payload)) in lines.iter().enumerate() {
            let line = serde_json::json!({"ts_recv_ns": ts, "seq": i, "payload": payload});
            writeln!(gz, "{line}").unwrap();
        }
        // A marker line, which silver must ignore.
        writeln!(gz, r#"{{"ts_recv_ns":1,"seq":99,"marker":"connect"}}"#).unwrap();
        gz.finish().unwrap();
    }

    /// (ts_recv, seq, is_snapshot, side, price) per row.
    fn read_rows(path: &std::path::Path) -> Vec<(i64, i64, bool, String, f64)> {
        use arrow::array::{BooleanArray, Float64Array, Int64Array, StringArray};
        let bytes = bytes::Bytes::from(std::fs::read(path).unwrap());
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(bytes)
            .unwrap()
            .build()
            .unwrap();
        let mut out = Vec::new();
        for b in reader {
            let b = b.unwrap();
            let col = |n: &str| b.column_by_name(n).unwrap().clone();
            let rv = col("ts_recv");
            let rv = rv.as_any().downcast_ref::<Int64Array>().unwrap();
            let seq = col("seq");
            let seq = seq.as_any().downcast_ref::<Int64Array>().unwrap();
            let snap = col("is_snapshot");
            let snap = snap.as_any().downcast_ref::<BooleanArray>().unwrap();
            let side = col("side");
            let side = side.as_any().downcast_ref::<StringArray>().unwrap();
            let px = col("price");
            let px = px.as_any().downcast_ref::<Float64Array>().unwrap();
            for i in 0..b.num_rows() {
                out.push((
                    rv.value(i),
                    seq.value(i),
                    snap.value(i),
                    side.value(i).to_string(),
                    px.value(i),
                ));
            }
        }
        out
    }

    /// Real frames laid out as two bronze hours, two streams, two boots,
    /// a duplicated diff, a partial-depth frame and a marker line —
    /// everything the day path has to survive. Then: same bronze twice →
    /// byte-identical silver (spec §7 / plan §8).
    #[tokio::test]
    async fn bluefin_day_is_deterministic_and_merges_diffs_with_rest_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let date = "2026-09-01";
        let h0 = 1_788_307_200_000_000_000i64; // 2026-09-01T00:00:00Z
        let m = 60_000_000_000i64;

        // Hour 00: REST snapshot lands between two diffs from the socket;
        // the second boot re-captures the multi diff (dedup on seq).
        bronze(
            root,
            "bluefin",
            "book.SUI-PERP",
            date,
            "00",
            "boot-a-0.jsonl.gz",
            &[
                (h0 + m, DIFF),
                (h0 + 3 * m, DIFF_MULTI),
                (h0 + 4 * m, PARTIAL),
            ],
        );
        bronze(
            root,
            "bluefin",
            "book.SUI-PERP",
            date,
            "00",
            "boot-b-0.jsonl.gz",
            &[(h0 + 5 * m, DIFF_MULTI)],
        );
        bronze(
            root,
            "bluefin",
            "depth.SUI-PERP",
            date,
            "00",
            "boot-a-0.jsonl.gz",
            &[(h0 + 2 * m, DEPTH_REST)],
        );
        // The real diff frame again but at the next update id — the same
        // id would (correctly) dedup.
        let later = {
            let mut v: serde_json::Value = serde_json::from_str(DIFF).unwrap();
            v["payload"]["firstUpdateId"] = 67_638_386.into();
            v["payload"]["lastUpdateId"] = 67_638_386.into();
            v.to_string()
        };
        // Hour 01: one more diff, out of file-name order vs hour 00 to
        // prove hour order comes from the path, not the listing.
        bronze(
            root,
            "bluefin",
            "book.SUI-PERP",
            date,
            "01",
            "boot-b-1.jsonl.gz",
            &[(h0 + 61 * m, &later)],
        );
        // Another day, which must not leak in.
        bronze(
            root,
            "bluefin",
            "book.SUI-PERP",
            "2026-09-02",
            "00",
            "boot-b-2.jsonl.gz",
            &[(h0 + 1441 * m, DIFF_MULTI)],
        );

        let store = data_room_store::open(&format!("file://{}", root.display())).unwrap();
        assert_eq!(normalize_day(&store, "bluefin", date).await.unwrap(), 1);
        let key = root.join(data_room_schema::silver_key(
            "book_l2", "bluefin", "SUI-PERP", date,
        ));
        let rows = read_rows(&key);

        // 1 (diff) + 97 (snapshot) + 12 (multi, once) + 1 (diff again) = 111.
        assert_eq!(rows.len(), 111);
        assert_eq!(rows.iter().filter(|r| r.2).count(), 97);
        // Global order by (ts_recv, seq, side, price).
        let keys: Vec<_> = rows.iter().map(|r| (r.0, r.1, r.3.clone())).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let bid_prices: Vec<f64> = rows
            .iter()
            .filter(|r| r.1 == 67_639_469 && r.3 == "bid")
            .map(|r| r.4)
            .collect();
        let mut asc = bid_prices.clone();
        asc.sort_by(f64::total_cmp);
        assert_eq!(bid_prices, asc, "within a frame, price ascending");
        assert_eq!(rows[0], (h0 + m, 16_837_928, false, "bid".into(), 0.6787));
        assert_eq!(rows.last().unwrap().0, h0 + 61 * m);
        // The dedup dropped boot-b's copy: no row carries its ts_recv.
        assert!(rows.iter().all(|r| r.0 != h0 + 5 * m));

        let first = std::fs::read(&key).unwrap();
        assert_eq!(normalize_day(&store, "bluefin", date).await.unwrap(), 1);
        assert_eq!(
            std::fs::read(&key).unwrap(),
            first,
            "same bronze must give byte-identical silver"
        );
    }

    #[tokio::test]
    async fn deepbook_snapshots_dedup_on_venue_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let date = "2026-09-01";
        let h0 = 1_788_307_200_000_000_000i64;
        // The indexer served the same book (same timestamp) to two polls.
        bronze(
            root,
            "deepbook",
            "book.SUI_USDC",
            date,
            "03",
            "boot-0.jsonl.gz",
            &[(h0 + 1, DEEPBOOK), (h0 + 2, DEEPBOOK)],
        );
        let store = data_room_store::open(&format!("file://{}", root.display())).unwrap();
        assert_eq!(normalize_day(&store, "deepbook", date).await.unwrap(), 1);
        let key = root.join(data_room_schema::silver_key(
            "book_l2", "deepbook", "SUI-USDC", date,
        ));
        let rows = read_rows(&key);
        assert_eq!(rows.len(), 100);
        assert!(rows
            .iter()
            .all(|r| r.2 && r.1 == 1_788_325_441_259 && r.0 == h0 + 1));
        let first = std::fs::read(&key).unwrap();
        normalize_day(&store, "deepbook", date).await.unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), first);
    }

    #[tokio::test]
    async fn unknown_exchange_is_an_error_and_empty_day_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = data_room_store::open(&format!("file://{}", tmp.path().display())).unwrap();
        assert!(normalize_day(&store, "coinbase", "2026-09-01")
            .await
            .is_err());
        assert_eq!(
            normalize_day(&store, "bluefin", "2026-09-01")
                .await
                .unwrap(),
            0
        );
    }
}
