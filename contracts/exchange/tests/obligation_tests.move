#[test_only]
/// The dependency-inverted escrow protocol (SO-372): parity with the
/// classic paths (bit-identical amounts, fees, events), the external
/// OwnerCap-bound leg, bearer legs, and the binding/state-machine aborts.
module exchange::obligation_tests;

use sui::balance;
use sui::clock::{Self, Clock};
use sui::coin;
use sui::event;
use sui::sui::SUI;
use sui::test_scenario as ts;
use exchange::admin;
use exchange::balance_manager::{Self as bm, BalanceManager};
use exchange::order;
use exchange::registry::{Self, SettlementRegistry};
use exchange::settlement::{Self, FillEvent};
use whitelist::whitelist;

public struct USDC has drop {}

const ADMIN: address = @0xAD;
const MAKER_A: address = @0xA1;
const MAKER_B: address = @0xB1;
const TAKER: address = @0xC1;
const RELAYER: address = @0xD1;

const NOW: u64 = 1_000_000;
const EXPIRY: u64 = 1_060_000;
const FEE_BPS: u64 = 10;

fun setup(): (ts::Scenario, Clock) {
    let mut s = ts::begin(ADMIN);
    let cap = admin::mint_for_testing(s.ctx());
    registry::create_market<SUI, USDC>(&cap, 1, 1, FEE_BPS, s.ctx());
    admin::burn_for_testing(cap);
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(NOW);
    (s, clk)
}

fun new_bm(s: &mut ts::Scenario, owner: address): ID {
    s.next_tx(owner);
    bm::new(s.ctx())
}

fun fund<T>(s: &mut ts::Scenario, id: ID, owner: address, amount: u64) {
    s.next_tx(owner);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let mut m = s.take_shared_by_id<BalanceManager>(id);
    bm::deposit(&mut m, &wl, coin::mint_for_testing<T>(amount, s.ctx()), s.ctx());
    ts::return_shared(m);
    whitelist::destroy_for_testing(wl);
}

/// Maker sells `maker_amount` SUI (base) for `taker_amount` USDC.
fun ask(maker: address, bm_id: ID, maker_amount: u64, taker_amount: u64, salt: u64): vector<u8> {
    order::to_bytes(&order::new_for_testing(
        order::canonical_type<SUI>(),
        order::canonical_type<USDC>(),
        maker_amount,
        taker_amount,
        FEE_BPS,
        maker,
        bm_id,
        @0x0,
        @0x0,
        EXPIRY,
        salt,
    ))
}

/// Maker sells `maker_amount` USDC (quote) for `taker_amount` SUI.
fun bid(maker: address, bm_id: ID, maker_amount: u64, taker_amount: u64, salt: u64): vector<u8> {
    order::to_bytes(&order::new_for_testing(
        order::canonical_type<USDC>(),
        order::canonical_type<SUI>(),
        maker_amount,
        taker_amount,
        FEE_BPS,
        maker,
        bm_id,
        @0x0,
        @0x0,
        EXPIRY,
        salt,
    ))
}

// ═══════════════════════════ parity: Path A ═══════════════════════════

