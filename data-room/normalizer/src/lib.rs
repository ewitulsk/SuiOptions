//! bronze → silver batch normalization (spec §7). Unit of work is one
//! (stream, UTC day) partition; re-running a partition overwrites it
//! exactly (idempotent), and output is deterministic byte-for-byte.

pub mod coinbase;
pub mod instruments;
pub mod vision;

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
