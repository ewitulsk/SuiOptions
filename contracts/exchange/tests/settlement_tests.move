/// Settlement logic tests (fills, matching, cancellation, fees, gc, admin).
///
/// These use the `*_for_testing` wrappers, which run the identical code path
/// with signature verification disabled — Move tests cannot mint wallet
/// signatures over scenario-generated object IDs. The signature layer itself
/// (hashing, intent wrapping, both schemes, delegated signers) is covered by
/// conformance_tests.move against Rust-generated wallet-format vectors.
#[test_only]
module exchange::settlement_tests;

use sui::clock::{Self, Clock};
use sui::coin::{Self, Coin};
use sui::sui::SUI;
use sui::test_scenario as ts;
use exchange::admin;
use exchange::balance_manager::{Self as bm, BalanceManager};
use exchange::fees;
use exchange::order;
use exchange::registry::{Self, SettlementRegistry};
use exchange::settlement;

/// Test quote coin.
public struct USDC has drop {}

const ADMIN: address = @0xAD;
const MAKER_A: address = @0xA1;
const MAKER_B: address = @0xB1;
const TAKER: address = @0xC1;
const RELAYER: address = @0xD1;

const NOW: u64 = 1_000_000;
const EXPIRY: u64 = 1_060_000;
const FEE_BPS: u64 = 10;

// === Helpers ===

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
    let mut m = s.take_shared_by_id<BalanceManager>(id);
    bm::deposit(&mut m, coin::mint_for_testing<T>(amount, s.ctx()), s.ctx());
    ts::return_shared(m);
}

/// Maker sells `maker_amount` SUI (base) for `taker_amount` USDC (quote).
fun ask(
    maker: address,
    bm_id: ID,
    maker_amount: u64,
    taker_amount: u64,
    taker: address,
    sender: address,
    salt: u64,
): vector<u8> {
    order::to_bytes(&order::new_for_testing(
        order::canonical_type<SUI>(),
        order::canonical_type<USDC>(),
        maker_amount,
        taker_amount,
        FEE_BPS,
        maker,
        bm_id,
        taker,
        sender,
        EXPIRY,
        salt,
    ))
}

/// Maker sells `maker_amount` USDC (quote) for `taker_amount` SUI (base).
fun bid(
    maker: address,
    bm_id: ID,
    maker_amount: u64,
    taker_amount: u64,
    taker: address,
    sender: address,
    salt: u64,
): vector<u8> {
    order::to_bytes(&order::new_for_testing(
        order::canonical_type<USDC>(),
        order::canonical_type<SUI>(),
        maker_amount,
        taker_amount,
        FEE_BPS,
        maker,
        bm_id,
        taker,
        sender,
        EXPIRY,
        salt,
    ))
}

fun fill(
    s: &mut ts::Scenario,
    bm_id: ID,
    order_bytes: vector<u8>,
    quote_in: u64,
    taker_fill_amount: u64,
    min_out: u64,
    clk: &Clock,
): (u64, u64) {
    s.next_tx(TAKER);
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut m = s.take_shared_by_id<BalanceManager>(bm_id);
    let pay = coin::mint_for_testing<USDC>(quote_in, s.ctx());
    let (got, change) = settlement::fill_limit_order_for_testing(
        &mut reg, &mut m, order_bytes, pay, taker_fill_amount, min_out, clk, s.ctx(),
    );
    let got_value = got.value();
    let change_value = change.value();
    coin::burn_for_testing(got);
    coin::burn_for_testing(change);
    ts::return_shared(m);
    ts::return_shared(reg);
    (got_value, change_value)
}

// === Open-orderbook fills (Path A) ===