#[test]
fun obligation_fill_matches_classic_exactly() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 100_000);
    let ord1 = ask(MAKER_A, a, 50_000, 100_000, 1);
    let ord2 = ask(MAKER_A, a, 50_000, 100_000, 2);

    // Classic fill: 40_000 quote in. Events are per-transaction in
    // test_scenario, so capture this fill's event fields here.
    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let (classic_base, classic_quote_bm, mk1, tk1, b1, q1, mf1, tf1, sold1, tot1) = {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let mut m = s.take_shared_by_id<BalanceManager>(a);
        let (got, change) = settlement::fill_limit_order_for_testing<SUI, USDC>(
            &mut reg,
            &wl,
            &mut m,
            ord1,
            coin::mint_for_testing<USDC>(40_000, s.ctx()),
            40_000,
            0,
            &clk,
            s.ctx(),
        );
        let got_v = got.value();
        coin::burn_for_testing(got);
        coin::burn_for_testing(change);
        let q = bm::balance_of<USDC>(&m);
        ts::return_shared(m);
        ts::return_shared(reg);
        let evs = event::events_by_type<FillEvent>();
        assert!(evs.length() == 1, 100);
        let (mk, tk, b, q_ev, mf, tf, sold, tot) = settlement::fill_event_fields(&evs[0]);
        (got_v, q, mk, tk, b, q_ev, mf, tf, sold, tot)
    };

    // Obligation fill of the identical twin order.
    s.next_tx(TAKER);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let mut m = s.take_shared_by_id<BalanceManager>(a);
        let mut ob = settlement::begin_fill_for_testing<SUI, USDC>(
            &mut reg, &wl, &m, ord2, 40_000, 0, &clk, s.ctx(),
        );
        settlement::provide_quote(&mut ob, balance::create_for_testing<USDC>(40_000));
        settlement::provide_base_from_manager(&mut ob, &mut m);
        settlement::collect_quote_to_manager(&mut ob, &mut m);
        let got = settlement::collect_base_bearer(&mut ob);
        settlement::finish(&mut reg, ob);

        // Identical taker take, identical maker credit, doubled fees.
        assert!(got.value() == classic_base, 0);
        assert!(bm::balance_of<USDC>(&m) == classic_quote_bm * 2, 1);
        assert!(bm::balance_of<SUI>(&m) == 100_000 - 2 * 20_000, 2);
        assert!(registry::fee_vault_base_value(&reg) == 2 * 20, 3);
        assert!(registry::fee_vault_quote_value(&reg) == 2 * 40, 4);
        balance::destroy_for_testing(got);
        ts::return_shared(m);
        ts::return_shared(reg);

        // Field-level event parity (all economics; digests differ by
        // salt).
        let fills = event::events_by_type<FillEvent>();
        assert!(fills.length() == 1, 5);
        let (mk2, tk2, b2, q2, mf2, tf2, sold2, tot2) =
            settlement::fill_event_fields(&fills[0]);
        assert!(mk1 == mk2 && tk1 == tk2, 6);
        assert!(b1 == b2 && q1 == q2, 7);
        assert!(mf1 == mf2 && tf1 == tf2, 8);
        assert!(sold1 == sold2 && tot1 == tot2, 9);
    };

    whitelist::destroy_for_testing(wl);
    clk.destroy_for_testing();
    s.end();
}

#[test]
fun obligation_reverse_fill_matches_classic_exactly() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<USDC>(&mut s, a, MAKER_A, 200_000);
    let ord1 = bid(MAKER_A, a, 100_000, 50_000, 1);
    let ord2 = bid(MAKER_A, a, 100_000, 50_000, 2);

    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let classic_quote = {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let mut m = s.take_shared_by_id<BalanceManager>(a);
        let (got, change) = settlement::fill_limit_order_reverse_for_testing<SUI, USDC>(
            &mut reg,
            &wl,
            &mut m,
            ord1,
            coin::mint_for_testing<SUI>(20_000, s.ctx()),
            20_000,
            0,
            &clk,
            s.ctx(),
        );
        let v = got.value();
        coin::burn_for_testing(got);
        coin::burn_for_testing(change);
        ts::return_shared(m);
        ts::return_shared(reg);
        v
    };

    s.next_tx(TAKER);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let mut m = s.take_shared_by_id<BalanceManager>(a);
        let mut ob = settlement::begin_fill_reverse_for_testing<SUI, USDC>(
            &mut reg, &wl, &m, ord2, 20_000, 0, &clk, s.ctx(),
        );
        settlement::provide_base(&mut ob, balance::create_for_testing<SUI>(20_000));
        settlement::provide_quote_from_manager(&mut ob, &mut m);
        settlement::collect_base_to_manager(&mut ob, &mut m);
        let got = settlement::collect_quote_bearer(&mut ob);
        settlement::finish(&mut reg, ob);
        assert!(got.value() == classic_quote, 0);
        balance::destroy_for_testing(got);
        ts::return_shared(m);
        ts::return_shared(reg);
    };

    whitelist::destroy_for_testing(wl);
    clk.destroy_for_testing();
    s.end();
}

// ═══════════════════════════ parity: Path B ═══════════════════════════

