#[test_only]
module vault_v2::tranche_tests;

use sui::clock::Clock;
use sui::coin::Coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::treasury::Treasury;

use vault_v2::capital;
use vault_v2::test_helpers::{Self as h, USDC};
use vault_v2::vault::{Self, CuratorCap, TradingVault};
use vault_v2::vault_position::{Self, VaultPosition};

const HOUR_MS: u64 = 3_600_000;
const DAY_MS: u64 = 86_400_000;
const MS_PER_YEAR: u64 = 31_536_000_000;

/// Standard tranched genesis at t=0: 10% hurdle, 20%/10% junior
/// thresholds, PreferredOnly. Curator escrows 50k junior commitment,
/// alice deposits 200k junior, bob 750k senior. NAV 1M, buffer 25%.
fun setup_tranched(scenario: &mut Scenario): Clock {
    let clock = h::init_protocol(scenario);
    h::new_tranched_vault(scenario, &clock);
    h::fund_commitment(scenario, 50_000, &clock);
    h::deposit_usdc(scenario, h::alice_addr(), 200_000, h::junior(), &clock);
    h::deposit_usdc(scenario, h::bob_addr(), 750_000, h::senior(), &clock);
    clock
}

fun risk_state_of(v: &TradingVault): u8 {
    capital::risk_state_code(&capital::risk_state(vault::book(v)))
}

#[test]
#[expected_failure(abort_code = 123, location = vault_v2::vault)]
fun senior_requires_junior_seed_first() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    h::new_tranched_vault(&mut scenario, &clock);
    // Genesis ordering (§3.4): with zero junior NAV, the post-deposit
    // target test can never pass for a senior deposit.
    h::deposit_usdc(&mut scenario, h::bob_addr(), 100_000, h::senior(), &clock);
    abort 0
}

#[test]
#[expected_failure(abort_code = 123, location = vault_v2::vault)]
fun senior_issuance_gated_on_target_buffer() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_tranched(&mut scenario);
    // 250k junior against 1.3M post-deposit total = 19.2% < 20% target.
    h::deposit_usdc(&mut scenario, h::bob_addr(), 300_000, h::senior(), &clock);
    abort 0
}

#[test]
#[expected_failure(abort_code = 121, location = vault_v2::vault)]
fun untranched_code_rejected_on_tranched_vault() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_tranched(&mut scenario);
    h::deposit_usdc(&mut scenario, h::alice_addr(), 1_000, h::untranched(), &clock);
    abort 0
}

#[test]
fun hurdle_accrues_and_senior_exit_takes_claim_with_pro_rata_reduction() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_tranched(&mut scenario);
    clock.increment_for_testing(MS_PER_YEAR);
    h::crank_capital(&mut scenario, &clock);

    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    // 10% simple over exactly one year on the 750k claim.
    assert!(capital::senior_claim(vault::book(&v)) == 825_000);
    let bob_shares = capital::senior_shares(vault::book(&v));
    assert!(bob_shares == 750_000 * vault::share_offset());
    ts::return_shared(v);

    // Bob exits his whole senior position: PreferredOnly senior NAV is
    // min(1M, 825k) = 825k.
    h::request_withdraw_all(&mut scenario, h::bob_addr(), &clock);
    h::run_fulfillment(&mut scenario, &clock);

    let value = h::expected_value(bob_shares, bob_shares, 825_000);
    let profit = value - 750_000;
    let gross_fee = profit * 1_000 / 10_000;
    let protocol_cut = gross_fee * 1_000 / 10_000;
    let curator_net = gross_fee - protocol_cut;
    let minted = h::expected_shares(curator_net, bob_shares, 825_000);

    ts::next_tx(&mut scenario, h::bob_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(coin.value() == value - gross_fee);
    ts::return_to_sender(&scenario, coin);

    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    // Pro-rata claim reduction extinguished the whole claim (§3.3); the
    // senior fee mint then re-credited exactly curator_net (§3.5), so
    // remaining senior supply is the fee shares and claim-per-share is
    // preserved for them.
    assert!(capital::senior_shares(vault::book(&v)) == minted);
    assert!(capital::senior_claim(vault::book(&v)) == (curator_net as u128));
    assert!(capital::senior_principal_basis(vault::book(&v)) == (curator_net as u128));
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun coverage_breach_blocks_junior_lane_but_senior_keeps_flowing() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_tranched(&mut scenario);
    clock.increment_for_testing(2 * HOUR_MS);

    // Queue junior FIRST (lower global sequence), then senior.
    h::request_withdraw_all(&mut scenario, h::alice_addr(), &clock);
    h::request_withdraw_all(&mut scenario, h::bob_addr(), &clock);

    // 200k strategy loss: junior NAV ≈ 50k on ~800k total = ~6%, below
    // the 10% maintenance threshold ⇒ CoverageBreach.
    h::session_loss(&mut scenario, 200_000);
    h::crank_capital(&mut scenario, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(risk_state_of(&v) == 1);
    assert!(vault::is_risk_off(&v));
    ts::return_shared(v);

    // Fulfillment: the junior head (seq 0) is class-blocked; the senior
    // head (seq 1) behind it must still be paid (§3.6).
    h::run_fulfillment(&mut scenario, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::pending_withdrawals(&v) == 1);
    assert!(vault::has_request(&v, 0)); // junior still queued
    assert!(!vault::has_request(&v, 1)); // senior paid
    ts::return_shared(v);
    ts::next_tx(&mut scenario, h::bob_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(coin.value() > 0);
    ts::return_to_sender(&scenario, coin);

    // Cure the breach (put-only sessions work while risk-off), then the
    // junior lane resumes at its own head in original order.
    h::session_gain<USDC>(&mut scenario, 400_000);
    h::run_fulfillment(&mut scenario, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::pending_withdrawals(&v) == 0);
    ts::return_shared(v);
    ts::next_tx(&mut scenario, h::alice_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(coin.value() > 0);
    ts::return_to_sender(&scenario, coin);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 135, location = vault_v2::vault)]
fun senior_deposits_blocked_in_breach() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_tranched(&mut scenario);
    h::session_loss(&mut scenario, 200_000);
    h::crank_capital(&mut scenario, &clock);
    h::deposit_usdc(&mut scenario, h::bob_addr(), 10_000, h::senior(), &clock);
    abort 0
}

#[test]
fun junior_deposits_allowed_in_breach_and_cure_it() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_tranched(&mut scenario);
    h::session_loss(&mut scenario, 200_000);
    h::crank_capital(&mut scenario, &clock);

    // Junior recapitalization is exactly what a breach asks for.
    h::deposit_usdc(&mut scenario, h::alice_addr(), 200_000, h::junior(), &clock);
    h::crank_capital(&mut scenario, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    // Junior ≈ 250k−ε on 1M total = ~25% > 10% maintenance ⇒ Healthy.
    assert!(risk_state_of(&v) == 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 124, location = vault_v2::vault)]
fun quote_sessions_abort_when_risk_off() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_tranched(&mut scenario);
    h::session_loss(&mut scenario, 200_000);
    h::crank_capital(&mut scenario, &clock);

    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let ireg = ts::take_shared<vault_v2::registry::IntegrationRegistry>(&scenario);
    let s = vault::begin_quote_session(&v, &ireg, h::test_adapter());
    vault::end_session(&v, s);
    abort 0
}

