//! Bluefin funding settlements → silver `funding_rates` (SO-446, doc 08
//! §3.2). Two independent sources per symbol, cross-checked:
//!
//! - **(a)** the `funding.{sym}` REST-history poller → `part-settled`
//!   (`kind = settled`). Authoritative rates; a poll on day D+1 still
//!   carries day D's last settlement, so the partition for D reads the
//!   bronze of D and D+1 and keeps rows whose settlement falls on D.
//! - **(b)** `nextFundingTimeAtMillis` rollovers on the `ticker.{sym}`
//!   stream → `part-derived` (`kind = derived`). Live-observed timing
//!   (`ts_recv` is the first post-rollover frame); the rate is that
//!   frame's `lastFundingRateE9`. The rollover clock is seeded from the
//!   last hour of D-1 so the 00:00 settlement is not lost at the day
//!   boundary. Kept in its own part and `kind` so consumers filtering
//!   `kind = 'settled'` never double count.
//!
//! Memory: the ticker stream is folded frame by frame into one
//! `Option<i64>` of state — no day-sized vectors.

use std::collections::BTreeMap;

use chrono::DateTime;
use data_room_schema::FundingRate;
use tracing::{info, warn};

use crate::{for_each_bronze_payload, handle_rejects, list_keys, put_bytes, Store};

/// (a) and (b) match when their settlement times sit within this window:
/// the ticker clock is on the hour, the history endpoint stamps the
/// actual application time a few seconds later.
const MATCH_WINDOW_MS: i64 = 5 * 60 * 1_000;

fn day_of_ms(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn shift_day(date: &str, days: i64) -> anyhow::Result<String> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    Ok((d + chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string())
}

fn stream_symbol<'a>(key: &'a str, kind: &str) -> Option<&'a str> {
    key.split("/stream=")
        .nth(1)?
        .split('/')
        .next()?
        .strip_prefix(kind)
}

