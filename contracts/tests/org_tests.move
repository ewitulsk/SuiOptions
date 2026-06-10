#[test_only]
module options_protocol::org_tests;

use std::string;
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_protocol::org::{Self, Org, OrgCap};
use options_protocol::test_helpers::{Self as th, USDC};

fun create_org_as(
    scenario: &mut Scenario,
    owner: address,
    name: vector<u8>,
    fee_bps: u64,
): ID {
    ts::next_tx(scenario, owner);
    let cap = org::create_org(string::utf8(name), fee_bps, scenario.ctx());
    let org_id = org::cap_org_id(&cap);
    transfer::public_transfer(cap, owner);
    org_id
}

#[test]
fun test_create_org_shares_org_and_returns_cap() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_id = create_org_as(&mut scenario, th::stranger_addr(), b"acme", 250);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let org_obj = ts::take_shared_by_id<Org>(&scenario, org_id);
    assert!(org::fee_bps(&org_obj) == 250, 0);
    assert!(org::name(&org_obj) == string::utf8(b"acme"), 0);
    assert!(org::balance_of<USDC>(&org_obj) == 0, 0);
    ts::return_shared(org_obj);

    let cap = ts::take_from_sender<OrgCap>(&scenario);
    assert!(org::cap_org_id(&cap) == org_id, 0);
    ts::return_to_sender(&scenario, cap);

    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 32, location = options_protocol::org)] // org_name_invalid
fun test_create_org_empty_name_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    create_org_as(&mut scenario, th::stranger_addr(), b"", 0);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 32, location = options_protocol::org)] // org_name_invalid
fun test_create_org_name_too_long_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    // 65 bytes — one over MAX_ORG_NAME_BYTES.
    create_org_as(
        &mut scenario,
        th::stranger_addr(),
        b"01234567890123456789012345678901234567890123456789012345678901234",
        0,
    );
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 18, location = options_protocol::org)] // fee_too_high
fun test_create_org_fee_above_cap_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    create_org_as(&mut scenario, th::stranger_addr(), b"greedy", 1001);
    ts::end(scenario);
}

#[test]
fun test_set_org_fee_bps_updates_value() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_id = create_org_as(&mut scenario, th::stranger_addr(), b"acme", 0);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let cap = ts::take_from_sender<OrgCap>(&scenario);
    let mut org_obj = ts::take_shared_by_id<Org>(&scenario, org_id);
    org::set_org_fee_bps(&cap, &mut org_obj, 1000);
    assert!(org::fee_bps(&org_obj) == 1000, 0);
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(org_obj);

    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 18, location = options_protocol::org)] // fee_too_high
fun test_set_org_fee_bps_above_cap_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_id = create_org_as(&mut scenario, th::stranger_addr(), b"acme", 0);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let cap = ts::take_from_sender<OrgCap>(&scenario);
    let mut org_obj = ts::take_shared_by_id<Org>(&scenario, org_id);
    org::set_org_fee_bps(&cap, &mut org_obj, 1001);
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(org_obj);

    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 30, location = options_protocol::org)] // org_cap_mismatch
fun test_set_org_fee_with_wrong_cap_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_a = create_org_as(&mut scenario, th::stranger_addr(), b"org-a", 0);
    let _org_b = create_org_as(&mut scenario, th::writer_addr(), b"org-b", 0);

    // writer's cap (org-b) against org-a.
    ts::next_tx(&mut scenario, th::writer_addr());
    let wrong_cap = ts::take_from_sender<OrgCap>(&scenario);
    let mut org_obj = ts::take_shared_by_id<Org>(&scenario, org_a);
    org::set_org_fee_bps(&wrong_cap, &mut org_obj, 1);
    ts::return_to_sender(&scenario, wrong_cap);
    ts::return_shared(org_obj);

    ts::end(scenario);
}

#[test]
fun test_org_deposit_and_withdraw_round_trip() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_id = create_org_as(&mut scenario, th::stranger_addr(), b"acme", 0);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut org_obj = ts::take_shared_by_id<Org>(&scenario, org_id);
    let coin_in = coin::mint_for_testing<USDC>(500_000, scenario.ctx());
    org::deposit_balance(&mut org_obj, coin_in.into_balance());
    assert!(org::balance_of<USDC>(&org_obj) == 500_000, 0);

    let cap = ts::take_from_sender<OrgCap>(&scenario);
    let out = org::withdraw<USDC>(&cap, &mut org_obj, 200_000, scenario.ctx());
    assert!(out.value() == 200_000, 0);
    assert!(org::balance_of<USDC>(&org_obj) == 300_000, 0);
    coin::burn_for_testing(out);
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(org_obj);

    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 33, location = options_protocol::org)] // insufficient_org_balance
fun test_org_withdraw_overdraw_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_id = create_org_as(&mut scenario, th::stranger_addr(), b"acme", 0);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut org_obj = ts::take_shared_by_id<Org>(&scenario, org_id);
    let coin_in = coin::mint_for_testing<USDC>(100, scenario.ctx());
    org::deposit_balance(&mut org_obj, coin_in.into_balance());

    let cap = ts::take_from_sender<OrgCap>(&scenario);
    let out = org::withdraw<USDC>(&cap, &mut org_obj, 101, scenario.ctx());
    coin::burn_for_testing(out);
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(org_obj);

    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 33, location = options_protocol::org)] // insufficient_org_balance — type never deposited
fun test_org_withdraw_unknown_asset_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_id = create_org_as(&mut scenario, th::stranger_addr(), b"acme", 0);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut org_obj = ts::take_shared_by_id<Org>(&scenario, org_id);
    let cap = ts::take_from_sender<OrgCap>(&scenario);
    let out = org::withdraw<USDC>(&cap, &mut org_obj, 1, scenario.ctx());
    coin::burn_for_testing(out);
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(org_obj);

    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 30, location = options_protocol::org)] // org_cap_mismatch
fun test_org_withdraw_with_wrong_cap_aborts() {
    let mut scenario = ts::begin(th::stranger_addr());
    let org_a = create_org_as(&mut scenario, th::stranger_addr(), b"org-a", 0);
    let _org_b = create_org_as(&mut scenario, th::writer_addr(), b"org-b", 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let wrong_cap = ts::take_from_sender<OrgCap>(&scenario);
    let mut org_obj = ts::take_shared_by_id<Org>(&scenario, org_a);
    let out = org::withdraw<USDC>(&wrong_cap, &mut org_obj, 1, scenario.ctx());
    coin::burn_for_testing(out);
    ts::return_to_sender(&scenario, wrong_cap);
    ts::return_shared(org_obj);

    ts::end(scenario);
}
