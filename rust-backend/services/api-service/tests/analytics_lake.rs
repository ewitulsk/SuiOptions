//! Lake reader against a file:// fixture lake shaped like the data-room
//! gold layer (SO-389).

use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;

use api_service::analytics::lake::Lake;

fn write_parquet(path: &std::path::Path, batch: &RecordBatch) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    let mut w = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
    w.write(batch).unwrap();
    w.close().unwrap();
}

fn bars_batch(rows: &[(i64, f64)]) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("ts_open", DataType::Int64, false),
        Field::new("close", DataType::Float64, false),
    ]);
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.0))),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.1))),
        ],
    )
    .unwrap()
}

#[allow(clippy::type_complexity)]
fn rv_batch(rows: &[(i64, &str, i64, i64, &str, f64)]) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("ts", DataType::Int64, false),
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("window_s", DataType::Int64, false),
        Field::new("sample_interval_s", DataType::Int64, false),
        Field::new("estimator", DataType::Utf8, false),
        Field::new("sigma_ann", DataType::Float64, false),
    ]);
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.0))),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.1))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.2))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.3))),
            Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.4))),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.5))),
        ],
    )
    .unwrap()
}

const NS: i64 = 1_000_000_000;

#[tokio::test]
async fn spot_and_rv_series_read_fixture_lake() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Two days of 1h bars for binance BTC-USDC, one day missing in the
    // middle of the requested range (must be skipped, not an error).
    write_parquet(
        &root.join("gold/v1/bars/freq=3600s/exchange=binance/symbol=BTC-USDC/date=2026-08-10/part-00.parquet"),
        &bars_batch(&[(1_000 * NS, 100.0), (4_600 * NS, 101.0)]),
    );
    write_parquet(
        &root.join("gold/v1/bars/freq=3600s/exchange=binance/symbol=BTC-USDC/date=2026-08-12/part-00.parquet"),
        &bars_batch(&[(200_000 * NS, 105.0)]),
    );
    // RV partition holding two param combos + a second instrument.
    write_parquet(
        &root.join("gold/v1/rv/date=2026-08-10/part-00.parquet"),
        &rv_batch(&[
            (
                3_600 * NS,
                "btc-usdc.binance",
                86_400,
                60,
                "rv_subsampled",
                0.42,
            ),
            (
                3_600 * NS,
                "btc-usdc.binance",
                86_400,
                300,
                "rv_subsampled",
                0.44,
            ),
            (
                3_600 * NS,
                "btc-usd.coinbase",
                86_400,
                60,
                "rv_subsampled",
                0.40,
            ),
            (
                7_200 * NS,
                "btc-usdc.binance",
                86_400,
                60,
                "rv_subsampled",
                0.43,
            ),
        ]),
    );

    let lake = Lake::open(&format!("file://{}", root.display())).unwrap();
    let dates: Vec<String> = ["2026-08-10", "2026-08-11", "2026-08-12"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let spot = lake
        .spot_series("binance", "BTC-USDC", 3_600, &dates)
        .await
        .unwrap();
    assert_eq!(spot.len(), 3);
    assert_eq!(spot[0], (1_000 * NS / 1_000_000, 100.0)); // ns → ms
    assert_eq!(spot[2].1, 105.0);

    let rv = lake
        .rv_series("btc-usdc.binance", 86_400, 60, "rv_subsampled", &dates)
        .await
        .unwrap();
    assert_eq!(rv.len(), 2, "filters window+interval+estimator+instrument");
    assert_eq!(rv[0].1, 0.42);
    assert_eq!(rv[1].1, 0.43);

    // Unknown instrument/date range: empty, not an error.
    let none = lake
        .rv_series("nope.nowhere", 86_400, 60, "rv_subsampled", &dates)
        .await
        .unwrap();
    assert!(none.is_empty());

    // Catalog listing sees the pair with its date range.
    let keys = lake.list("gold/v1/bars/freq=3600s/").await.unwrap();
    assert_eq!(keys.len(), 2);
}
