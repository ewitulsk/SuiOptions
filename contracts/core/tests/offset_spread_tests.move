/// Tests for the mm-bot V2 protocol prerequisites: exact-offset closure
/// (calls + puts) and spread collateral compression (calls).
#[test_only]
module options_core::offset_spread_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::bucket::{Self, Bucket};
use options_core::position::{Self, Position};
use options_core::put_bucket::{Self, PutBucket};
use options_core::test_helpers::{Self as th, BTC, USDC, CALL, CALL2, PUT, PUT2};

const STRIKE: u128 = 6; // 6 USDC-units per BTC-unit
const LONG_STRIKE: u128 = 5;
const EXPIRY_MS: u64 = 1_000_000;

fun setup_call_bucket(scenario: &mut Scenario) {
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, 0);
}

/// The lower-strike, same-expiry bucket used as the spread's long leg.
fun setup_long_bucket(scenario: &mut Scenario) {
    th::new_bucket<BTC, USDC, CALL2>(scenario, EXPIRY_MS, LONG_STRIKE, 0);
}

fun self_write(
    scenario: &mut Scenario,
    b: &mut Bucket<BTC, USDC, CALL>,
    amount: u64,
    clock: &sui::clock::Clock,
): (Position, Coin<CALL>) {
    bucket::write_collateralized<BTC, USDC, CALL>(
        b,
        coin::mint_for_testing<BTC>(amount, scenario.ctx()),
        clock,
        scenario.ctx(),
    )
}

// ───────────────────────── exact-offset closure: calls ─────────────────────────

