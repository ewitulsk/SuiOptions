#[test_only]
module vault_v2::vault_tests;

use sui::coin::Coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::treasury::Treasury;

use vault_v2::capital;
use vault_v2::registry::IntegrationRegistry;
use vault_v2::test_helpers::{Self as h, USDC};
use vault_v2::vault::{Self, CuratorCap, TradingVault};
use vault_v2::vault_position::{Self, VaultPosition};

const HOUR_MS: u64 = 3_600_000;

/// Standard genesis: untranched vault, curator escrows 100k commitment,
/// alice deposits 900k. NAV 1M.
fun setup_untranched(scenario: &mut Scenario): sui::clock::Clock {
    let clock = h::init_protocol(scenario);
    h::new_default_vault(scenario, &clock);
    h::fund_commitment(scenario, 100_000, &clock);
    h::simple_deposit(scenario, h::alice_addr(), 900_000, &clock);
    clock
}

#[test]
fun deposit_mints_transferable_position_with_lot_metadata() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_untranched(&mut scenario);

    ts::next_tx(&mut scenario, h::alice_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    // Curator escrowed 100k first: supply 100k×O, nav 100k when alice
    // entered.
    let expected = h::expected_shares(900_000, 100_000 * vault::share_offset(), 100_000);
    assert!(vault_position::shares(&p) == expected);
    assert!(vault_position::cost_basis(&p) == 900_000);
    assert!(vault_position::vault_id(&p) == object::id(&v));
    assert!(vault_position::locked_until_ms(&p) == HOUR_MS * 0 + HOUR_MS); // lockup 1h from t=0
    assert!(vault_position::capital_generation(&p) == 0);
    assert!(capital::tranche_code(&vault_position::tranche(&p)) == 0);
    assert!(vault::total_shares(&v) == 100_000 * vault::share_offset() + expected);

    // Freely transferable: hand it to bob, no vault call involved.
    transfer::public_transfer(p, h::bob_addr());
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun split_and_merge_conserve_shares_and_basis() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_untranched(&mut scenario);

    ts::next_tx(&mut scenario, h::alice_addr());
    let mut p = ts::take_from_sender<VaultPosition>(&scenario);
    let total_shares = vault_position::shares(&p);
    let total_basis = vault_position::cost_basis(&p);

    let child = vault_position::split(&mut p, total_shares / 3, scenario.ctx());
    assert!(vault_position::shares(&child) == total_shares / 3);
    assert!(vault_position::shares(&p) + vault_position::shares(&child) == total_shares);
    assert!(vault_position::cost_basis(&p) + vault_position::cost_basis(&child) == total_basis);
    assert!(vault_position::locked_until_ms(&child) == vault_position::locked_until_ms(&p));

    // Merge back: exact conservation, no averaging.
    vault_position::merge(&mut p, child);
    assert!(vault_position::shares(&p) == total_shares);
    assert!(vault_position::cost_basis(&p) == total_basis);

    ts::return_to_sender(&scenario, p);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 122, location = vault_v2::vault_position)]
fun merge_rejects_cross_vault_positions() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    h::new_default_vault(&mut scenario, &clock);
    h::fund_commitment(&mut scenario, 100_000, &clock);
    h::simple_deposit(&mut scenario, h::alice_addr(), 100_000, &clock);
    // Second vault, second deposit.
    h::new_default_vault(&mut scenario, &clock);
    ts::next_tx(&mut scenario, h::alice_addr());
    let mut v2 = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let wl = h::take_whitelist(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v2);
    let p2 = vault::deposit<USDC>(
        &mut v2,
        &cfg,
        &wl,
        appraisal,
        sui::coin::from_balance(h::mint<USDC>(100_000), scenario.ctx()),
        option::none(),
        h::untranched(),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);
    ts::return_shared(cfg);
    ts::return_shared(v2);

    ts::next_tx(&mut scenario, h::alice_addr());
    let mut p1 = ts::take_from_sender<VaultPosition>(&scenario);
    vault_position::merge(&mut p1, p2);
    abort 0
}

#[test]
#[expected_failure(abort_code = 79, location = vault_v2::vault)]
fun withdraw_respects_lockup() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_untranched(&mut scenario);
    // No clock advance: still locked.
    h::request_withdraw_all(&mut scenario, h::alice_addr(), &clock);
    abort 0
}

