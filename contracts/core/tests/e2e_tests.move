/// End-to-end lifecycle tests, one per RFQ flow.
///
/// Each test threads the full protocol path: bucket creation → two writes
/// (so we exercise the cursor's FIFO assignment across positions) → partial
/// exercise → expiry → both redemptions → burn of remaining (unexercised)
/// call tokens → admin bucket cleanup → admin treasury withdraw. The
/// accumulated fee, coin inventories, bucket balances, and cursor state are
/// asserted after every step, so conservation across the whole lifecycle is
/// verified — not just point invariants.
///
/// The MM side of each write is the two-step collateral protocol: a
/// `request_*_flow` potato sized by the quote, fulfilled here with an
/// inline-minted `Balance` standing in for the external release
/// implementation (mm_collateral & co. live outside core).
///
/// The call option is a per-bucket fungible `Coin<CALL>`: split/join are coin
/// ops, and the bucket's coin supply tracks outstanding option amount.
#[test_only]
module options_core::e2e_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin;
use options_core::bucket::{Self, Bucket};
use options_core::position::{Self, Position};
use options_core::quote;
use options_core::treasury;
use options_core::test_helpers::{Self as th, BTC, USDC, CALL};

const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;
const EXPIRY_MS: u64 = 1_000_000;
const FEE_BPS: u64 = 50; // 0.5%

fun bps(x: u64): u64 {
    (((x as u128) * (FEE_BPS as u128)) / 10_000) as u64
}

/// Set fee to 50 bps and create a single BTC/USDC bucket at STRIKE.
fun setup_protocol_with_bucket(scenario: &mut Scenario) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let mut config = th::take_config(scenario);
    admin::set_fee_bps(&cap, &mut config, FEE_BPS);
    th::return_admin_cap(scenario, cap);
    ts::return_shared(config);
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
}

// ---------------------------------------------------------------------------
// Writer flow E2E: retail writers sell covered calls to a Trader MM.
// ---------------------------------------------------------------------------
//
// Cast
//   trader_mm_addr  — Trader MM (signs quotes; receives call tokens)
//   writer_addr     — Retail writer #1 (first writer; range [0,100))
//   writer_mm_addr   — Retail writer #2 (second writer; range [100,150))
//
// Numbers
//   write1: 100 BTC for premium 5_000_000 → fee 25_000, net 4_975_000
//   write2:  50 BTC for premium 3_000_000 → fee 15_000, net 2_985_000
//   exercise 120 by Trader MM:
//     writer #1 → exercised 100 / unexercised 0  → 0 BTC + (((100 as u128) * STRIKE) as u64) USDC
//     writer #2 → exercised  20 / unexercised 30 → 30 BTC +  (((20 as u128) * STRIKE) as u64) USDC
//   trader MM burns leftover 30 call tokens (unexercised at expiry).

