#[test_only]
module options_protocol::session_integration_tests;

use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};
use sui::test_utils;

use siws_session::account::{Self as siws_account, Account as SessionAccount};
use siws_session::session::{Self, SessionCap, SpendLimit};

use options_protocol::account::{Self, Account};
use options_protocol::admin;
use options_protocol::bucket::{Self, Bucket};
use options_protocol::quote;
use options_protocol::session_account;
use options_protocol::session_bucket;
use options_protocol::session_vault;
use options_protocol::test_helpers::{Self as th, BTC, USDC, CALL};
use options_protocol::vault;

const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;
const EXPIRY_MS: u64 = 1_000_000;

/// The ephemeral Sui address the cap is bound to.
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
        session_account::withdraw_selector(),
        session_account::set_quote_signing_key_selector(),
        session_bucket::execute_write_selector(),
        session_bucket::exercise_selector(),
        session_bucket::redeem_selector(),
        session_bucket::burn_expired_selector(),
        session_vault::deposit_selector(),
        session_vault::claim_shares_selector(),
        session_vault::initiate_withdraw_selector(),
        session_vault::complete_withdraw_selector(),
        session_vault::instant_withdraw_selector(),
    ]
}

/// Generous limits for BTC + USDC (the assets session flows spend).
fun default_limits(): vector<SpendLimit> {
    vector[
        session::new_limit_for_testing(
            siws_account::canonical_type_bytes<BTC>(), 1_000, 10_000,
        ),
        session::new_limit_for_testing(
            siws_account::canonical_type_bytes<USDC>(), 100_000_000, 100_000_000,
        ),
    ]
}

/// Session account + live cap bound to HOLDER, and a session-rooted shared
/// options Account linked to it. Returns (session_account, cap, account_id).
fun setup_session_user(
    scenario: &mut Scenario,
    clock: &Clock,
    limits: vector<SpendLimit>,
): (SessionAccount, SessionCap, ID) {
    ts::next_tx(scenario, HOLDER);
    let sess = siws_account::new_for_testing(fill(32, 0x42), scenario.ctx());
    let cap = session::mint_for_testing(
        object::id(&sess), 0, 9_999_999_999, limits, allowlist(), HOLDER, scenario.ctx(),
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

// --- account create / withdraw ---

#[test]
fun test_create_account_and_withdraw_with_session() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<USDC>(&mut scenario, account_id, 1_000_000);

    ts::next_tx(&mut scenario, HOLDER);
    let mut acc = th::take_account_by_id(&scenario, account_id);
    // owner is the session account id rendered as an address, and the link
    // points at the session account.
    assert!(account::owner(&acc) == session_account::session_owner_address(&sess), 0);
    assert!(session_account::session_owner(&acc).destroy_some() == object::id(&sess), 0);

    let out = session_account::withdraw_with_session<USDC>(
        &mut acc, &cap, &mut sess, &clock, 250_000, scenario.ctx(),
    );
    assert!(out.value() == 250_000, 0);
    assert!(account::balance_of<USDC>(&acc) == 750_000, 0);
    // The spend was charged against the cap's USDC budget.
    assert!(siws_account::spent_of<USDC>(&sess, object::id(&cap)) == 250_000, 0);
    coin::burn_for_testing(out);

    ts::return_shared(acc);
    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 7, location = siws_session::session)] // over_per_tx
fun test_withdraw_with_session_over_per_tx_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    // BTC per-tx limit is 1_000.
    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<BTC>(&mut scenario, account_id, 10_000);

    ts::next_tx(&mut scenario, HOLDER);
    let mut acc = th::take_account_by_id(&scenario, account_id);
    let out = session_account::withdraw_with_session<BTC>(
        &mut acc, &cap, &mut sess, &clock, 1_001, scenario.ctx(),
    );
    coin::burn_for_testing(out);
    abort 0
}

#[test]
#[expected_failure(abort_code = 4, location = siws_session::session)] // wrong_holder
fun test_withdraw_with_session_wrong_sender_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<USDC>(&mut scenario, account_id, 1_000_000);

    // A different sender than the cap's holder.
    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut acc = th::take_account_by_id(&scenario, account_id);
    let out = session_account::withdraw_with_session<USDC>(
        &mut acc, &cap, &mut sess, &clock, 1, scenario.ctx(),
    );
    coin::burn_for_testing(out);
    abort 0
}

