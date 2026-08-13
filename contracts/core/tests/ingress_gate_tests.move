/// Guarded-launch ingress gate: whitelist membership, the go-public lever
/// (`whitelist_enabled = false`), and the ingress pause. Ingress (writes) is
/// gated; exits (exercise, redeem, close_offset) must keep working for
/// non-members and while paused.
#[test_only]
module options_core::ingress_gate_tests;

use sui::coin;
use sui::test_scenario::{Self as ts};

use options_core::admin;
use options_core::bucket::{Self, Bucket};
use options_core::put_bucket::{Self, PutBucket};
use options_core::quote;
use options_core::test_helpers::{Self as th, BTC, USDC, CALL, CALL2, PUT};

const STRIKE: u128 = 6;
const EXPIRY_MS: u64 = 1_000_000;

fun set_whitelist_enabled(scenario: &mut ts::Scenario, enabled: bool) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let mut config = th::take_config(scenario);
    admin::set_whitelist_enabled(&cap, &mut config, enabled);
    ts::return_shared(config);
    th::return_admin_cap(scenario, cap);
}

fun set_ingress_paused(scenario: &mut ts::Scenario, paused: bool) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let mut config = th::take_config(scenario);
    admin::set_ingress_paused(&cap, &mut config, paused);
    ts::return_shared(config);
    th::return_admin_cap(scenario, cap);
}

// ─────────────────────────── membership gate ───────────────────────────

#[test]
#[expected_failure(abort_code = 71, location = options_core::admin)] // ingress_restricted
fun test_non_member_write_collateralized_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, 0);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &config,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(pos, th::stranger_addr());
    transfer::public_transfer(call, th::stranger_addr());
    ts::return_shared(config);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 71, location = options_core::admin)] // ingress_restricted
fun test_non_member_put_write_collateralized_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, 0);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let (pos, put) = put_bucket::write_collateralized<BTC, USDC, PUT>(
        &mut b,
        &config,
        coin::mint_for_testing<USDC>(10 * (STRIKE as u64), scenario.ctx()),
        10,
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(pos, th::stranger_addr());
    transfer::public_transfer(put, th::stranger_addr());
    ts::return_shared(config);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 71, location = options_core::admin)] // ingress_restricted
fun test_non_member_write_spread_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, 0);
    th::new_bucket<BTC, USDC, CALL2>(&mut scenario, EXPIRY_MS, 5, 0);

    // A member writes the long leg, then hands the coins to the stranger.
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let config = th::take_config(&scenario);
    let (long_pos, long_call) = bucket::write_collateralized<BTC, USDC, CALL2>(
        &mut long_b,
        &config,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(long_pos, th::writer_addr());
    transfer::public_transfer(long_call, th::stranger_addr());
    ts::return_shared(config);
    ts::return_shared(long_b);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let config = th::take_config(&scenario);
    let long_call = ts::take_from_sender<coin::Coin<CALL2>>(&scenario);
    let (p, c) = bucket::write_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b,
        &config,
        &long_b,
        long_call,
        coin::mint_for_testing<USDC>(10 * 5, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(p, th::stranger_addr());
    transfer::public_transfer(c, th::stranger_addr());
    ts::return_shared(config);
    ts::return_shared(short_b);
    ts::return_shared(long_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 71, location = options_core::admin)] // ingress_restricted
fun test_non_member_execute_writer_flow_aborts() {
    // A signed quote is a bearer instrument; the ingress gate is what stops
    // a non-member from executing one they obtained.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, 0);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        object::id(&b),
        10,
        1_000,
        EXPIRY_MS,
        1,
    );
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(1_000, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        th::stranger_addr(),
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// ─────────────────────────── go-public lever ───────────────────────────

#[test]
fun test_whitelist_disabled_lets_non_member_write() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, 0);

    set_whitelist_enabled(&mut scenario, false);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    assert!(!admin::is_member(&config, th::stranger_addr()), 0);
    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &config,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(config);
    ts::return_shared(b);

    // Re-enabling restores the gate with membership intact.
    set_whitelist_enabled(&mut scenario, true);
    ts::next_tx(&mut scenario, th::admin_addr());
    let config = th::take_config(&scenario);
    assert!(admin::whitelist_enabled(&config), 0);
    assert!(admin::is_member(&config, th::writer_addr()), 0);
    assert!(!admin::is_member(&config, th::stranger_addr()), 0);
    ts::return_shared(config);

    transfer::public_transfer(pos, th::stranger_addr());
    transfer::public_transfer(call, th::stranger_addr());
    clock.destroy_for_testing();
    ts::end(scenario);
}

