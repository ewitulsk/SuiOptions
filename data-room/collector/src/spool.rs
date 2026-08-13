//! Local spool: verbatim JSONL per (exchange, stream, UTC hour), rotated
//! on hour boundary or size cap, then gzipped and uploaded to bronze
//! (spec §6.1). Local layout mirrors the bronze key so upload is a pure
//! path translation:
//!
//!   {spool}/exchange=E/stream=S/date=D/hour=H/{boot_id}-{seq}.jsonl
//!   → bronze/v1/exchange=E/stream=S/date=D/hour=H/{boot_id}-{seq}.jsonl.gz

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use flate2::write::GzEncoder;
use object_store::ObjectStore;
use serde::Serialize;

#[derive(Serialize)]
struct Line<'a> {
    ts_recv_ns: i64,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    marker: Option<&'a str>,
}

struct OpenFile {
    path: PathBuf,
    w: BufWriter<File>,
    bytes: u64,
    hour_key: String,
    file_seq: u32,
}

pub struct Spool {
    root: PathBuf,
    boot_id: String,
    max_file_bytes: u64,
    open: HashMap<(String, String), OpenFile>, // (exchange, stream)
}

impl Spool {
    pub fn new(root: impl Into<PathBuf>, boot_id: String, max_file_bytes: u64) -> Self {
        Self {
            root: root.into(),
            boot_id,
            max_file_bytes,
            open: HashMap::new(),
        }
    }

    fn hour_key(ts_ns: i64) -> String {
        let dt: DateTime<Utc> = DateTime::from_timestamp_nanos(ts_ns);
        dt.format("date=%Y-%m-%d/hour=%H").to_string()
    }

    /// Append one captured payload (or marker). Returns any file that was
    /// closed by rotation and now needs uploading.
    pub fn write(
        &mut self,
        exchange: &str,
        stream: &str,
        ts_recv_ns: i64,
        seq: u64,
        payload: Option<&str>,
        marker: Option<&str>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let hour_key = Self::hour_key(ts_recv_ns);
        let key = (exchange.to_string(), stream.to_string());

        let mut closed = None;
        let needs_new = match self.open.get(&key) {
            Some(f) => f.hour_key != hour_key || f.bytes >= self.max_file_bytes,
            None => true,
        };
        if needs_new {
            let next_seq = if let Some(mut f) = self.open.remove(&key) {
                f.w.flush()?;
                closed = Some(f.path.clone());
                if f.hour_key == hour_key {
                    f.file_seq + 1
                } else {
                    0
                }
            } else {
                0
            };
            let dir = self
                .root
                .join(format!("exchange={exchange}"))
                .join(format!("stream={stream}"))
                .join(&hour_key);
            fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}-{:04}.jsonl", self.boot_id, next_seq));
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.open.insert(
                key.clone(),
                OpenFile {
                    path,
                    w: BufWriter::new(file),
                    bytes: 0,
                    hour_key,
                    file_seq: next_seq,
                },
            );
        }

        let f = self.open.get_mut(&key).expect("just inserted");
        let line = serde_json::to_string(&Line {
            ts_recv_ns,
            seq,
            payload,
            marker,
        })?;
        f.w.write_all(line.as_bytes())?;
        f.w.write_all(b"\n")?;
        f.bytes += line.len() as u64 + 1;
        Ok(closed)
    }

    /// Close any open file whose UTC hour has passed — catches quiet
    /// streams that would otherwise hold an hour open forever.
    pub fn rotate_expired(&mut self, now_ns: i64) -> anyhow::Result<Vec<PathBuf>> {
        let current = Self::hour_key(now_ns);
        let expired: Vec<_> = self
            .open
            .iter()
            .filter(|(_, f)| f.hour_key != current)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::new();
        for k in expired {
            if let Some(mut f) = self.open.remove(&k) {
                f.w.flush()?;
                out.push(f.path);
            }
        }
        Ok(out)
    }

    /// Close every open file (shutdown / final flush) and return them.
    pub fn close_all(&mut self) -> anyhow::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for (_, mut f) in self.open.drain() {
            f.w.flush()?;
            out.push(f.path);
        }
        Ok(out)
    }

    /// Files no longer open (previous boots, or rotated but not yet
    /// uploaded) — the boot sweep (§6.1).
    pub fn stale_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let open: Vec<&PathBuf> = self.open.values().map(|f| &f.path).collect();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "jsonl") && !open.contains(&&p) {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }
}