#[test]
#[expected_failure(abort_code = 56, location = options_protocol::session_account)] // session_mismatch
fun test_with_session_on_unlinked_account_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    let (mut sess, cap, _) = setup_session_user(&mut scenario, &clock, default_limits());

    // A PLAIN wallet-owned account (no session link).
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());
    ts::next_tx(&mut scenario, HOLDER);
    let mut plain = th::take_account(&scenario);
    let out = session_account::withdraw_with_session<USDC>(
        &mut plain, &cap, &mut sess, &clock, 1, scenario.ctx(),
    );
    coin::burn_for_testing(out);
    abort 0
}

// --- writer flow: user writes from custody, premium + position settle back ---

#[test]
fun test_writer_flow_with_session() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    // The trader MM (quote signer / option buyer) with a funded account.
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut mm_acc = th::take_account(&scenario);
    account::deposit(&mut mm_acc, coin::mint_for_testing<USDC>(10_000_000, scenario.ctx()));

    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<BTC>(&mut scenario, account_id, 500);

    ts::next_tx(&mut scenario, HOLDER);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut user_acc = th::take_account_by_id(&scenario, account_id);

    let write_amount: u64 = 100;
    let premium: u64 = 5_000_000;
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

    session_bucket::execute_write_with_session_for_testing<BTC, USDC, CALL>(
        &mut b, &config, &mut treasury, &mut mm_acc, &mut user_acc, &cap, &mut sess,
        bucket::writer_flow(), sq, &clock, scenario.ctx(),
    );

    // Bucket collateralized from custody; premium settled back into custody.
    assert!(bucket::underlying_balance(&b) == write_amount, 0);
    assert!(account::balance_of<BTC>(&user_acc) == 500 - write_amount, 0);
    assert!(account::balance_of<USDC>(&user_acc) == premium, 0);
    assert!(account::balance_of<USDC>(&mm_acc) == 10_000_000 - premium, 0);
    // The underlying spend was charged against the cap's BTC budget.
    assert!(siws_account::spent_of<BTC>(&sess, object::id(&cap)) == write_amount, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    // The Position is custodied on (and indexed by) the user's account, and
    // the MM holds the call coins.
    let position_ids = session_account::position_ids(&user_acc);
    assert!(position_ids.length() == 1, 0);
    assert!(session_account::has_position(&user_acc, position_ids[0]), 0);
    ts::return_shared(user_acc);
    ts::next_tx(&mut scenario, HOLDER);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    assert!(call.value() == write_amount, 0);
    ts::return_to_sender(&scenario, call);

    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- trader flow + exercise: user buys from custody, then exercises ---

#[test]
fun test_trader_flow_then_exercise_with_session() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    // The writer MM (quote signer / option seller) with funded underlying.
    th::create_account(&mut scenario, th::writer_mm_addr(), th::pubkey_a());
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    let mut mm_acc = th::take_account(&scenario);
    account::deposit(&mut mm_acc, coin::mint_for_testing<BTC>(1_000, scenario.ctx()));

    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<USDC>(&mut scenario, account_id, 20_000_000);

    ts::next_tx(&mut scenario, HOLDER);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut user_acc = th::take_account_by_id(&scenario, account_id);

    let write_amount: u64 = 100;
    let premium: u64 = 5_000_000;
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

    session_bucket::execute_write_with_session_for_testing<BTC, USDC, CALL>(
        &mut b, &config, &mut treasury, &mut mm_acc, &mut user_acc, &cap, &mut sess,
        bucket::trader_flow(), sq, &clock, scenario.ctx(),
    );

    // Premium left custody; the call coins are custodied as a balance.
    assert!(account::balance_of<USDC>(&user_acc) == 20_000_000 - premium, 0);
    assert!(account::balance_of<CALL>(&user_acc) == write_amount, 0);
    assert!(account::balance_of<BTC>(&mm_acc) == 1_000 - write_amount, 0);
    assert!(siws_account::spent_of<USDC>(&sess, object::id(&cap)) == premium, 0);

    // Exercise 10 from custody: strike 50_000 × 10 = 500_000 settlement.
    let exercise_amount: u64 = 10;
    let strike_cost: u64 = 500_000;
    session_bucket::exercise_with_session<BTC, USDC, CALL>(
        &mut b, &mut user_acc, &cap, &mut sess, exercise_amount, &clock, scenario.ctx(),
    );
    assert!(account::balance_of<CALL>(&user_acc) == write_amount - exercise_amount, 0);
    assert!(account::balance_of<BTC>(&user_acc) == exercise_amount, 0);
    assert!(
        account::balance_of<USDC>(&user_acc) == 20_000_000 - premium - strike_cost,
        0,
    );
    // Settlement spend accumulated on the cap's USDC budget.
    assert!(
        siws_account::spent_of<USDC>(&sess, object::id(&cap)) == premium + strike_cost,
        0,
    );

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

// --- redeem after expiry: position out of custody, proceeds back in ---

#[test]
fun test_redeem_with_session_after_expiry() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut mm_acc = th::take_account(&scenario);
    account::deposit(&mut mm_acc, coin::mint_for_testing<USDC>(10_000_000, scenario.ctx()));

    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<BTC>(&mut scenario, account_id, 500);

    ts::next_tx(&mut scenario, HOLDER);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut user_acc = th::take_account_by_id(&scenario, account_id);

    let write_amount: u64 = 100;
    let premium: u64 = 5_000_000;
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
    session_bucket::execute_write_with_session_for_testing<BTC, USDC, CALL>(
        &mut b, &config, &mut treasury, &mut mm_acc, &mut user_acc, &cap, &mut sess,
        bucket::writer_flow(), sq, &clock, scenario.ctx(),
    );
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    // The custodied position is indexed on the account.
    let position_ids = session_account::position_ids(&user_acc);
    assert!(position_ids.length() == 1, 0);
    let position_id = position_ids[0];

    // Nothing exercised; after expiry the full underlying comes back.
    clock.set_for_testing(EXPIRY_MS + 1);
    let btc_before = account::balance_of<BTC>(&user_acc);
    session_bucket::redeem_position_with_session<BTC, USDC, CALL>(
        &mut b, &mut user_acc, &cap, &sess, position_id, &clock, scenario.ctx(),
    );
    assert!(account::balance_of<BTC>(&user_acc) == btc_before + write_amount, 0);
    assert!(!session_account::has_position(&user_acc, position_id), 0);

    ts::return_shared(b);
    ts::return_shared(user_acc);
    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- vault flows: deposit/claim/withdraw entirely from custody ---

/// Share coin for the session vault tests.
public struct VSH has drop {}

const WEEK_MS: u64 = 604_800_000;
const DAY_MS: u64 = 86_400_000;
const VAULT_SPOT: u128 = 47_619_000_000_000_000;
const VAULT_SPOT_SCALE: u8 = 12;

/// Mirror of `vault_tests::default_config`.
fun vault_config(): vault::VaultConfig {
    vault::new_config(
        200, 1_000, WEEK_MS, 12 * 60 * 60 * 1_000, 300, 6_000,
        3 * DAY_MS, 9 * DAY_MS, 10, 1_000_000_000_000, 4,
        400_000, 60_000, 120_000, 100_000, 500,
        false, 50,
        b"underlying-feed", b"settlement-feed", 60, 100, 9, 6,
    )
}

fun setup_vault(scenario: &mut Scenario) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let tcap = coin::create_treasury_cap_for_testing<VSH>(scenario.ctx());
    vault::create_vault<BTC, USDC, VSH>(&cap, tcap, vault_config(), scenario.ctx());
    th::return_admin_cap(scenario, cap);
}