#[test]
fun impairment_reset_end_to_end() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_tranched(&mut scenario);
    clock.increment_for_testing(2 * HOUR_MS);

    // 300k loss: 700k total < 750k+ε claim ⇒ junior wiped, Impaired.
    h::session_loss(&mut scenario, 300_000);
    h::crank_capital(&mut scenario, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(risk_state_of(&v) == 2);
    assert!(capital::impaired_since_ms(vault::book(&v)).is_some());
    ts::return_shared(v);

    // Anyone may propose once the objective conditions hold.
    ts::next_tx(&mut scenario, h::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::propose_junior_reset(&mut v, &cfg, appraisal, &clock);
    assert!(risk_state_of(&v) == 3); // ResetPending
    assert!(capital::has_reset_proposal(vault::book(&v)));
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Season past the immutable 7-day minimum, then execute atomically
    // with the recomputed minimum deposit.
    clock.increment_for_testing(7 * DAY_MS + HOUR_MS);
    ts::next_tx(&mut scenario, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let wl = h::take_whitelist(&scenario);
    let now = clock.timestamp_ms();
    let claim_exec =
        capital::accrued_claim_at(vault::book(&v), vault::capital_structure(&v), now);
    let required = capital::min_reset_deposit(700_000, claim_exec, 2_000);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    let new_junior = vault::execute_junior_reset<USDC>(
        &mut v,
        &cfg,
        &wl,
        appraisal,
        sui::coin::from_balance(h::mint<USDC>(required as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    // Generation rolled; the recapitalizer owns the whole new junior
    // book; the senior claim is NOT written down; state is Healthy.
    assert!(capital::active_junior_generation(vault::book(&v)) == 1);
    assert!(vault_position::capital_generation(&new_junior) == 1);
    assert!(
        capital::junior_shares(vault::book(&v)) == vault_position::shares(&new_junior),
    );
    assert!(capital::senior_claim(vault::book(&v)) == claim_exec);
    assert!(risk_state_of(&v) == 0);
    // The old escrowed junior commitment is wiped: risk stays off until
    // the curator funds a new-generation commitment (§8.5.7).
    assert!(vault::curator_commitment_breached(&v));
    transfer::public_transfer(new_junior, h::alice_addr());
    ts::return_shared(wl);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Alice's ORIGINAL gen-0 junior position is a permanent zero-value
    // claim: only the burn path accepts it.
    ts::next_tx(&mut scenario, h::alice_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    // take the older of the two positions (gen 0)
    let mut ids = ts::ids_for_sender<VaultPosition>(&scenario);
    let mut old: Option<VaultPosition> = option::none();
    while (!ids.is_empty()) {
        let id = ids.pop_back();
        let p = ts::take_from_sender_by_id<VaultPosition>(&scenario, id);
        if (vault_position::capital_generation(&p) == 0) {
            old.fill(p);
        } else {
            ts::return_to_sender(&scenario, p);
        };
    };
    vault::burn_wiped_position(&v, old.destroy_some());
    ts::return_shared(v);

    // Curator refunds a new-generation commitment; risk-on resumes.
    h::fund_commitment(&mut scenario, 50_000, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(!vault::is_risk_off(&v));
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 126, location = vault_v2::vault)]
fun reset_cannot_execute_before_deadline() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_tranched(&mut scenario);
    clock.increment_for_testing(2 * HOUR_MS);
    h::session_loss(&mut scenario, 300_000);
    h::crank_capital(&mut scenario, &clock);

    ts::next_tx(&mut scenario, h::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::propose_junior_reset(&mut v, &cfg, appraisal, &clock);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Only 1 day of the 7-day seasoning has passed.
    clock.increment_for_testing(DAY_MS);
    ts::next_tx(&mut scenario, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let wl = h::take_whitelist(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    let p = vault::execute_junior_reset<USDC>(
        &mut v,
        &cfg,
        &wl,
        appraisal,
        sui::coin::from_balance(h::mint<USDC>(500_000), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(p, h::alice_addr());
    abort 0
}

#[test]
#[expected_failure(abort_code = 127, location = vault_v2::vault)]
fun reset_rejects_insufficient_recapitalization() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_tranched(&mut scenario);
    clock.increment_for_testing(2 * HOUR_MS);
    h::session_loss(&mut scenario, 300_000);
    h::crank_capital(&mut scenario, &clock);

    ts::next_tx(&mut scenario, h::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::propose_junior_reset(&mut v, &cfg, appraisal, &clock);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.increment_for_testing(8 * DAY_MS);
    ts::next_tx(&mut scenario, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let wl = h::take_whitelist(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    // 60k cannot even cure the ≥50k senior deficit AND restore a 20%
    // buffer (needs ~250k).
    let p = vault::execute_junior_reset<USDC>(
        &mut v,
        &cfg,
        &wl,
        appraisal,
        sui::coin::from_balance(h::mint<USDC>(60_000), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(p, h::alice_addr());
    abort 0
}

#[test]
#[expected_failure(abort_code = 126, location = vault_v2::vault)]
fun recovery_cancels_reset_proposal() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_tranched(&mut scenario);
    clock.increment_for_testing(2 * HOUR_MS);
    h::session_loss(&mut scenario, 300_000);
    h::crank_capital(&mut scenario, &clock);

    ts::next_tx(&mut scenario, h::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::propose_junior_reset(&mut v, &cfg, appraisal, &clock);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Marks recover: any complete appraisal showing junior NAV > 0
    // cancels the proposal and clears the impairment clock (§8.5.2).
    h::session_gain<USDC>(&mut scenario, 400_000);
    h::crank_capital(&mut scenario, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(!capital::has_reset_proposal(vault::book(&v)));
    assert!(capital::impaired_since_ms(vault::book(&v)).is_none());
    ts::return_shared(v);

    // Time alone can never execute a wipe: even after the deadline, the
    // cancelled proposal makes execution abort.
    clock.increment_for_testing(30 * DAY_MS);
    // Re-lose value so the vault is impaired again but WITHOUT a live
    // proposal (the old one is gone).
    ts::next_tx(&mut scenario, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let wl = h::take_whitelist(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    let p = vault::execute_junior_reset<USDC>(
        &mut v,
        &cfg,
        &wl,
        appraisal,
        sui::coin::from_balance(h::mint<USDC>(500_000), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(p, h::alice_addr());
    abort 0
}

#[test]
fun settlement_is_senior_first_under_shortfall() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_tranched(&mut scenario);
    clock.increment_for_testing(2 * HOUR_MS);
    // 400k loss: 600k total against a 750k+ε senior claim.
    h::session_loss(&mut scenario, 400_000);

    ts::next_tx(&mut scenario, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    vault::initiate_close(&mut v, &cap);
    ts::return_to_sender(&scenario, cap);
    vault::finalize_close(&mut v);
    ts::return_shared(v);

    ts::next_tx(&mut scenario, h::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::snapshot_settlement(&mut v, &cfg, appraisal, &clock);
    let (senior_pool, _, junior_pool, _, _, _) = vault::settlement_pool(&v);
    // Senior settles first: it takes the entire shortfall NAV; junior
    // gets zero.
    assert!(senior_pool == 600_000);
    assert!(junior_pool == 0);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Bob (sole senior) redeems the whole pool; below basis ⇒ no fee.
    ts::next_tx(&mut scenario, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let mut treasury = ts::take_shared<Treasury>(&scenario);
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    vault::redeem_settled_position<USDC>(&mut v, &cfg, &mut treasury, p, scenario.ctx());
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);
    ts::next_tx(&mut scenario, h::bob_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(coin.value() == 600_000);
    ts::return_to_sender(&scenario, coin);

    // Alice's junior position redeems at exactly zero.
    ts::next_tx(&mut scenario, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let mut treasury = ts::take_shared<Treasury>(&scenario);
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    vault::redeem_settled_position<USDC>(&mut v, &cfg, &mut treasury, p, scenario.ctx());
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}
