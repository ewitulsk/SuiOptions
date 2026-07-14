//! Vault-ensure executor.
//!
//! For each configured pair the scheduler guarantees exactly one covered-call
//! vault exists per round cadence. On Solana the share mint is a PDA created
//! inside `create_vault`, so the Sui twin's publish-coin → harvest-cap →
//! create three-step collapses to a **single tx**; the `coin_published`
//! crash-recovery state is gone.
//!
//! Idempotency + crash recovery live in the `scheduler_vaults` table (partial
//! UNIQUE index on the active states) plus the deterministic vault salt
//! ([`crate::salt::vault_salt`]), which folds in the row's **replacement
//! generation** (the retired-row count, stamped at claim time):
//!
//! - a create whose confirmation was lost retries at the SAME generation →
//!   same PDA → collides "already in use" → adopted as confirmed (after an
//!   on-chain paused check);
//! - a paused vault that was retired bumps the generation → the replacement
//!   derives a NEW PDA, so it can never collide with (and re-adopt) the
//!   decommissioned vault.
//!
//! The on-chain vault set (via the indexer) is the backstop: a live vault the
//! indexer already reports is recorded confirmed and never recreated, even if
//! the scheduler DB was wiped.

use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use options_vault::state::{Vault, VaultConfig};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use tracing::{debug, error, info, warn};

use solana_tx::SolanaClientWrapper;

use crate::config::VaultTemplate;
use crate::db::{self, models::VaultState};
use crate::roller::{self, ErrorClass};
use crate::salt;

/// Everything `ensure_vault` needs for one pair: symbols, mints, decimals,
/// and the 32-byte Pyth feed ids the vault pins for its oracle reads. Built
/// at boot from solana-token-info.
pub struct VaultPairSpec {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying_mint: String,
    pub settlement_mint: String,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
    pub underlying_feed_id: [u8; 32],
    pub settlement_feed_id: [u8; 32],
}

/// Merge the pair's oracle pins (feeds, decimals) with the policy template
/// into the on-chain `VaultConfig` — the `[vault_template]` fields map 1:1.
pub fn build_vault_config(spec: &VaultPairSpec, t: &VaultTemplate) -> VaultConfig {
    VaultConfig {
        mgmt_fee_bps_annual: t.mgmt_fee_bps_annual,
        perf_fee_bps: t.perf_fee_bps,
        round_ms: t.round_ms,
        selling_window_ms: t.selling_window_ms,
        min_strike_bps_over_spot: t.min_strike_bps_over_spot,
        max_strike_bps_over_spot: t.max_strike_bps_over_spot,
        min_expiry_lead_ms: t.min_expiry_lead_ms,
        max_expiry_lead_ms: t.max_expiry_lead_ms,
        min_reserve_premium_bps: t.min_reserve_premium_bps,
        max_slice_amount: t.max_slice_amount,
        max_open_rfqs: t.max_open_rfqs,
        rfq_duration_ms: t.rfq_duration_ms,
        rfq_snipe_window_ms: t.rfq_snipe_window_ms,
        rfq_snipe_extension_ms: t.rfq_snipe_extension_ms,
        rfq_max_extension_ms: t.rfq_max_extension_ms,
        rfq_min_increment_bps: t.rfq_min_increment_bps,
        hold_premium_in_settlement: t.hold_premium_in_settlement,
        max_swap_slippage_bps: t.max_swap_slippage_bps,
        underlying_feed_id: spec.underlying_feed_id,
        settlement_feed_id: spec.settlement_feed_id,
        max_price_age_secs: t.max_price_age_secs,
        max_conf_bps: t.max_conf_bps,
        underlying_decimals: spec.underlying_decimals,
        settlement_decimals: spec.settlement_decimals,
    }
}

