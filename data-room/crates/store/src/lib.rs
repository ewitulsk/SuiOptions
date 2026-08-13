//! One-liner around `object_store` so every binary opens the lake the
//! same way: `s3://bucket` in prod (creds/region from the environment /
//! instance profile), `file:///path` in tests and local runs.

use std::sync::Arc;

use anyhow::Context;
use object_store::{aws::AmazonS3Builder, local::LocalFileSystem, ObjectStore};
use url::Url;

pub use object_store::path::Path;

pub fn open(store_url: &str) -> anyhow::Result<Arc<dyn ObjectStore>> {
    let url = Url::parse(store_url).with_context(|| format!("bad store url {store_url}"))?;
    match url.scheme() {
        "s3" => {
            let bucket = url.host_str().context("s3 url missing bucket")?;
            let s3 = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .build()?;
            Ok(Arc::new(s3))
        }
        "file" => {
            std::fs::create_dir_all(url.path()).ok();
            Ok(Arc::new(LocalFileSystem::new_with_prefix(url.path())?))
        }
        other => anyhow::bail!("unsupported store scheme {other}"),
    }
}
