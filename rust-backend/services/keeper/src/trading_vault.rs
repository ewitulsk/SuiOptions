//! Trading-vault fulfillment pass (SO-287): a permissionless crank that
//! pays queued withdrawals for CASH-ONLY vaults (deposit asset only, no
//! custodied positions) — the one appraisal shape that needs no
//! attestation legs. Vaults holding positions or foreign assets are
//! skipped (their appraisal PTBs need oracle/adapter legs; the frontend
//! or a follow-up keeper pass composes those).
//!
//! Discovery mirrors the covered-call path: the indexer's
//! `trading_vaults` view (fed by TvVaultCreated). The shared
//! `VaultProtocolConfig` id isn't in deployments.json — it's recovered
//! once at boot from the trading_vault package's publish transaction
//! effects.

use anyhow::{anyhow, Context, Result};
use indexer_graphql::IndexerClient;
use sui_sdk::rpc_types::{SuiTransactionBlockResponseOptions, ObjectChange};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::digests::TransactionDigest;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use tracing::{debug, error, info};

use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::submit_ptb;
use sui_tx::tx::trading_vault as tv_tx;

/// Boot-time context for the pass; `None` when the deployment predates
/// the product.
pub struct TradingVaultCtx {
    pub package: ObjectID,
    pub protocol_config_id: ObjectID,
    pub treasury_id: ObjectID,
    pub gas_budget: u64,
}

/// Recover the shared `VaultProtocolConfig` created by the package's
/// `init` from the publish transaction's object changes.
pub async fn discover_protocol_config(
    client: &SuiClient,
    package: ObjectID,
    publish_digest: &str,
) -> Result<ObjectID> {
    let digest: TransactionDigest = publish_digest
        .parse()
        .with_context(|| format!("parsing trading_vault publish digest {publish_digest}"))?;
    let resp = client
        .read_api()
        .get_transaction_with_options(
            digest,
            SuiTransactionBlockResponseOptions::new().with_object_changes(),
        )
        .await
        .context("fetching trading_vault publish tx")?;
    let wanted = format!("{}::registry::VaultProtocolConfig", package);
    for change in resp.object_changes.unwrap_or_default() {
        if let ObjectChange::Created { object_type, object_id, .. } = change {
            if object_type.to_string() == wanted {
                return Ok(object_id);
            }
        }
    }
    Err(anyhow!("VaultProtocolConfig not found in publish effects of {publish_digest}"))
}

/// One tick: fulfill every cash-only vault with a non-empty queue.
pub async fn tick(wrap: &SuiClientWrapper, indexer: &IndexerClient, ctx: &TradingVaultCtx) {
    let vaults = match indexer.trading_vaults().await {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %format!("{e:#}"), "trading-vault discovery failed; next tick");
            return;
        }
    };
    for v in vaults {
        if v.pending_withdrawals == 0 || v.position_count > 0 {
            continue;
        }
        let vault_id = match ObjectID::from_hex_literal(&v.vault_id.to_hex()) {
            Ok(id) => id,
            Err(e) => {
                debug!(vault = %v.vault_id.to_hex(), error = %e, "bad vault id from indexer");
                continue;
            }
        };
        let deposit_type = v.deposit_asset.to_canonical();
        let refs = tv_tx::TradingVaultRefs {
            package: ctx.package,
            vault_id,
            protocol_config_id: ctx.protocol_config_id,
            deposit_type: &deposit_type,
        };
        match fulfill(wrap, &refs, ctx).await {
            Ok(()) => {
                info!(vault = %vault_id, "trading-vault withdrawals fulfilled");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                // Appraisal-shape aborts (82 incomplete / 83 moved) mean
                // the vault isn't cash-only or raced a session — benign,
                // a fuller appraisal path or the curator handles it.
                if msg.contains(", 82)") || msg.contains(", 83)") || msg.contains(", 78)") {
                    debug!(vault = %vault_id, error = %msg, "fulfillment skipped (appraisal shape)");
                } else {
                    error!(
                        alert_id = "tx-failed-keeper",
                        vault = %vault_id,
                        class = "retry",
                        error = %msg,
                        "trading-vault fulfillment failed; retrying next tick"
                    );
                }
            }
        }
    }
}

async fn fulfill(
    wrap: &SuiClientWrapper,
    refs: &tv_tx::TradingVaultRefs<'_>,
    ctx: &TradingVaultCtx,
) -> Result<()> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let appraisal = tv_tx::build_begin_appraisal(&wrap.client, &mut pt, refs).await?;
    tv_tx::build_fulfill_withdrawals(&wrap.client, &mut pt, refs, ctx.treasury_id, appraisal)
        .await?;
    submit_ptb(
        &wrap.client,
        &wrap.signer,
        pt,
        ctx.gas_budget,
        "trading_vault::fulfill_withdrawals",
    )
    .await?;
    Ok(())
}
