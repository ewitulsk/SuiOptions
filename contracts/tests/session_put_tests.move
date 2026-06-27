#[test_only]
module options_protocol::session_put_tests;

use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};
use sui::test_utils;

use siws_session::account::{Self as siws_account, Account as SessionAccount};
use siws_session::session::{Self, SessionCap, SpendLimit};

use options_protocol::account::{Self, Account};
use options_protocol::admin;
use options_protocol::bucket;
use options_protocol::put_bucket::{Self, PutBucket};
use options_protocol::quote;
use options_protocol::session_account;
use options_protocol::session_put_bucket;
use options_protocol::test_helpers::{Self as th, BTC, USDC, PUT};

const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;
const EXPIRY_MS: u64 = 1_000_000;

const HOLDER: address = @0x5E55;

fun fill(n: u64, v: u8): vector<u8> {
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < n) { out.push_back(v); i = i + 1; };
    out
}

fun allowlist(): vector<vector<u8>> {
    vector[
        session_account::create_account_selector(),
        session_account::set_quote_signing_key_selector(),
        session_put_bucket::execute_write_selector(),
        session_put_bucket::exercise_selector(),
        session_put_bucket::redeem_selector(),
        session_put_bucket::burn_expired_selector(),
    ]
}

fun setup_session_user(scenario: &mut Scenario, clock: &Clock): (SessionAccount, SessionCap, ID) {
    ts::next_tx(scenario, HOLDER);
    let sess = siws_account::new_for_testing(fill(32, 0x42), scenario.ctx());
    let cap = session::mint_for_testing(
        object::id(&sess), 0, 9_999_999_999, vector<SpendLimit>[], allowlist(), HOLDER,
        scenario.ctx(),
    );
    session_account::create_and_share_account_with_session(
        &cap, &sess, clock, th::scheme_ed25519(), th::pubkey_b(), scenario.ctx(),
    );
    ts::next_tx(scenario, HOLDER);
    let account_id = ts::most_recent_id_shared<Account>().destroy_some();
    (sess, cap, account_id)
}

fun fund_session_user<T>(scenario: &mut Scenario, account_id: ID, amount: u64) {
    ts::next_tx(scenario, HOLDER);
    let mut acc = th::take_account_by_id(scenario, account_id);
    account::deposit(&mut acc, coin::mint_for_testing<T>(amount, scenario.ctx()));
    ts::return_shared(acc);
}

// --- writer flow: user writes a put from custody ---

#[test]
fun test_writer_flow_with_session() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    // Trader MM (quote signer / put buyer) funded with premium cash.
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut mm_acc = th::take_account(&scenario);
    account::deposit(&mut mm_acc, coin::mint_for_testing<USDC>(10_000_000, scenario.ctx()));

    let (sess, cap, account_id) = setup_session_user(&mut scenario, &clock);
    // User posts cash collateral from custody.
    fund_session_user<USDC>(&mut scenario, account_id, 6_000_000);

    ts::next_tx(&mut scenario, HOLDER);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut user_acc = th::take_account_by_id(&scenario, account_id);

    let write_amount: u64 = 100;
    let premium: u64 = 1_000_000;
    let collateral = 100 * (STRIKE as u64); // 5_000_000
    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    session_put_bucket::execute_write_with_session_for_testing<BTC, USDC, PUT>(
        &mut b, &config, &mut treasury, &mut mm_acc, &mut user_acc, &cap, &sess,
        bucket::writer_flow(), sq, &clock, scenario.ctx(),
    );

    // Cash collateral left custody; net premium settled back into custody.
    assert!(put_bucket::settlement_balance(&b) == collateral, 0);
    assert!(account::balance_of<USDC>(&user_acc) == 6_000_000 - collateral + premium, 0);
    assert!(account::balance_of<USDC>(&mm_acc) == 10_000_000 - premium, 0);

    // Position custodied on the user's account; MM holds the put coins.
    let position_ids = session_account::position_ids(&user_acc);
    assert!(position_ids.length() == 1, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);
    ts::return_shared(user_acc);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    assert!(put.value() == write_amount, 0);
    ts::return_to_sender(&scenario, put);

    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- trader flow + exercise: user buys a put from custody, then exercises ---