#[test]
fun test_close_offset_full_frees_all_collateral() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (mut pos, call) = self_write(&mut scenario, &mut b, 100, &clock);
    assert!(bucket::underlying_balance(&b) == 100, 0);
    assert!(bucket::call_supply(&b) == 100, 0);

    let freed = bucket::close_offset<BTC, USDC, CALL>(
        &mut b, &mut pos, call, &clock, scenario.ctx(),
    );
    assert!(freed.value() == 100, 0);
    assert!(bucket::underlying_balance(&b) == 0, 0);
    assert!(bucket::call_supply(&b) == 0, 0);
    assert!(bucket::closed_pending(&b) == 100, 0);
    assert!(position::amount(&pos) == 0, 0);
    position::destroy_empty(pos);

    coin::burn_for_testing(freed);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_close_offset_partial_then_exercise_skips_tombstone() {
    // A writes [0,50), B writes [50,100). B closes 30 → tombstone [70,100).
    // The remaining 70 coins all exercise: the cursor sweeps 0→70, jumps the
    // tombstone, and both redeems attribute exactly.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (pos_a, call_a) = self_write(&mut scenario, &mut b, 50, &clock);
    let (mut pos_b, mut call_b) = self_write(&mut scenario, &mut b, 50, &clock);

    let close_chunk = coin::split(&mut call_b, 30, scenario.ctx());
    let freed = bucket::close_offset<BTC, USDC, CALL>(
        &mut b, &mut pos_b, close_chunk, &clock, scenario.ctx(),
    );
    assert!(freed.value() == 30, 0);
    assert!(bucket::closed_pending(&b) == 30, 0);
    assert!(position::range_start(&pos_b) == 50 && position::range_end(&pos_b) == 70, 0);

    // Exercise all 70 outstanding coins (50 from A's write + B's kept 20).
    let mut all_calls = call_a;
    all_calls.join(call_b);
    let payment = coin::mint_for_testing<USDC>(70 * (STRIKE as u64), scenario.ctx());
    let exercised = bucket::exercise<BTC, USDC, CALL>(
        &mut b, all_calls, payment, &clock, scenario.ctx(),
    );
    assert!(exercised.value() == 70, 0);
    // The cursor jumped the tombstone to the end of written space.
    assert!(bucket::exercise_cursor(&b) == 100, 0);
    assert!(bucket::closed_pending(&b) == 0, 0);

    clock.set_for_testing(EXPIRY_MS + 1);
    let (ua, sa) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos_a, &clock, scenario.ctx());
    assert!(ua.value() == 0 && sa.value() == 50 * (STRIKE as u64), 0);
    let (ub, sb) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos_b, &clock, scenario.ctx());
    assert!(ub.value() == 0 && sb.value() == 20 * (STRIKE as u64), 0);
    // Fully drained: 100 in = 30 closed + 70 exercised out.
    assert!(bucket::underlying_balance(&b) == 0, 0);
    assert!(bucket::settlement_balance(&b) == 0, 0);

    coin::burn_for_testing(freed);
    coin::burn_for_testing(exercised);
    coin::burn_for_testing(ua);
    coin::burn_for_testing(sa);
    coin::burn_for_testing(ub);
    coin::burn_for_testing(sb);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_close_offset_adjacent_tombstones_merge_and_cursor_jumps() {
    // Close [70,100) then [40,70): intervals merge to [40,100); exercising
    // the remaining 40 sweeps the cursor straight to 100.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (mut pos, mut call) = self_write(&mut scenario, &mut b, 100, &clock);

    let c1 = coin::split(&mut call, 30, scenario.ctx());
    let f1 = bucket::close_offset<BTC, USDC, CALL>(&mut b, &mut pos, c1, &clock, scenario.ctx());
    let c2 = coin::split(&mut call, 30, scenario.ctx());
    let f2 = bucket::close_offset<BTC, USDC, CALL>(&mut b, &mut pos, c2, &clock, scenario.ctx());
    assert!(bucket::closed_pending(&b) == 60, 0);
    assert!(position::range_end(&pos) == 40, 0);

    let payment = coin::mint_for_testing<USDC>(40 * (STRIKE as u64), scenario.ctx());
    let exercised = bucket::exercise<BTC, USDC, CALL>(
        &mut b, call, payment, &clock, scenario.ctx(),
    );
    assert!(exercised.value() == 40, 0);
    assert!(bucket::exercise_cursor(&b) == 100, 0);
    assert!(bucket::closed_pending(&b) == 0, 0);
    assert!(bucket::underlying_balance(&b) == 0, 0);

    coin::burn_for_testing(f1);
    coin::burn_for_testing(f2);
    coin::burn_for_testing(exercised);
    transfer::public_transfer(pos, th::writer_addr());
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 61, location = options_core::bucket)] // close_exceeds_position
fun test_close_offset_more_than_position_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (mut pos_small, _call_small) = self_write(&mut scenario, &mut b, 10, &clock);
    let (pos_big, call_big) = self_write(&mut scenario, &mut b, 50, &clock);
    // 50 coins against the 10-unit position.
    let freed = bucket::close_offset<BTC, USDC, CALL>(
        &mut b, &mut pos_small, call_big, &clock, scenario.ctx(),
    );
    coin::burn_for_testing(freed);
    coin::burn_for_testing(_call_small);
    transfer::public_transfer(pos_small, th::writer_addr());
    transfer::public_transfer(pos_big, th::writer_addr());
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 62, location = options_core::bucket)] // close_range_exercised
fun test_close_offset_exercised_range_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (mut pos_a, mut call_a) = self_write(&mut scenario, &mut b, 100, &clock);
    // Second write for extra fungible coins ([100,200)).
    let (pos_b, call_b) = self_write(&mut scenario, &mut b, 100, &clock);

    // Exercise 50 → cursor 50; closing 60 of pos_a cuts at 40 < cursor.
    let chunk = coin::split(&mut call_a, 50, scenario.ctx());
    let payment = coin::mint_for_testing<USDC>(50 * (STRIKE as u64), scenario.ctx());
    let exercised = bucket::exercise<BTC, USDC, CALL>(
        &mut b, chunk, payment, &clock, scenario.ctx(),
    );
    coin::burn_for_testing(exercised);

    call_a.join(call_b);
    let chunk2 = coin::split(&mut call_a, 60, scenario.ctx());
    let freed = bucket::close_offset<BTC, USDC, CALL>(
        &mut b, &mut pos_a, chunk2, &clock, scenario.ctx(),
    );
    coin::burn_for_testing(freed);
    coin::burn_for_testing(call_a);
    transfer::public_transfer(pos_a, th::writer_addr());
    transfer::public_transfer(pos_b, th::writer_addr());
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 8, location = options_core::bucket)] // bucket_expired
fun test_close_offset_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (mut pos, call) = self_write(&mut scenario, &mut b, 10, &clock);
    clock.set_for_testing(EXPIRY_MS + 1);
    let freed = bucket::close_offset<BTC, USDC, CALL>(
        &mut b, &mut pos, call, &clock, scenario.ctx(),
    );
    coin::burn_for_testing(freed);
    transfer::public_transfer(pos, th::writer_addr());
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// ───────────────────────── exact-offset closure: puts ─────────────────────────

