#[test_only]
module options_protocol::session_deepbook_tests;

use sui::clock::Clock;
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};
use sui::test_utils;

use deepbook::balance_manager::{Self, BalanceManager};
use deepbook::constants;
use deepbook::pool::{Self, Pool};
use deepbook::registry::{Self, Registry};

use siws_session::account::{Self as siws_account, Account as SessionAccount};
use siws_session::session::{Self, SessionCap, SpendLimit};

use options_protocol::account::{Self, Account};
use options_protocol::session_account;
use options_protocol::session_deepbook;
use options_protocol::test_helpers::{Self as th, BTC, USDC};

/// The ephemeral Sui address the session cap is bound to (taker).
const HOLDER: address = @0x5E55;
/// DeepBook registry/pool admin.
const OWNER: address = @0x0A;
/// Counterparty resting the ask the session buy fills against.
const MAKER: address = @0x11A;

// Whitelisted pool params (DeepBook test defaults): zero fees, no DEEP needed.
const TICK: u64 = 1000;
const LOT: u64 = 1000;
const MIN: u64 = 10000;
/// 2 quote per base, scaled by FLOAT_SCALING (1e9).
const PRICE: u64 = 2_000_000_000;
/// Base atomic units traded (multiple of LOT, ≥ MIN).
const QTY: u64 = 100_000;
/// Quote cost of QTY at PRICE = PRICE * QTY / 1e9.
const COST: u64 = 200_000;

fun fill(n: u64, v: u8): vector<u8> {
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < n) { out.push_back(v); i = i + 1; };
    out
}

fun allowlist(): vector<vector<u8>> {
    vector[
        session_account::create_account_selector(),
        session_deepbook::enable_trading_selector(),
        session_deepbook::place_market_order_selector(),
    ]
}

fun default_limits(): vector<SpendLimit> { vector<SpendLimit>[] }

/// Session account + cap bound to HOLDER, plus a session-rooted shared options
/// Account linked to it. Returns (session_account, cap, account_id).
fun setup_session_user(scenario: &mut Scenario, clock: &Clock): (SessionAccount, SessionCap, ID) {
    ts::next_tx(scenario, HOLDER);
    let sess = siws_account::new_for_testing(fill(32, 0x42), scenario.ctx());
    let cap = session::mint_for_testing(
        object::id(&sess), 0, 9_999_999_999, default_limits(), allowlist(), HOLDER, scenario.ctx(),
    );
    session_account::create_and_share_account_with_session(
        &cap, &sess, clock, th::scheme_ed25519(), th::pubkey_b(), scenario.ctx(),
    );
    ts::next_tx(scenario, HOLDER);
    let account_id = ts::most_recent_id_shared<Account>().destroy_some();
    (sess, cap, account_id)
}

fun fund_custody<T>(scenario: &mut Scenario, account_id: ID, amount: u64) {
    ts::next_tx(scenario, HOLDER);
    let mut acc = th::take_account_by_id(scenario, account_id);
    account::deposit(&mut acc, coin::mint_for_testing<T>(amount, scenario.ctx()));
    ts::return_shared(acc);
}

/// Shared DeepBook registry id, with the balance-manager map initialized (a
/// post-publish migration the live staging/prod registries already ran, so
/// `register_balance_manager` works there — the test registry must match).
fun setup_registry(scenario: &mut Scenario): ID {
    ts::next_tx(scenario, OWNER);
    let registry_id = registry::test_registry(scenario.ctx());
    ts::next_tx(scenario, OWNER);
    let admin_cap = registry::get_admin_cap_for_testing(scenario.ctx());
    let mut registry = ts::take_shared_by_id<Registry>(scenario, registry_id);
    registry::init_balance_manager_map(&mut registry, &admin_cap, scenario.ctx());
    ts::return_shared(registry);
    test_utils::destroy(admin_cap);
    registry_id
}

/// Create a whitelisted (zero-fee) BTC/USDC pool and return its id.
fun setup_pool(scenario: &mut Scenario, registry_id: ID): ID {
    ts::next_tx(scenario, OWNER);
    let admin_cap = registry::get_admin_cap_for_testing(scenario.ctx());
    let mut registry = ts::take_shared_by_id<Registry>(scenario, registry_id);
    let pool_id = pool::create_pool_admin<BTC, USDC>(
        &mut registry, TICK, LOT, MIN, true /* whitelisted */, false, &admin_cap, scenario.ctx(),
    );
    ts::return_shared(registry);
    test_utils::destroy(admin_cap);
    pool_id
}

