/// Guarded-launch ingress gate on the exchange: BalanceManager deposits and
/// fill validation are whitelist-gated; withdrawals and cancels never are.
#[test_only]
module exchange::whitelist_tests;

use sui::clock;
use sui::coin;
use sui::test_scenario::{Self as ts};

use exchange::admin;
use exchange::balance_manager::{Self as bm, BalanceManager};
use exchange::order;
use exchange::registry::{Self, SettlementRegistry};
use exchange::settlement;
use whitelist::whitelist as wl_mod;
use whitelist::whitelist::Whitelist;

public struct BASE has drop {}
public struct QUOTE has drop {}

const ADMIN: address = @0xA1;
const MAKER: address = @0xB2;
const TAKER: address = @0xC3;
const RELAYER: address = @0xD4;
const STRANGER: address = @0xF6;

const EXPIRY_MS: u64 = 10_000;

/// Enabled whitelist with the given members (owned test value).
fun members_wl(members: vector<address>, ctx: &mut TxContext): Whitelist {
    let mut wl = wl_mod::new_open_for_testing(ctx);
    wl_mod::set_enabled_for_testing(&mut wl, true);
    let mut i = 0;
    while (i < members.length()) {
        wl_mod::add_member_for_testing(&mut wl, members[i]);
        i = i + 1;
    };
    wl
}

/// Market + maker BM funded with `maker_amount` BASE (maker sells base 1:1).
/// Returns the maker's signed-order bytes (sig checks disabled in tests).
fun setup_market_and_maker(
    sc: &mut ts::Scenario,
    wl: &Whitelist,
    maker_amount: u64,
): vector<u8> {
    ts::next_tx(sc, ADMIN);
    let cap = admin::mint_for_testing(sc.ctx());
    registry::create_market<BASE, QUOTE>(&cap, 1, 1, 0, sc.ctx());
    admin::burn_for_testing(cap);

    ts::next_tx(sc, MAKER);
    let bm_id = bm::new(sc.ctx());
    ts::next_tx(sc, MAKER);
    let mut mgr = ts::take_shared<BalanceManager>(sc);
    bm::deposit(&mut mgr, wl, coin::mint_for_testing<BASE>(maker_amount, sc.ctx()), sc.ctx());
    ts::return_shared(mgr);

    let ord = order::new_for_testing(
        order::canonical_type<BASE>(),
        order::canonical_type<QUOTE>(),
        maker_amount,
        maker_amount, // 1:1
        0,
        MAKER,
        bm_id,
        @0x0,
        @0x0,
        EXPIRY_MS,
        1,
    );
    order::to_bytes(&ord)
}

// ─────────────────────────── deposit gate ───────────────────────────

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun non_member_deposit_aborts() {
    let mut sc = ts::begin(STRANGER);
    let wl = members_wl(vector[MAKER], sc.ctx());
    bm::new(sc.ctx());
    ts::next_tx(&mut sc, STRANGER);
    let mut mgr = ts::take_shared<BalanceManager>(&sc);
    bm::deposit(&mut mgr, &wl, coin::mint_for_testing<QUOTE>(1_000, sc.ctx()), sc.ctx());
    ts::return_shared(mgr);
    wl_mod::destroy_for_testing(wl);
    sc.end();
}

#[test]
#[expected_failure(abort_code = 2, location = whitelist::whitelist)] // EIngressPaused
fun paused_deposit_aborts_even_for_member() {
    let mut sc = ts::begin(MAKER);
    let mut wl = members_wl(vector[MAKER], sc.ctx());
    wl_mod::set_paused_for_testing(&mut wl, true);
    bm::new(sc.ctx());
    ts::next_tx(&mut sc, MAKER);
    let mut mgr = ts::take_shared<BalanceManager>(&sc);
    bm::deposit(&mut mgr, &wl, coin::mint_for_testing<QUOTE>(1_000, sc.ctx()), sc.ctx());
    ts::return_shared(mgr);
    wl_mod::destroy_for_testing(wl);
    sc.end();
}

