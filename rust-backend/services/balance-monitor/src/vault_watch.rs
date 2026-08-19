//! Trading-vault capital watches (SO-418, offchain plan §1.3).
//!
//! Reads the indexer's `tradingVaults` view each poll and re-derives the
//! two senior-protection conditions from the raw fields — an independent
//! tripwire alongside the keeper's risk-state transition alerts (the
//! chain's `risk_state` only refreshes on capital cranks, while the senior
//! claim accrues continuously):
//!
//! - **`tv-claim-coverage`** — the senior claim exceeds the latest NAV
//!   (`senior_shares > 0 && latest_nav < senior_claim`): the on-chain
//!   Impaired condition. Fires each poll while it holds.
//! - **`tv-buffer-low`** — the junior buffer sits below the maintenance
//!   floor (`junior_nav * 10_000 < maintenance_junior_bps * nav`): the
//!   on-chain CoverageBreach condition. Fires each poll while it holds.
//!
//! Both mirror `capital::update_risk_state` in
//! `contracts/trading-vault-v2/sources/capital.move`. Untranched vaults
//! (`structure_code == 0`), vaults without senior supply, and settled
//! vaults (frozen NAV) are skipped.

use indexer_graphql::{IndexerClient, TradingVault};
use tracing::{error, warn};

const BPS_DENOM: u128 = 10_000;

pub struct VaultWatch {
    indexer: IndexerClient,
}

impl VaultWatch {
    pub fn new(graphql_url: String) -> Self {
        Self { indexer: IndexerClient::new(graphql_url) }
    }

    pub async fn poll(&self) {
        let vaults = match self.indexer.trading_vaults().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "trading-vault poll failed");
                metrics::counter!(
                    "balance_monitor_poll_errors_total",
                    "service" => "trading-vaults",
                )
                .increment(1);
                return;
            }
        };
        for v in &vaults {
            check_vault(v);
        }
    }
}

/// What one vault's capital fields evaluate to. Split from the logging so
/// the predicates are testable.
#[derive(Debug, Default, PartialEq, Eq)]
struct Evaluation {
    /// senior_claim as bps of NAV (>= 10_000 ⇒ under-covered).
    claim_bps_of_nav: Option<u128>,
    /// junior buffer as bps of NAV.
    buffer_bps: Option<u128>,
    claim_uncovered: bool,
    buffer_low: bool,
}

/// Mirror of `capital::update_risk_state`'s impairment / coverage-breach
/// math over the indexed fields.
fn evaluate(v: &TradingVault) -> Evaluation {
    let mut eval = Evaluation::default();
    // Only tranched vaults with live senior supply carry a senior claim;
    // a settled vault's NAV is frozen in the settlement pools.
    if v.structure_code == 0 || v.senior_shares == 0 || v.settled {
        return eval;
    }
    // No appraisal yet → nothing to compare.
    let Some(nav) = v.latest_nav else { return eval };

    if nav > 0 {
        eval.claim_bps_of_nav = Some(v.senior_claim.saturating_mul(BPS_DENOM) / nav);
    }
    eval.claim_uncovered = nav < v.senior_claim;

    // Junior buffer vs the maintenance floor. `junior_nav` comes from the
    // latest waterfall (TvCapitalSynced); absent → skip.
    if let Some(junior_nav) = v.junior_nav {
        if nav > 0 {
            eval.buffer_bps = Some(junior_nav.saturating_mul(BPS_DENOM) / nav);
            eval.buffer_low = junior_nav.saturating_mul(BPS_DENOM)
                < (v.maintenance_junior_bps as u128).saturating_mul(nav);
        }
    }
    eval
}