/// The vault PDA this pair+cadence derives to at a given replacement
/// generation (salt is deterministic).
pub fn derived_vault_pda(spec: &VaultPairSpec, round_ms: u64, generation: u64) -> Result<Pubkey> {
    let u = Pubkey::from_str(&spec.underlying_mint)
        .with_context(|| format!("parsing underlying mint {}", spec.underlying_mint))?;
    let s = Pubkey::from_str(&spec.settlement_mint)
        .with_context(|| format!("parsing settlement mint {}", spec.settlement_mint))?;
    let vault_salt = salt::vault_salt(&u, &s, round_ms, generation);
    Ok(solana_tx::pda::vault(&options_vault::ID, &u, &s, vault_salt))
}

/// Ensure a vault exists for one pair at one cadence. Cheap and idempotent:
/// returns early when the pair already has a vault (confirmed row, or one
/// the indexer reports); otherwise claims the slot (stamping the replacement
/// generation on the row) and submits the single `create_vault` tx.
///
/// `existing_vault_id` is the vault id the caller matched from the indexer's
/// `vaults` view for this pair+cadence (if any, non-paused).
pub async fn ensure_vault(
    wrap: &SolanaClientWrapper,
    db_pool: &db::DbPool,
    spec: &VaultPairSpec,
    template: &VaultTemplate,
    existing_vault_id: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let u = spec.underlying_symbol.as_str();
    let s = spec.settlement_symbol.as_str();
    let pair_label = format!("{u}/{s}");
    // Round cadence is the per-pair vault discriminator: a weekly and an
    // hourly vault for the same pair are distinct rows / distinct PDAs.
    let round_ms = template.round_ms;

    let row = db::active_vault_row(db_pool, u, s, round_ms)?;
    // A confirmed row is authoritative: we created it. Trust the DB even if
    // the indexer briefly lags right after creation.
    if row
        .as_ref()
        .is_some_and(|r| r.state == VaultState::Confirmed.as_str())
    {
        return Ok(());
    }

    // On chain already but no confirmed row (e.g. the scheduler DB was
    // wiped): record it confirmed and never recreate.
    if let Some(vault_id) = existing_vault_id {
        info!(pair = %pair_label, %vault_id, round_ms, "vault already on chain; recording confirmed");
        db::record_existing_vault(db_pool, u, s, round_ms, &vault_id)?;
        return Ok(());
    }

    if dry_run {
        info!(pair = %pair_label, round_ms, "dry-run: would create vault");
        return Ok(());
    }

    // Resolve the replacement generation. Crash-resume (a leftover pending
    // row) MUST reuse the value stamped on the row — never recompute it
    // from a retired count that may have moved since the claim.
    let generation = match &row {
        Some(pending) => pending.generation.max(0) as u64,
        None => match db::claim_vault_slot(db_pool, u, s, round_ms)? {
            Some(generation) => generation,
            None => {
                debug!(pair = %pair_label, "vault slot already claimed; skipping");
                return Ok(());
            }
        },
    };

    let underlying_mint = Pubkey::from_str(&spec.underlying_mint)
        .with_context(|| format!("parsing underlying mint {}", spec.underlying_mint))?;
    let settlement_mint = Pubkey::from_str(&spec.settlement_mint)
        .with_context(|| format!("parsing settlement mint {}", spec.settlement_mint))?;
    let vault_salt = salt::vault_salt(&underlying_mint, &settlement_mint, round_ms, generation);
    let vault_pda =
        solana_tx::pda::vault(&options_vault::ID, &underlying_mint, &settlement_mint, vault_salt);
    let config = build_vault_config(spec, template);
    let ix = solana_tx::ix::create_vault(
        &wrap.signer.pubkey(),
        &underlying_mint,
        &settlement_mint,
        vault_salt,
        config,
    );

    let blockhash = match wrap.client.get_latest_blockhash().await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("fetching latest blockhash for create_vault: {e}");
            db::mark_vault_failed(db_pool, u, s, round_ms, &msg)?;
            return Err(anyhow!(msg));
        }
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&wrap.signer.pubkey()),
        &[&wrap.signer.keypair],
        blockhash,
    );

    match wrap.client.send_and_confirm_transaction(&tx).await {
        Ok(sig) => {
            metrics::counter!("scheduler_tx_total", "job" => "vault", "outcome" => "ok")
                .increment(1);
            info!(
                pair = %pair_label,
                vault_id = %vault_pda,
                signature = %sig,
                round_ms,
                generation,
                "vault created"
            );
            db::mark_vault_confirmed(
                db_pool,
                u,
                s,
                round_ms,
                &vault_pda.to_string(),
                Some(&sig.to_string()),
            )?;
            Ok(())
        }
        Err(e) => {
            let err = anyhow!("create_vault for {pair_label} failed: {e}");
            if roller::is_already_in_use(&err) {
                // The deterministic salt collided: a vault already exists at
                // this (pair, cadence, generation) PDA — normally a prior
                // create of OURS whose confirmation we lost. Adopt it only
                // after verifying on-chain that it is live: a paused vault
                // here means the generation bookkeeping is wrong (e.g. the
                // DB lost its retired rows) and adopting it would re-enter
                // the decommissioned vault.
                return adopt_collided_vault(wrap, db_pool, spec, round_ms, generation, &vault_pda)
                    .await;
            }
            metrics::counter!("scheduler_tx_total", "job" => "vault", "outcome" => "error")
                .increment(1);
            let class = roller::classify_error(&err);
            warn!(
                pair = %pair_label,
                class = ?class,
                error = %format!("{err:#}"),
                "create_vault failed; row marked failed for a fresh retry next pass"
            );
            // Single-tx create: whether or not the ambiguous case landed,
            // `failed` frees the slot and the retry is salt-idempotent — the
            // failed row does not bump the generation, so the retry reuses
            // the SAME PDA (a landed create collides and gets adopted).
            let park = match class {
                ErrorClass::DefinitelyNotSent => "definitely not sent",
                ErrorClass::Ambiguous => "ambiguous; retry resolves via salt collision",
            };
            db::mark_vault_failed(db_pool, u, s, round_ms, &format!("{park}: {err:#}"))?;
            Err(err)
        }
    }
}