#[test]
fun test_e2e_writer_flow_full_lifecycle() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_protocol_with_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    // ---- Write #1: writer_addr writes 100 BTC. Premium 5_000_000, nonce 1.
    let write1_amount: u64 = 100;
    let write1_premium: u64 = 5_000_000;
    let write1_fee = bps(write1_premium);
    let write1_net = write1_premium - write1_fee;
    let bucket_id;
    {
        ts::next_tx(&mut scenario, th::writer_addr());
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let config = th::take_config(&scenario);
        let wl = th::take_whitelist(&scenario);
        let mut treas = th::take_treasury(&scenario);
        let mut signer = th::take_signer(&scenario);
        bucket_id = object::id(&b);

        let q = th::new_test_quote(
            *admin::protocol_id(&config),
            object::id(&signer),
            th::trader_mm_addr(),
            &b,
            write1_amount,
            write1_premium,
            EXPIRY_MS,
            1,
        );
        let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
            &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
        );
        bucket::execute_writer_flow<BTC, USDC, CALL>(
            &mut b,
            &config,
            &wl,
            &mut treas,
            req,
            coin::mint_for_testing<USDC>(write1_premium, scenario.ctx()).into_balance(),
            coin::mint_for_testing<BTC>(write1_amount, scenario.ctx()),
            th::writer_addr(),
            &clock,
            scenario.ctx(),
        );

        assert!(bucket::total_written(&b) == 100, 0);
        assert!(bucket::underlying_balance(&b) == 100, 0);
        assert!(bucket::exercise_cursor(&b) == 0, 0);
        assert!(bucket::call_supply(&b) == 100, 0);
        assert!(treasury::balance_of<USDC>(&treas) == write1_fee, 0);

        ts::return_shared(b);
        ts::return_shared(config);
        ts::return_shared(wl);
        ts::return_shared(treas);
        ts::return_shared(signer);
    };

    // Writer #1 receives net premium + position Object.
    ts::next_tx(&mut scenario, th::writer_addr());
    {
        let net = ts::take_from_sender<Coin<USDC>>(&scenario);
        assert!(net.value() == write1_net, 0);
        coin::burn_for_testing(net);
        let p = ts::take_from_sender<Position>(&scenario);
        assert!(position::range_start(&p) == 0 && position::range_end(&p) == 100, 0);
        ts::return_to_sender(&scenario, p);
    };

    // ---- Write #2: writer_mm_addr writes 50 BTC. Premium 3_000_000, nonce 2.
    let write2_amount: u64 = 50;
    let write2_premium: u64 = 3_000_000;
    let write2_fee = bps(write2_premium);
    let write2_net = write2_premium - write2_fee;
    {
        ts::next_tx(&mut scenario, th::writer_mm_addr());
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let config = th::take_config(&scenario);
        let wl = th::take_whitelist(&scenario);
        let mut treas = th::take_treasury(&scenario);
        let mut signer = th::take_signer(&scenario);

        let q = th::new_test_quote(
            *admin::protocol_id(&config),
            object::id(&signer),
            th::trader_mm_addr(),
            &b,
            write2_amount,
            write2_premium,
            EXPIRY_MS,
            2,
        );
        let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
            &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
        );
        bucket::execute_writer_flow<BTC, USDC, CALL>(
            &mut b,
            &config,
            &wl,
            &mut treas,
            req,
            coin::mint_for_testing<USDC>(write2_premium, scenario.ctx()).into_balance(),
            coin::mint_for_testing<BTC>(write2_amount, scenario.ctx()),
            th::writer_mm_addr(),
            &clock,
            scenario.ctx(),
        );

        assert!(bucket::total_written(&b) == 150, 0);
        assert!(bucket::underlying_balance(&b) == 150, 0);
        assert!(bucket::call_supply(&b) == 150, 0);
        assert!(treasury::balance_of<USDC>(&treas) == write1_fee + write2_fee, 0);

        ts::return_shared(b);
        ts::return_shared(config);
        ts::return_shared(wl);
        ts::return_shared(treas);
        ts::return_shared(signer);
    };

    ts::next_tx(&mut scenario, th::writer_mm_addr());
    {
        let net = ts::take_from_sender<Coin<USDC>>(&scenario);
        assert!(net.value() == write2_net, 0);
        coin::burn_for_testing(net);
        let p = ts::take_from_sender<Position>(&scenario);
        assert!(position::range_start(&p) == 100 && position::range_end(&p) == 150, 0);
        ts::return_to_sender(&scenario, p);
    };

    // ---- Trader MM joins both call coins, splits off 120 to exercise.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let exercise_chunk: Coin<CALL>;
    {
        let mut call_a = ts::take_from_sender<Coin<CALL>>(&scenario);
        let call_b = ts::take_from_sender<Coin<CALL>>(&scenario);
        coin::join(&mut call_a, call_b);
        assert!(call_a.value() == 150, 0);
        exercise_chunk = coin::split(&mut call_a, 120, scenario.ctx());
        // Keep the remaining 30 with the MM for later burn_expired.
        ts::return_to_sender(&scenario, call_a);
    };

    // ---- Trader MM exercises 120 BTC. Pays (((120 as u128) * STRIKE) as u64) USDC. Cursor → 120.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    {
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let payment = coin::mint_for_testing<USDC>((((120 as u128) * STRIKE) as u64), scenario.ctx());
        let underlying_out = bucket::exercise<BTC, USDC, CALL>(
            &mut b,
            exercise_chunk,
            payment,
            &clock,
            scenario.ctx(),
        );
        assert!(underlying_out.value() == 120, 0);
        assert!(bucket::exercise_cursor(&b) == 120, 0);
        assert!(bucket::underlying_balance(&b) == 30, 0);
        assert!(bucket::settlement_balance(&b) == (((120 as u128) * STRIKE) as u64), 0);
        // 150 written − 120 burned-on-exercise = 30 outstanding.
        assert!(bucket::call_supply(&b) == 30, 0);
        coin::burn_for_testing(underlying_out);
        ts::return_shared(b);
    };

    // ---- Time passes. Now past expiry: redemptions and burn are unlocked.
    clock.set_for_testing(EXPIRY_MS + 1);

    // ---- Redeem writer #1's position (range [0,100), cursor=120 → fully exercised).
    ts::next_tx(&mut scenario, th::writer_addr());
    {
        let p = ts::take_from_sender<Position>(&scenario);
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let (u, s) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, p, &clock, scenario.ctx());
        assert!(u.value() == 0, 0);
        assert!(s.value() == (((100 as u128) * STRIKE) as u64), 0);
        // Bucket drains down: 30 BTC remain, (((20 as u128) * STRIKE) as u64) USDC remain.
        assert!(bucket::underlying_balance(&b) == 30, 0);
        assert!(bucket::settlement_balance(&b) == (((20 as u128) * STRIKE) as u64), 0);
        coin::burn_for_testing(u);
        coin::burn_for_testing(s);
        ts::return_shared(b);
    };

    // ---- Redeem writer #2's position (range [100,150), cursor=120 → 20 exercised, 30 unexercised).
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    {
        let p = ts::take_from_sender<Position>(&scenario);
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let (u, s) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, p, &clock, scenario.ctx());
        assert!(u.value() == 30, 0);
        assert!(s.value() == (((20 as u128) * STRIKE) as u64), 0);
        // Bucket fully drained.
        assert!(bucket::underlying_balance(&b) == 0, 0);
        assert!(bucket::settlement_balance(&b) == 0, 0);
        coin::burn_for_testing(u);
        coin::burn_for_testing(s);
        ts::return_shared(b);
    };

    // ---- Trader MM burns the remaining 30 unexercised call tokens.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    {
        let leftover = ts::take_from_sender<Coin<CALL>>(&scenario);
        assert!(leftover.value() == 30, 0);
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        bucket::burn_expired_option<BTC, USDC, CALL>(&mut b, leftover, &clock, scenario.ctx());
        // All options now burned.
        assert!(bucket::call_supply(&b) == 0, 0);
        ts::return_shared(b);
    };

    // ---- Admin reaps the bucket (now drained) and withdraws accumulated fees.
    ts::next_tx(&mut scenario, th::admin_addr());
    {
        let cap = th::take_admin_cap(&scenario);
        let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        bucket::cleanup_bucket<BTC, USDC, CALL>(&cap, b, &clock, scenario.ctx());

        let mut treas = th::take_treasury(&scenario);
        let total_fee = write1_fee + write2_fee;
        assert!(treasury::balance_of<USDC>(&treas) == total_fee, 0);
        treasury::withdraw<USDC>(&cap, &mut treas, total_fee, th::admin_addr(), scenario.ctx());
        assert!(treasury::balance_of<USDC>(&treas) == 0, 0);
        th::return_admin_cap(&scenario, cap);
        ts::return_shared(treas);
    };

    ts::next_tx(&mut scenario, th::admin_addr());
    {
        let fees = ts::take_from_sender<Coin<USDC>>(&scenario);
        assert!(fees.value() == write1_fee + write2_fee, 0);
        coin::burn_for_testing(fees);
    };

    clock.destroy_for_testing();
    ts::end(scenario);
}