#[test]
fun test_trader_flow_then_exercise_with_session() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    // Writer MM (quote signer / put seller) funded with cash collateral.
    th::create_account(&mut scenario, th::writer_mm_addr(), th::pubkey_a());
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    let mut mm_acc = th::take_account(&scenario);
    account::deposit(&mut mm_acc, coin::mint_for_testing<USDC>(10_000_000, scenario.ctx()));

    let (sess, cap, account_id) = setup_session_user(&mut scenario, &clock);
    fund_session_user<USDC>(&mut scenario, account_id, 5_000_000); // for premium
    fund_session_user<BTC>(&mut scenario, account_id, 500);        // for delivery

    ts::next_tx(&mut scenario, HOLDER);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut user_acc = th::take_account_by_id(&scenario, account_id);

    let write_amount: u64 = 100;
    let premium: u64 = 2_000_000;
    let collateral = 100 * (STRIKE as u64);
    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::writer_mm_addr(),
        object::id(&b),
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    session_put_bucket::execute_write_with_session_for_testing<BTC, USDC, PUT>(
        &mut b, &config, &mut treasury, &mut mm_acc, &mut user_acc, &cap, &sess,
        bucket::trader_flow(), sq, &clock, scenario.ctx(),
    );

    // Premium left custody; put coins custodied as a balance.
    assert!(account::balance_of<USDC>(&user_acc) == 5_000_000 - premium, 0);
    assert!(account::balance_of<PUT>(&user_acc) == write_amount, 0);
    // MM: collateral out, net premium in.
    assert!(account::balance_of<USDC>(&mm_acc) == 10_000_000 - collateral + premium, 0);

    // Exercise 10 from custody: deliver 10 BTC, receive 10 × 50_000 = 500_000.
    let exercise_amount: u64 = 10;
    let cash_out: u64 = 500_000;
    session_put_bucket::exercise_with_session<BTC, USDC, PUT>(
        &mut b, &mut user_acc, &cap, &sess, exercise_amount, &clock, scenario.ctx(),
    );
    assert!(account::balance_of<PUT>(&user_acc) == write_amount - exercise_amount, 0);
    assert!(account::balance_of<BTC>(&user_acc) == 500 - exercise_amount, 0);
    assert!(account::balance_of<USDC>(&user_acc) == 5_000_000 - premium + cash_out, 0);
    assert!(put_bucket::exercise_cursor(&b) == (exercise_amount as u128), 0);
    assert!(put_bucket::underlying_balance(&b) == exercise_amount, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);
    ts::return_shared(user_acc);
    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- redeem after expiry from custody ---

#[test]
fun test_redeem_with_session_after_expiry() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut mm_acc = th::take_account(&scenario);
    account::deposit(&mut mm_acc, coin::mint_for_testing<USDC>(10_000_000, scenario.ctx()));

    let (sess, cap, account_id) = setup_session_user(&mut scenario, &clock);
    fund_session_user<USDC>(&mut scenario, account_id, 6_000_000);

    // User writes a put (writer flow) so the Position is custodied.
    ts::next_tx(&mut scenario, HOLDER);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut user_acc = th::take_account_by_id(&scenario, account_id);

    let write_amount: u64 = 100;
    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        write_amount,
        1_000_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    session_put_bucket::execute_write_with_session_for_testing<BTC, USDC, PUT>(
        &mut b, &config, &mut treasury, &mut mm_acc, &mut user_acc, &cap, &sess,
        bucket::writer_flow(), sq, &clock, scenario.ctx(),
    );
    let position_id = session_account::position_ids(&user_acc)[0];

    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    // No exercise; expire and redeem from custody → all cash collateral back.
    clock.set_for_testing(EXPIRY_MS + 1);
    session_put_bucket::redeem_position_with_session<BTC, USDC, PUT>(
        &mut b, &mut user_acc, &cap, &sess, position_id, &clock, scenario.ctx(),
    );
    // Custody started at 6_000_000; the writer posted 5_000_000 collateral,
    // earned 1_000_000 premium, and (unexercised) gets the 5_000_000 back ⇒
    // 6M − 5M + 1M + 5M = 7_000_000.
    assert!(account::balance_of<USDC>(&user_acc) == 7_000_000, 0);
    assert!(session_account::position_ids(&user_acc).length() == 0, 0);

    ts::return_shared(b);
    ts::return_shared(user_acc);
    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}
