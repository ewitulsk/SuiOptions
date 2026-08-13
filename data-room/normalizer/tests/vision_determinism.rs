//! Vision normalize through the chunked day writer: correct rows and
//! byte-identical re-runs (the incremental writer must stay as
//! deterministic as the old whole-day path).

async fn seed_and_run(root: &std::path::Path) -> Vec<u8> {
    let store = store::open(&format!("file://{}", root.display())).unwrap();
    let zip_bytes: &[u8] =
        include_bytes!("../../crates/adapters/fixtures/BTCUSDC-trades-2018-12-15.zip");
    store
        .put(
            &object_store::path::Path::from(
                "bronze/v1/exchange=binance/source=vision/market=spot/kind=trades/symbol=BTCUSDC/BTCUSDC-trades-2018-12-15.zip",
            ),
            bytes::Bytes::from_static(zip_bytes).into(),
        )
        .await
        .unwrap();

    let n = normalizer::vision::normalize_pending(&store, "spot", "BTCUSDC")
        .await
        .unwrap();
    assert_eq!(n, 1);
    store
        .get(&object_store::path::Path::from(
            "silver/v1/trades/exchange=binance/symbol=BTC-USDC/date=2018-12-15/part-00.parquet",
        ))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn vision_normalize_is_deterministic_and_complete() {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let a = seed_and_run(d1.path()).await;
    let b = seed_and_run(d2.path()).await;
    assert_eq!(a, b, "chunked writer must be deterministic");

    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::from(a),
    )
    .unwrap()
    .build()
    .unwrap();
    let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
    assert_eq!(
        rows, 1050,
        "fixture row count preserved through chunked path"
    );
}