/// MAKER rests an ask selling QTY base at PRICE, funded from a fresh BM.
fun rest_maker_ask(scenario: &mut Scenario, pool_id: ID, clock: &Clock) {
    ts::next_tx(scenario, MAKER);
    let mut maker_bm = balance_manager::new(scenario.ctx());
    maker_bm.deposit(coin::mint_for_testing<BTC>(QTY, scenario.ctx()), scenario.ctx());
    let mut pool = ts::take_shared_by_id<Pool<BTC, USDC>>(scenario, pool_id);
    let proof = maker_bm.generate_proof_as_owner(scenario.ctx());
    let _ = pool.place_limit_order<BTC, USDC>(
        &mut maker_bm,
        &proof,
        1,
        constants::no_restriction(),
        constants::self_matching_allowed(),
        PRICE,
        QTY,
        false, // ask
        false, // pay_with_deep
        constants::max_u64(),
        clock,
        scenario.ctx(),
    );
    ts::return_shared(pool);
    transfer::public_share_object(maker_bm);
}

#[test]
fun test_enable_trading_creates_owned_balance_manager() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    let registry_id = setup_registry(&mut scenario);
    let (sess, cap, _account_id) = setup_session_user(&mut scenario, &clock);

    ts::next_tx(&mut scenario, HOLDER);
    let mut registry = ts::take_shared_by_id<Registry>(&scenario, registry_id);
    session_deepbook::enable_trading_with_session(&mut registry, &cap, &sess, &clock, scenario.ctx());
    ts::return_shared(registry);

    // A BalanceManager was shared, owned by the ephemeral session key.
    ts::next_tx(&mut scenario, HOLDER);
    let bm = ts::take_shared<BalanceManager>(&scenario);
    assert!(bm.owner() == HOLDER, 0);
    assert!(bm.balance<BTC>() == 0, 0);
    ts::return_shared(bm);

    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_market_buy_settles_back_to_custody() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    let registry_id = setup_registry(&mut scenario);
    let pool_id = setup_pool(&mut scenario, registry_id);
    rest_maker_ask(&mut scenario, pool_id, &clock);

    let (sess, cap, account_id) = setup_session_user(&mut scenario, &clock);
    // Fund custody with more quote than the fill costs, to prove the refund path.
    fund_custody<USDC>(&mut scenario, account_id, 300_000);

    // Enable trading: per-session BalanceManager owned by HOLDER.
    ts::next_tx(&mut scenario, HOLDER);
    let mut registry = ts::take_shared_by_id<Registry>(&scenario, registry_id);
    session_deepbook::enable_trading_with_session(&mut registry, &cap, &sess, &clock, scenario.ctx());
    ts::return_shared(registry);

    ts::next_tx(&mut scenario, HOLDER);
    let session_bm_id = ts::most_recent_id_shared<BalanceManager>().destroy_some();

    // Market buy QTY base, budgeting all 300_000 quote from custody.
    ts::next_tx(&mut scenario, HOLDER);
    let mut pool = ts::take_shared_by_id<Pool<BTC, USDC>>(&scenario, pool_id);
    let mut session_bm = ts::take_shared_by_id<BalanceManager>(&scenario, session_bm_id);
    let mut user_acc = th::take_account_by_id(&scenario, account_id);

    session_deepbook::place_market_order_with_session<BTC, USDC>(
        &mut pool, &mut session_bm, &mut user_acc, &cap, &sess,
        1, true /* is_bid */, QTY, 300_000 /* max_quote_in */, &clock, scenario.ctx(),
    );

    // Bought base lands in custody; unspent quote (300k − cost) returns; the
    // per-session BM is left completely empty.
    assert!(account::balance_of<BTC>(&user_acc) == QTY, 0);
    assert!(account::balance_of<USDC>(&user_acc) == 300_000 - COST, 0);
    assert!(session_bm.balance<BTC>() == 0, 0);
    assert!(session_bm.balance<USDC>() == 0, 0);

    ts::return_shared(pool);
    ts::return_shared(session_bm);
    ts::return_shared(user_acc);
    test_utils::destroy(sess);
    test_utils::destroy(cap);
    clock.destroy_for_testing();
    ts::end(scenario);
}
