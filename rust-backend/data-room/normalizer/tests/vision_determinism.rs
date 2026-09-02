//! Vision normalize through the chunked day writer: correct rows and
//! byte-identical re-runs (the incremental writer must stay as
//! deterministic as the old whole-day path).

async fn seed_and_run(root: &std::path::Path) -> Vec<u8> {
    let store = data_room_store::open(&format!("file://{}", root.display())).unwrap();
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

#[tokio::test]
async fn duplicate_internal_entry_zip_uses_top_level_csv() {
    // Real-world quirk (BTCUSDC-trades-2021-04.zip): the CSV appears twice,
    // once bare and once under an internal fsx-data/ path.
    let dir = tempfile::tempdir().unwrap();
    let store = data_room_store::open(&format!("file://{}", dir.path().display())).unwrap();

    let csv = "0,100.0,1.0,100.0,1617235200000,True,True\n";
    let mut buf = Vec::new();
    {
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::SimpleFileOptions = Default::default();
        zw.start_file("BTCUSDC-trades-2021-04.csv", opts).unwrap();
        zw.write_all(csv.as_bytes()).unwrap();
        zw.start_file("fsx-data/collector_data/BTCUSDC-trades-2021-04.csv", opts)
            .unwrap();
        zw.write_all(csv.as_bytes()).unwrap();
        zw.finish().unwrap();
    }
    store
        .put(
            &object_store::path::Path::from(
                "bronze/v1/exchange=binance/source=vision/market=spot/kind=trades/symbol=BTCUSDC/BTCUSDC-trades-2021-04.zip",
            ),
            bytes::Bytes::from(buf).into(),
        )
        .await
        .unwrap();

    let n = normalizer::vision::normalize_pending(&store, "spot", "BTCUSDC")
        .await
        .unwrap();
    assert_eq!(n, 1);
    let got = store
        .get(&object_store::path::Path::from(
            "silver/v1/trades/exchange=binance/symbol=BTC-USDC/date=2021-04-01/part-00.parquet",
        ))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(got)
        .unwrap()
        .build()
        .unwrap();
    let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
    assert_eq!(rows, 1, "one row from the top-level csv, duplicate ignored");
}

#[tokio::test]
async fn out_of_order_days_normalize_into_both_partitions() {
    // Real-world quirk (BTCUSDT-trades-2023-01.zip): rows for an earlier
    // day appear after a later day's rows.
    let dir = tempfile::tempdir().unwrap();
    let store = data_room_store::open(&format!("file://{}", dir.path().display())).unwrap();

    let csv = "0,100.0,1.0,100.0,1672000000000,True,True\n\
               1,101.0,1.0,101.0,1674600000000,True,True\n\
               2,102.0,1.0,102.0,1672000001000,True,True\n";
    let mut buf = Vec::new();
    {
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::SimpleFileOptions = Default::default();
        zw.start_file("BTCUSDC-trades-2022-12.csv", opts).unwrap();
        zw.write_all(csv.as_bytes()).unwrap();
        zw.finish().unwrap();
    }
    store
        .put(
            &object_store::path::Path::from(
                "bronze/v1/exchange=binance/source=vision/market=spot/kind=trades/symbol=BTCUSDC/BTCUSDC-trades-2022-12.zip",
            ),
            bytes::Bytes::from(buf).into(),
        )
        .await
        .unwrap();

    let n = normalizer::vision::normalize_pending(&store, "spot", "BTCUSDC")
        .await
        .unwrap();
    assert_eq!(n, 1);
    // 1672000000s = 2022-12-25; 1674600000s = 2023-01-24 (spills past the
    // nominal month, mirroring the real dump quirk).
    for (date, want) in [("2022-12-25", 2usize), ("2023-01-24", 1usize)] {
        let got = store
            .get(&object_store::path::Path::from(format!(
                "silver/v1/trades/exchange=binance/symbol=BTC-USDC/date={date}/part-00.parquet"
            )))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(got)
            .unwrap()
            .build()
            .unwrap();
        let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
        assert_eq!(rows, want, "{date}");
    }
}