#[test]
fun test_put_close_offset_full_lifecycle_and_cleanup() {
    // Strike 6/unit: write 100 (collateral 600), close 40 (refund 240),
    // exercise 60 (payout 360), redeem [0,60) → 60 delivered underlying.
    // total_redeemed = 40 (closed) + 60 (redeemed) = total_written → cleanup.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let (mut pos, mut put) = put_bucket::write_collateralized<BTC, USDC, PUT>(
        &mut b,
        coin::mint_for_testing<USDC>(600, scenario.ctx()),
        100,
        &clock,
        scenario.ctx(),
    );
    assert!(put_bucket::settlement_balance(&b) == 600, 0);

    let close_chunk = coin::split(&mut put, 40, scenario.ctx());
    let refund = put_bucket::close_offset<BTC, USDC, PUT>(
        &mut b, &mut pos, close_chunk, &clock, scenario.ctx(),
    );
    assert!(refund.value() == 240, 0);
    assert!(put_bucket::closed_pending(&b) == 40, 0);
    assert!(put_bucket::total_redeemed(&b) == 40, 0);
    assert!(position::range_end(&pos) == 60, 0);

    let payout = put_bucket::exercise<BTC, USDC, PUT>(
        &mut b,
        put,
        coin::mint_for_testing<BTC>(60, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(payout.value() == 360, 0);
    // Cursor swept 0→60 and eagerly consumed the flush tombstone [60,100).
    assert!(put_bucket::exercise_cursor(&b) == 100, 0);
    assert!(put_bucket::closed_pending(&b) == 0, 0);

    clock.set_for_testing(EXPIRY_MS + 1);
    let (u, s) = put_bucket::redeem_position<BTC, USDC, PUT>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 60 && s.value() == 0, 0);
    assert!(put_bucket::total_redeemed(&b) == 100, 0);
    assert!(put_bucket::underlying_balance(&b) == 0, 0);
    assert!(put_bucket::settlement_balance(&b) == 0, 0);

    // The cleanup gate is reachable even though 40 units never redeem.
    ts::return_shared(b);
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let b2 = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    put_bucket::cleanup_bucket<BTC, USDC, PUT>(&cap, b2, &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);

    coin::burn_for_testing(refund);
    coin::burn_for_testing(payout);
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_put_close_offset_fractional_strike_rounds_down() {
    // Strike 0.6 (6, scale 1): write 25 → collateral ceil(15.0) = 15;
    // close 25 → refund floor(15.0) = 15. With 7: collateral ceil(4.2)=5,
    // close refund floor(4.2)=4, dust 1 stays for the admin sweep.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let (mut pos, put) = put_bucket::write_collateralized<BTC, USDC, PUT>(
        &mut b,
        coin::mint_for_testing<USDC>(5, scenario.ctx()), // ceil(7 × 0.6) = 5
        7,
        &clock,
        scenario.ctx(),
    );
    let refund = put_bucket::close_offset<BTC, USDC, PUT>(
        &mut b, &mut pos, put, &clock, scenario.ctx(),
    );
    assert!(refund.value() == 4, 0); // floor(7 × 0.6)
    assert!(put_bucket::settlement_balance(&b) == 1, 0); // solvency dust
    assert!(position::amount(&pos) == 0, 0);
    position::destroy_empty(pos);

    coin::burn_for_testing(refund);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// ─────────────────────── spread collateral compression ───────────────────────

/// Standard fixture: writer self-writes 100 in the long bucket (strike 5)
/// and compresses a 100-unit write in the short bucket (strike 6) against
/// those long coins + 500 exercise cash. Returns both positions and the
/// short coins (the long position stays with the writer's inventory).
fun setup_spread(
    scenario: &mut Scenario,
    clock: &sui::clock::Clock,
): (Position, Position, Coin<CALL>) {
    ts::next_tx(scenario, th::writer_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(scenario);
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);

    let (long_pos, long_call) = bucket::write_collateralized<BTC, USDC, CALL2>(
        &mut long_b,
        coin::mint_for_testing<BTC>(100, scenario.ctx()),
        clock,
        scenario.ctx(),
    );
    let (short_pos, short_call) = bucket::write_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b,
        &long_b,
        long_call,
        coin::mint_for_testing<USDC>(100 * (LONG_STRIKE as u64), scenario.ctx()),
        clock,
        scenario.ctx(),
    );
    assert!(bucket::underlying_balance(&short_b) == 0, 0);
    assert!(bucket::spread_count(&short_b) == 1, 0);
    assert!(bucket::call_supply(&short_b) == 100, 0);

    ts::return_shared(long_b);
    ts::return_shared(short_b);
    (long_pos, short_pos, short_call)
}

