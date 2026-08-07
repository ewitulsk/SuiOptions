#[test_only]
module exchange::balance_manager_tests;

use sui::coin;
use sui::sui::SUI;
use sui::test_scenario as ts;
use exchange::balance_manager::{Self as bm, BalanceManager};

const OWNER: address = @0xA1;
const STRANGER: address = @0xB1;
const HOT: address = @0xC1;

#[test]
fun deposit_withdraw_lifecycle() {
    let mut s = ts::begin(OWNER);
    bm::new(s.ctx());

    s.next_tx(STRANGER);
    {
        // anyone may deposit
        let mut m = s.take_shared<BalanceManager>();
        bm::deposit(&mut m, coin::mint_for_testing<SUI>(1_000, s.ctx()));
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
    s.end();
}

#[test, expected_failure(abort_code = bm::ENotOwner)]
fun stranger_cannot_withdraw() {
    let mut s = ts::begin(OWNER);
    bm::new(s.ctx());
    s.next_tx(STRANGER);
    let mut m = s.take_shared<BalanceManager>();
    bm::deposit(&mut m, coin::mint_for_testing<SUI>(1_000, s.ctx()));
    let c = bm::withdraw<SUI>(&mut m, 1, s.ctx());
    coin::burn_for_testing(c);
    ts::return_shared(m);
    s.end();
}

#[test, expected_failure(abort_code = bm::EInsufficientEscrow)]
fun overdraw_rejected() {
    let mut s = ts::begin(OWNER);
    bm::new(s.ctx());
    s.next_tx(OWNER);
    let mut m = s.take_shared<BalanceManager>();
    bm::deposit(&mut m, coin::mint_for_testing<SUI>(100, s.ctx()));
    let c = bm::withdraw<SUI>(&mut m, 101, s.ctx());
    coin::burn_for_testing(c);
    ts::return_shared(m);
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
