#[test_only]
module trading_vault::vault_tests;

use sui::balance;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::test_scenario as ts;

use sui::event;

use options_core::admin::ProtocolConfig as CoreProtocolConfig;
use options_core::treasury::{Self, Treasury};

use trading_vault::events;
use trading_vault::registry::{Self, IntegrationRegistry, OracleRegistry};
use trading_vault::test_helpers as h;
use trading_vault::vault::{Self, CuratorCap, TradingVault};

// Share numbers below carry the virtual offset: a genesis deposit of V
// accounting units mints V × SHARE_OFFSET (1e6) shares, and later mints
// follow shares = value × (S + O) / (nav + 1) — asserted through
// h::expected_shares / h::expected_value where the dust makes literal
// constants unreadable.

// ═══════════════════════ shares and deposits ═══════════════════════

#[test]
fun genesis_deposit_mints_at_offset() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::alice_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let (shares, basis, _) = vault::stake_of(&v, h::alice_addr());
    // value × (0 + O) / (0 + 1) = 1_000_000 × 1e6.
    assert!(shares == 1_000_000_000_000);
    assert!(basis == 1_000_000);
    assert!(vault::total_shares(&v) == 1_000_000_000_000);
    assert!(vault::free_balance_of<h::USDC>(&v) == 1_000_000);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun second_deposit_prices_at_nav() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    // Strategy gain: NAV 1.5M against 1M-unit genesis stake → pps 1.5.
    h::session_gain<h::USDC>(&mut sc, 500_000);
    h::simple_deposit(&mut sc, h::bob_addr(), 1_500_000, &clock);

    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let (bob_shares, _, _) = vault::stake_of(&v, h::bob_addr());
    let expected = h::expected_shares(1_500_000, 1_000_000_000_000, 1_500_000);
    assert!(bob_shares == expected);
    // ≈ 1e12 (same value-per-share as Alice), offset dust only.
    assert!(bob_shares >= 999_999_000_000 && bob_shares <= 1_000_001_000_000);
    assert!(vault::total_shares(&v) == 1_000_000_000_000 + expected);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

// ═══════════════ crystallization: fees, pps neutrality ═══════════════

#[test]
fun withdraw_with_profit_charges_fees_and_keeps_pps() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    h::session_gain<h::USDC>(&mut sc, 1_000_000); // NAV 2M

    let s0: u128 = 1_000_000_000_000;
    clock.set_for_testing(4_000_000); // past lockup
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::USDC>(&mut v, s0, &clock, sc.ctx());
    ts::return_shared(v);

    // Permissionless fulfillment by a stranger.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());

    // value = 1e12 × (2M+1) / (1e12+1e6) = 1_999_999 (1 unit of offset
    // dust to the vault); profit 999_999; gross fee 99_999 (10%);
    // protocol cut 9_999 (10% of fee); curator net 90_000;
    // payout = value − gross_fee = 1_900_000.
    let value = h::expected_value(s0, s0, 2_000_000);
    assert!(value == 1_999_999);
    assert!(treasury::balance_of<h::USDC>(&treasury) == 9_999);
    // Curator fee minted at the batch ratio.
    let cap_id = vault::curator_cap_id(&v);
    let (cshares, cbasis, _) = vault::curator_stake_of(&v, cap_id);
    let m = h::expected_shares(90_000, s0, 2_000_000);
    assert!(cshares == m);
    assert!(cbasis == 90_000);
    assert!(vault::total_shares(&v) == m);
    // Remaining assets back the curator's net (+ offset dust).
    assert!(vault::free_balance_of<h::USDC>(&v) == 2_000_000 - 1_900_000 - 9_999);
    assert!(vault::pending_withdrawals(&v) == 0);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Alice got value − gross_fee.
    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(paid.value() == 1_900_000);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun withdraw_at_loss_charges_no_fee() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    h::session_loss(&mut sc, 400_000); // NAV 600k

    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::USDC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());
    assert!(treasury::balance_of<h::USDC>(&treasury) == 0);
    assert!(vault::total_shares(&v) == 0);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(paid.value() == 600_000);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun fulfillment_stops_when_value_is_deployed() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    h::simple_deposit(&mut sc, h::bob_addr(), 1_000_000, &clock);

    // At pps exactly 1 the offset cancels: Bob also holds 1e12 shares.
    // Curator deploys 1.5M into a position: free 500k, NAV still 2M
    // (position appraises at its cost).
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let p = h::new_position(&mut sc);
    let pid = object::id(&p);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    let deployed = vault::take<h::USDC>(&mut v, &mut s, 1_500_000);
    balance::destroy_for_testing(deployed); // "sent to the venue"
    vault::put_position(&mut v, &mut s, p);
    vault::end_session(&v, s);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Alice queues her full stake: worth 1M > 500k free.
    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::USDC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    // The crank cannot fund the head request and leaves it queued.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::record_position_value(&v, &mut appraisal, h::test_adapter(), pid, 1_500_000);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());
    assert!(vault::pending_withdrawals(&v) == 1);
    assert!(vault::free_balance_of<h::USDC>(&v) == 500_000);
    assert!(vault::total_shares(&v) == 2_000_000_000_000);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Curator unwinds the position (returns its value as cash); the
    // crank then pays Alice in full: value 1M, no profit, no fee.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    let p = vault::take_position<h::TestPosition>(&mut v, &mut s, pid);
    h::destroy_position(p);
    vault::put<h::USDC>(&mut v, &mut s, h::mint<h::USDC>(1_500_000));
    vault::end_session(&v, s);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());
    assert!(vault::pending_withdrawals(&v) == 0);
    assert!(vault::total_shares(&v) == 1_000_000_000_000);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(paid.value() == 1_000_000);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