#[test]
fun test_spread_unwind_then_exercise_full_conservation() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);
    let (long_pos, short_pos, short_call) = setup_spread(&mut scenario, &clock);

    // Physicalize (permissionless — run as a stranger).
    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::unwind_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b, &mut long_b, 0, &clock, scenario.ctx(),
    );
    // The long leg was exercised: its pool swapped 100 underlying for 500
    // cash; the short pool now holds real underlying.
    assert!(bucket::underlying_balance(&short_b) == 100, 0);
    assert!(bucket::spread_count(&short_b) == 0, 0);
    assert!(bucket::exercise_cursor(&long_b) == 100, 0);
    assert!(bucket::settlement_balance(&long_b) == 100 * (LONG_STRIKE as u64), 0);

    // Now the short coins exercise like any covered write.
    let payment = coin::mint_for_testing<USDC>(100 * (STRIKE as u64), scenario.ctx());
    let exercised = bucket::exercise<BTC, USDC, CALL>(
        &mut short_b, short_call, payment, &clock, scenario.ctx(),
    );
    assert!(exercised.value() == 100, 0);

    // After expiry both positions redeem: the physicalized short position
    // was fully assigned (600 cash), the long position was fully exercised
    // by the unwind (500 cash).
    clock.set_for_testing(EXPIRY_MS + 1);
    let (us, ss) = bucket::redeem_position<BTC, USDC, CALL>(
        &mut short_b, short_pos, &clock, scenario.ctx(),
    );
    assert!(us.value() == 0 && ss.value() == 100 * (STRIKE as u64), 0);
    let (ul, sl) = bucket::redeem_position<BTC, USDC, CALL2>(
        &mut long_b, long_pos, &clock, scenario.ctx(),
    );
    assert!(ul.value() == 0 && sl.value() == 100 * (LONG_STRIKE as u64), 0);
    assert!(bucket::underlying_balance(&short_b) == 0, 0);
    assert!(bucket::settlement_balance(&short_b) == 0, 0);
    assert!(bucket::underlying_balance(&long_b) == 0, 0);
    assert!(bucket::settlement_balance(&long_b) == 0, 0);

    coin::burn_for_testing(exercised);
    coin::burn_for_testing(us);
    coin::burn_for_testing(ss);
    coin::burn_for_testing(ul);
    coin::burn_for_testing(sl);
    ts::return_shared(long_b);
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 63, location = options_core::bucket)] // spread_unwind_required
fun test_spread_exercise_without_unwind_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);
    let (long_pos, short_pos, short_call) = setup_spread(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::trader_addr());
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let payment = coin::mint_for_testing<USDC>(100 * (STRIKE as u64), scenario.ctx());
    let exercised = bucket::exercise<BTC, USDC, CALL>(
        &mut short_b, short_call, payment, &clock, scenario.ctx(),
    );
    coin::burn_for_testing(exercised);
    transfer::public_transfer(long_pos, th::writer_addr());
    transfer::public_transfer(short_pos, th::writer_addr());
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_spread_partial_exercise_below_spread_range_ok() {
    // A physical write in front of the spread range exercises fine; only
    // crossing INTO the spread range needs the unwind.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    // Physical write [0,40).
    let (phys_pos, phys_call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut short_b,
        coin::mint_for_testing<BTC>(40, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    // Spread write [40,140).
    let (long_pos, long_call) = bucket::write_collateralized<BTC, USDC, CALL2>(
        &mut long_b,
        coin::mint_for_testing<BTC>(100, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    let (spread_pos, spread_call) = bucket::write_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b,
        &long_b,
        long_call,
        coin::mint_for_testing<USDC>(100 * (LONG_STRIKE as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );

    // Exercising 40 stops exactly at the spread boundary.
    let payment = coin::mint_for_testing<USDC>(40 * (STRIKE as u64), scenario.ctx());
    let exercised = bucket::exercise<BTC, USDC, CALL>(
        &mut short_b, phys_call, payment, &clock, scenario.ctx(),
    );
    assert!(exercised.value() == 40, 0);
    assert!(bucket::exercise_cursor(&short_b) == 40, 0);

    coin::burn_for_testing(exercised);
    coin::burn_for_testing(spread_call);
    transfer::public_transfer(phys_pos, th::writer_addr());
    transfer::public_transfer(long_pos, th::writer_addr());
    transfer::public_transfer(spread_pos, th::writer_addr());
    ts::return_shared(long_b);
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_close_spread_returns_escrow_and_tombstones() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);
    let (long_pos, short_pos, short_call) = setup_spread(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (long_back, cash_back) = bucket::close_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b, short_pos, short_call, &clock, scenario.ctx(),
    );
    assert!(long_back.value() == 100, 0);
    assert!(cash_back.value() == 100 * (LONG_STRIKE as u64), 0);
    assert!(bucket::spread_count(&short_b) == 0, 0);
    assert!(bucket::closed_pending(&short_b) == 100, 0);
    assert!(bucket::call_supply(&short_b) == 0, 0);

    // The returned long coins are still live: exercise them directly.
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let exercised = bucket::exercise<BTC, USDC, CALL2>(
        &mut long_b,
        long_back,
        coin::mint_for_testing<USDC>(100 * (LONG_STRIKE as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(exercised.value() == 100, 0);

    coin::burn_for_testing(exercised);
    coin::burn_for_testing(cash_back);
    transfer::public_transfer(long_pos, th::writer_addr());
    ts::return_shared(long_b);
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_redeem_spread_position_after_expiry_returns_escrow() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);
    let (long_pos, short_pos, short_call) = setup_spread(&mut scenario, &clock);

    clock.set_for_testing(EXPIRY_MS + 1);
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (long_back, cash_back) = bucket::redeem_spread_position<BTC, USDC, CALL, CALL2>(
        &mut short_b, short_pos, &clock, scenario.ctx(),
    );
    assert!(long_back.value() == 100, 0);
    assert!(cash_back.value() == 100 * (LONG_STRIKE as u64), 0);
    assert!(bucket::spread_count(&short_b) == 0, 0);

    coin::burn_for_testing(long_back);
    coin::burn_for_testing(cash_back);
    coin::burn_for_testing(short_call);
    transfer::public_transfer(long_pos, th::writer_addr());
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 68, location = options_core::bucket)] // spread_position
fun test_redeem_position_on_spread_position_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);
    let (long_pos, short_pos, short_call) = setup_spread(&mut scenario, &clock);

    clock.set_for_testing(EXPIRY_MS + 1);
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC, CALL>(
        &mut short_b, short_pos, &clock, scenario.ctx(),
    );
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    coin::burn_for_testing(short_call);
    transfer::public_transfer(long_pos, th::writer_addr());
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 65, location = options_core::bucket)] // spread_strike_too_high
fun test_write_spread_long_strike_above_short_aborts() {
    // Long at 6 backing a write at 5: the long leg must be equal-or-lower.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, LONG_STRIKE, 0); // short @ 5
    th::new_bucket<BTC, USDC, CALL2>(&mut scenario, EXPIRY_MS, STRIKE, 0); // long @ 6

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (long_pos, long_call) = bucket::write_collateralized<BTC, USDC, CALL2>(
        &mut long_b,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    let (p, c) = bucket::write_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b,
        &long_b,
        long_call,
        coin::mint_for_testing<USDC>(10 * (STRIKE as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    coin::burn_for_testing(c);
    transfer::public_transfer(p, th::writer_addr());
    transfer::public_transfer(long_pos, th::writer_addr());
    ts::return_shared(long_b);
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 64, location = options_core::bucket)] // spread_expiry_mismatch
fun test_write_spread_long_expires_earlier_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario); // short expires at EXPIRY_MS
    th::new_bucket<BTC, USDC, CALL2>(&mut scenario, EXPIRY_MS - 1, LONG_STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (long_pos, long_call) = bucket::write_collateralized<BTC, USDC, CALL2>(
        &mut long_b,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    let (p, c) = bucket::write_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b,
        &long_b,
        long_call,
        coin::mint_for_testing<USDC>(10 * (LONG_STRIKE as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    coin::burn_for_testing(c);
    transfer::public_transfer(p, th::writer_addr());
    transfer::public_transfer(long_pos, th::writer_addr());
    ts::return_shared(long_b);
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 13, location = options_core::bucket)] // settlement_amount_mismatch
fun test_write_spread_wrong_cash_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut long_b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (long_pos, long_call) = bucket::write_collateralized<BTC, USDC, CALL2>(
        &mut long_b,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    let (p, c) = bucket::write_spread<BTC, USDC, CALL, CALL2>(
        &mut short_b,
        &long_b,
        long_call,
        coin::mint_for_testing<USDC>(10 * (LONG_STRIKE as u64) - 1, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    coin::burn_for_testing(c);
    transfer::public_transfer(p, th::writer_addr());
    transfer::public_transfer(long_pos, th::writer_addr());
    ts::return_shared(long_b);
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 68, location = options_core::bucket)] // spread_position
fun test_close_offset_on_spread_position_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_call_bucket(&mut scenario);
    setup_long_bucket(&mut scenario);
    let (long_pos, mut short_pos, short_call) = setup_spread(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut short_b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let freed = bucket::close_offset<BTC, USDC, CALL>(
        &mut short_b, &mut short_pos, short_call, &clock, scenario.ctx(),
    );
    coin::burn_for_testing(freed);
    transfer::public_transfer(long_pos, th::writer_addr());
    transfer::public_transfer(short_pos, th::writer_addr());
    ts::return_shared(short_b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// ───────────────────── put spread compression ─────────────────────

/// The long-put bucket (PUT2) used as the put spread's long leg.
fun setup_put_pair(scenario: &mut Scenario, short_strike: u128, long_strike: u128, scale: u8) {
    th::new_put_bucket<BTC, USDC, PUT>(scenario, EXPIRY_MS, short_strike, scale);
    th::new_put_bucket<BTC, USDC, PUT2>(scenario, EXPIRY_MS, long_strike, scale);
}

/// Writer self-writes `amount` longs physically, then compresses a
/// same-size short against them. Returns (spread position, short puts).
fun put_spread_write(
    scenario: &mut Scenario,
    short_b: &mut PutBucket<BTC, USDC, PUT>,
    long_b: &mut PutBucket<BTC, USDC, PUT2>,
    amount: u64,
    clock: &sui::clock::Clock,
): (Position, Coin<PUT>) {
    let long_collateral = put_bucket::required_collateral(long_b, amount);
    let (long_pos, long_coins) = put_bucket::write_collateralized<BTC, USDC, PUT2>(
        long_b,
        coin::mint_for_testing<USDC>(long_collateral, scenario.ctx()),
        amount,
        clock,
        scenario.ctx(),
    );
    transfer::public_transfer(long_pos, th::writer_addr());
    let top_up = put_bucket::required_spread_top_up(short_b, long_b, amount);
    put_bucket::write_spread<BTC, USDC, PUT, PUT2>(
        short_b,
        long_b,
        long_coins,
        coin::mint_for_testing<USDC>(top_up, scenario.ctx()),
        clock,
        scenario.ctx(),
    )
}

#[test]
fun test_put_spread_full_assignment_conserves() {
    // Ks 6 > Kl 5, scale 0: top_up = g(100)+1 = (600−500)+1 = 101.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_put_pair(&mut scenario, STRIKE, LONG_STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut sb = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let mut lb = ts::take_shared<PutBucket<BTC, USDC, PUT2>>(&scenario);
    let (pos, puts) = put_spread_write(&mut scenario, &mut sb, &mut lb, 100, &clock);
    // Short bucket pool cash untouched by the spread write.
    assert!(put_bucket::settlement_balance(&sb) == 0, 0);
    let (long_left, cash, _) =
        put_bucket::spread_escrow_view<BTC, USDC, PUT, PUT2>(&sb, 0, 100);
    assert!(long_left == 100 && cash == 101, 0);

    // Assign in two chunks: 40 then 60. Payouts telescope ceil(a·6).
    ts::next_tx(&mut scenario, th::trader_addr());
    let mut puts = puts;
    let first = coin::split(&mut puts, 40, scenario.ctx());
    let pay1 = put_bucket::exercise_spread<BTC, USDC, PUT, PUT2>(
        &mut sb,
        &mut lb,
        first,
        coin::mint_for_testing<BTC>(40, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(pay1.value() == 240, 0); // 40 × 6, exact at scale 0
    let pay2 = put_bucket::exercise_spread<BTC, USDC, PUT, PUT2>(
        &mut sb,
        &mut lb,
        puts,
        coin::mint_for_testing<BTC>(60, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(pay2.value() == 360, 0);
    assert!(put_bucket::exercise_cursor(&sb) == 100, 0);
    // The delivered underlying assigned the LONG bucket, not this one.
    assert!(put_bucket::underlying_balance(&sb) == 0, 0);
    assert!(put_bucket::underlying_balance(&lb) == 100, 0);
    // Only the 1-unit cushion is left in escrow.
    let (long_left, cash, _) =
        put_bucket::spread_escrow_view<BTC, USDC, PUT, PUT2>(&sb, 0, 100);
    assert!(long_left == 0 && cash == 1, 0);

    // Post-expiry: the writer redeems the residue; the range retires.
    clock.set_for_testing(EXPIRY_MS + 1);
    ts::next_tx(&mut scenario, th::writer_addr());
    let (long_back, cash_back) = put_bucket::redeem_spread_position<BTC, USDC, PUT, PUT2>(
        &mut sb,
        pos,
        &clock,
        scenario.ctx(),
    );
    assert!(long_back.value() == 0 && cash_back.value() == 1, 0);
    assert!(put_bucket::spread_count(&sb) == 0, 0);
    assert!(put_bucket::total_redeemed(&sb) == 100, 0);

    coin::burn_for_testing(pay1);
    coin::burn_for_testing(pay2);
    coin::burn_for_testing(long_back);
    coin::burn_for_testing(cash_back);
    ts::return_shared(sb);
    ts::return_shared(lb);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_put_spread_partial_assign_close_and_continue() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_put_pair(&mut scenario, STRIKE, LONG_STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut sb = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let mut lb = ts::take_shared<PutBucket<BTC, USDC, PUT2>>(&scenario);
    let (pos, mut puts) = put_spread_write(&mut scenario, &mut sb, &mut lb, 100, &clock);

    // Assign 30, then close the 70-unit remainder pre-expiry.
    ts::next_tx(&mut scenario, th::trader_addr());
    let chunk = coin::split(&mut puts, 30, scenario.ctx());
    let pay = put_bucket::exercise_spread<BTC, USDC, PUT, PUT2>(
        &mut sb,
        &mut lb,
        chunk,
        coin::mint_for_testing<BTC>(30, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(pay.value() == 180, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let (long_back, cash_back) = put_bucket::close_spread<BTC, USDC, PUT, PUT2>(
        &mut sb,
        pos,
        puts,
        &clock,
        scenario.ctx(),
    );
    // 70 longs back; cash back = 101 − 30 drawn = 71.
    assert!(long_back.value() == 70, 0);
    assert!(cash_back.value() == 71, 0);
    assert!(put_bucket::spread_count(&sb) == 0, 0);
    assert!(put_bucket::total_redeemed(&sb) == 100, 0);

    // The bucket keeps working: a physical write after the spread range
    // exercises across the tombstone.
    ts::next_tx(&mut scenario, th::writer_addr());
    let phys_collateral = put_bucket::required_collateral(&sb, 50);
    let (pos2, puts2) = put_bucket::write_collateralized<BTC, USDC, PUT>(
        &mut sb,
        coin::mint_for_testing<USDC>(phys_collateral, scenario.ctx()),
        50,
        &clock,
        scenario.ctx(),
    );
    ts::next_tx(&mut scenario, th::trader_addr());
    let pay2 = put_bucket::exercise<BTC, USDC, PUT>(
        &mut sb,
        puts2,
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(pay2.value() == 300, 0);
    // Cursor swept 30 → tombstone jump → through [100,150).
    assert!(put_bucket::exercise_cursor(&sb) == 150, 0);

    transfer::public_transfer(pos2, th::writer_addr());
    coin::burn_for_testing(pay);
    coin::burn_for_testing(pay2);
    coin::burn_for_testing(long_back);
    coin::burn_for_testing(cash_back);
    ts::return_shared(sb);
    ts::return_shared(lb);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_put_spread_fractional_strikes_stay_solvent() {
    // Ks 1.5 / Kl 1.3 (scale 1): adversarial rounding, unit-sized chunks.
    // top_up = g(7)+1 = ceil(10.5)−floor(9.1)+1 = 11−9+1 = 3.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_put_pair(&mut scenario, 15, 13, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut sb = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let mut lb = ts::take_shared<PutBucket<BTC, USDC, PUT2>>(&scenario);
    let (pos, mut puts) = put_spread_write(&mut scenario, &mut sb, &mut lb, 7, &clock);
    let (_, cash0, _) = put_bucket::spread_escrow_view<BTC, USDC, PUT, PUT2>(&sb, 0, 7);
    assert!(cash0 == 3, 0);

    // Assign 1, 2, 3, 1 — payouts telescope ceil(a×1.5): 2,3,5,1 = 11.
    ts::next_tx(&mut scenario, th::trader_addr());
    let mut total_paid = 0;
    let mut i = 0;
    let chunks = vector[1u64, 2, 3, 1];
    while (i < chunks.length()) {
        let n = chunks[i];
        let c = coin::split(&mut puts, n, scenario.ctx());
        let pay = put_bucket::exercise_spread<BTC, USDC, PUT, PUT2>(
            &mut sb,
            &mut lb,
            c,
            coin::mint_for_testing<BTC>(n, scenario.ctx()),
            &clock,
            scenario.ctx(),
        );
        total_paid = total_paid + pay.value();
        coin::burn_for_testing(pay);
        i = i + 1;
    };
    coin::destroy_zero(puts);
    // Per-chunk targets telescope ceil(a×1.5): 2,3,4,2. The long proceeds
    // Σfloor(n×1.3) = 7 leak 2 units vs the aggregate floor(9.1) = 9, one
    // more than the drawn cushion — the final chunk's clamp bites and the
    // exerciser bears exactly that unit: total 10, not 11.
    assert!(total_paid == 10, 0);
    let (long_left, cash_left, _) =
        put_bucket::spread_escrow_view<BTC, USDC, PUT, PUT2>(&sb, 0, 7);
    assert!(long_left == 0, 0);
    // Exact conservation: Σpay + escrow remainder == Σproceeds + top_up.
    assert!(total_paid + cash_left == 7 + 3, 0);
    assert!(cash_left == 0, 0);
    // The short pool never funded a spread payout.
    assert!(put_bucket::settlement_balance(&sb) == 0, 0);

    transfer::public_transfer(pos, th::writer_addr());
    ts::return_shared(sb);
    ts::return_shared(lb);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_put_spread_higher_long_strike_surplus_to_writer() {
    // Kl 6 > Ks 5: fully covered, top_up = 0+1 = 1; every assignment's
    // long surplus accrues to the writer's escrow.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_put_pair(&mut scenario, LONG_STRIKE, STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut sb = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let mut lb = ts::take_shared<PutBucket<BTC, USDC, PUT2>>(&scenario);
    let (pos, puts) = put_spread_write(&mut scenario, &mut sb, &mut lb, 10, &clock);

    ts::next_tx(&mut scenario, th::trader_addr());
    let pay = put_bucket::exercise_spread<BTC, USDC, PUT, PUT2>(
        &mut sb,
        &mut lb,
        puts,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    // Exerciser gets the SHORT strike (50), not the long proceeds (60).
    assert!(pay.value() == 50, 0);
    let (_, cash_left, _) = put_bucket::spread_escrow_view<BTC, USDC, PUT, PUT2>(&sb, 0, 10);
    // Writer's escrow: 1 cushion + 10 surplus.
    assert!(cash_left == 11, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let (long_back, cash_back) = put_bucket::close_spread<BTC, USDC, PUT, PUT2>(
        &mut sb,
        pos,
        coin::zero<PUT>(scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(long_back.value() == 0 && cash_back.value() == 11, 0);

    coin::burn_for_testing(pay);
    coin::burn_for_testing(long_back);
    coin::burn_for_testing(cash_back);
    ts::return_shared(sb);
    ts::return_shared(lb);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 69, location = options_core::put_bucket)]
fun test_plain_put_exercise_refuses_spread_range() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_put_pair(&mut scenario, STRIKE, LONG_STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut sb = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let mut lb = ts::take_shared<PutBucket<BTC, USDC, PUT2>>(&scenario);
    let (_pos, puts) = put_spread_write(&mut scenario, &mut sb, &mut lb, 10, &clock);

    ts::next_tx(&mut scenario, th::trader_addr());
    let _pay = put_bucket::exercise<BTC, USDC, PUT>(
        &mut sb,
        puts,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    abort 99
}

#[test]
#[expected_failure(abort_code = 70, location = options_core::put_bucket)]
fun test_put_spread_exercise_before_cursor_reaches_it_aborts() {
    // A physical write sits ahead of the spread range in the queue.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_put_pair(&mut scenario, STRIKE, LONG_STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut sb = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let mut lb = ts::take_shared<PutBucket<BTC, USDC, PUT2>>(&scenario);
    let phys_collateral = put_bucket::required_collateral(&sb, 20);
    let (pos0, puts0) = put_bucket::write_collateralized<BTC, USDC, PUT>(
        &mut sb,
        coin::mint_for_testing<USDC>(phys_collateral, scenario.ctx()),
        20,
        &clock,
        scenario.ctx(),
    );
    transfer::public_transfer(pos0, th::writer_addr());
    let (_pos, spread_puts) = put_spread_write(&mut scenario, &mut sb, &mut lb, 10, &clock);
    transfer::public_transfer(puts0, th::writer_addr());

    ts::next_tx(&mut scenario, th::trader_addr());
    let _pay = put_bucket::exercise_spread<BTC, USDC, PUT, PUT2>(
        &mut sb,
        &mut lb,
        spread_puts,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    abort 99
}

#[test]
#[expected_failure(abort_code = 68, location = options_core::put_bucket)]
fun test_put_spread_position_rejected_by_plain_redeem() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_put_pair(&mut scenario, STRIKE, LONG_STRIKE, 0);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut sb = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let mut lb = ts::take_shared<PutBucket<BTC, USDC, PUT2>>(&scenario);
    let (pos, puts) = put_spread_write(&mut scenario, &mut sb, &mut lb, 10, &clock);
    transfer::public_transfer(puts, th::writer_addr());

    clock.set_for_testing(EXPIRY_MS + 1);
    ts::next_tx(&mut scenario, th::writer_addr());
    let (_u, _s) = put_bucket::redeem_position<BTC, USDC, PUT>(
        &mut sb,
        pos,
        &clock,
        scenario.ctx(),
    );
    abort 99
}