#[test]
fun partial_fills_with_fees() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    // sell 50k SUI at 2 USDC/SUI
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);

    // fill 40k quote -> 20k base gross, fees 10bps: maker 40 quote, taker 20 base
    let (got, change) = fill(&mut s, a, ord, 100_000, 40_000, 19_980, &clk);
    assert!(got == 19_980, 0);
    assert!(change == 60_000, 1);

    s.next_tx(TAKER);
    {
        let reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let m = s.take_shared_by_id<BalanceManager>(a);
        assert!(bm::balance_of<SUI>(&m) == 30_000, 2);
        assert!(bm::balance_of<USDC>(&m) == 39_960, 3);
        assert!(registry::fee_vault_base_value(&reg) == 20, 4);
        assert!(registry::fee_vault_quote_value(&reg) == 40, 5);
        ts::return_shared(m);
        ts::return_shared(reg);
    };

    // ask for more than remains: capped to the remaining 60k quote
    let (got2, change2) = fill(&mut s, a, ord, 70_000, 100_000, 0, &clk);
    assert!(got2 == 29_970, 6);
    assert!(change2 == 10_000, 7);

    s.next_tx(TAKER);
    {
        let m = s.take_shared_by_id<BalanceManager>(a);
        assert!(bm::balance_of<SUI>(&m) == 0, 8);
        assert!(bm::balance_of<USDC>(&m) == 99_900, 9);
        ts::return_shared(m);
    };
    clk.destroy_for_testing();
    s.end();
}

#[test]
fun reverse_fill() {
    let (mut s, clk) = setup();
    let b = new_bm(&mut s, MAKER_B);
    fund<USDC>(&mut s, b, MAKER_B, 100_000);
    // maker sells 100k USDC for 50k SUI (2 USDC/SUI)
    let ord = bid(MAKER_B, b, 100_000, 50_000, @0x0, @0x0, 1);

    s.next_tx(TAKER);
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut m = s.take_shared_by_id<BalanceManager>(b);
    let pay = coin::mint_for_testing<SUI>(10_000, s.ctx());
    let (got, change) = settlement::fill_limit_order_reverse_for_testing(
        &mut reg, &mut m, ord, pay, 10_000, 19_980, &clk, s.ctx(),
    );
    // 10k base -> 20k quote gross; taker fee 10bps of 20k = 20
    assert!(got.value() == 19_980, 0);
    assert!(change.value() == 0, 1);
    // maker credited base net of 10bps maker fee (10)
    assert!(bm::balance_of<SUI>(&m) == 9_990, 2);
    assert!(bm::balance_of<USDC>(&m) == 80_000, 3);
    assert!(registry::fee_vault_base_value(&reg) == 10, 4);
    assert!(registry::fee_vault_quote_value(&reg) == 20, 5);
    coin::burn_for_testing(got);
    coin::burn_for_testing(change);
    ts::return_shared(m);
    ts::return_shared(reg);
    clk.destroy_for_testing();
    s.end();
}

#[test, expected_failure(abort_code = exchange::registry::EOverfill)]
fun cannot_overfill_via_match_after_fills() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let b = new_bm(&mut s, MAKER_B);
    fund<USDC>(&mut s, b, MAKER_B, 200_000);
    let oa = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    // b happy to buy 60k base at 2.0 — but a only has 50k
    let ob = bid(MAKER_B, b, 120_000, 60_000, @0x0, @0x0, 2);

    s.next_tx(RELAYER);
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut ma = s.take_shared_by_id<BalanceManager>(a);
    let mut mb = s.take_shared_by_id<BalanceManager>(b);
    // 60k base at price 2.0 = 120k quote > a's 100k taker_amount cap
    settlement::match_orders_for_testing(
        &mut reg, &mut ma, &mut mb, oa, ob, 60_000, &clk, s.ctx(),
    );
    abort 99
}

#[test, expected_failure(abort_code = settlement::EExpired)]
fun expired_order_rejected() {
    let (mut s, mut clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    clk.set_for_testing(EXPIRY); // expiry_ms is exclusive
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::EPaused)]
fun paused_market_rejects_fills() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    s.next_tx(ADMIN);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let cap = admin::mint_for_testing(s.ctx());
        registry::set_paused(&cap, &mut reg, true);
        admin::burn_for_testing(cap);
        ts::return_shared(reg);
    };
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::ETokenMismatch)]
fun wrong_orientation_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<USDC>(&mut s, a, MAKER_A, 100_000);
    // a bid (maker sells quote) pushed through the ask-side entry point
    let ord = bid(MAKER_A, a, 100_000, 50_000, @0x0, @0x0, 1);
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::EBadManager)]
fun wrong_manager_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    let b = new_bm(&mut s, MAKER_B);
    fund<SUI>(&mut s, b, MAKER_B, 50_000);
    // order pins a's manager but the fill passes b's
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    fill(&mut s, b, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::ETakerRestricted)]
fun taker_restriction_enforced() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, /*taker*/ MAKER_B, @0x0, 1);
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk); // TAKER != MAKER_B
    abort 99
}

