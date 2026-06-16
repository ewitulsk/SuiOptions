//! Vault-ensure executor.
//!
//! For each configured pair the scheduler guarantees exactly one covered-call
//! vault exists. Creating one mirrors the bucket roll's coin machinery: a vault
//! needs a fresh per-vault share coin (`TreasuryCap<VShare>`, zero supply), so
//! we
//!
//!   1. codegen + compile a one-module `vshare` OTW coin package,
//!   2. publish it and harvest the `TreasuryCap<VShare>` its init minted,
//!   3. `vault::new_config(...)` + `vault::create_vault<U, S, VShare>` in one
//!      PTB, reading the created vault id back from `ObjectChanges`.
//!
//! Idempotency + crash recovery live in the `scheduler_vaults` table (partial
//! UNIQUE index on the active states). The `coin_published` state is the hinge:
//! a crash between publish and create resumes `create_vault` reusing the stored
//! cap, so a second (orphan) share coin is never published. The on-chain vault
//! set (via the indexer) is the backstop: a vault the indexer already reports
//! is recorded confirmed and never recreated, even if the scheduler DB was
//! wiped.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use sui_move_build::BuildConfig;
use sui_types::base_types::ObjectID;
use tracing::{debug, info, warn};

use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::coin_pkg;
use sui_tx::tx::vault_create::{self, CreateVaultSpec, VaultConfigArgs};

use crate::codegen;
use crate::config::VaultTemplate;
use crate::db::{self, models::VaultState};

/// Vault share coins are fixed at 9 decimals. The share accounting is a pure
/// ratio at `PPS_SCALE` (1e12), so decimals are cosmetic (wallet display only).
const SHARE_DECIMALS: u8 = 9;

/// Everything `ensure_vault` needs for one pair: the asset triple's symbols,
/// canonical types, decimals, and the 32-byte Pyth feed ids the vault pins for
/// its oracle reads. Built at boot from token-info.
pub struct VaultPairSpec {
    pub underlying_symbol: String,
    pub settlement_symbol: String,
    pub underlying_type: String,
    pub settlement_type: String,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
    pub underlying_feed_id: Vec<u8>,
    pub settlement_feed_id: Vec<u8>,
}

/// Ensure a vault exists for one pair. Cheap and idempotent: returns early when
/// the pair already has a vault (confirmed row, or one the indexer reports);
/// otherwise publishes a share coin and creates the vault, resuming a partial
/// attempt where it left off.
///
/// `existing_vault_id` is the vault id the caller matched from the indexer's
/// `vaults` view for this pair (if any).
#[allow(clippy::too_many_arguments)]
pub async fn ensure_vault(
    wrap: &SuiClientWrapper,
    package: ObjectID,
    admin_cap: ObjectID,
    db_pool: &db::DbPool,
    spec: &VaultPairSpec,
    template: &VaultTemplate,
    existing_vault_id: Option<String>,
    dry_run: bool,
    gas_budget: u64,
) -> Result<()> {
    let u = spec.underlying_symbol.as_str();
    let s = spec.settlement_symbol.as_str();
    let pair_label = format!("{u}/{s}");
    // Round cadence is the per-pair vault discriminator: a weekly and an hourly
    // vault for the same pair are distinct rows / distinct on-chain vaults.
    let round_ms = template.round_ms;

    let row = db::active_vault_row(db_pool, u, s, round_ms)?;
    // A confirmed row is authoritative: we created it. Trust the DB even if the
    // indexer briefly lags right after creation.
    if row
        .as_ref()
        .is_some_and(|r| r.state == VaultState::Confirmed.as_str())
    {
        return Ok(());
    }

    // On chain already but no confirmed row (e.g. the scheduler DB was wiped):
    // record it confirmed and never recreate.
    if let Some(vault_id) = existing_vault_id {
        info!(pair = %pair_label, %vault_id, round_ms, "vault already on chain; recording confirmed");
        db::record_existing_vault(db_pool, u, s, round_ms, &vault_id)?;
        return Ok(());
    }

    if dry_run {
        info!(pair = %pair_label, "dry-run: would create vault");
        return Ok(());
    }

    // Resume: a coin_published row already owns a harvested share cap — go
    // straight to create_vault, never re-publishing.
    if row
        .as_ref()
        .is_some_and(|r| r.state == VaultState::CoinPublished.as_str())
    {
        let r = row.as_ref().unwrap();
        match (r.share_coin_type.as_deref(), r.share_cap_id.as_deref()) {
            (Some(share_type), Some(cap_id)) => {
                info!(pair = %pair_label, %share_type, "resuming create_vault from published share coin");
                return finish_create(
                    wrap, package, admin_cap, db_pool, spec, template, share_type, cap_id,
                    gas_budget,
                )
                .await;
            }
            _ => {
                warn!(pair = %pair_label, "coin_published row missing share cap fields; failing for retry");
                db::mark_vault_failed(
                    db_pool,
                    u,
                    s,
                    round_ms,
                    "coin_published row missing share cap fields",
                )?;
                return Ok(());
            }
        }
    }

    // Fresh create. Claim the slot if we don't already own a `pending` row.
    if row.is_none() && !db::claim_vault_slot(db_pool, u, s, round_ms)? {
        debug!(pair = %pair_label, "vault slot already claimed; skipping");
        return Ok(());
    }

    // 1. codegen + compile the per-vault share coin package.
    let symbol = format!("v{u}");
    let label = format!("{u}-{s}");
    let pkg_gen =
        codegen::generate_share(SHARE_DECIMALS, &label, &symbol).context("generating vshare package")?;
    let compiled = match BuildConfig::new_for_testing().build(&pkg_gen.dir) {
        Ok(c) => c,
        Err(e) => {
            pkg_gen.cleanup();
            db::mark_vault_failed(db_pool, u, s, round_ms, &format!("compiling vshare: {e:#}"))?;
            return Err(e.context("compiling vshare package"));
        }
    };
    let modules = compiled.get_package_bytes(/* with_unpublished_deps */ false);
    let deps = compiled.get_dependency_storage_package_ids();
    pkg_gen.cleanup();

    // 2. publish + harvest the single TreasuryCap<VShare>.
    let published = match coin_pkg::publish_coin_package(
        &wrap.client,
        &wrap.signer,
        modules,
        deps,
        gas_budget,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(pair = %pair_label, error = %format!("{e:#}"), "vshare publish failed");
            db::mark_vault_failed(db_pool, u, s, round_ms, &format!("publishing vshare: {e:#}"))?;
            return Err(e.context("publishing vshare package"));
        }
    };
    let cap = published
        .caps
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("vshare publish harvested no TreasuryCap"))?;
    let cap_id = cap.cap_ref.0.to_string();
    info!(
        pair = %pair_label,
        coin_package = %published.package_id,
        share_type = %cap.call_type,
        cap_id = %cap_id,
        "vshare coin published"
    );
    db::mark_vault_coin_published(
        db_pool,
        u,
        s,
        round_ms,
        &published.package_id.to_string(),
        &cap.call_type,
        &cap_id,
        &published.digest,
    )?;

    finish_create(
        wrap,
        package,
        admin_cap,
        db_pool,
        spec,
        template,
        &cap.call_type,
        &cap_id,
        gas_budget,
    )
    .await
}

