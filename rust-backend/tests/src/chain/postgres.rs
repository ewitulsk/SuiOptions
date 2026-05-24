//! Ephemeral Postgres for the indexer, via testcontainers.
//!
//! Boots a `postgres:16` container with a random DB name; returns a URL
//! the indexer's `establish_pool` can consume. `Drop` stops the
//! container (testcontainers does this automatically when the
//! `ContainerAsync` handle is dropped).

use anyhow::{Context, Result};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;
use tracing::info;

/// Container handle + the URL the indexer should connect to.
pub struct EphemeralPostgres {
    pub url: String,
    /// Hold the container so Drop stops it. We never need to call into it.
    _container: ContainerAsync<Postgres>,
}

impl EphemeralPostgres {
    pub async fn start() -> Result<Self> {
        info!("starting ephemeral postgres container");
        // testcontainers-modules' Postgres image runs initdb with
        // default user/password/db = "postgres".
        let container = Postgres::default()
            .start()
            .await
            .context("starting postgres container")?;
        let host = container
            .get_host()
            .await
            .context("getting postgres host")?;
        let port = container
            .get_host_port_ipv4(5432.tcp())
            .await
            .context("getting postgres port")?;
        let url = format!("postgresql://postgres:postgres@{host}:{port}/postgres");
        info!(%url, "ephemeral postgres ready");
        Ok(Self {
            url,
            _container: container,
        })
    }
}