#[test, expected_failure(abort_code = settlement::ESenderRestricted)]
fun sender_pinning_enforced() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    // pinned to the relayer: third parties cannot race it (§7.6)
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, /*sender*/ RELAYER, 1);
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::ESlippage)]
fun min_out_enforced_net_of_fee() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    // gross 20_000 but net 19_980 < 19_981
    fill(&mut s, a, ord, 100_000, 40_000, 19_981, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::EZeroFill)]
fun zero_fill_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    fill(&mut s, a, ord, 100_000, 0, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = exchange::balance_manager::EInsufficientEscrow)]
fun overcommitted_maker_fails_late() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 10_000); // escrow < signed maker_amount
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk); // needs 20k base, has 10k
    abort 99
}

// === Cancellation (§4.7) ===

#[test, expected_failure(abort_code = settlement::ECancelled)]
fun cancelled_order_cannot_fill() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    s.next_tx(MAKER_A);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        settlement::cancel(&mut reg, ord, s.ctx());
        ts::return_shared(reg);
    };
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::ENotMaker)]
fun only_maker_can_cancel() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    s.next_tx(TAKER);
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    settlement::cancel(&mut reg, ord, s.ctx());
    clk.destroy_for_testing();
    abort 99
}

#[test, expected_failure(abort_code = settlement::ESaltVoided)]
fun salt_watermark_bulk_cancel() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 5);
    s.next_tx(MAKER_A);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        settlement::cancel_up_to(&mut reg, 5, s.ctx()); // voids salts <= 5
        ts::return_shared(reg);
    };
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test]
fun higher_salt_survives_watermark() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 6);
    s.next_tx(MAKER_A);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        settlement::cancel_up_to(&mut reg, 5, s.ctx());
        ts::return_shared(reg);
    };
    let (got, _) = fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    assert!(got == 19_980, 0);
    clk.destroy_for_testing();
    s.end();
}

#[test, expected_failure(abort_code = settlement::ESaltVoided)]
fun signer_watermark_cancel_voids_cap_owner_orders() {
    // Cap-owned manager: the owner address never signs transactions, so
    // an approved signer raises the owner's watermark instead.
    let (mut s, clk) = setup();
    s.next_tx(MAKER_A);
    let (bm_id, cap) = bm::new_with_owner_cap(@0xF00D, s.ctx());
    s.next_tx(MAKER_A);
    {
        let mut m = s.take_shared_by_id<BalanceManager>(bm_id);
        bm::deposit_with_cap(&mut m, &cap, coin::mint_for_testing<SUI>(50_000, s.ctx()));
        bm::add_signer_with_cap(&mut m, &cap, MAKER_B);
        ts::return_shared(m);
    };
    let ord = ask(@0xF00D, bm_id, 50_000, 100_000, @0x0, @0x0, 5);
    s.next_tx(MAKER_B); // the signer, not the owner
    {
        let m = s.take_shared_by_id<BalanceManager>(bm_id);
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        settlement::cancel_up_to_for_manager(&mut reg, &m, 5, s.ctx());
        ts::return_shared(reg);
        ts::return_shared(m);
    };
    transfer::public_transfer(cap, MAKER_A);
    fill(&mut s, bm_id, ord, 100_000, 40_000, 0, &clk);
    abort 99
}

#[test, expected_failure(abort_code = settlement::ENotMaker)]
fun non_signer_cannot_watermark_cancel_for_manager() {
    let (mut s, _clk) = setup();
    s.next_tx(MAKER_A);
    let (bm_id, cap) = bm::new_with_owner_cap(@0xF00D, s.ctx());
    s.next_tx(TAKER);
    let m = s.take_shared_by_id<BalanceManager>(bm_id);
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    settlement::cancel_up_to_for_manager(&mut reg, &m, 5, s.ctx());
    transfer::public_transfer(cap, MAKER_A);
    abort 99
}