// ─────────────────────────── ingress pause ───────────────────────────

#[test]
#[expected_failure(abort_code = 72, location = options_core::admin)] // ingress_paused
fun test_ingress_pause_blocks_member_write() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, 0);

    set_ingress_paused(&mut scenario, true);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &config,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(pos, th::writer_addr());
    transfer::public_transfer(call, th::writer_addr());
    ts::return_shared(config);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 72, location = options_core::admin)] // ingress_paused
fun test_ingress_pause_blocks_even_with_whitelist_disabled() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, 0);

    set_whitelist_enabled(&mut scenario, false);
    set_ingress_paused(&mut scenario, true);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &config,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(pos, th::stranger_addr());
    transfer::public_transfer(call, th::stranger_addr());
    ts::return_shared(config);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_pause_and_removal_never_block_exits() {
    // A member writes; then the member is removed AND ingress is paused;
    // exercise and redeem still work — exits are never gated.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let (pos, mut call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &config,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(config);
    ts::return_shared(b);

    // Remove the writer from the whitelist and slam the pause.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut config = th::take_config(&scenario);
    admin::remove_member(&cap, &mut config, th::writer_addr());
    admin::set_ingress_paused(&cap, &mut config, true);
    assert!(!admin::is_member(&config, th::writer_addr()), 0);
    ts::return_shared(config);
    th::return_admin_cap(&scenario, cap);

    // Exercise 4 of 10 as the now-removed writer.
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let chunk = coin::split(&mut call, 4, scenario.ctx());
    let u = bucket::exercise<BTC, USDC, CALL>(
        &mut b,
        chunk,
        coin::mint_for_testing<USDC>(4 * (STRIKE as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(u.value() == 4, 0);

    // Redeem the position after expiry.
    clock.set_for_testing(EXPIRY_MS + 1);
    bucket::burn_expired_option<BTC, USDC, CALL>(&mut b, call, &clock, scenario.ctx());
    let (ru, rs) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos, &clock, scenario.ctx());
    assert!(ru.value() == 6, 0);
    assert!(rs.value() == 4 * (STRIKE as u64), 0);

    coin::burn_for_testing(u);
    coin::burn_for_testing(ru);
    coin::burn_for_testing(rs);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// ─────────────────────────── admin events ───────────────────────────

#[test]
fun test_whitelist_admin_events_emitted() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut config = th::take_config(&scenario);
    admin::add_member(&cap, &mut config, th::stranger_addr());
    admin::remove_member(&cap, &mut config, th::stranger_addr());
    admin::set_whitelist_enabled(&cap, &mut config, false);
    admin::set_ingress_paused(&cap, &mut config, true);
    ts::return_shared(config);
    th::return_admin_cap(&scenario, cap);

    let added = sui::event::events_by_type<options_core::events::MemberAdded>();
    let removed = sui::event::events_by_type<options_core::events::MemberRemoved>();
    let enabled = sui::event::events_by_type<options_core::events::WhitelistEnabledSet>();
    let paused = sui::event::events_by_type<options_core::events::IngressPauseSet>();
    assert!(added.length() == 1, 0);
    assert!(removed.length() == 1, 0);
    assert!(enabled.length() == 1, 0);
    assert!(paused.length() == 1, 0);

    clock.destroy_for_testing();
    ts::end(scenario);
}
