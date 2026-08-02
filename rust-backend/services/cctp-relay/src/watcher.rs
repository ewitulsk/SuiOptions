//! Attestation poller: every tick, check ALL `pending_attestation` transfers
//! against Circle's iris API, backfill the burn tx's on-chain timestamp, and
//! advance rows to `attested` once the attestation is signed.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bigdecimal::BigDecimal;
use chrono::{TimeZone, Utc};
use tracing::{error, info, warn};

use crate::db::models::{chain, status, TransferRow};
use crate::iris::{Attestation, IrisClient};
use crate::message;
use crate::solana_rpc::SolanaRpc;
use crate::state::AppState;
use crate::{DOMAIN_SOLANA, DOMAIN_SUI};

pub struct WatcherParams {
    pub state: Arc<AppState>,
    pub iris: IrisClient,
    pub sui: sui_tx::chain::ChainClient,
    pub solana: SolanaRpc,
    pub poll_interval: Duration,
}

pub fn spawn(p: WatcherParams) {
    tokio::spawn(async move { run(p).await });
}

async fn run(p: WatcherParams) {
    let mut ticker = tokio::time::interval(p.poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut consecutive_failures: u32 = 0;

    loop {
        ticker.tick().await;
        match tick(&p).await {
            Ok(()) => consecutive_failures = 0,
            Err(e) => {
                consecutive_failures += 1;
                if consecutive_failures >= 5 {
                    error!(
                        alert_id = "cctp-attestation-poll-failed",
                        consecutive_failures,
                        error = %format!("{e:#}"),
                        "attestation poll failing repeatedly"
                    );
                } else {
                    warn!(error = %format!("{e:#}"), "attestation poll tick failed; retrying");
                }
            }
        }
    }
}

pub fn origin_domain(origin_chain: &str) -> u32 {
    if origin_chain == chain::SUI {
        DOMAIN_SUI
    } else {
        DOMAIN_SOLANA
    }
}

async fn tick(p: &WatcherParams) -> Result<()> {
    let repo = p.state.repo.clone();
    let rows = tokio::task::spawn_blocking(move || {
        repo.transfers_with_status(status::PENDING_ATTESTATION)
    })
    .await
    .context("join")??;

    for row in rows {
        if let Err(e) = process_row(p, &row).await {
            warn!(id = row.id, tx = %row.origin_tx_hash, error = %format!("{e:#}"), "transfer poll failed");
        }
        // Stay far under iris's 35 req/s limit.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

async fn process_row(p: &WatcherParams, row: &TransferRow) -> Result<()> {
    // Backfill the on-chain burn timestamp once (chain time, not ingest time,
    // so the bridge-duration metric isn't skewed by POST latency).
    if row.burned_at.is_none() {
        if let Some(ts_ms) = burn_timestamp_ms(p, row).await? {
            let at = Utc
                .timestamp_millis_opt(ts_ms as i64)
                .single()
                .context("bad burn timestamp")?;
            let repo = p.state.repo.clone();
            let id = row.id;
            tokio::task::spawn_blocking(move || repo.set_burned_at(id, at)).await??;
        }
    }

    let domain = origin_domain(&row.origin_chain);
    match p.iris.attestation(domain, &row.origin_tx_hash).await? {
        Attestation::NotReady => Ok(()),
        Attestation::Ready { message_hex, attestation_hex } => {
            let raw = message::hex_bytes(&message_hex)?;
            let decoded = message::decode(&raw).context("decoding CCTP message")?;
            if decoded.source_domain != domain {
                anyhow::bail!(
                    "message source domain {} does not match origin chain {}",
                    decoded.source_domain,
                    row.origin_chain
                );
            }
            let amount = BigDecimal::from(decoded.burn.amount);
            let mint_recipient = format!("0x{}", hex::encode(decoded.burn.mint_recipient));
            let repo = p.state.repo.clone();
            let id = row.id;
            let (m, a) = (message_hex.clone(), attestation_hex.clone());
            tokio::task::spawn_blocking(move || {
                repo.mark_attested(id, &m, &a, amount, &mint_recipient)
            })
            .await??;
            info!(id = row.id, tx = %row.origin_tx_hash, nonce = decoded.nonce, "attestation ready");
            Ok(())
        }
    }
}

/// On-chain timestamp (ms) of the burn tx, when available.
async fn burn_timestamp_ms(p: &WatcherParams, row: &TransferRow) -> Result<Option<u64>> {
    if row.origin_chain == chain::SUI {
        use sui_types::digests::TransactionDigest;
        let digest = TransactionDigest::from_str(&row.origin_tx_hash)
            .map_err(|e| anyhow::anyhow!("bad sui tx digest: {e}"))?;
        match p.sui.try_get_transaction(&digest).await {
            Ok(Some(tx)) => Ok(tx.timestamp_ms()),
            Ok(None) | Err(_) => Ok(None), // not indexed yet
        }
    } else {
        Ok(p.solana
            .transaction_status(&row.origin_tx_hash)
            .await?
            .map(|(block_time, _)| block_time as u64 * 1000))
    }
}