/// `vault::new_config` + `vault::create_vault` using an already-published share
/// coin. On failure the row stays `coin_published`, so the next pass resumes
/// here (or the indexer existence check confirms it if the tx actually landed)
/// — we never re-publish the share coin.
#[allow(clippy::too_many_arguments)]
async fn finish_create(
    wrap: &SuiClientWrapper,
    package: ObjectID,
    admin_cap: ObjectID,
    db_pool: &db::DbPool,
    spec: &VaultPairSpec,
    template: &VaultTemplate,
    share_type: &str,
    cap_id: &str,
    gas_budget: u64,
) -> Result<()> {
    let u = spec.underlying_symbol.as_str();
    let s = spec.settlement_symbol.as_str();
    let pair_label = format!("{u}/{s}");
    let round_ms = template.round_ms;

    let share_cap_id = ObjectID::from_str(cap_id)
        .with_context(|| format!("parsing stored share_cap_id {cap_id}"))?;
    let create_spec = CreateVaultSpec {
        underlying_type: spec.underlying_type.clone(),
        settlement_type: spec.settlement_type.clone(),
        share_type: share_type.to_string(),
        share_cap_id,
        config: build_config_args(spec, template),
    };

    match vault_create::create_vault(
        &wrap.client,
        &wrap.signer,
        package,
        admin_cap,
        &create_spec,
        gas_budget,
    )
    .await
    {
        Ok(out) => {
            metrics::counter!("scheduler_tx_total", "job" => "vault", "outcome" => "ok")
                .increment(1);
            info!(pair = %pair_label, vault_id = %out.vault_id, digest = %out.digest, round_ms, "vault created");
            db::mark_vault_confirmed(db_pool, u, s, round_ms, &out.vault_id.to_string(), &out.digest)?;
            Ok(())
        }
        Err(e) => {
            metrics::counter!("scheduler_tx_total", "job" => "vault", "outcome" => "error")
                .increment(1);
            warn!(
                pair = %pair_label,
                error = %format!("{e:#}"),
                "create_vault failed; row stays coin_published for retry next pass"
            );
            Err(e)
        }
    }
}

/// Merge the pair's oracle pins (feeds, decimals) with the policy template into
/// the `vault::new_config` argument set.
fn build_config_args(spec: &VaultPairSpec, t: &VaultTemplate) -> VaultConfigArgs {
    VaultConfigArgs {
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
        underlying_feed_id: spec.underlying_feed_id.clone(),
        settlement_feed_id: spec.settlement_feed_id.clone(),
        max_price_age_secs: t.max_price_age_secs,
        max_conf_bps: t.max_conf_bps,
        underlying_decimals: spec.underlying_decimals,
        settlement_decimals: spec.settlement_decimals,
    }
}