fn check_vault(v: &TradingVault) {
    let eval = evaluate(v);
    let vault = v.vault_id.to_string();
    if let Some(bps) = eval.claim_bps_of_nav {
        metrics::gauge!("tv_senior_claim_bps_of_nav", "vault" => vault.clone()).set(bps as f64);
    }
    if let Some(bps) = eval.buffer_bps {
        metrics::gauge!("tv_junior_buffer_bps", "vault" => vault.clone()).set(bps as f64);
    }
    if eval.claim_uncovered {
        error!(
            alert_id = "tv-claim-coverage",
            vault = %vault,
            nav = %v.latest_nav.unwrap_or_default(),
            senior_claim = %v.senior_claim,
            risk_state = v.risk_state,
            "senior claim exceeds latest NAV (impairment condition)"
        );
    }
    if eval.buffer_low {
        error!(
            alert_id = "tv-buffer-low",
            vault = %vault,
            nav = %v.latest_nav.unwrap_or_default(),
            junior_nav = %v.junior_nav.unwrap_or_default(),
            buffer_bps = %eval.buffer_bps.unwrap_or_default(),
            maintenance_junior_bps = v.maintenance_junior_bps,
            target_junior_bps = v.target_junior_bps,
            risk_state = v.risk_state,
            "junior buffer below maintenance floor (coverage-breach condition)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(
        structure_code: u8,
        senior_shares: u128,
        nav: Option<u128>,
        junior_nav: Option<u128>,
        senior_claim: u128,
        maintenance_junior_bps: u64,
        settled: bool,
    ) -> TradingVault {
        use protocol_types::asset::AssetType;
        use protocol_types::ids::{ObjectId, SuiAddress};
        TradingVault {
            vault_id: ObjectId::new([1; 32]),
            accounting_asset: AssetType::new("0x2::sui::SUI"),
            creator: SuiAddress::new([2; 32]),
            curator: SuiAddress::new([2; 32]),
            curator_cap_id: ObjectId::new([3; 32]),
            state: "open".into(),
            lockup_ms: 0,
            curator_fee_bps: 0,
            unwind_grace_ms: 0,
            deposits_paused: false,
            mm_release_enabled: false,
            total_shares: senior_shares,
            position_count: 0,
            pending_withdrawals: 0,
            latest_pps_e12: None,
            updated_at_ms: 0,
            external_account: None,
            external_exposure: 0,
            latest_external_equity: None,
            external_equity_updated_at_ms: None,
            latest_nav: nav,
            nav_updated_at_ms: None,
            structure_code,
            senior_hurdle_bps_annual: 500,
            target_junior_bps: 2_000,
            maintenance_junior_bps,
            upside_code: 0,
            residual_participation_bps: 0,
            total_return_cap_bps: 0,
            terms_version: 1,
            spec_hash: None,
            senior_shares,
            junior_shares: 0,
            senior_claim,
            senior_principal_basis: senior_claim,
            senior_nav: None,
            junior_nav,
            latest_senior_pps_e12: None,
            latest_junior_pps_e12: None,
            risk_state: 0,
            curator_commitment_breached: false,
            impaired_since_ms: None,
            active_junior_generation: 0,
            reset_old_generation: None,
            reset_proposed_at_ms: None,
            reset_executable_at_ms: None,
            reset_recorded_nav: None,
            reset_recorded_senior_claim: None,
            reset_recorded_required_deposit: None,
            settled,
            settlement_final_nav: None,
            senior_pool: None,
            senior_supply: None,
            junior_pool: None,
            junior_supply: None,
            settlement_snapshot_at_ms: None,
            settlement_redeemed: 0,
            senior_lane_head: 0,
            senior_lane_tail: 0,
            junior_lane_head: 0,
            junior_lane_tail: 0,
        }
    }

    #[test]
    fn claim_coverage_mirrors_impairment_condition() {
        // nav < senior_claim → impaired.
        assert!(evaluate(&vault(1, 10, Some(90), None, 100, 1_000, false)).claim_uncovered);
        // covered
        assert!(!evaluate(&vault(1, 10, Some(100), None, 100, 1_000, false)).claim_uncovered);
        // untranched / no senior supply / settled / no appraisal → skip
        assert!(!evaluate(&vault(0, 10, Some(90), None, 100, 1_000, false)).claim_uncovered);
        assert!(!evaluate(&vault(1, 0, Some(90), None, 100, 1_000, false)).claim_uncovered);
        assert!(!evaluate(&vault(1, 10, Some(90), None, 100, 1_000, true)).claim_uncovered);
        assert!(!evaluate(&vault(1, 10, None, None, 100, 1_000, false)).claim_uncovered);
    }

    #[test]
    fn buffer_low_mirrors_coverage_breach_condition() {
        // buffer 5% < maintenance 10% → breach.
        let e = evaluate(&vault(1, 10, Some(1_000), Some(50), 950, 1_000, false));
        assert!(e.buffer_low);
        assert_eq!(e.buffer_bps, Some(500));
        // buffer exactly at maintenance → healthy (strict < on-chain).
        assert!(!evaluate(&vault(1, 10, Some(1_000), Some(100), 900, 1_000, false)).buffer_low);
        // no junior_nav yet → skip.
        assert!(!evaluate(&vault(1, 10, Some(1_000), None, 950, 1_000, false)).buffer_low);
        // untranched → skip even with a thin buffer.
        assert!(!evaluate(&vault(0, 10, Some(1_000), Some(50), 950, 1_000, false)).buffer_low);
    }

    /// evaluate/check_vault must not panic on edge inputs (nav == 0,
    /// saturation near u128::MAX).
    #[test]
    fn handles_edges_without_panicking() {
        let zero_nav = evaluate(&vault(1, 10, Some(0), Some(0), 100, 1_000, false));
        assert!(zero_nav.claim_uncovered); // 0 < 100
        assert!(!zero_nav.buffer_low); // nav == 0 skips the ratio
        check_vault(&vault(1, 10, Some(0), Some(0), 100, 1_000, false));
        check_vault(&vault(1, 10, Some(u128::MAX), Some(u128::MAX), u128::MAX, 10_000, false));
        check_vault(&vault(0, 0, None, None, 0, 0, false));
    }
}