fun finalize_vault(scenario: &mut Scenario, clock: &Clock) {
    ts::next_tx(scenario, th::stranger_addr());
    let mut v = ts::take_shared<vault::Vault<BTC, USDC, VSH>>(scenario);
    let mut treasury = th::take_treasury(scenario);
    vault::finalize_round_with_spot_for_testing(
        &mut v, &mut treasury, VAULT_SPOT, VAULT_SPOT_SCALE, clock, scenario.ctx(),
    );
    ts::return_shared(v);
    ts::return_shared(treasury);
}

#[test]
fun test_vault_deposit_and_instant_cancel_with_session() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_vault(&mut scenario);
    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<BTC>(&mut scenario, account_id, 1_000);

    ts::next_tx(&mut scenario, HOLDER);
    let mut v = ts::take_shared<vault::Vault<BTC, USDC, VSH>>(&scenario);
    let mut acc = th::take_account_by_id(&scenario, account_id);

    session_vault::deposit_with_session(
        &mut v, &mut acc, &cap, &mut sess, 700, &clock, scenario.ctx(),
    );
    // Underlying left custody (charged against the cap); the receipt is
    // custodied + indexed on the account.
    assert!(account::balance_of<BTC>(&acc) == 300, 0);
    assert!(siws_account::spent_of<BTC>(&sess, object::id(&cap)) == 700, 0);
    let objects = session_account::object_ids(&acc);
    assert!(objects.length() == 1, 0);
    let receipt_id = objects[0];
    assert!(session_account::has_object(&acc, receipt_id), 0);

    // Round hasn't started: cancel refunds straight back into custody.
    session_vault::instant_withdraw_pending_with_session(
        &mut v, &mut acc, &cap, &sess, receipt_id, &clock, scenario.ctx(),
    );
    assert!(account::balance_of<BTC>(&acc) == 1_000, 0);
    assert!(session_account::object_ids(&acc).is_empty(), 0);

    ts::return_shared(v);
    ts::return_shared(acc);
    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_vault_full_cycle_with_session() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_vault(&mut scenario);
    let (mut sess, cap, account_id) = setup_session_user(&mut scenario, &clock, default_limits());
    fund_session_user<BTC>(&mut scenario, account_id, 1_000);

    // Genesis deposit from custody (receipt round 1).
    ts::next_tx(&mut scenario, HOLDER);
    let mut v = ts::take_shared<vault::Vault<BTC, USDC, VSH>>(&scenario);
    let mut acc = th::take_account_by_id(&scenario, account_id);
    session_vault::deposit_with_session(
        &mut v, &mut acc, &cap, &mut sess, 700, &clock, scenario.ctx(),
    );
    let receipt_id = session_account::object_ids(&acc)[0];
    ts::return_shared(v);
    ts::return_shared(acc);

    // First finalize activates round 1 at pps[0] = 1.0.
    finalize_vault(&mut scenario, &clock);

    // Claim shares at pps[0]: 700 shares settle into custody.
    ts::next_tx(&mut scenario, HOLDER);
    let mut v = ts::take_shared<vault::Vault<BTC, USDC, VSH>>(&scenario);
    let mut acc = th::take_account_by_id(&scenario, account_id);
    session_vault::claim_shares_with_session(
        &mut v, &mut acc, &cap, &sess, receipt_id, &clock, scenario.ctx(),
    );
    assert!(account::balance_of<VSH>(&acc) == 700, 0);
    assert!(session_account::object_ids(&acc).is_empty(), 0);

    // Queue a 300-share withdrawal (escrowed; receipt custodied).
    session_vault::initiate_withdraw_with_session(
        &mut v, &mut acc, &cap, &sess, 300, &clock, scenario.ctx(),
    );
    assert!(account::balance_of<VSH>(&acc) == 400, 0);
    let wreceipt_id = session_account::object_ids(&acc)[0];
    ts::return_shared(v);
    ts::return_shared(acc);

    // No bucket selected in round 1 ⇒ finalize is legal immediately; the
    // flat round pays 300 × 1.0 back into custody.
    finalize_vault(&mut scenario, &clock);

    ts::next_tx(&mut scenario, HOLDER);
    let mut v = ts::take_shared<vault::Vault<BTC, USDC, VSH>>(&scenario);
    let mut acc = th::take_account_by_id(&scenario, account_id);
    let btc_before = account::balance_of<BTC>(&acc);
    session_vault::complete_withdraw_with_session(
        &mut v, &mut acc, &cap, &sess, wreceipt_id, &clock, scenario.ctx(),
    );
    assert!(account::balance_of<BTC>(&acc) == btc_before + 300, 0);
    assert!(session_account::object_ids(&acc).is_empty(), 0);

    ts::return_shared(v);
    ts::return_shared(acc);
    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}