/// Resolve an "already in use" collision on the derived vault PDA: read the
/// vault account and adopt it as confirmed ONLY if deposits are not paused.
/// A paused vault at this generation's PDA is a generation-bookkeeping bug
/// (adopting it would loop the decommissioned vault back in) — mark the
/// attempt failed and surface the error so the caller alerts every pass
/// until an operator intervenes.
async fn adopt_collided_vault(
    wrap: &SolanaClientWrapper,
    db_pool: &db::DbPool,
    spec: &VaultPairSpec,
    round_ms: u64,
    generation: u64,
    vault_pda: &Pubkey,
) -> Result<()> {
    let u = spec.underlying_symbol.as_str();
    let s = spec.settlement_symbol.as_str();
    let pair_label = format!("{u}/{s}");

    let vault: Vault = match wrap.get_account_deserialized(vault_pda).await {
        Ok(v) => v,
        Err(e) => {
            // Can't verify liveness — never adopt blind. Leave the row
            // pending so the next pass retries the read/create.
            bail!(
                "create_vault for {pair_label} collided at {vault_pda} but the vault \
                 account could not be read for the paused check: {e:#}"
            );
        }
    };
    if vault.paused_deposits {
        let msg = format!(
            "vault PDA {vault_pda} (generation {generation}) is already in use by a PAUSED \
             vault — generation bookkeeping is wrong (retired rows lost?); refusing to adopt"
        );
        error!(pair = %pair_label, round_ms, generation, "{msg}");
        db::mark_vault_failed(db_pool, u, s, round_ms, &msg)?;
        return Err(anyhow!(msg));
    }
    info!(
        pair = %pair_label,
        vault_id = %vault_pda,
        generation,
        "vault PDA already in use by a live vault; adopting as confirmed"
    );
    db::mark_vault_confirmed(db_pool, u, s, round_ms, &vault_pda.to_string(), None)?;
    Ok(())
}
