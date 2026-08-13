#[test_only]
module exchange::balance_manager_tests;

use sui::coin;
use sui::sui::SUI;
use sui::test_scenario as ts;
use exchange::balance_manager::{Self as bm, BalanceManager};
use whitelist::whitelist;

const OWNER: address = @0xA1;
const STRANGER: address = @0xB1;
const HOT: address = @0xC1;

#[test]
fun deposit_withdraw_lifecycle() {
    let mut s = ts::begin(OWNER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    bm::new(s.ctx());

    s.next_tx(OWNER);
    {
        let mut m = s.take_shared<BalanceManager>();
        bm::deposit(&mut m, &wl, coin::mint_for_testing<SUI>(1_000, s.ctx()), s.ctx());
        assert!(bm::balance_of<SUI>(&m) == 1_000, 0);
        assert!(bm::owner(&m) == OWNER, 1);
        ts::return_shared(m);
    };

    s.next_tx(OWNER);
    {
        let mut m = s.take_shared<BalanceManager>();
        let c = bm::withdraw<SUI>(&mut m, 400, s.ctx());
        assert!(c.value() == 400, 2);
        assert!(bm::balance_of<SUI>(&m) == 600, 3);
        coin::burn_for_testing(c);
        ts::return_shared(m);
    };
    whitelist::destroy_for_testing(wl);
    s.end();
}

#[test, expected_failure(abort_code = bm::ENotOwner)]
fun stranger_cannot_withdraw() {
    let mut s = ts::begin(OWNER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    bm::new(s.ctx());
    s.next_tx(OWNER);
    {
        let mut m = s.take_shared<BalanceManager>();
        bm::deposit(&mut m, &wl, coin::mint_for_testing<SUI>(1_000, s.ctx()), s.ctx());
        ts::return_shared(m);
    };
    s.next_tx(STRANGER);
    let mut m = s.take_shared<BalanceManager>();
    let c = bm::withdraw<SUI>(&mut m, 1, s.ctx());
    coin::burn_for_testing(c);
    ts::return_shared(m);
    whitelist::destroy_for_testing(wl);
    s.end();
}

#[test, expected_failure(abort_code = bm::EInsufficientEscrow)]
fun overdraw_rejected() {
    let mut s = ts::begin(OWNER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    bm::new(s.ctx());
    s.next_tx(OWNER);
    let mut m = s.take_shared<BalanceManager>();
    bm::deposit(&mut m, &wl, coin::mint_for_testing<SUI>(100, s.ctx()), s.ctx());
    let c = bm::withdraw<SUI>(&mut m, 101, s.ctx());
    coin::burn_for_testing(c);
    ts::return_shared(m);
    whitelist::destroy_for_testing(wl);
    s.end();
}

#[test]
fun signer_management() {
    let mut s = ts::begin(OWNER);
    bm::new(s.ctx());
    s.next_tx(OWNER);
    {
        let mut m = s.take_shared<BalanceManager>();
        bm::add_signer(&mut m, HOT, s.ctx());
        assert!(bm::is_approved_signer(&m, HOT), 0);
        bm::remove_signer(&mut m, HOT, s.ctx());
        assert!(!bm::is_approved_signer(&m, HOT), 1);
        ts::return_shared(m);
    };
    s.end();
}

#[test, expected_failure(abort_code = bm::ENotOwner)]
fun stranger_cannot_add_signer() {
    let mut s = ts::begin(OWNER);
    bm::new(s.ctx());
    s.next_tx(STRANGER);
    let mut m = s.take_shared<BalanceManager>();
    bm::add_signer(&mut m, HOT, s.ctx());
    ts::return_shared(m);
    s.end();
}

#[test, expected_failure(abort_code = bm::EDepositRestricted)]
fun stranger_cannot_deposit() {
    let mut s = ts::begin(OWNER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    bm::new(s.ctx());
    s.next_tx(STRANGER);
    let mut m = s.take_shared<BalanceManager>();
    bm::deposit(&mut m, &wl, coin::mint_for_testing<SUI>(1, s.ctx()), s.ctx());
    ts::return_shared(m);
    whitelist::destroy_for_testing(wl);
    s.end();
}

#[test]
fun approved_signer_can_deposit() {
    let mut s = ts::begin(OWNER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    bm::new(s.ctx());
    s.next_tx(OWNER);
    {
        let mut m = s.take_shared<BalanceManager>();
        bm::add_signer(&mut m, HOT, s.ctx());
        ts::return_shared(m);
    };
    s.next_tx(HOT);
    {
        let mut m = s.take_shared<BalanceManager>();
        bm::deposit(&mut m, &wl, coin::mint_for_testing<SUI>(500, s.ctx()), s.ctx());
        assert!(bm::balance_of<SUI>(&m) == 500, 0);
        ts::return_shared(m);
    };
    whitelist::destroy_for_testing(wl);
    s.end();
}

#[test]
fun cap_owned_manager_lifecycle() {
    // A cap-owned manager: object-address owner never signs; the cap
    // authorizes deposit/withdraw/signer management.
    let mut s = ts::begin(OWNER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let (_, cap) = bm::new_with_owner_cap(@0xF00D, s.ctx());
    s.next_tx(STRANGER); // any sender — authority is the cap
    {
        let mut m = s.take_shared<BalanceManager>();
        assert!(bm::owner(&m) == @0xF00D, 0);
        bm::deposit_with_cap(&mut m, &wl, &cap, coin::mint_for_testing<SUI>(1_000, s.ctx()), s.ctx());
        bm::add_signer_with_cap(&mut m, &cap, HOT);
        assert!(bm::is_approved_signer(&m, HOT), 1);
        let c = bm::withdraw_with_cap<SUI>(&mut m, &cap, 400, s.ctx());
        assert!(c.value() == 400, 2);
        assert!(bm::balance_of<SUI>(&m) == 600, 3);
        coin::burn_for_testing(c);
        bm::remove_signer_with_cap(&mut m, &cap, HOT);
        assert!(!bm::is_approved_signer(&m, HOT), 4);
        ts::return_shared(m);
    };
    transfer::public_transfer(cap, OWNER);
    whitelist::destroy_for_testing(wl);
    s.end();
}

#[test, expected_failure(abort_code = bm::EWrongCap)]
fun foreign_cap_rejected() {
    let mut s = ts::begin(OWNER);
    let (_, cap_a) = bm::new_with_owner_cap(@0xF00D, s.ctx());
    let (bm_b, cap_b) = bm::new_with_owner_cap(@0xBEEF, s.ctx());
    s.next_tx(STRANGER);
    let mut m = s.take_shared_by_id<BalanceManager>(bm_b);
    let c = bm::withdraw_with_cap<SUI>(&mut m, &cap_a, 1, s.ctx());
    coin::burn_for_testing(c);
    transfer::public_transfer(cap_a, OWNER);
    transfer::public_transfer(cap_b, OWNER);
    ts::return_shared(m);
    s.end();
}

#[test, expected_failure(abort_code = bm::ETooManySigners)]
fun signer_set_is_bounded() {
    let mut s = ts::begin(OWNER);
    bm::new(s.ctx());
    s.next_tx(OWNER);
    let mut m = s.take_shared<BalanceManager>();
    let mut i: u256 = 1;
    while (i <= 17) {
        bm::add_signer(&mut m, sui::address::from_u256(i), s.ctx());
        i = i + 1;
    };
    ts::return_shared(m);
    s.end();
}