// ═══════════════════════ lockup and floor ═══════════════════════

#[test]
#[expected_failure(abort_code = 79, location = trading_vault::vault)]
fun withdraw_before_lockup_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::USDC>(&mut v, 1_000_000, &clock, sc.ctx());
    abort 0
}

#[test]
#[expected_failure(abort_code = 80, location = trading_vault::vault)]
fun curator_below_floor_cannot_withdraw() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 950_000, &clock);

    // Curator stakes exactly 5% (pps is exactly 1, so the offset
    // cancels: 50_000 × 1e6 shares of 1e12 total).
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::deposit_as_curator<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        &cap,
        appraisal,
        coin::from_balance(h::mint<h::USDC>(50_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);

    // Any curator withdrawal now breaches the 5% floor.
    clock.set_for_testing(4_000_000);
    vault::request_withdraw_as_curator<h::USDC>(
        &mut v,
        &cfg,
        &cap,
        10_000,
        h::curator_addr(),
        &clock,
    );
    abort 0
}

#[test]
fun curator_can_withdraw_when_floor_disabled() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 950_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::deposit_as_curator<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        &cap,
        appraisal,
        coin::from_balance(h::mint<h::USDC>(50_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Protocol-level disablement.
    ts::next_tx(&mut sc, h::admin_addr());
    let admin_cap = h::take_admin_cap(&sc);
    let mut cfg = h::take_protocol_config(&sc);
    registry::set_enforce_curator_share(&admin_cap, &mut cfg, false);
    ts::return_shared(cfg);
    h::return_admin_cap(&sc, admin_cap);

    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::request_withdraw_as_curator<h::USDC>(
        &mut v,
        &cfg,
        &cap,
        10_000,
        h::curator_addr(),
        &clock,
    );
    assert!(vault::pending_withdrawals(&v) == 1);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

// ═══════════════════ sessions, custody, appraisal ═══════════════════

#[test]
#[expected_failure(abort_code = 75, location = trading_vault::vault)]
fun unlisted_adapter_cannot_open_session() {
    let mut sc = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let _s = vault::begin_session(&v, &cap, &ireg, h::other_adapter());
    abort 0
}

#[test]
#[expected_failure(abort_code = 82, location = trading_vault::vault)]
fun deposit_with_unappraised_asset_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    h::session_gain<h::BTC>(&mut sc, 100_000_000); // vault now holds BTC too

    // Appraisal without a BTC attestation must not complete.
    h::simple_deposit(&mut sc, h::bob_addr(), 1_000_000, &clock);
    abort 0
}

#[test]
fun appraisal_with_attestation_prices_second_asset() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    // 1.0 BTC (8 decimals) into the vault.
    h::session_gain<h::BTC>(&mut sc, 100_000_000);

    // BTC→USDC raw ratio: 1e8 BTC-raw = 1_000_000 USDC-raw, i.e.
    // price = 1_000_000 × 1e12 / 1e8 = 1e10.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    let att = h::attest<h::BTC, h::USDC>(&sc, 10_000_000_000, clock.timestamp_ms());
    vault::appraise_balance<h::BTC>(&v, &cfg, &mut appraisal, att, &clock);
    // NAV = 1_000_000 USDC + 1_000_000 (BTC valued) = 2_000_000.
    vault::deposit<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        appraisal,
        coin::from_balance(h::mint<h::USDC>(1_000_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    let (bob_shares, _, _) = vault::stake_of(&v, h::bob_addr());
    let expected = h::expected_shares(1_000_000, 1_000_000_000_000, 2_000_000);
    assert!(bob_shares == expected);
    // ≈ half of Alice's stake, offset dust only.
    assert!(bob_shares >= 499_999_000_000 && bob_shares <= 500_001_000_000);
    ts::return_shared(core_cfg);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun position_custody_and_valuation() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Curator custodies a position via the test adapter.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let p = h::new_position(&mut sc);
    let pid = object::id(&p);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    vault::put_position(&mut v, &mut s, p);
    vault::end_session(&v, s);
    assert!(vault::position_count(&v) == 1);
    assert!(vault::has_position(&v, pid));

    // Deposit prices the position via record_position_value.
    let cfg = h::take_protocol_config(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::record_position_value(&v, &mut appraisal, h::test_adapter(), pid, 1_000_000);
    // NAV 2M; 1M buys ≈ half of Alice's stake.
    vault::deposit<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        appraisal,
        coin::from_balance(h::mint<h::USDC>(1_000_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
    let (cur_shares, _, _) = vault::stake_of(&v, h::curator_addr());
    assert!(cur_shares == h::expected_shares(1_000_000, 1_000_000_000_000, 2_000_000));

    // Take the position back out through a session.
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    let p = vault::take_position<h::TestPosition>(&mut v, &mut s, pid);
    vault::end_session(&v, s);
    h::destroy_position(p);
    assert!(vault::position_count(&v) == 0);

    ts::return_to_sender(&sc, cap);
    ts::return_shared(cfg);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 75, location = trading_vault::vault)]
fun wrong_adapter_cannot_value_position() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let p = h::new_position(&mut sc);
    let pid = object::id(&p);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    vault::put_position(&mut v, &mut s, p);
    vault::end_session(&v, s);

    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::record_position_value(&v, &mut appraisal, h::other_adapter(), pid, 1);
    abort 0
}

#[test]
#[expected_failure(abort_code = 76, location = trading_vault::price)]
fun rogue_oracle_cannot_attest() {
    let mut sc = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut sc);

    ts::next_tx(&mut sc, h::alice_addr());
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let _att = trading_vault::price::attest(
        h::rogue_oracle(),
        &oreg,
        std::type_name::with_defining_ids<h::BTC>(),
        std::type_name::with_defining_ids<h::USDC>(),
        1,
        0,
    );
    abort 0
}

#[test]
#[expected_failure(abort_code = 91, location = trading_vault::vault)]
fun forced_session_cannot_take() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Close → force sessions unlock, but they can only return value.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::initiate_close(&mut v, &cap);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut s = vault::begin_force_session(&v, &ireg, h::test_adapter(), &clock);
    let _funds = vault::take<h::USDC>(&mut v, &mut s, 1);
    abort 0
}

#[test]
fun delisted_adapter_can_still_be_unwound_by_crank_and_force_sessions() {
    // SO-310: delisting is a kill switch on new deployment, not on exits.
    // A vault holding a position under a since-disallowed adapter must
    // still be unwindable by anyone through the take-less sessions.
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let p = h::new_position(&mut sc);
    let pid = object::id(&p);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    vault::put_position(&mut v, &mut s, p);
    vault::end_session(&v, s);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Kill switch: the adapter is delisted with the position still held.
    ts::next_tx(&mut sc, h::admin_addr());
    let admin_cap = h::take_admin_cap(&sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(&sc);
    registry::disallow_adapter(
        &admin_cap,
        &mut ireg,
        std::type_name::with_defining_ids<h::TestAdapter>(),
    );
    assert!(
        !registry::is_adapter_allowed(&ireg, &std::type_name::with_defining_ids<h::TestAdapter>()),
    );
    ts::return_shared(ireg);
    h::return_admin_cap(&sc, admin_cap);

    // Crank session (always unlocked): redeem the stranded position.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut s = vault::begin_crank_session(&v, &ireg, h::test_adapter());
    let p: h::TestPosition = vault::take_position(&mut v, &mut s, pid);
    vault::put<h::USDC>(&mut v, &mut s, h::mint<h::USDC>(500_000)); // redemption proceeds
    vault::end_session(&v, s);
    h::destroy_position(p);
    assert!(vault::position_count(&v) == 0);
    assert!(vault::free_balance_of<h::USDC>(&v) == 1_500_000);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Force session (unlocked by Closing) against the same delisted adapter.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::initiate_close(&mut v, &cap);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut s = vault::begin_force_session(&v, &ireg, h::test_adapter(), &clock);
    vault::put<h::USDC>(&mut v, &mut s, h::mint<h::USDC>(1));
    vault::end_session(&v, s);
    assert!(vault::free_balance_of<h::USDC>(&v) == 1_500_001);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

// ══════════════════ appraisal events + crank (SO-304) ══════════════════

#[test]
fun appraisal_emits_position_and_vault_events() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Curator custodies a position via the test adapter.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let p = h::new_position(&mut sc);
    let pid = object::id(&p);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    vault::put_position(&mut v, &mut s, p);
    vault::end_session(&v, s);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // A deposit through a complete appraisal emits both events.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let vault_id = object::id(&v);
    let cfg = h::take_protocol_config(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::record_position_value(&v, &mut appraisal, h::test_adapter(), pid, 500_000);
    vault::deposit<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        appraisal,
        coin::from_balance(h::mint<h::USDC>(300_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);

    let pos_events = event::events_by_type<events::PositionAppraised>();
    assert!(pos_events.length() == 1);
    let (ev_vault, ev_adapter, ev_pos, ev_value) =
        events::position_appraised_fields(&pos_events[0]);
    assert!(ev_vault == vault_id);
    assert!(ev_adapter == std::type_name::with_defining_ids<h::TestAdapter>());
    assert!(ev_pos == pid);
    assert!(ev_value == 500_000);

    let nav_events = event::events_by_type<events::VaultAppraised>();
    assert!(nav_events.length() == 1);
    let (ev_vault, ev_total, ev_positions) = events::vault_appraised_fields(&nav_events[0]);
    assert!(ev_vault == vault_id);
    assert!(ev_total == 1_500_000); // 1M free + 500k position mark
    assert!(ev_positions == 1);

    ts::return_shared(cfg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun crank_appraisal_is_permissionless_and_emits() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let p = h::new_position(&mut sc);
    let pid = object::id(&p);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    vault::put_position(&mut v, &mut s, p);
    vault::end_session(&v, s);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // A stranger cranks a full appraisal; nothing moves, marks emit.
    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::record_position_value(&v, &mut appraisal, h::test_adapter(), pid, 750_000);
    vault::crank_appraisal(&v, appraisal);

    assert!(event::events_by_type<events::PositionAppraised>().length() == 1);
    let nav_events = event::events_by_type<events::VaultAppraised>();
    assert!(nav_events.length() == 1);
    let (_, ev_total, ev_positions) = events::vault_appraised_fields(&nav_events[0]);
    assert!(ev_total == 1_750_000);
    assert!(ev_positions == 1);
    assert!(vault::total_shares(&v) == 1_000_000_000_000);
    assert!(vault::free_balance_of<h::USDC>(&v) == 1_000_000);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 82, location = trading_vault::vault)]
fun crank_appraisal_rejects_incomplete_appraisal() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let p = h::new_position(&mut sc);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    vault::put_position(&mut v, &mut s, p);
    vault::end_session(&v, s);

    // The position was never appraised — crank must abort like consume.
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::crank_appraisal(&v, appraisal);
    abort 0
}

#[test]
#[expected_failure(abort_code = 83, location = trading_vault::vault)]
fun crank_appraisal_rejects_skewed_snapshot() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Same-tx session between begin and crank invalidates the snapshot
    // (any balance mutation bumps the vault's mutation_seq).
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    let mut s = vault::begin_session(&v, &cap, &ireg, h::test_adapter());
    vault::put<h::USDC>(&mut v, &mut s, h::mint<h::USDC>(1));
    vault::end_session(&v, s);
    vault::crank_appraisal(&v, appraisal);
    abort 0
}

// ═══════════════════════ closure and rotation ═══════════════════════

#[test]
fun closure_pays_everyone_out() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Close immediately: lockup has NOT passed, closure waives it.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::initiate_close(&mut v, &cap);
    vault::finalize_close(&mut v);
    assert!(vault::is_closed(&v));
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    // Anyone can enqueue Alice's stake and crank the payout.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    vault::enqueue_closed_stake(&mut v, h::alice_addr(), &clock);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());
    assert!(vault::total_shares(&v) == 0);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(paid.value() == 1_000_000); // no profit, no fee
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun rotation_moves_the_role_not_the_stake() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 950_000, &clock);

    // Curator stakes 50k → exactly 5e10 shares (pps 1, offset cancels).
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let old_cap_id = object::id(&cap);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::deposit_as_curator<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        &cap,
        appraisal,
        coin::from_balance(h::mint<h::USDC>(50_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Curator rotates the role to Bob.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::rotate_curator_by_curator(&mut v, &cap, h::bob_addr(), sc.ctx());
    ts::return_to_sender(&sc, cap);
    assert!(vault::curator_cap_id(&v) != old_cap_id);
    // The old cap's stake is intact, keyed by the old cap id.
    let (old_shares, _, _) = vault::curator_stake_of(&v, old_cap_id);
    assert!(old_shares == 50_000_000_000);
    ts::return_shared(v);

    // The old cap can still exit its stake — no floor (it's not the
    // curator any more), normal lockup.
    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let old_cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::request_withdraw_as_curator<h::USDC>(
        &mut v,
        &cfg,
        &old_cap,
        50_000_000_000,
        h::curator_addr(),
        &clock,
    );
    assert!(vault::pending_withdrawals(&v) == 1);
    ts::return_to_sender(&sc, old_cap);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // But it cannot open sessions.
    ts::next_tx(&mut sc, h::curator_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let old_cap = ts::take_from_sender<CuratorCap>(&sc);
    assert!(object::id(&old_cap) != vault::curator_cap_id(&v));
    ts::return_to_sender(&sc, old_cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 70, location = trading_vault::vault)]
fun old_cap_cannot_open_sessions() {
    let mut sc = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::rotate_curator_by_curator(&mut v, &cap, h::bob_addr(), sc.ctx());
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::curator_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let old_cap = ts::take_from_sender<CuratorCap>(&sc);
    let _s = vault::begin_session(&v, &old_cap, &ireg, h::test_adapter());
    abort 0
}