/// Normalize every symbol with `funding.*` or `ticker.*` bronze on
/// `date`. Returns the number of partition files written.
pub async fn normalize_day(store: &Store, date: &str) -> anyhow::Result<usize> {
    let keys = list_keys(store, "bronze/v1/exchange=bluefin/").await?;
    let prev = shift_day(date, -1)?;
    let next = shift_day(date, 1)?;

    let mut symbols: Vec<String> = keys
        .iter()
        .filter(|k| k.contains(&format!("/date={date}/")))
        .filter_map(|k| stream_symbol(k, "funding.").or_else(|| stream_symbol(k, "ticker.")))
        .map(str::to_string)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    symbols.sort();

    let mut written = 0usize;
    for symbol in &symbols {
        let mut rejects = Vec::new();
        let mut lines_total = 0usize;

        // (a) REST history: first capture of a settlement wins (keys are
        // sorted, so this is deterministic).
        let mut rest: BTreeMap<i64, FundingRate> = BTreeMap::new();
        for day in [date, next.as_str()] {
            let prefix =
                data_room_schema::bronze_ws_prefix("bluefin", &format!("funding.{symbol}"), day);
            for key in keys.iter().filter(|k| k.starts_with(&prefix)) {
                lines_total +=
                    for_each_bronze_payload(store, key, &mut rejects, |line_no, ts, payload| {
                        for r in data_room_adapters::bluefin::parse_funding_history(
                            payload,
                            Some(ts),
                            key,
                            line_no,
                        )? {
                            let ms = r.ts_event.unwrap() / 1_000_000;
                            if day_of_ms(ms) == date {
                                rest.entry(ms).or_insert(r);
                            }
                        }
                        Ok(())
                    })
                    .await?;
            }
        }

        // (b) ticker rollovers, seeded from the last hour of the prior day.
        let ticker_stream = format!("ticker.{symbol}");
        let seed_prefix = format!(
            "{}hour=23/",
            data_room_schema::bronze_ws_prefix("bluefin", &ticker_stream, &prev)
        );
        let day_prefix = data_room_schema::bronze_ws_prefix("bluefin", &ticker_stream, date);
        let mut derived: Vec<FundingRate> = Vec::new();
        let mut clock: Option<i64> = None;
        let mut clock_regressions = 0usize;
        for key in keys
            .iter()
            .filter(|k| k.starts_with(&seed_prefix) || k.starts_with(&day_prefix))
        {
            lines_total +=
                for_each_bronze_payload(store, key, &mut rejects, |line_no, ts, payload| {
                    let t = data_room_adapters::bluefin::parse_ticker(payload, key, line_no)?;
                    if let Some(prev_next) = clock {
                        if t.next_funding_ms > prev_next {
                            if day_of_ms(prev_next) == date {
                                derived.push(FundingRate {
                                    ts_event: Some(prev_next * 1_000_000),
                                    ts_recv: Some(ts),
                                    exchange: data_room_adapters::bluefin::EXCHANGE.into(),
                                    instrument_id: data_room_adapters::bluefin::instrument_id(
                                        &t.symbol,
                                    ),
                                    rate: t.last_rate,
                                    interval_hours:
                                        data_room_adapters::bluefin::FUNDING_INTERVAL_HOURS,
                                    kind: "derived".into(),
                                    mark_price: Some(t.mark_price),
                                    index_price: Some(t.oracle_price),
                                    src_file: key.clone(),
                                    src_line: line_no,
                                });
                            }
                        } else if t.next_funding_ms < prev_next {
                            clock_regressions += 1;
                        }
                    }
                    clock = Some(t.next_funding_ms);
                    Ok(())
                })
                .await?;
        }

        // Cross-check (a) against (b).
        let (mut matched, mut mismatched, mut ticker_only) = (0usize, 0usize, 0usize);
        for d in &derived {
            let ms = d.ts_event.unwrap() / 1_000_000;
            match rest
                .range(ms - MATCH_WINDOW_MS..=ms + MATCH_WINDOW_MS)
                .next()
            {
                Some((_, r)) if r.rate == d.rate => matched += 1,
                Some(_) => mismatched += 1,
                None => ticker_only += 1,
            }
        }
        let rest_only = rest.len().saturating_sub(matched + mismatched);
        info!(
            symbol,
            date,
            rest = rest.len(),
            derived = derived.len(),
            matched,
            mismatched,
            rest_only,
            ticker_only,
            clock_regressions,
            "bluefin funding cross-check"
        );
        if mismatched > 0 || clock_regressions > 0 {
            warn!(
                symbol,
                date,
                mismatched,
                clock_regressions,
                "bluefin funding: ticker-derived settlements disagree with REST history"
            );
        }

        let partition = data_room_adapters::bluefin::partition_symbol(symbol);
        if !rest.is_empty() {
            let rows: Vec<FundingRate> = rest.into_values().collect();
            let bytes =
                data_room_schema::write_parquet(&data_room_schema::funding_rates_batch(rows)?)?;
            put_bytes(
                store,
                &data_room_schema::funding_silver_key("bluefin", &partition, date, "settled"),
                bytes,
            )
            .await?;
            written += 1;
        }
        if !derived.is_empty() {
            let bytes =
                data_room_schema::write_parquet(&data_room_schema::funding_rates_batch(derived)?)?;
            put_bytes(
                store,
                &data_room_schema::funding_silver_key("bluefin", &partition, date, "derived"),
                bytes,
            )
            .await?;
            written += 1;
        }
        handle_rejects(
            store,
            &format!("exchange=bluefin/funding.{symbol}/date={date}"),
            &rejects,
            lines_total,
        )
        .await?;
    }
    info!(
        date,
        symbols = symbols.len(),
        written,
        "bluefin funding normalized"
    );
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TICKER: &str = include_str!("../../crates/adapters/fixtures/bluefin-ticker.json");
    const HISTORY: &str =
        include_str!("../../crates/adapters/fixtures/bluefin-fundingRateHistory.json");

    /// Real ticker fixture with the funding clock moved: `next` is the
    /// scheduled settlement (ms), `rate` the E9 string.
    fn ticker(next: i64, rate: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(TICKER).unwrap();
        v["payload"]["nextFundingTimeAtMillis"] = next.into();
        v["payload"]["lastFundingRateE9"] = rate.into();
        v.to_string()
    }

    fn bronze(root: &std::path::Path, stream: &str, date: &str, hour: &str, lines: &[(i64, &str)]) {
        let dir = root.join(format!(
            "bronze/v1/exchange=bluefin/stream={stream}/date={date}/hour={hour}"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(dir.join("boot-0.jsonl.gz")).unwrap(),
            flate2::Compression::default(),
        );
        for (i, (ts, payload)) in lines.iter().enumerate() {
            let line = serde_json::json!({"ts_recv_ns": ts, "seq": i, "payload": payload});
            writeln!(gz, "{line}").unwrap();
        }
        gz.finish().unwrap();
    }

    fn read_rows(path: &std::path::Path) -> Vec<(i64, Option<i64>, f64, String)> {
        use arrow::array::{Array, Float64Array, Int64Array, StringArray};
        let bytes = bytes::Bytes::from(std::fs::read(path).unwrap());
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(bytes)
            .unwrap()
            .build()
            .unwrap();
        let mut out = Vec::new();
        for b in reader {
            let b = b.unwrap();
            let ev = b
                .column_by_name("ts_event")
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .clone();
            let rv = b
                .column_by_name("ts_recv")
                .unwrap()
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .clone();
            let rate = b
                .column_by_name("rate")
                .unwrap()
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .clone();
            let kind = b
                .column_by_name("kind")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .clone();
            for i in 0..b.num_rows() {
                out.push((
                    ev.value(i),
                    rv.is_valid(i).then(|| rv.value(i)),
                    rate.value(i),
                    kind.value(i).to_string(),
                ));
            }
        }
        out
    }

    /// 2026-08-15: the fixture history rows are 2026-09-01, so the
    /// settled part is driven by a synthetic day-D window of the same
    /// shape; the derived part comes from three rollovers, one of which
    /// crosses midnight and is only visible thanks to the D-1 seed.
    #[tokio::test]
    async fn rollovers_and_history_land_in_separate_parts_and_cross_check() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let date = "2026-08-15";
        let h = 3_600_000i64;
        let t0 = 1_786_752_000_000i64; // 2026-08-15T00:00:00Z
        let ns = |ms: i64| ms * 1_000_000;

        // D-1 hour 23: clock says next = 00:00 (t0).
        bronze(
            root,
            "ticker.SUI-PERP",
            "2026-08-14",
            "23",
            &[(ns(t0 - 60_000), &ticker(t0, "12500"))],
        );
        // D: rollover to 01:00 observed at 00:00:04 (settles t0), then a
        // flat frame, then rollover to 02:00 at 01:00:03 (settles t0+1h).
        bronze(
            root,
            "ticker.SUI-PERP",
            date,
            "00",
            &[
                (ns(t0 + 4_000), &ticker(t0 + h, "12500")),
                (ns(t0 + 900_000), &ticker(t0 + h, "12500")),
            ],
        );
        bronze(
            root,
            "ticker.SUI-PERP",
            date,
            "01",
            &[(ns(t0 + h + 3_000), &ticker(t0 + 2 * h, "-25000"))],
        );

        // REST history polled on D (covers t0 and t0+1h, applied 6 s late,
        // plus a D-1 row that must be filtered out) and on D+1 (dup of
        // t0+1h, plus the 02:00 one only D+1 saw). The t0+1h rate
        // deliberately disagrees with the ticker to exercise the counter.
        let hist = |rows: &[(i64, &str)]| {
            serde_json::to_string(
                &rows
                    .iter()
                    .map(|(t, r)| serde_json::json!({"fundingRateE9": r, "fundingTimeAtMillis": t, "symbol": "SUI-PERP"}))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        bronze(
            root,
            "funding.SUI-PERP",
            date,
            "01",
            &[(
                ns(t0 + h + 30_000),
                &hist(&[
                    (t0 + h + 6_000, "12500"),
                    (t0 + 6_000, "12500"),
                    (t0 - h + 6_000, "12500"),
                ]),
            )],
        );
        bronze(
            root,
            "funding.SUI-PERP",
            "2026-08-16",
            "00",
            &[(
                ns(t0 + 24 * h + 30_000),
                &hist(&[
                    (t0 + 24 * h + 6_000, "12500"),
                    (t0 + 2 * h + 6_000, "12500"),
                    (t0 + h + 6_000, "99"),
                ]),
            )],
        );
        // The real fixture parses too (shape regression), even though its
        // dates fall outside the window and contribute nothing.
        bronze(
            root,
            "funding.SUI-PERP",
            date,
            "02",
            &[(ns(t0 + 2 * h), HISTORY)],
        );

        let store = data_room_store::open(&format!("file://{}", root.display())).unwrap();
        assert_eq!(normalize_day(&store, date).await.unwrap(), 2);

        let settled = read_rows(&root.join(data_room_schema::funding_silver_key(
            "bluefin", "SUI-PERP", date, "settled",
        )));
        assert_eq!(
            settled.iter().map(|r| (r.0, r.2)).collect::<Vec<_>>(),
            vec![
                (ns(t0 + 6_000), 0.0000125),
                (ns(t0 + h + 6_000), 0.0000125),
                (ns(t0 + 2 * h + 6_000), 0.0000125)
            ],
            "settlement-day filter + first-capture-wins dedup"
        );
        assert!(settled.iter().all(|r| r.3 == "settled" && r.1.is_some()));

        let derived = read_rows(&root.join(data_room_schema::funding_silver_key(
            "bluefin", "SUI-PERP", date, "derived",
        )));
        assert_eq!(
            derived,
            vec![
                (ns(t0), Some(ns(t0 + 4_000)), 0.0000125, "derived".into()),
                (
                    ns(t0 + h),
                    Some(ns(t0 + h + 3_000)),
                    -0.000025,
                    "derived".into()
                ),
            ],
            "one row per observed rollover, midnight one included via the D-1 seed"
        );

        // Determinism: same bronze → byte-identical silver.
        let before = std::fs::read(root.join(data_room_schema::funding_silver_key(
            "bluefin", "SUI-PERP", date, "derived",
        )))
        .unwrap();
        normalize_day(&store, date).await.unwrap();
        let after = std::fs::read(root.join(data_room_schema::funding_silver_key(
            "bluefin", "SUI-PERP", date, "derived",
        )))
        .unwrap();
        assert_eq!(before, after);
    }
}
