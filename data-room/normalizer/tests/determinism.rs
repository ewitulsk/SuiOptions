//! Spec §7 hard requirement: same bronze in → byte-identical silver out.
//! Builds a fixture bronze day on a local file store, normalizes it
//! twice, and compares output bytes.

use std::io::Write;

fn gz(lines: &[String]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    for l in lines {
        enc.write_all(l.as_bytes()).unwrap();
        enc.write_all(b"\n").unwrap();
    }
    enc.finish().unwrap()
}

fn bronze_line(ts_recv_ns: i64, seq: u64, payload: &str) -> String {
    serde_json::json!({"ts_recv_ns": ts_recv_ns, "seq": seq, "payload": payload}).to_string()
}

fn match_payload(trade_id: u64, price: &str, time: &str) -> String {
    format!(
        r#"{{"type":"match","trade_id":{trade_id},"side":"sell","size":"0.5","price":"{price}","product_id":"BTC-USD","sequence":{trade_id},"time":"{time}"}}"#
    )
}

async fn seed_and_normalize(root: &std::path::Path) -> Vec<u8> {
    let store = store::open(&format!("file://{}", root.display())).unwrap();

    let lines = vec![
        bronze_line(
            2_000,
            1,
            &match_payload(11, "63000.00", "2026-08-12T05:00:01.000000Z"),
        ),
        bronze_line(
            1_000,
            0,
            &match_payload(10, "62999.00", "2026-08-12T05:00:00.500000Z"),
        ),
        // Duplicate trade_id (reconnect replay) — must dedup.
        bronze_line(
            3_000,
            2,
            &match_payload(11, "63000.00", "2026-08-12T05:00:01.000000Z"),
        ),
        // A marker line — must be skipped, not rejected.
        serde_json::json!({"ts_recv_ns": 4_000, "seq": 3, "marker": "disconnect"}).to_string(),
    ];
    let key = "bronze/v1/exchange=coinbase/stream=matches.BTC-USD/date=2026-08-12/hour=05/boot-0000.jsonl.gz";
    store
        .put(&object_store::path::Path::from(key), gz(&lines).into())
        .await
        .unwrap();

    let n = normalizer::ws::normalize_day(&store, "coinbase", "2026-08-12")
        .await
        .unwrap();
    assert_eq!(n, 1, "one stream discovered");

    let out = store
        .get(&object_store::path::Path::from(
            "silver/v1/trades/exchange=coinbase/symbol=BTC-USD/date=2026-08-12/part-00.parquet",
        ))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    out.to_vec()
}

#[tokio::test]
async fn normalize_twice_is_byte_identical_and_dedups() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let a = seed_and_normalize(dir1.path()).await;
    let b = seed_and_normalize(dir2.path()).await;
    assert!(!a.is_empty());
    assert_eq!(a, b, "normalizer output must be deterministic");

    // Re-running over the SAME store must also produce identical bytes
    // (overwrite-partition idempotency).
    let store = store::open(&format!("file://{}", dir1.path().display())).unwrap();
    normalizer::ws::normalize_day(&store, "coinbase", "2026-08-12")
        .await
        .unwrap();
    let again = store
        .get(&object_store::path::Path::from(
            "silver/v1/trades/exchange=coinbase/symbol=BTC-USD/date=2026-08-12/part-00.parquet",
        ))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(a, again.to_vec());

    // Contents: dedup left 2 trades, sorted by ts_recv.
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::from(a),
    )
    .unwrap()
    .build()
    .unwrap();
    let batches: Vec<_> = reader.collect::<Result<_, _>>().unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 2, "duplicate trade_id must be dropped");
}
