//! Mint relayer: submits the destination-chain mint for `attested`
//! transfers and confirms `minting` ones, recording end-to-end bridge
//! duration. Per tx-alerting convention, terminal submission failures fire
//! `alert_id = "tx-failed-cctp-relay"` here (and benign nonce races —
//! someone else broadcast the mint — are suppressed and treated as
//! complete).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use tracing::{error, info, warn};

use crate::db::models::{chain, status, TransferRow};
use crate::message;
use crate::solana_mint::SolanaMinter;
use crate::state::AppState;
use crate::sui_mint::SuiMinter;

pub struct RelayerParams {
    pub state: Arc<AppState>,
    pub sui: SuiMinter,
    pub solana: SolanaMinter,
    pub relay_interval: Duration,
    pub max_mint_attempts: i32,
}

pub fn spawn(p: RelayerParams) {
    tokio::spawn(async move { run(p).await });
}

async fn run(p: RelayerParams) {
    let mut ticker = tokio::time::interval(p.relay_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if let Err(e) = tick(&p).await {
            warn!(error = %format!("{e:#}"), "relay tick failed; retrying next tick");
        }
    }
}

async fn tick(p: &RelayerParams) -> Result<()> {
    let repo = p.state.repo.clone();
    let attested =
        tokio::task::spawn_blocking(move || repo.transfers_with_status(status::ATTESTED))
            .await
            .context("join")??;
    for row in attested {
        // Linear backoff: wait attempts × 30s after the last failure.
        let backoff = chrono::Duration::seconds(30) * row.attempts;
        if row.attempts > 0 && Utc::now() - row.updated_at < backoff {
            continue;
        }
        if let Err(e) = submit_mint(p, &row).await {
            handle_mint_error(p, &row, &e).await;
        }
    }

    let repo = p.state.repo.clone();
    let minting = tokio::task::spawn_blocking(move || repo.transfers_with_status(status::MINTING))
        .await
        .context("join")??;
    for row in minting {
        if let Err(e) = confirm_mint(p, &row).await {
            warn!(id = row.id, error = %format!("{e:#}"), "mint confirmation check failed");
        }
    }
    Ok(())
}

async fn submit_mint(p: &RelayerParams, row: &TransferRow) -> Result<()> {
    let raw = message::hex_bytes(row.message_hex.as_deref().context("row missing message")?)?;
    let attestation =
        message::hex_bytes(row.attestation_hex.as_deref().context("row missing attestation")?)?;

    let mint_tx = if row.destination_chain() == chain::SUI {
        p.sui.mint(&raw, &attestation).await?
    } else {
        let decoded = message::decode(&raw)?;
        p.solana
            .mint(&raw, &attestation, &decoded, row.destination_wallet.as_deref())
            .await?
    };

    let repo = p.state.repo.clone();
    let (id, tx) = (row.id, mint_tx.clone());
    tokio::task::spawn_blocking(move || repo.mark_minting(id, &tx)).await??;
    info!(id = row.id, mint_tx = %mint_tx, dest = row.destination_chain(), "mint submitted");
    Ok(())
}

async fn handle_mint_error(p: &RelayerParams, row: &TransferRow, e: &anyhow::Error) {
    let msg = format!("{e:#}");
    let lower = msg.to_lowercase();

    // Benign race: the nonce is already used — someone else broadcast this
    // mint (or a previous submission of ours landed without us seeing it).
    // The transfer IS complete; suppress the alert per tx-alerting.md.
    if lower.contains("nonce") && (lower.contains("used") || lower.contains("already")) {
        info!(id = row.id, "nonce already used — mint landed externally; marking complete");
        let repo = p.state.repo.clone();
        let id = row.id;
        let _ = tokio::task::spawn_blocking(move || {
            repo.mark_complete(id, Utc::now(), Some("minted externally (nonce already used)"))
        })
        .await;
        return;
    }

    let attempts = row.attempts + 1;
    if attempts >= p.max_mint_attempts {
        error!(
            alert_id = "tx-failed-cctp-relay",
            id = row.id,
            origin_tx = %row.origin_tx_hash,
            destination = row.destination_chain(),
            attempts,
            error = %msg,
            "mint submission failed terminally"
        );
        let repo = p.state.repo.clone();
        let id = row.id;
        let _ = tokio::task::spawn_blocking(move || repo.mark_failed(id, &msg)).await;
    } else {
        warn!(id = row.id, attempts, error = %msg, "mint submission failed; will retry");
        let repo = p.state.repo.clone();
        let id = row.id;
        let _ = tokio::task::spawn_blocking(move || repo.record_mint_failure(id, &msg)).await;
    }
}

async fn confirm_mint(p: &RelayerParams, row: &TransferRow) -> Result<()> {
    let mint_tx = row.mint_tx_hash.as_deref().context("minting row missing mint_tx_hash")?;

    let minted_at_ms: Option<u64> = if row.destination_chain() == chain::SUI {
        p.sui.tx_timestamp_ms(mint_tx).await?
    } else {
        match p.solana.rpc.transaction_status(mint_tx).await? {
            Some((block_time, true)) => Some(block_time as u64 * 1000),
            Some((_, false)) => {
                // Mint tx landed but reverted — back to attested for retry.
                let repo = p.state.repo.clone();
                let id = row.id;
                tokio::task::spawn_blocking(move || {
                    repo.record_mint_failure(id, "mint tx reverted on chain")
                })
                .await??;
                return Ok(());
            }
            None => None,
        }
    };

    match minted_at_ms {
        Some(ts_ms) => {
            let minted_at = Utc
                .timestamp_millis_opt(ts_ms as i64)
                .single()
                .context("bad mint timestamp")?;
            let repo = p.state.repo.clone();
            let id = row.id;
            tokio::task::spawn_blocking(move || repo.mark_complete(id, minted_at, None))
                .await??;
            let direction = format!("{}->{}", row.origin_chain, row.destination_chain());
            if let Some(burned_at) = row.burned_at {
                let secs = (minted_at - burned_at).num_milliseconds() as f64 / 1000.0;
                metrics::histogram!("cctp_bridge_duration_seconds", "direction" => direction.clone())
                    .record(secs);
                info!(id = row.id, direction, duration_secs = secs, "bridge complete");
            } else {
                info!(id = row.id, direction, "bridge complete (burn timestamp unknown)");
            }
            Ok(())
        }
        None => {
            // Unknown for >2 minutes → assume the blockhash expired and retry.
            if Utc::now() - row.updated_at > chrono::Duration::seconds(120) {
                let repo = p.state.repo.clone();
                let id = row.id;
                tokio::task::spawn_blocking(move || {
                    repo.record_mint_failure(id, "mint tx not found after 2m; resubmitting")
                })
                .await??;
            }
            Ok(())
        }
    }
}
