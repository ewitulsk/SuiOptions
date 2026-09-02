//! bronze → silver batch normalization (spec §7). Unit of work is one
//! (stream, UTC day) partition; re-running a partition overwrites it
//! exactly (idempotent), and output is deterministic byte-for-byte.

pub mod aftermath;
pub mod deribit;
pub mod funding;
pub mod instruments;
pub mod vision;
pub mod ws;

use std::sync::Arc;

use anyhow::Context;
use futures::TryStreamExt;
use object_store::ObjectStore;

pub type Store = Arc<dyn ObjectStore>;

/// Max tolerated reject rate per partition before the run fails loudly
/// (spec §7: "> 0.1 % rejects on a partition fails the run").
pub const MAX_REJECT_RATE: f64 = 0.001;

pub async fn list_keys(store: &Store, prefix: &str) -> anyhow::Result<Vec<String>> {
    let path = object_store::path::Path::from(prefix.trim_end_matches('/'));
    let mut keys: Vec<String> = store
        .list(Some(&path))
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await?;
    keys.sort();
    Ok(keys)
}

pub async fn get_bytes(store: &Store, key: &str) -> anyhow::Result<Vec<u8>> {
    Ok(store
        .get(&object_store::path::Path::from(key))
        .await
        .with_context(|| format!("get {key}"))?
        .bytes()
        .await?
        .to_vec())
}

/// Stream an object to a temp file — for multi-GB zips that must not
/// sit in RAM (the host has 2 GB). Returns None if the key is missing.
pub async fn get_to_tempfile(
    store: &Store,
    key: &str,
) -> anyhow::Result<Option<tempfile::NamedTempFile>> {
    use futures::TryStreamExt;
    use tokio::io::AsyncWriteExt;

    let result = match store.get(&object_store::path::Path::from(key)).await {
        Ok(r) => r,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("get {key}")),
    };
    let tmp = tempfile::NamedTempFile::new()?;
    let mut file = tokio::fs::File::create(tmp.path()).await?;
    let mut stream = result.into_stream();
    while let Some(chunk) = stream.try_next().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(Some(tmp))
}

/// One collector bronze line (spec §5.6).
#[derive(serde::Deserialize)]
struct BronzeLine {
    ts_recv_ns: i64,
    payload: Option<String>,
    marker: Option<String>,
}

/// Fetch one bronze object and hand every captured payload to `f` as
/// `(line_no, ts_recv_ns, payload)`. Marker lines are skipped (they feed
/// the gaps job); bad envelopes and `f`'s parse failures both land in
/// `rejects`. Returns the line count.
pub async fn for_each_bronze_payload(
    store: &Store,
    key: &str,
    rejects: &mut Vec<adapters::Reject>,
    mut f: impl FnMut(i32, i64, &str) -> Result<(), adapters::Reject>,
) -> anyhow::Result<usize> {
    use std::io::Read;
    let gz = get_bytes(store, key).await?;
    let mut raw = String::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut raw)?;
    let mut lines = 0usize;
    for (i, line) in raw.lines().enumerate() {
        lines += 1;
        let Ok(bl) = serde_json::from_str::<BronzeLine>(line) else {
            rejects.push(adapters::Reject {
                src_file: key.to_string(),
                src_line: i as i32,
                reason: "bad bronze envelope".into(),
            });
            continue;
        };
        if bl.marker.is_some() {
            continue;
        }
        if let Some(payload) = bl.payload {
            if let Err(r) = f(i as i32, bl.ts_recv_ns, &payload) {
                rejects.push(r);
            }
        }
    }
    Ok(lines)
}

pub async fn put_bytes(store: &Store, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
    store
        .put(&object_store::path::Path::from(key), bytes.into())
        .await
        .with_context(|| format!("put {key}"))?;
    Ok(())
}

/// Write rejects (if any) and fail the partition when the rate is over
/// budget. `total` is the number of parsed records + rejects.
pub async fn handle_rejects(
    store: &Store,
    partition_label: &str,
    rejects: &[adapters::Reject],
    total: usize,
) -> anyhow::Result<()> {
    if rejects.is_empty() {
        return Ok(());
    }
    let body: String = rejects.iter().map(|r| format!("{r}\n")).collect();
    let key = format!("silver/v1/_rejects/{partition_label}/rejects.txt");
    put_bytes(store, &key, body.into_bytes()).await?;
    let rate = rejects.len() as f64 / total.max(1) as f64;
    tracing::warn!(
        partition_label,
        rejects = rejects.len(),
        total,
        "rejects written"
    );
    anyhow::ensure!(
        rate <= MAX_REJECT_RATE,
        "reject rate {:.4}% over budget on {partition_label}",
        rate * 100.0
    );
    Ok(())
}