/// Gzip a closed spool file, upload it under `bronze/v1/`, delete the
/// local copy. The bronze key is the spool-relative path + `.gz`.
pub async fn upload(
    store: &Arc<dyn ObjectStore>,
    spool_root: &Path,
    file: &Path,
) -> anyhow::Result<()> {
    let rel = file
        .strip_prefix(spool_root)
        .with_context(|| format!("{file:?} not under spool root"))?;
    let key = format!("bronze/v1/{}.gz", rel.to_string_lossy());

    // A file can be queued twice (rotation queue + stale sweep, or the
    // shutdown flush). Already gone = already uploaded: idempotent no-op.
    let raw = match fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&raw)?;
    let gz = enc.finish()?;

    store
        .put(&object_store::path::Path::from(key.as_str()), gz.into())
        .await
        .with_context(|| format!("uploading {key}"))?;
    fs::remove_file(file)?;
    metrics::counter!("dataroom_collector_uploads_total").increment(1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_on_hour_boundary_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Spool::new(dir.path(), "boot1".into(), 1_000_000);
        let h0 = 1_754_000_000_000_000_000i64; // some fixed ns
        assert!(s
            .write("coinbase", "matches.BTC-USD", h0, 0, Some("{}"), None)
            .unwrap()
            .is_none());
        // Same hour: no rotation.
        assert!(s
            .write("coinbase", "matches.BTC-USD", h0 + 1, 1, Some("{}"), None)
            .unwrap()
            .is_none());
        // +1h: rotation returns the closed file.
        let closed = s
            .write(
                "coinbase",
                "matches.BTC-USD",
                h0 + 3_600_000_000_000,
                2,
                Some("{}"),
                None,
            )
            .unwrap();
        assert!(closed.is_some());
        let closed = closed.unwrap();
        assert!(closed.to_string_lossy().contains("stream=matches.BTC-USD"));
        assert!(closed.to_string_lossy().ends_with("boot1-0000.jsonl"));
    }

    #[test]
    fn size_cap_rotates_with_incremented_file_seq() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Spool::new(dir.path(), "b".into(), 10); // tiny cap
        let t = 1_754_000_000_000_000_000i64;
        s.write("coinbase", "x", t, 0, Some("aaaaaaaaaaaaaaa"), None)
            .unwrap();
        let closed = s
            .write("coinbase", "x", t + 1, 1, Some("b"), None)
            .unwrap()
            .unwrap();
        assert!(closed.to_string_lossy().ends_with("b-0000.jsonl"));
        let open_now = s.close_all().unwrap();
        assert!(open_now[0].to_string_lossy().ends_with("b-0001.jsonl"));
    }

    #[test]
    fn stale_files_finds_leftovers_not_open_files() {
        let dir = tempfile::tempdir().unwrap();
        // A leftover from a previous boot:
        let old = dir
            .path()
            .join("exchange=coinbase/stream=x/date=2026-08-13/hour=01");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("deadbeef-0000.jsonl"), "{}\n").unwrap();

        let mut s = Spool::new(dir.path(), "boot2".into(), 1_000_000);
        s.write(
            "coinbase",
            "x",
            1_754_000_000_000_000_000,
            0,
            Some("{}"),
            None,
        )
        .unwrap();
        let stale = s.stale_files();
        assert_eq!(stale.len(), 1);
        assert!(stale[0].to_string_lossy().contains("deadbeef"));
    }

    #[tokio::test]
    async fn upload_translates_path_gzips_and_removes() {
        let spool_dir = tempfile::tempdir().unwrap();
        let lake = tempfile::tempdir().unwrap();
        let store = store::open(&format!("file://{}", lake.path().display())).unwrap();

        let d = spool_dir
            .path()
            .join("exchange=coinbase/stream=x/date=2026-08-13/hour=02");
        fs::create_dir_all(&d).unwrap();
        let f = d.join("boot-0000.jsonl");
        fs::write(&f, "{\"a\":1}\n").unwrap();

        upload(&store, spool_dir.path(), &f).await.unwrap();
        assert!(!f.exists());
        let uploaded = lake.path().join(
            "bronze/v1/exchange=coinbase/stream=x/date=2026-08-13/hour=02/boot-0000.jsonl.gz",
        );
        assert!(uploaded.exists());
        // Round-trips through gzip.
        let gz = fs::read(uploaded).unwrap();
        let mut dec = flate2::read::GzDecoder::new(&gz[..]);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut dec, &mut out).unwrap();
        assert_eq!(out, "{\"a\":1}\n");
    }
}
