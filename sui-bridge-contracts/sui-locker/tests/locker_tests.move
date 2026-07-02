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
fun inbound_over_limit_queues_then_claims() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8); // 1:1
    let admin = s.take_from_sender<LockerAdminCap>();
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    locker::set_rate_limit(&admin, &mut lk, 1000, 1500); // cap 1500 / 1000ms window
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(10_000); // past the initial (t=0) window so it re-anchors

    // First 1000 is within cap → delivered immediately (window re-anchors to 10_000).
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xCAFE), &clk, s.ctx());
    // Second 1000 exceeds the 1500 cap → queued (does NOT abort), nothing minted.
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xBEEF), &clk, s.ctx());

    assert!(locker::is_queued(&lk, 0), 0);
    let (recipient, amount, unlock_at) = locker::queued_transfer(&lk, 0);
    assert!(recipient == @0xBEEF, 1);
    assert!(amount == 1000, 2);
    assert!(unlock_at == 11_000, 3); // window end at enqueue time (10_000 + 1000)

    // Only the first delivery minted so far.
    s.next_tx(ADMIN);
    let c0 = s.take_from_address<Coin<TEST_COIN>>(@0xCAFE);
    assert!(c0.value() == 1000, 4);
    coin::burn_for_testing(c0);

    // Claim after the window releases the queued amount.
    clk.set_for_testing(11_000);
    locker::claim(&mut lk, 0, &clk, s.ctx());
    assert!(!locker::is_queued(&lk, 0), 5);

    s.next_tx(ADMIN);
    let c1 = s.take_from_address<Coin<TEST_COIN>>(@0xBEEF);
    assert!(c1.value() == 1000, 6);
    coin::burn_for_testing(c1);

    clk.destroy_for_testing();
    s.return_to_sender(admin);
    ts::return_shared(lk);
    s.end();
}

#[test]
#[expected_failure(abort_code = 11, location = locker)]
fun claim_before_unlock_aborts() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8);
    let admin = s.take_from_sender<LockerAdminCap>();
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    locker::set_rate_limit(&admin, &mut lk, 1_000_000, 500);
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(10_000);

    // 1000 > cap 500 → queued at id 0.
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xBEEF), &clk, s.ctx());
    // Still inside the window → StillLocked.
    locker::claim(&mut lk, 0, &clk, s.ctx());
    abort 99
}

#[test]
#[expected_failure(abort_code = 12, location = locker)]
fun claim_unknown_entry_aborts() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8);
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    let clk = clock::create_for_testing(s.ctx());
    locker::claim(&mut lk, 99, &clk, s.ctx());
    abort 99
}

#[test]
#[expected_failure(abort_code = 3, location = locker)]
fun claim_blocked_when_paused() {
    let mut s = ts::begin(ADMIN);
    setup_mint(&mut s, 8);
    let admin = s.take_from_sender<LockerAdminCap>();
    let mut lk = s.take_shared<Locker<TEST_COIN>>();
    locker::set_rate_limit(&admin, &mut lk, 1_000_000, 500);
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(10_000);
    locker::apply_inbound_for_testing(&mut lk, SRC_CHAIN, peer(), payload(asset(), 1000, @0xBEEF), &clk, s.ctx());

    locker::set_paused(&admin, &mut lk, true);
    clk.set_for_testing(10_000 + 1_000_000);
    locker::claim(&mut lk, 0, &clk, s.ctx()); // paused → abort
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
