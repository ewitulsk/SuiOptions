#[test_only]
module locker::locker_tests;

use sui::clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use locker::locker::{Self, Locker, LockerAdminCap};
use locker::transfer_payload;

public struct TEST_COIN has drop {}

const ADMIN: address = @0xA;
const SRC_CHAIN: u32 = 2;

fun filled(b: u8, n: u64): vector<u8> {
    let mut v = vector<u8>[];
    let mut i = 0;
    while (i < n) { v.push_back(b); i = i + 1; };
    v
}

fun asset(): vector<u8> { filled(0xaa, 32) }
fun peer(): vector<u8> { filled(0xbb, 32) }

fun payload(asset_id: vector<u8>, amount: u64, recipient: address): vector<u8> {
    transfer_payload::encode(
        &transfer_payload::new(asset_id, amount, sui::address::to_bytes(recipient)),
    )
}

/// Create a mint locker (local decimals == wire decimals → 1:1) and register the
/// source peer. Leaves the scenario holding the admin cap + shared Locker.
fun setup_mint(s: &mut Scenario, local_decimals: u8) {
    let cap = coin::create_treasury_cap_for_testing<TEST_COIN>(s.ctx());
    locker::create_mint_locker<TEST_COIN>(cap, asset(), local_decimals, s.ctx());
    s.next_tx(ADMIN);
    let admin = s.take_from_sender<LockerAdminCap>();
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    locker::set_peer(&admin, &mut lk, SRC_CHAIN, peer());
    ts::return_shared(lk);
    s.return_to_sender(admin);
    s.next_tx(ADMIN);
}

#[test]
fun inbound_mint_delivers_to_recipient() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8); // 1:1
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    let clk = clock::create_for_testing(s.ctx());

    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xCAFE), &clk, s.ctx());

    s.next_tx(ADMIN);
    let c = s.take_from_address<Coin<TEST_COIN>>(@0xCAFE);
    assert!(c.value() == 1000, 0);

    coin::burn_for_testing(c);
    clk.destroy_for_testing();
    ts::return_shared(lk);
    s.end();
}

#[test]
fun inbound_escrow_releases_to_recipient() {
    let mut s = ts::begin(ADMIN);
    // Escrow locker + a separate treasury to fund it.
    let mut cap = coin::create_treasury_cap_for_testing<TEST_COIN>(s.ctx());
    locker::create_escrow_locker<TEST_COIN>(asset(), 8, s.ctx());
    s.next_tx(ADMIN);
    let admin = s.take_from_sender<LockerAdminCap>();
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    locker::set_peer(&admin, &mut lk, SRC_CHAIN, peer());
    locker::fund_escrow_for_testing(&mut lk, coin::mint(&mut cap, 5000, s.ctx()));

    let clk = clock::create_for_testing(s.ctx());
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xCAFE), &clk, s.ctx());
    assert!(locker::escrowed(&lk) == 4000, 0);

    s.next_tx(ADMIN);
    let c = s.take_from_address<Coin<TEST_COIN>>(@0xCAFE);
    assert!(c.value() == 1000, 1);

    coin::burn_for_testing(c);
    clk.destroy_for_testing();
    transfer::public_transfer(cap, ADMIN);
    s.return_to_sender(admin);
    ts::return_shared(lk);
    s.end();
}

#[test]
fun inbound_scales_up_for_higher_local_decimals() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 9); // local 9 > wire 8 → ×10
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    let clk = clock::create_for_testing(s.ctx());

    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xCAFE), &clk, s.ctx());

    s.next_tx(ADMIN);
    let c = s.take_from_address<Coin<TEST_COIN>>(@0xCAFE);
    assert!(c.value() == 10000, 0); // 1000 wire → 10000 local

    coin::burn_for_testing(c);
    clk.destroy_for_testing();
    ts::return_shared(lk);
    s.end();
}

#[test]
#[expected_failure(abort_code = 5, location = locker)]
fun inbound_rejects_wrong_peer() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8);
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    let clk = clock::create_for_testing(s.ctx());
    // src_app is not the registered peer.
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, filled(0xee, 32), payload(asset(), 1, @0xCAFE), &clk, s.ctx());
    abort 99
}

#[test]
#[expected_failure(abort_code = 6, location = locker)]
fun inbound_rejects_wrong_asset() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8);
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    let clk = clock::create_for_testing(s.ctx());
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(filled(0x99, 32), 1, @0xCAFE), &clk, s.ctx());
    abort 99
}

#[test]
#[expected_failure(abort_code = 7, location = locker)]
fun inbound_enforces_rate_limit() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8);
    let admin = s.take_from_sender<LockerAdminCap>();
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    locker::set_rate_limit(&admin, &mut lk, 1_000_000, 1500); // cap 1500/window
    let clk = clock::create_for_testing(s.ctx());

    // First 1000 ok, second 1000 exceeds the 1500 cap.
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xCAFE), &clk, s.ctx());
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xCAFE), &clk, s.ctx());
    abort 99
}

#[test]
#[expected_failure(abort_code = 8, location = locker)]
fun outbound_rejects_dust() {
    // local 6 < wire 8: a local amount not divisible by 10^(8-6)=100 is dust.
    // Exercised via the scaling helper through a mint locker bridge_out would
    // use; here we hit it through from_wire's inverse on inbound is exact, so we
    // assert via apply_inbound with local 6 and a wire amount that doesn't scale.
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 6); // wire 8 > local 6 → from_wire divides by 100
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    let clk = clock::create_for_testing(s.ctx());
    // 1005 wire / 100 has remainder → dust.
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1005, @0xCAFE), &clk, s.ctx());
    abort 99
}