#[test]
fun obligation_match_matches_classic_exactly() {
    let (mut s, clk) = setup();
    // Pair 1 (classic) and pair 2 (obligation), identical economics;
    // relative salt order preserved so the resting-price rule picks the
    // same side both times.
    let a1 = new_bm(&mut s, MAKER_A);
    let b1 = new_bm(&mut s, MAKER_B);
    let a2 = new_bm(&mut s, MAKER_A);
    let b2 = new_bm(&mut s, MAKER_B);
    fund<SUI>(&mut s, a1, MAKER_A, 50_000);
    fund<USDC>(&mut s, b1, MAKER_B, 105_000);
    fund<SUI>(&mut s, a2, MAKER_A, 50_000);
    fund<USDC>(&mut s, b2, MAKER_B, 105_000);

    let ord_a1 = ask(MAKER_A, a1, 50_000, 100_000, 1);
    let ord_b1 = bid(MAKER_B, b1, 105_000, 50_000, 2);
    let ord_a2 = ask(MAKER_A, a2, 50_000, 100_000, 3);
    let ord_b2 = bid(MAKER_B, b2, 105_000, 50_000, 4);

    s.next_tx(RELAYER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let (b_1, q_1, mf_1, tf_1) = {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let mut ma = s.take_shared_by_id<BalanceManager>(a1);
        let mut mb = s.take_shared_by_id<BalanceManager>(b1);
        settlement::match_orders_for_testing<SUI, USDC>(
            &mut reg, &wl, &mut ma, &mut mb, ord_a1, ord_b1, 50_000, &clk, s.ctx(),
        );
        ts::return_shared(mb);
        ts::return_shared(ma);
        ts::return_shared(reg);
        let evs = event::events_by_type<FillEvent>();
        assert!(evs.length() == 2, 100);
        let (_, _, b, q, mf, tf, _, _) = settlement::fill_event_fields(&evs[0]);
        (b, q, mf, tf)
    };

    s.next_tx(RELAYER);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let mut ma = s.take_shared_by_id<BalanceManager>(a2);
        let mut mb = s.take_shared_by_id<BalanceManager>(b2);
        let mut ob = settlement::begin_match_for_testing<SUI, USDC>(
            &mut reg, &wl, &ma, &mb, ord_a2, ord_b2, 50_000, &clk, s.ctx(),
        );
        settlement::provide_base_from_manager(&mut ob, &mut ma);
        settlement::provide_quote_from_manager(&mut ob, &mut mb);
        settlement::collect_quote_to_manager(&mut ob, &mut ma);
        settlement::collect_base_to_manager(&mut ob, &mut mb);
        settlement::finish(&mut reg, ob);

        // The obligation pair's managers land exactly where the classic
        // pair's did.
        let ma1 = s.take_shared_by_id<BalanceManager>(a1);
        let mb1 = s.take_shared_by_id<BalanceManager>(b1);
        assert!(bm::balance_of<SUI>(&ma) == bm::balance_of<SUI>(&ma1), 0);
        assert!(bm::balance_of<USDC>(&ma) == bm::balance_of<USDC>(&ma1), 1);
        assert!(bm::balance_of<SUI>(&mb) == bm::balance_of<SUI>(&mb1), 2);
        assert!(bm::balance_of<USDC>(&mb) == bm::balance_of<USDC>(&mb1), 3);
        ts::return_shared(mb1);
        ts::return_shared(ma1);
        ts::return_shared(mb);
        ts::return_shared(ma);
        ts::return_shared(reg);

        // 2 events per match, economics identical across the two
        // matches.
        let fills = event::events_by_type<FillEvent>();
        assert!(fills.length() == 2, 4);
        let (_, _, b_3, q_3, mf_3, tf_3, _, _) = settlement::fill_event_fields(&fills[0]);
        assert!(b_1 == b_3 && q_1 == q_3 && mf_1 == mf_3 && tf_1 == tf_3, 5);
    };

    whitelist::destroy_for_testing(wl);
    clk.destroy_for_testing();
    s.end();
}

// ═══════════════════ the external (OwnerCap-bound) leg ═══════════════════

#[test]
fun external_leg_settles_via_owner_cap_with_empty_manager() {
    let (mut s, clk) = setup();
    // Identity-only manager: owner is an object-ish address, escrow is
    // external (this is the vault shape, minus the vault).
    s.next_tx(MAKER_A);
    let (bm_id, cap) = bm::new_with_owner_cap(@0xF00D, s.ctx());
    let ord = ask(@0xF00D, bm_id, 50_000, 100_000, 1);

    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let m = s.take_shared_by_id<BalanceManager>(bm_id);
        let mut ob = settlement::begin_fill_for_testing<SUI, USDC>(
            &mut reg, &wl, &m, ord, 40_000, 0, &clk, s.ctx(),
        );
        // External provision (simulating the vault's quote session).
        settlement::provide_base(&mut ob, balance::create_for_testing<SUI>(20_000));
        settlement::provide_quote(&mut ob, balance::create_for_testing<USDC>(40_000));
        // Control proof: the OwnerCap collects the maker's due.
        let maker_due = settlement::collect_quote_with_cap(&mut ob, &cap);
        assert!(maker_due.value() == 40_000 - 40, 0);
        let taker_due = settlement::collect_base_bearer(&mut ob);
        assert!(taker_due.value() == 20_000 - 20, 1);
        settlement::finish(&mut reg, ob);

        // The identity manager held nothing throughout.
        assert!(bm::balance_of<SUI>(&m) == 0, 2);
        assert!(bm::balance_of<USDC>(&m) == 0, 3);
        assert!(registry::fee_vault_base_value(&reg) == 20, 4);
        assert!(registry::fee_vault_quote_value(&reg) == 40, 5);
        balance::destroy_for_testing(maker_due);
        balance::destroy_for_testing(taker_due);
        ts::return_shared(m);
        ts::return_shared(reg);
    };
    transfer::public_transfer(cap, MAKER_A);

    whitelist::destroy_for_testing(wl);
    clk.destroy_for_testing();
    s.end();
}

