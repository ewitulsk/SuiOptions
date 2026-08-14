//! Read-only access to the data-room gold layer (SO-389).
//!
//! api-service is the data room's first consumer, under the coupling
//! rules of the data-room spec §10.1/§14.4: no reads at boot, every
//! request degrades to an error response if the lake is unreachable, and
//! nothing here can fail a health check. Reads go straight to the gold
//! parquet partitions with a bounded in-memory cache (partitions are
//! written once per day, so a short TTL is plenty).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use arrow::array::{Array, Float64Array, Int64Array, StringArray};
use futures::StreamExt;
use object_store::ObjectStore;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const CACHE_TTL: Duration = Duration::from_secs(900);
const CACHE_MAX_ENTRIES: usize = 4096;
const FETCH_CONCURRENCY: usize = 32;

/// One decoded gold partition, cached by object key.
#[derive(Clone)]
enum Decoded {
    /// (ts_open ns, close) rows of a bars partition.
    Bars(Arc<Vec<(i64, f64)>>),
    /// Full rv partition rows.
    Rv(Arc<Vec<RvRow>>),
    /// Sorted object keys under a listed prefix.
    Listing(Arc<Vec<String>>),
    /// funding partition rows: (ts ns, per-interval rate, interval hours,
    /// mark price, index price).
    Funding(Arc<Vec<FundingRow>>),
}

#[derive(Clone, Copy)]
pub struct FundingRow {
    pub ts: i64,
    pub rate: f64,
    pub interval_hours: f64,
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
}

#[derive(Clone)]
pub struct RvRow {
    pub ts: i64,
    pub instrument_id: String,
    pub window_s: i64,
    pub sample_interval_s: i64,
    pub estimator: String,
    pub sigma_ann: f64,
}

pub struct Lake {
    store: Arc<dyn ObjectStore>,
    cache: Mutex<HashMap<String, (Instant, Decoded)>>,
}