// ---------------------------------------------------------------------------
// Trader flow E2E: retail traders buy covered calls from a Writer MM.
// ---------------------------------------------------------------------------
//
// Cast
//   writer_mm_addr  — Writer MM (signs quotes; receives Positions + net premium)
//   trader_addr     — Retail trader #1 (first buyer; underlying range [0,100))
//   trader_mm_addr   — Retail trader #2 (second buyer; underlying range [100,180))
//
// Numbers
//   buy1: 100 BTC for premium 6_000_000 → fee 30_000, net 5_970_000 to MM
//   buy2:  80 BTC for premium 5_000_000 → fee 25_000, net 4_975_000 to MM
//   trader#1 exercises full 100 → cursor 100
//   trader#2 exercises 30 of 80  → cursor 130
//   trader#2 burns leftover 50 call tokens (unexercised) post-expiry
//   MM redeems both positions:
//     pos [0,100):   cursor=130 ≥ 100  → 0 BTC + (((100 as u128) * STRIKE) as u64) USDC
//     pos [100,180): cursor=130 mid-range → 50 BTC + (((30u64 as u128) * STRIKE) as u64) USDC

#[test]
fun test_e2e_trader_flow_full_lifecycle() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_protocol_with_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::writer_mm_addr(), th::pubkey_a());

    // ---- Buy #1: trader_addr buys 100 BTC option. Premium 6_000_000, nonce 1.
    let buy1_amount: u64 = 100;
    let buy1_premium: u64 = 6_000_000;
    let buy1_fee = bps(buy1_premium);
    let buy1_net = buy1_premium - buy1_fee;
    let bucket_id;
    {
        ts::next_tx(&mut scenario, th::trader_addr());
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let config = th::take_config(&scenario);
        let wl = th::take_whitelist(&scenario);
        let mut treas = th::take_treasury(&scenario);
        let mut signer = th::take_signer(&scenario);
        bucket_id = object::id(&b);

        let q = th::new_test_quote(
            *admin::protocol_id(&config),
            object::id(&signer),
            th::writer_mm_addr(),  // signer (MM) gets the position Object + net premium
            &b,
            buy1_amount,
            buy1_premium,
            EXPIRY_MS,
            1,
        );
        let req = bucket::request_trader_flow_for_testing<BTC, USDC, CALL>(
            &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
        );
        bucket::execute_trader_flow<BTC, USDC, CALL>(
            &mut b,
            &config,
            &wl,
            &mut treas,
            req,
            coin::mint_for_testing<BTC>(buy1_amount, scenario.ctx()).into_balance(),
            coin::mint_for_testing<USDC>(buy1_premium, scenario.ctx()),
            th::trader_addr(),
            &clock,
            scenario.ctx(),
        );

        assert!(bucket::total_written(&b) == 100, 0);
        assert!(bucket::underlying_balance(&b) == 100, 0);
        assert!(bucket::call_supply(&b) == 100, 0);
        assert!(treasury::balance_of<USDC>(&treas) == buy1_fee, 0);

        ts::return_shared(b);
        ts::return_shared(config);
        ts::return_shared(wl);
        ts::return_shared(treas);
        ts::return_shared(signer);
    };

    // Writer MM receives buy #1's net premium.
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    {
        let net = ts::take_from_sender<Coin<USDC>>(&scenario);
        assert!(net.value() == buy1_net, 0);
        coin::burn_for_testing(net);
    };

    // ---- Buy #2: trader_mm_addr buys 80 BTC option. Premium 5_000_000, nonce 2.
    let buy2_amount: u64 = 80;
    let buy2_premium: u64 = 5_000_000;
    let buy2_fee = bps(buy2_premium);
    let buy2_net = buy2_premium - buy2_fee;
    {
        ts::next_tx(&mut scenario, th::trader_mm_addr());
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let config = th::take_config(&scenario);
        let wl = th::take_whitelist(&scenario);
        let mut treas = th::take_treasury(&scenario);
        let mut signer = th::take_signer(&scenario);

        let q = th::new_test_quote(
            *admin::protocol_id(&config),
            object::id(&signer),
            th::writer_mm_addr(),
            &b,
            buy2_amount,
            buy2_premium,
            EXPIRY_MS,
            2,
        );
        let req = bucket::request_trader_flow_for_testing<BTC, USDC, CALL>(
            &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
        );
        bucket::execute_trader_flow<BTC, USDC, CALL>(
            &mut b,
            &config,
            &wl,
            &mut treas,
            req,
            coin::mint_for_testing<BTC>(buy2_amount, scenario.ctx()).into_balance(),
            coin::mint_for_testing<USDC>(buy2_premium, scenario.ctx()),
            th::trader_mm_addr(),
            &clock,
            scenario.ctx(),
        );

        assert!(bucket::total_written(&b) == 180, 0);
        assert!(bucket::underlying_balance(&b) == 180, 0);
        assert!(bucket::call_supply(&b) == 180, 0);
        assert!(treasury::balance_of<USDC>(&treas) == buy1_fee + buy2_fee, 0);

        ts::return_shared(b);
        ts::return_shared(config);
        ts::return_shared(wl);
        ts::return_shared(treas);
        ts::return_shared(signer);
    };

    // Writer MM receives buy #2's net premium.
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    {
        let net = ts::take_from_sender<Coin<USDC>>(&scenario);
        assert!(net.value() == buy2_net, 0);
        coin::burn_for_testing(net);
    };

    // ---- Trader #1 exercises full 100. Cursor → 100.
    ts::next_tx(&mut scenario, th::trader_addr());
    {
        let call = ts::take_from_sender<Coin<CALL>>(&scenario);
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let payment = coin::mint_for_testing<USDC>((((buy1_amount as u128) * STRIKE) as u64), scenario.ctx());
        let underlying_out = bucket::exercise<BTC, USDC, CALL>(&mut b, call, payment, &clock, scenario.ctx());
        assert!(underlying_out.value() == buy1_amount, 0);
        assert!(bucket::exercise_cursor(&b) == 100, 0);
        assert!(bucket::underlying_balance(&b) == 80, 0);
        assert!(bucket::settlement_balance(&b) == (((100 as u128) * STRIKE) as u64), 0);
        assert!(bucket::call_supply(&b) == 80, 0);
        coin::burn_for_testing(underlying_out);
        ts::return_shared(b);
    };

    // ---- Trader #2 splits their 80 call into [30, 50] and exercises 30. Cursor → 130.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let exercise_30: Coin<CALL>;
    {
        let mut call = ts::take_from_sender<Coin<CALL>>(&scenario);
        exercise_30 = coin::split(&mut call, 30, scenario.ctx());
        ts::return_to_sender(&scenario, call); // remaining 50 stays with trader #2 for later burn
    };
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    {
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        let payment = coin::mint_for_testing<USDC>((((30u64 as u128) * STRIKE) as u64), scenario.ctx());
        let underlying_out = bucket::exercise<BTC, USDC, CALL>(&mut b, exercise_30, payment, &clock, scenario.ctx());
        assert!(underlying_out.value() == 30, 0);
        assert!(bucket::exercise_cursor(&b) == 130, 0);
        assert!(bucket::underlying_balance(&b) == 50, 0);
        assert!(bucket::settlement_balance(&b) == (((130 as u128) * STRIKE) as u64), 0);
        assert!(bucket::call_supply(&b) == 50, 0);
        coin::burn_for_testing(underlying_out);
        ts::return_shared(b);
    };

    // ---- Time passes past expiry.
    clock.set_for_testing(EXPIRY_MS + 1);

    // ---- Stranger burns their leftover 50 unexercised call tokens.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    {
        let leftover = ts::take_from_sender<Coin<CALL>>(&scenario);
        assert!(leftover.value() == 50, 0);
        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        bucket::burn_expired_option<BTC, USDC, CALL>(&mut b, leftover, &clock, scenario.ctx());
        assert!(bucket::call_supply(&b) == 0, 0);
        ts::return_shared(b);
    };

    // ---- Writer MM redeems both positions. Each redemption drains the bucket further.
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    {
        // Take both positions and identify them by range_end.
        let p_a = ts::take_from_sender<Position>(&scenario);
        let p_b = ts::take_from_sender<Position>(&scenario);
        let (early, late) = if (position::range_end(&p_a) == 100) { (p_a, p_b) } else { (p_b, p_a) };

        let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);

        // Redeem position [0,100): cursor=130 ≥ 100 → 0 BTC + (((100 as u128) * STRIKE) as u64) USDC.
        let (u1, s1) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, early, &clock, scenario.ctx());
        assert!(u1.value() == 0, 0);
        assert!(s1.value() == (((100 as u128) * STRIKE) as u64), 0);
        assert!(bucket::underlying_balance(&b) == 50, 0);
        assert!(bucket::settlement_balance(&b) == (((30u64 as u128) * STRIKE) as u64), 0);
        coin::burn_for_testing(u1);
        coin::burn_for_testing(s1);

        // Redeem position [100,180): cursor=130 → 30 exercised, 50 unexercised.
        let (u2, s2) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, late, &clock, scenario.ctx());
        assert!(u2.value() == 50, 0);
        assert!(s2.value() == (((30u64 as u128) * STRIKE) as u64), 0);
        assert!(bucket::underlying_balance(&b) == 0, 0);
        assert!(bucket::settlement_balance(&b) == 0, 0);
        coin::burn_for_testing(u2);
        coin::burn_for_testing(s2);

        ts::return_shared(b);
    };

    // ---- Admin cleans up bucket and withdraws total fees.
    ts::next_tx(&mut scenario, th::admin_addr());
    {
        let cap = th::take_admin_cap(&scenario);
        let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
        bucket::cleanup_bucket<BTC, USDC, CALL>(&cap, b, &clock, scenario.ctx());

        let mut treas = th::take_treasury(&scenario);
        let total_fee = buy1_fee + buy2_fee;
        assert!(treasury::balance_of<USDC>(&treas) == total_fee, 0);
        treasury::withdraw<USDC>(&cap, &mut treas, total_fee, th::admin_addr(), scenario.ctx());
        assert!(treasury::balance_of<USDC>(&treas) == 0, 0);
        th::return_admin_cap(&scenario, cap);
        ts::return_shared(treas);
    };

    ts::next_tx(&mut scenario, th::admin_addr());
    {
        let fees = ts::take_from_sender<Coin<USDC>>(&scenario);
        assert!(fees.value() == buy1_fee + buy2_fee, 0);
        coin::burn_for_testing(fees);
    };

    clock.destroy_for_testing();
    ts::end(scenario);
}