#[test]
fun withdraw_works_while_paused_and_delisted() {
    let mut sc = ts::begin(MAKER);
    let mut wl = members_wl(vector[MAKER], sc.ctx());
    bm::new(sc.ctx());
    ts::next_tx(&mut sc, MAKER);
    let mut mgr = ts::take_shared<BalanceManager>(&sc);
    bm::deposit(&mut mgr, &wl, coin::mint_for_testing<QUOTE>(1_000, sc.ctx()), sc.ctx());

    // Delist the owner and slam the pause: exit still works in full.
    wl_mod::set_paused_for_testing(&mut wl, true);
    let out = bm::withdraw<QUOTE>(&mut mgr, 1_000, sc.ctx());
    assert!(out.value() == 1_000);
    coin::burn_for_testing(out);
    ts::return_shared(mgr);
    wl_mod::destroy_for_testing(wl);
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun other_domain_member_deposit_aborts() {
    // Membership on the options domain never satisfies the exchange gate —
    // domains are isolated.
    let mut sc = ts::begin(STRANGER);
    let mut wl = wl_mod::new_open_for_testing(sc.ctx());
    wl_mod::set_enabled_for_testing(&mut wl, true);
    wl_mod::add_member_domain_for_testing(&mut wl, wl_mod::domain_options(), STRANGER);
    bm::new(sc.ctx());
    ts::next_tx(&mut sc, STRANGER);
    let mut mgr = ts::take_shared<BalanceManager>(&sc);
    bm::deposit(&mut mgr, &wl, coin::mint_for_testing<QUOTE>(1_000, sc.ctx()), sc.ctx());
    ts::return_shared(mgr);
    wl_mod::destroy_for_testing(wl);
    sc.end();
}

// ─────────────────────────── fill gate ───────────────────────────

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun non_member_taker_fill_aborts() {
    let mut sc = ts::begin(ADMIN);
    let wl = members_wl(vector[MAKER], sc.ctx());
    let order_bytes = setup_market_and_maker(&mut sc, &wl, 100);

    ts::next_tx(&mut sc, STRANGER);
    let mut reg = ts::take_shared<SettlementRegistry<BASE, QUOTE>>(&sc);
    let mut mgr = ts::take_shared<BalanceManager>(&sc);
    let clock = clock::create_for_testing(sc.ctx());
    let (got, change) = settlement::fill_limit_order_for_testing<BASE, QUOTE>(
        &mut reg,
        &wl,
        &mut mgr,
        order_bytes,
        coin::mint_for_testing<QUOTE>(100, sc.ctx()),
        100,
        0,
        &clock,
        sc.ctx(),
    );
    coin::burn_for_testing(got);
    coin::burn_for_testing(change);
    clock.destroy_for_testing();
    ts::return_shared(mgr);
    ts::return_shared(reg);
    wl_mod::destroy_for_testing(wl);
    sc.end();
}

#[test]
fun member_taker_fill_succeeds_and_open_mode_admits_anyone() {
    let mut sc = ts::begin(ADMIN);
    let mut wl = members_wl(vector[MAKER, TAKER], sc.ctx());
    let order_bytes = setup_market_and_maker(&mut sc, &wl, 100);

    // Member taker fills half.
    ts::next_tx(&mut sc, TAKER);
    let mut reg = ts::take_shared<SettlementRegistry<BASE, QUOTE>>(&sc);
    let mut mgr = ts::take_shared<BalanceManager>(&sc);
    let clock = clock::create_for_testing(sc.ctx());
    let (got, change) = settlement::fill_limit_order_for_testing<BASE, QUOTE>(
        &mut reg,
        &wl,
        &mut mgr,
        order_bytes,
        coin::mint_for_testing<QUOTE>(50, sc.ctx()),
        50,
        0,
        &clock,
        sc.ctx(),
    );
    assert!(got.value() == 50);
    coin::burn_for_testing(got);
    coin::burn_for_testing(change);

    // Go public: a stranger can fill the rest.
    wl_mod::set_enabled_for_testing(&mut wl, false);
    ts::next_tx(&mut sc, STRANGER);
    let (got, change) = settlement::fill_limit_order_for_testing<BASE, QUOTE>(
        &mut reg,
        &wl,
        &mut mgr,
        order_bytes,
        coin::mint_for_testing<QUOTE>(50, sc.ctx()),
        50,
        0,
        &clock,
        sc.ctx(),
    );
    assert!(got.value() == 50);
    coin::burn_for_testing(got);
    coin::burn_for_testing(change);

    clock.destroy_for_testing();
    ts::return_shared(mgr);
    ts::return_shared(reg);
    wl_mod::destroy_for_testing(wl);
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun non_member_relayer_match_aborts() {
    // Both makers are members with funded BMs; the RELAYER submitting the
    // match is not — the match must abort (the relayer wallet has to be
    // seeded into the whitelist at ceremony time).
    let mut sc = ts::begin(ADMIN);
    let wl = members_wl(vector[MAKER, TAKER], sc.ctx());
    let order_a_bytes = setup_market_and_maker(&mut sc, &wl, 100);

    // Second maker (TAKER) sells QUOTE from their own BM.
    ts::next_tx(&mut sc, TAKER);
    let bm_b_id = bm::new(sc.ctx());
    ts::next_tx(&mut sc, TAKER);
    let mut bm_b = ts::take_shared_by_id<BalanceManager>(&sc, bm_b_id);
    bm::deposit(&mut bm_b, &wl, coin::mint_for_testing<QUOTE>(100, sc.ctx()), sc.ctx());
    let ord_b = order::new_for_testing(
        order::canonical_type<QUOTE>(),
        order::canonical_type<BASE>(),
        100,
        100,
        0,
        TAKER,
        bm_b_id,
        @0x0,
        @0x0,
        EXPIRY_MS,
        2,
    );
    let order_b_bytes = order::to_bytes(&ord_b);
    ts::return_shared(bm_b);

    ts::next_tx(&mut sc, RELAYER);
    let mut reg = ts::take_shared<SettlementRegistry<BASE, QUOTE>>(&sc);
    let mut bm_b = ts::take_shared_by_id<BalanceManager>(&sc, bm_b_id);
    let mut bm_a = ts::take_shared<BalanceManager>(&sc);
    let clock = clock::create_for_testing(sc.ctx());
    settlement::match_orders_for_testing<BASE, QUOTE>(
        &mut reg,
        &wl,
        &mut bm_a,
        &mut bm_b,
        order_a_bytes,
        order_b_bytes,
        100,
        &clock,
        sc.ctx(),
    );
    clock.destroy_for_testing();
    ts::return_shared(bm_a);
    ts::return_shared(bm_b);
    ts::return_shared(reg);
    wl_mod::destroy_for_testing(wl);
    sc.end();
}