// ═══════════════════════ bindings and state machine ═══════════════════════

#[test, expected_failure(abort_code = settlement::EWrongEscrow)]
fun foreign_cap_cannot_collect() {
    let (mut s, clk) = setup();
    s.next_tx(MAKER_A);
    let (bm_id, cap) = bm::new_with_owner_cap(@0xF00D, s.ctx());
    let (_other_id, other_cap) = bm::new_with_owner_cap(@0xBEEF, s.ctx());
    let ord = ask(@0xF00D, bm_id, 50_000, 100_000, 1);

    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let m = s.take_shared_by_id<BalanceManager>(bm_id);
    let mut ob = settlement::begin_fill_for_testing<SUI, USDC>(
        &mut reg, &wl, &m, ord, 40_000, 0, &clk, s.ctx(),
    );
    settlement::provide_base(&mut ob, balance::create_for_testing<SUI>(20_000));
    settlement::provide_quote(&mut ob, balance::create_for_testing<USDC>(40_000));
    let stolen = settlement::collect_quote_with_cap(&mut ob, &other_cap);
    balance::destroy_for_testing(stolen);
    transfer::public_transfer(cap, MAKER_A);
    transfer::public_transfer(other_cap, MAKER_A);
    abort 99
}

#[test, expected_failure(abort_code = settlement::ELegAmountMismatch)]
fun short_provision_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 100_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, 1);

    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let m = s.take_shared_by_id<BalanceManager>(a);
    let mut ob = settlement::begin_fill_for_testing<SUI, USDC>(
        &mut reg, &wl, &m, ord, 40_000, 0, &clk, s.ctx(),
    );
    settlement::provide_base(&mut ob, balance::create_for_testing<SUI>(19_999));
    abort 99
}

#[test, expected_failure(abort_code = settlement::EAlreadyProvided)]
fun double_provision_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 100_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, 1);

    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut m = s.take_shared_by_id<BalanceManager>(a);
    let mut ob = settlement::begin_fill_for_testing<SUI, USDC>(
        &mut reg, &wl, &m, ord, 40_000, 0, &clk, s.ctx(),
    );
    settlement::provide_base_from_manager(&mut ob, &mut m);
    settlement::provide_base(&mut ob, balance::create_for_testing<SUI>(20_000));
    abort 99
}

#[test, expected_failure(abort_code = settlement::ENotProvided)]
fun collect_before_counterparty_provision_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 100_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, 1);

    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut m = s.take_shared_by_id<BalanceManager>(a);
    let mut ob = settlement::begin_fill_for_testing<SUI, USDC>(
        &mut reg, &wl, &m, ord, 40_000, 0, &clk, s.ctx(),
    );
    // The maker's due comes from the taker's (quote) pool — not funded.
    settlement::provide_base_from_manager(&mut ob, &mut m);
    settlement::collect_quote_to_manager(&mut ob, &mut m);
    abort 99
}

#[test, expected_failure(abort_code = settlement::EObligationIncomplete)]
fun finish_with_uncollected_leg_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 100_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, 1);

    s.next_tx(TAKER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut m = s.take_shared_by_id<BalanceManager>(a);
    let mut ob = settlement::begin_fill_for_testing<SUI, USDC>(
        &mut reg, &wl, &m, ord, 40_000, 0, &clk, s.ctx(),
    );
    settlement::provide_base_from_manager(&mut ob, &mut m);
    settlement::provide_quote(&mut ob, balance::create_for_testing<USDC>(40_000));
    settlement::collect_quote_to_manager(&mut ob, &mut m);
    // Taker never collects; finish must refuse.
    settlement::finish(&mut reg, ob);
    abort 99
}

#[test, expected_failure(abort_code = settlement::ESelfMatch)]
fun self_match_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 100_000);
    fund<USDC>(&mut s, a, MAKER_A, 105_000);
    let ord_a = ask(MAKER_A, a, 50_000, 100_000, 1);
    let ord_b = bid(MAKER_A, a, 105_000, 50_000, 2);

    s.next_tx(RELAYER);
    let wl = whitelist::new_open_for_testing(s.ctx());
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let m = s.take_shared_by_id<BalanceManager>(a);
    let _ob = settlement::begin_match_for_testing<SUI, USDC>(
        &mut reg, &wl, &m, &m, ord_a, ord_b, 50_000, &clk, s.ctx(),
    );
    abort 99
}