#[test]
fun full_exit_crystallizes_fees_and_credits_commitment() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_untranched(&mut scenario);
    // +200k strategy profit; NAV 1.2M.
    h::session_gain<USDC>(&mut scenario, 200_000);
    clock.increment_for_testing(2 * HOUR_MS);

    // Snapshot pre-exit numbers.
    ts::next_tx(&mut scenario, h::alice_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    let alice_shares = vault_position::shares(&p);
    let supply = vault::total_shares(&v);
    let (_, commit_shares_before, _, _) = vault::commitment_of(&v, vault::curator_cap_id(&v));
    ts::return_to_sender(&scenario, p);
    ts::return_shared(v);

    h::request_withdraw_all(&mut scenario, h::alice_addr(), &clock);
    h::run_fulfillment(&mut scenario, &clock);

    // Expected crystallization at the locked ratio (nav 1.2M).
    let value = h::expected_value(alice_shares, supply, 1_200_000);
    let profit = value - 900_000;
    let gross_fee = profit * 1_000 / 10_000;
    let protocol_cut = gross_fee * 1_000 / 10_000;
    let curator_net = gross_fee - protocol_cut;
    let payout = value - gross_fee;
    let minted = h::expected_shares(curator_net, supply, 1_200_000);

    ts::next_tx(&mut scenario, h::alice_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(coin.value() == payout);
    ts::return_to_sender(&scenario, coin);

    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    // Curator fee shares minted into the escrowed commitment position.
    let (has, commit_shares_after, _, _) = vault::commitment_of(&v, vault::curator_cap_id(&v));
    assert!(has);
    assert!(commit_shares_after == commit_shares_before + minted);
    assert!(vault::total_shares(&v) == supply - alice_shares + minted);
    assert!(vault::pending_withdrawals(&v) == 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun secondary_buyer_inherits_basis_and_can_exit() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_untranched(&mut scenario);
    h::session_gain<USDC>(&mut scenario, 200_000);
    clock.increment_for_testing(2 * HOUR_MS);

    // Alice sells (transfers) her position to bob off-vault. Basis
    // travels with the NFT — the buyer inherits the embedded fee
    // liability (§2.4).
    ts::next_tx(&mut scenario, h::alice_addr());
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    let basis = vault_position::cost_basis(&p);
    assert!(basis == 900_000);
    transfer::public_transfer(p, h::bob_addr());

    h::request_withdraw_all(&mut scenario, h::bob_addr(), &clock);
    h::run_fulfillment(&mut scenario, &clock);

    ts::next_tx(&mut scenario, h::bob_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    // Bob paid the fee on profit above ALICE's basis.
    assert!(coin.value() > 0);
    ts::return_to_sender(&scenario, coin);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 91, location = vault_v2::vault)]
fun missing_commitment_forces_sessions_take_aborts() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    h::new_default_vault(&mut scenario, &clock);
    // Deposit WITHOUT funding the curator commitment: the sync marks the
    // commitment breached, so curator sessions open forced (§8.4b/§8.6).
    h::simple_deposit(&mut scenario, h::alice_addr(), 900_000, &clock);
    // take must abort (forced session).
    h::session_loss(&mut scenario, 1);
    abort 0
}

#[test]
fun funding_commitment_restores_risk_on() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    h::new_default_vault(&mut scenario, &clock);
    h::simple_deposit(&mut scenario, h::alice_addr(), 900_000, &clock);

    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::curator_commitment_breached(&v));
    assert!(vault::is_risk_off(&v));
    ts::return_shared(v);

    // 100k on 1M NAV ≫ the 2% floor: cured immediately in-call.
    h::fund_commitment(&mut scenario, 100_000, &clock);
    ts::next_tx(&mut scenario, h::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(!vault::curator_commitment_breached(&v));
    assert!(!vault::is_risk_off(&v));
    ts::return_shared(v);

    // Take-capable sessions work again.
    h::session_loss(&mut scenario, 1);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 80, location = vault_v2::vault)]
fun commitment_release_cannot_break_floor() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_untranched(&mut scenario);

    ts::next_tx(&mut scenario, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    // Release the whole commitment while Open: floor check must abort.
    let p = vault::release_commitment(&mut v, &cap, &cfg, appraisal, 0, &clock, scenario.ctx());
    transfer::public_transfer(p, h::curator_addr());
    abort 0
}