#[test, expected_failure(abort_code = settlement::EWatermarkRegression)]
fun watermark_is_monotonic() {
    let (mut s, clk) = setup();
    s.next_tx(MAKER_A);
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    settlement::cancel_up_to(&mut reg, 10, s.ctx());
    settlement::cancel_up_to(&mut reg, 9, s.ctx());
    clk.destroy_for_testing();
    abort 99
}

// === Matched settlement (Path B) ===

#[test]
fun match_at_resting_price_with_improvement() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let b = new_bm(&mut s, MAKER_B);
    fund<USDC>(&mut s, b, MAKER_B, 63_000);

    // resting: A sells 50k SUI at 2.0 USDC/SUI (salt 1)
    let oa = ask(MAKER_A, a, 50_000, 100_000, @0x0, RELAYER, 1);
    // incoming: B pays up to 2.1 (63k USDC for 30k SUI, salt 2)
    let ob = bid(MAKER_B, b, 63_000, 30_000, @0x0, RELAYER, 2);

    s.next_tx(RELAYER);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let mut ma = s.take_shared_by_id<BalanceManager>(a);
        let mut mb = s.take_shared_by_id<BalanceManager>(b);
        settlement::match_orders_for_testing(
            &mut reg, &mut ma, &mut mb, oa, ob, 30_000, &clk, s.ctx(),
        );
        // executes at the RESTING price 2.0, not B's 2.1: q = 60_000.
        // fees 10bps: A pays 60 quote, B pays 30 base.
        assert!(bm::balance_of<SUI>(&ma) == 20_000, 0);
        assert!(bm::balance_of<USDC>(&ma) == 59_940, 1);
        assert!(bm::balance_of<USDC>(&mb) == 3_000, 2);
        assert!(bm::balance_of<SUI>(&mb) == 29_970, 3);
        assert!(registry::fee_vault_base_value(&reg) == 30, 4);
        assert!(registry::fee_vault_quote_value(&reg) == 60, 5);
        // fill accounting: A in quote units, B in base units
        let da = order::digest(&order::from_bytes(oa), object::id(&reg));
        let db = order::digest(&order::from_bytes(ob), object::id(&reg));
        assert!(registry::filled(&reg, &da) == 60_000, 6);
        assert!(registry::filled(&reg, &db) == 30_000, 7);
        ts::return_shared(mb);
        ts::return_shared(ma);
        ts::return_shared(reg);
    };
    clk.destroy_for_testing();
    s.end();
}

#[test, expected_failure(abort_code = settlement::ENotCrossing)]
fun non_crossing_orders_rejected() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let b = new_bm(&mut s, MAKER_B);
    fund<USDC>(&mut s, b, MAKER_B, 57_000);
    // A asks 2.0; B bids only 1.9 (57k for 30k)
    let oa = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    let ob = bid(MAKER_B, b, 57_000, 30_000, @0x0, @0x0, 2);
    s.next_tx(RELAYER);
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut ma = s.take_shared_by_id<BalanceManager>(a);
    let mut mb = s.take_shared_by_id<BalanceManager>(b);
    settlement::match_orders_for_testing(
        &mut reg, &mut ma, &mut mb, oa, ob, 30_000, &clk, s.ctx(),
    );
    abort 99
}

#[test, expected_failure(abort_code = settlement::ESenderRestricted)]
fun matched_orders_pinned_to_relayer() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let b = new_bm(&mut s, MAKER_B);
    fund<USDC>(&mut s, b, MAKER_B, 63_000);
    let oa = ask(MAKER_A, a, 50_000, 100_000, @0x0, RELAYER, 1);
    let ob = bid(MAKER_B, b, 63_000, 30_000, @0x0, RELAYER, 2);
    s.next_tx(TAKER); // not the pinned relayer
    let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
    let mut ma = s.take_shared_by_id<BalanceManager>(a);
    let mut mb = s.take_shared_by_id<BalanceManager>(b);
    settlement::match_orders_for_testing(
        &mut reg, &mut ma, &mut mb, oa, ob, 30_000, &clk, s.ctx(),
    );
    abort 99
}

// === GC (§7.8) ===