impl Lake {
    /// `s3://bucket` (creds from the instance role / env) or
    /// `file:///path` (tests). Never touches the network here.
    pub fn open(url: &str) -> anyhow::Result<Self> {
        let parsed = url::Url::parse(url).with_context(|| format!("bad data_room_url {url}"))?;
        let store: Arc<dyn ObjectStore> = match parsed.scheme() {
            "s3" => {
                let bucket = parsed.host_str().context("s3 url missing bucket")?;
                Arc::new(
                    object_store::aws::AmazonS3Builder::from_env()
                        .with_bucket_name(bucket)
                        .build()?,
                )
            }
            "file" => Arc::new(object_store::local::LocalFileSystem::new_with_prefix(
                parsed.path(),
            )?),
            other => anyhow::bail!("unsupported data_room_url scheme {other}"),
        };
        Ok(Self {
            store,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn cache_get(&self, key: &str) -> Option<Decoded> {
        let cache = self.cache.lock().unwrap();
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, d)| d.clone())
    }

    fn cache_put(&self, key: String, val: Decoded) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_MAX_ENTRIES {
            // Bounded, not clever: drop expired first, else the oldest.
            cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
            if cache.len() >= CACHE_MAX_ENTRIES {
                if let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(k, _)| k.clone())
                {
                    cache.remove(&oldest);
                }
            }
        }
        cache.insert(key, (Instant::now(), val));
    }

    async fn get_bytes(&self, key: &str) -> anyhow::Result<Option<bytes::Bytes>> {
        match self.store.get(&object_store::path::Path::from(key)).await {
            Ok(r) => Ok(Some(r.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("lake get {key}")),
        }
    }

    /// Sorted keys under a prefix, cached.
    pub async fn list(&self, prefix: &str) -> anyhow::Result<Arc<Vec<String>>> {
        let cache_key = format!("list:{prefix}");
        if let Some(Decoded::Listing(l)) = self.cache_get(&cache_key) {
            return Ok(l);
        }
        let path = object_store::path::Path::from(prefix.trim_end_matches('/'));
        let mut keys: Vec<String> = self
            .store
            .list(Some(&path))
            .map(|m| m.map(|m| m.location.to_string()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .context("lake list")?;
        keys.sort();
        let arc = Arc::new(keys);
        self.cache_put(cache_key, Decoded::Listing(arc.clone()));
        Ok(arc)
    }

    async fn bars_partition(&self, key: &str) -> anyhow::Result<Arc<Vec<(i64, f64)>>> {
        if let Some(Decoded::Bars(v)) = self.cache_get(key) {
            return Ok(v);
        }
        let mut rows = Vec::new();
        if let Some(bytes) = self.get_bytes(key).await? {
            for batch in ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()? {
                let b = batch?;
                let ts = col_i64(&b, "ts_open")?;
                let close = col_f64(&b, "close")?;
                for i in 0..b.num_rows() {
                    rows.push((ts.value(i), close.value(i)));
                }
            }
        }
        let arc = Arc::new(rows);
        self.cache_put(key.to_string(), Decoded::Bars(arc.clone()));
        Ok(arc)
    }

    async fn rv_partition(&self, key: &str) -> anyhow::Result<Arc<Vec<RvRow>>> {
        if let Some(Decoded::Rv(v)) = self.cache_get(key) {
            return Ok(v);
        }
        let mut rows = Vec::new();
        if let Some(bytes) = self.get_bytes(key).await? {
            for batch in ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()? {
                let b = batch?;
                let ts = col_i64(&b, "ts")?;
                let instrument = col_str(&b, "instrument_id")?;
                let window = col_i64(&b, "window_s")?;
                let interval = col_i64(&b, "sample_interval_s")?;
                let estimator = col_str(&b, "estimator")?;
                let sigma = col_f64(&b, "sigma_ann")?;
                for i in 0..b.num_rows() {
                    rows.push(RvRow {
                        ts: ts.value(i),
                        instrument_id: instrument.value(i).to_string(),
                        window_s: window.value(i),
                        sample_interval_s: interval.value(i),
                        estimator: estimator.value(i).to_string(),
                        sigma_ann: sigma.value(i),
                    });
                }
            }
        }
        let arc = Arc::new(rows);
        self.cache_put(key.to_string(), Decoded::Rv(arc.clone()));
        Ok(arc)
    }

    /// Spot closes for (exchange, symbol) at `freq_s` over `dates`
    /// (YYYY-MM-DD, ascending). Missing partitions are skipped.
    /// Returned points are (unix ms, close), ascending.
    pub async fn spot_series(
        &self,
        exchange: &str,
        symbol: &str,
        freq_s: i64,
        dates: &[String],
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let keys: Vec<String> = dates
            .iter()
            .map(|d| {
                format!(
                    "gold/v1/bars/freq={freq_s}s/exchange={exchange}/symbol={symbol}/date={d}/part-00.parquet"
                )
            })
            .collect();
        let parts = futures::stream::iter(keys.into_iter())
            .map(|k| async move { self.bars_partition(&k).await })
            .buffered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut out = Vec::new();
        for p in parts {
            out.extend(p?.iter().map(|(ts, close)| (ts / 1_000_000, *close)));
        }
        Ok(out)
    }

    async fn funding_partition(&self, key: &str) -> anyhow::Result<Arc<Vec<FundingRow>>> {
        if let Some(Decoded::Funding(v)) = self.cache_get(key) {
            return Ok(v);
        }
        let mut rows = Vec::new();
        if let Some(bytes) = self.get_bytes(key).await? {
            for batch in ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()? {
                let b = batch?;
                let ts_event = col_i64(&b, "ts_event")?;
                let ts_recv = col_i64(&b, "ts_recv")?;
                let rate = col_f64(&b, "rate")?;
                let interval = col_f64(&b, "interval_hours")?;
                let mark = col_f64(&b, "mark_price")?;
                let index = col_f64(&b, "index_price")?;
                for i in 0..b.num_rows() {
                    let ts = if ts_event.is_valid(i) {
                        ts_event.value(i)
                    } else if ts_recv.is_valid(i) {
                        ts_recv.value(i)
                    } else {
                        continue;
                    };
                    rows.push(FundingRow {
                        ts,
                        rate: rate.value(i),
                        interval_hours: interval.value(i),
                        mark_price: mark.is_valid(i).then(|| mark.value(i)),
                        index_price: index.is_valid(i).then(|| index.value(i)),
                    });
                }
            }
        }
        let arc = Arc::new(rows);
        self.cache_put(key.to_string(), Decoded::Funding(arc.clone()));
        Ok(arc)
    }

    /// Funding rows of one kind ("settled" | "predicted") over `dates`.
    pub async fn funding_series(
        &self,
        exchange: &str,
        symbol: &str,
        kind: &str,
        dates: &[String],
    ) -> anyhow::Result<Vec<FundingRow>> {
        let keys: Vec<String> = dates
            .iter()
            .map(|d| {
                format!(
                    "silver/v1/funding_rates/exchange={exchange}/symbol={symbol}/date={d}/part-{kind}.parquet"
                )
            })
            .collect();
        let parts = futures::stream::iter(keys.into_iter())
            .map(|k| async move { self.funding_partition(&k).await })
            .buffered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut out = Vec::new();
        for p in parts {
            out.extend(p?.iter().copied());
        }
        Ok(out)
    }

    /// Vol-index closes (e.g. Deribit DVOL) over `dates`. Points are
    /// (unix ms, close as venue-reported percent), ascending.
    pub async fn vol_index_series(
        &self,
        exchange: &str,
        symbol: &str,
        dates: &[String],
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let keys: Vec<String> = dates
            .iter()
            .map(|d| {
                format!(
                    "silver/v1/vol_index/exchange={exchange}/symbol={symbol}/date={d}/part-00.parquet"
                )
            })
            .collect();
        let parts = futures::stream::iter(keys.into_iter())
            .map(|k| async move { self.vol_index_partition(&k).await })
            .buffered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut out = Vec::new();
        for p in parts {
            out.extend(p?.iter().map(|(ts, close)| (ts / 1_000_000, *close)));
        }
        Ok(out)
    }

    async fn vol_index_partition(&self, key: &str) -> anyhow::Result<Arc<Vec<(i64, f64)>>> {
        if let Some(Decoded::Bars(v)) = self.cache_get(key) {
            return Ok(v);
        }
        let mut rows = Vec::new();
        if let Some(bytes) = self.get_bytes(key).await? {
            for batch in ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()? {
                let b = batch?;
                let ts = col_i64(&b, "ts")?;
                let close = col_f64(&b, "close")?;
                for i in 0..b.num_rows() {
                    rows.push((ts.value(i), close.value(i)));
                }
            }
        }
        let arc = Arc::new(rows);
        self.cache_put(key.to_string(), Decoded::Bars(arc.clone()));
        Ok(arc)
    }

    /// RV series for one (instrument, window, interval, estimator) over
    /// `dates`. Points are (unix ms, annualized sigma), ascending.
    pub async fn rv_series(
        &self,
        instrument_id: &str,
        window_s: i64,
        sample_interval_s: i64,
        estimator: &str,
        dates: &[String],
    ) -> anyhow::Result<Vec<(i64, f64)>> {
        let keys: Vec<String> = dates
            .iter()
            .map(|d| format!("gold/v1/rv/date={d}/part-00.parquet"))
            .collect();
        let parts = futures::stream::iter(keys.into_iter())
            .map(|k| async move { self.rv_partition(&k).await })
            .buffered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut out = Vec::new();
        for p in parts {
            out.extend(
                p?.iter()
                    .filter(|r| {
                        r.instrument_id == instrument_id
                            && r.window_s == window_s
                            && r.sample_interval_s == sample_interval_s
                            && r.estimator == estimator
                    })
                    .map(|r| (r.ts / 1_000_000, r.sigma_ann)),
            );
        }
        Ok(out)
    }
}

fn col_i64(b: &arrow::array::RecordBatch, name: &str) -> anyhow::Result<Int64Array> {
    Ok(b.column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .with_context(|| format!("{name} not i64"))?
        .clone())
}

fn col_f64(b: &arrow::array::RecordBatch, name: &str) -> anyhow::Result<Float64Array> {
    Ok(b.column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<Float64Array>()
        .with_context(|| format!("{name} not f64"))?
        .clone())
}

fn col_str(b: &arrow::array::RecordBatch, name: &str) -> anyhow::Result<StringArray> {
    Ok(b.column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("{name} not utf8"))?
        .clone())
}