#[test]
fun commitment_release_above_floor_returns_transferable_position() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_untranched(&mut scenario);

    ts::next_tx(&mut scenario, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    let (_, shares, _, _) = vault::commitment_of(&v, object::id(&cap));
    let appraisal = vault::begin_appraisal<USDC>(&v);
    // Release half: 50k of 1M NAV stays well above the 2% floor.
    let p =
        vault::release_commitment(&mut v, &cap, &cfg, appraisal, shares / 2, &clock, scenario.ctx());
    assert!(vault_position::shares(&p) == shares / 2);
    let (_, remaining, _, _) = vault::commitment_of(&v, object::id(&cap));
    assert!(remaining == shares - shares / 2);
    assert!(!vault::is_risk_off(&v));
    transfer::public_transfer(p, h::curator_addr());
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun rotation_releases_escrow_and_blocks_risk_until_refunded() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = setup_untranched(&mut scenario);

    ts::next_tx(&mut scenario, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    vault::rotate_curator_by_curator(&mut v, &cap, h::bob_addr(), scenario.ctx());
    // Pessimistic breach until the incoming cap funds a commitment.
    assert!(vault::curator_commitment_breached(&v));
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(v);

    // Outgoing curator got their claim ticket — an ordinary transferable
    // position with the escrowed shares.
    ts::next_tx(&mut scenario, h::curator_addr());
    let ticket = ts::take_from_sender<VaultPosition>(&scenario);
    assert!(vault_position::shares(&ticket) == 100_000 * vault::share_offset());
    ts::return_to_sender(&scenario, ticket);

    // New curator (bob) funds a fresh commitment and gets risk-on.
    ts::next_tx(&mut scenario, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let wl = h::take_whitelist(&scenario);
    let new_cap = ts::take_from_sender<CuratorCap>(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::deposit_into_commitment<USDC>(
        &mut v,
        &cfg,
        &wl,
        &new_cap,
        appraisal,
        sui::coin::from_balance(h::mint<USDC>(100_000), scenario.ctx()),
        option::none(),
        &clock,
        scenario.ctx(),
    );
    assert!(!vault::curator_commitment_breached(&v));
    ts::return_to_sender(&scenario, new_cap);
    ts::return_shared(wl);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun terminal_settlement_pool_end_to_end() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_untranched(&mut scenario);
    // Bob also deposits, then queues an exit that will still be pending
    // at close — it must settle FROM THE POOL.
    h::simple_deposit(&mut scenario, h::bob_addr(), 200_000, &clock);
    // +300k strategy profit so exits crystallize real fees.
    h::session_gain<USDC>(&mut scenario, 300_000);
    clock.increment_for_testing(2 * HOUR_MS);
    h::request_withdraw_all(&mut scenario, h::bob_addr(), &clock);

    // Close: initiate (curator) → finalize (permissionless) → snapshot.
    ts::next_tx(&mut scenario, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    vault::initiate_close(&mut v, &cap);
    ts::return_to_sender(&scenario, cap);
    vault::finalize_close(&mut v);
    assert!(vault::is_closed(&v));
    ts::return_shared(v);

    ts::next_tx(&mut scenario, h::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::snapshot_settlement(&mut v, &cfg, appraisal, &clock);
    assert!(vault::is_settled(&v));
    let (_, _, junior_pool, junior_supply, _, _) = vault::settlement_pool(&v);
    // Untranched book rides in the junior fields; every outstanding
    // share (alice's wallet position, curator escrow, bob's queued
    // request) is in the supply.
    assert!(junior_pool == 1_500_000);
    assert!(junior_supply == vault::total_shares(&v));
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Bob's queued request settles from the pool, permissionlessly.
    ts::next_tx(&mut scenario, h::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let mut treasury = ts::take_shared<Treasury>(&scenario);
    let (_, _, _, junior_supply, _, _) = vault::settlement_pool(&v);
    let (_, _, _, _, bob_shares, _, _, _, _) = vault::queue_request(&v, 0);
    vault::settle_queued_request<USDC>(&mut v, &cfg, &mut treasury, 0, scenario.ctx());
    assert!(vault::pending_withdrawals(&v) == 0);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    let bob_entitlement =
        ((1_500_000u256 * (bob_shares as u256) / (junior_supply as u256)) as u64);
    ts::next_tx(&mut scenario, h::bob_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    let profit = if (bob_entitlement > 200_000) { bob_entitlement - 200_000 } else { 0 };
    assert!(coin.value() == bob_entitlement - profit * 1_000 / 10_000);
    ts::return_to_sender(&scenario, coin);

    // Alice redeems her wallet position directly against the pool —
    // no queue, no appraisal, at any later time.
    ts::next_tx(&mut scenario, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let mut treasury = ts::take_shared<Treasury>(&scenario);
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    vault::redeem_settled_position<USDC>(&mut v, &cfg, &mut treasury, p, scenario.ctx());
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);
    ts::next_tx(&mut scenario, h::alice_addr());
    let coin = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(coin.value() > 0);
    ts::return_to_sender(&scenario, coin);

    // The curator pulls the escrowed commitment as a settled claim and
    // redeems it, then claims the settlement-crystallized fees.
    ts::next_tx(&mut scenario, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = h::take_protocol_config(&scenario);
    let mut treasury = ts::take_shared<Treasury>(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    let escrowed = vault::withdraw_commitment_settled(&mut v, &cap);
    vault::redeem_settled_position<USDC>(&mut v, &cfg, &mut treasury, escrowed, scenario.ctx());
    let (_, _, _, _, _, fees_accrued) = vault::settlement_pool(&v);
    assert!(fees_accrued > 0);
    vault::claim_settlement_curator_fees<USDC>(&mut v, &cap, scenario.ctx());
    let (_, _, _, _, _, fees_after) = vault::settlement_pool(&v);
    assert!(fees_after == 0);
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 136, location = vault_v2::vault)]
fun no_new_requests_once_closed() {
    let mut scenario = ts::begin(h::admin_addr());
    let mut clock = setup_untranched(&mut scenario);
    clock.increment_for_testing(2 * HOUR_MS);

    ts::next_tx(&mut scenario, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    vault::initiate_close(&mut v, &cap);
    ts::return_to_sender(&scenario, cap);
    vault::finalize_close(&mut v);
    ts::return_shared(v);

    h::request_withdraw_all(&mut scenario, h::alice_addr(), &clock);
    abort 0
}
