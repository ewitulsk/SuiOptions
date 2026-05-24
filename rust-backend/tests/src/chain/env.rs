//! Composes [`super::localnet::LocalNet`], [`super::postgres::EphemeralPostgres`],
//! [`super::deploy`], and [`super::services`] into a single [`TestEnv`].
//!
//! Each test invokes `TestEnv::fresh().await` to spin up a complete
//! per-test stack: localnet → published contracts → indexer +
//! Postgres → quoting service. Dropping the env tears every layer down
//! deterministically (services first, then localnet, then containers).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use sui_json_rpc_types::{
    SuiTransactionBlockResponse, SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use tempfile::TempDir;
use tracing::info;

use shared::deployments::Deployments;
use shared::sui_client::Signer;
use shared::tx::admin::{new_call_option, NewCallOptionArgs};

use super::actors::{assert_tx_success, extract_created_object};
use super::deploy::publish_all;
use super::localnet::LocalNet;
use super::postgres::EphemeralPostgres;
use super::services::{spawn_indexer, spawn_quoting_service, IndexerHandle, QuotingHandle};

/// The whole stack. Drop order: services (which abort background tasks),
/// then localnet/postgres (which kill the OS resources). Tempdir lives
/// here so paths into it stay valid for the test's lifetime.
pub struct TestEnv {
    pub quoting: QuotingHandle,
    pub indexer: IndexerHandle,
    pub deployments: Deployments,
    pub deployments_path: PathBuf,
    pub postgres: EphemeralPostgres,
    pub localnet: LocalNet,
    pub tempdir: TempDir,
}

impl TestEnv {
    /// Per-test stack. ~25-35s on first run; ~15-20s afterwards once the
    /// Sui binary's Move-compile cache is warm.
    pub async fn fresh() -> Result<Self> {
        info!("spinning up fresh chain TestEnv");
        let tempdir = tempfile::tempdir().context("creating env tempdir")?;
        let localnet = LocalNet::spawn().await?;
        let postgres = EphemeralPostgres::start().await?;

        let deployed = publish_all(&localnet.client, &localnet.deployer, tempdir.path()).await?;
        let indexer = spawn_indexer(
            &localnet.data_ingestion_dir,
            &postgres.url,
            &deployed.deployments,
        )
        .await?;
        let quoting = spawn_quoting_service(indexer.fanout_addr, &deployed.deployments).await?;

        // Give the indexer-client → AppState propagation a beat for the
        // initial snapshot to land.
        tokio::time::sleep(Duration::from_millis(500)).await;

        Ok(Self {
            quoting,
            indexer,
            deployments: deployed.deployments,
            deployments_path: deployed.deployments_path,
            postgres,
            localnet,
            tempdir,
        })
    }

    pub fn client(&self) -> &SuiClient {
        &self.localnet.client
    }

    pub fn deployer(&self) -> &Signer {
        &self.localnet.deployer
    }

    pub fn deployments(&self) -> &Deployments {
        &self.deployments
    }

    pub fn indexer(&self) -> &IndexerHandle {
        &self.indexer
    }

    pub fn quoting(&self) -> &QuotingHandle {
        &self.quoting
    }

    pub fn localnet(&self) -> &LocalNet {
        &self.localnet
    }

    /// Convenience: call `bucket::new_call_option<TBTC, TUSDC>(...)` and
    /// return the fresh bucket's ObjectID. Tests use this in setup.
    pub async fn create_btc_usdc_bucket(
        &self,
        expiry_ms: u64,
        strike: u64,
    ) -> Result<ObjectID> {
        let net = self.deployments.for_network("devnet")?;
        let package = net.package()?;
        let admin_cap = net.admin_cap()?;
        let tbtc = net.token("TBTC")?;
        let tusdc = net.token("TUSDC")?;

        let args = NewCallOptionArgs {
            package,
            admin_cap,
            underlying_type: &tbtc.coin_type,
            settlement_type: &tusdc.coin_type,
            expiry_ms,
            start_strike: strike,
            strike_interval: 1,
            count: 1,
        };
        let resp: SuiTransactionBlockResponse =
            new_call_option(self.client(), self.deployer(), &args, 300_000_000).await?;
        assert_tx_success(&resp)?;
        let changes = resp
            .object_changes
            .as_ref()
            .context("new_call_option missing object_changes")?;
        let bucket = extract_created_object(changes, "bucket", "Bucket")
            .context("no Bucket created")?;
        info!(%bucket, expiry_ms, strike, "created bucket");

        // Wait for both indexer + quoting service to mirror the bucket.
        super::services::wait_for_bucket_propagation(
            &self.indexer,
            &self.quoting,
            bucket,
            Duration::from_secs(20),
        )
        .await?;
        Ok(bucket)
    }
}

/// Allow callers to assert on tx response options without re-importing
/// the sui-sdk types in every test.
pub fn full_response_options() -> SuiTransactionBlockResponseOptions {
    SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_events()
        .with_object_changes()
}