#[test]
fun gc_reclaims_expired_fill_state() {
    let (mut s, mut clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);

    s.next_tx(TAKER);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let d = order::digest(&order::from_bytes(ord), object::id(&reg));
        assert!(registry::filled(&reg, &d) == 40_000, 0);

        // too early: expiry + grace not reached -> no-op
        registry::gc(&mut reg, vector[d], &clk);
        assert!(registry::filled(&reg, &d) == 40_000, 1);

        clk.set_for_testing(EXPIRY + 7 * 24 * 60 * 60 * 1000);
        registry::gc(&mut reg, vector[d], &clk);
        assert!(registry::filled(&reg, &d) == 0, 2);
        ts::return_shared(reg);
    };
    clk.destroy_for_testing();
    s.end();
}

// === Fees & admin ===

#[test]
fun fee_capped_by_signed_ceiling() {
    // market fee 50bps but the order signed max 10bps -> 10 applies
    let mut s = ts::begin(ADMIN);
    let cap = admin::mint_for_testing(s.ctx());
    registry::create_market<SUI, USDC>(&cap, 1, 1, 50, s.ctx());
    admin::burn_for_testing(cap);
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(NOW);

    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1); // max_fee_bps = 10
    let (got, _) = fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    assert!(got == 19_980, 0); // 10bps, not 50
    clk.destroy_for_testing();
    s.end();
}

#[test]
fun fee_tier_override_applies() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    s.next_tx(ADMIN);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let cap = admin::mint_for_testing(s.ctx());
        registry::set_fee_tier(&cap, &mut reg, TAKER, 0); // vip taker
        admin::burn_for_testing(cap);
        ts::return_shared(reg);
    };
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    let (got, _) = fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);
    assert!(got == 20_000, 0); // taker fee 0; maker still pays 10bps
    s.next_tx(TAKER);
    {
        let m = s.take_shared_by_id<BalanceManager>(a);
        assert!(bm::balance_of<USDC>(&m) == 39_960, 1);
        ts::return_shared(m);
    };
    clk.destroy_for_testing();
    s.end();
}

#[test]
fun sweep_fee_vaults() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    let ord = ask(MAKER_A, a, 50_000, 100_000, @0x0, @0x0, 1);
    fill(&mut s, a, ord, 100_000, 40_000, 0, &clk);

    s.next_tx(ADMIN);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let cap = admin::mint_for_testing(s.ctx());
        let cb: Coin<SUI> = fees::sweep_base(&cap, &mut reg, s.ctx());
        let cq: Coin<USDC> = fees::sweep_quote(&cap, &mut reg, s.ctx());
        assert!(cb.value() == 20, 0);
        assert!(cq.value() == 40, 1);
        assert!(registry::fee_vault_base_value(&reg) == 0, 2);
        assert!(registry::fee_vault_quote_value(&reg) == 0, 3);
        coin::burn_for_testing(cb);
        coin::burn_for_testing(cq);
        admin::burn_for_testing(cap);
        ts::return_shared(reg);
    };
    clk.destroy_for_testing();
    s.end();
}

#[test, expected_failure(abort_code = exchange::registry::EFeeTooHigh)]
fun fee_ceiling_hard_coded() {
    let mut s = ts::begin(ADMIN);
    let cap = admin::mint_for_testing(s.ctx());
    registry::create_market<SUI, USDC>(&cap, 1, 1, 51, s.ctx());
    abort 99
}

#[test]
fun pause_never_blocks_withdrawal() {
    let (mut s, clk) = setup();
    let a = new_bm(&mut s, MAKER_A);
    fund<SUI>(&mut s, a, MAKER_A, 50_000);
    s.next_tx(ADMIN);
    {
        let mut reg = s.take_shared<SettlementRegistry<SUI, USDC>>();
        let cap = admin::mint_for_testing(s.ctx());
        registry::set_paused(&cap, &mut reg, true);
        admin::burn_for_testing(cap);
        ts::return_shared(reg);
    };
    // users can always exit escrow, paused or not (§7.10)
    s.next_tx(MAKER_A);
    {
        let mut m = s.take_shared_by_id<BalanceManager>(a);
        let c = bm::withdraw<SUI>(&mut m, 50_000, s.ctx());
        assert!(c.value() == 50_000, 0);
        coin::burn_for_testing(c);
        ts::return_shared(m);
    };
    clk.destroy_for_testing();
    s.end();
}
