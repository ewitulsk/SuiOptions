#[test_only]
module deepbook_adapter::deepbook_adapter_tests;

use std::type_name;
use sui::balance;
use sui::clock::{Self, Clock};
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};
use sui::test_utils::destroy;

use deepbook::constants;
use deepbook::pool::{Self, Pool};
use deepbook::registry::{Self, Registry};

use options_core::admin::{Self, AdminCap};

use trading_vault::registry as tv_registry;
use trading_vault::registry::{IntegrationRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, CuratorCap, TradingVault};

use deepbook_adapter::deepbook_adapter::{Self as adapter, DeepBookCustody, PoolAllowlist};

public struct USDC has drop {}
public struct BASE has drop {}

const ADMIN: address = @0xA1;
const CREATOR: address = @0xB2;
const CURATOR: address = @0xC3;
const ALICE: address = @0xD4;

fun setup(sc: &mut Scenario): Clock {
    ts::next_tx(sc, ADMIN);
    admin::init_for_testing(sc.ctx());
    tv_registry::init_for_testing(sc.ctx());
    adapter::init_for_testing(sc.ctx());

    ts::next_tx(sc, ADMIN);
    let admin_cap = ts::take_from_sender<AdminCap>(sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(sc);
    tv_registry::allow_adapter(
        &admin_cap,
        &mut ireg,
        type_name::with_defining_ids<adapter::DeepBookAdapter>(),
    );
    ts::return_shared(ireg);
    ts::return_to_sender(sc, admin_cap);

    // Vault + genesis deposit + custody.
    ts::next_tx(sc, CREATOR);
    let cfg = ts::take_shared<VaultProtocolConfig>(sc);
    vault::create_vault<USDC>(&cfg, CURATOR, 0, 1_000, 2, 8, 3_600_000, sc.ctx());
    ts::return_shared(cfg);

    ts::next_tx(sc, ADMIN);
    let clock = clock::create_for_testing(sc.ctx());

    ts::next_tx(sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(sc);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::deposit<USDC>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(balance::create_for_testing<USDC>(1_000_000_000), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock
}

fun init_custody(sc: &mut Scenario): ID {
    ts::next_tx(sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    let custody_id = adapter::init_custody(&mut v, &cap, &ireg, sc.ctx());
    ts::return_to_sender(sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
    custody_id
}

#[test]
fun custody_deposit_withdraw_roundtrip() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup(&mut sc);
    let custody_id = init_custody(&mut sc);

    ts::next_tx(&mut sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    adapter::deposit<USDC>(&mut v, &cap, &ireg, custody_id, 400_000_000, sc.ctx());
    assert!(vault::free_balance_of<USDC>(&v) == 600_000_000);
    {
        let custody: &DeepBookCustody = vault::borrow_position(&v, custody_id);
        assert!(adapter::custody_balance<USDC>(custody) == 400_000_000);
    };
    adapter::withdraw<USDC>(&mut v, &cap, &ireg, custody_id, 150_000_000, sc.ctx());
    assert!(vault::free_balance_of<USDC>(&v) == 750_000_000);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

/// The phase-2 gate: a NON-SHARED BalanceManager wrapped inside the
/// vault places and cancels a real order on a real DeepBook pool.
#[test]
fun wrapped_balance_manager_trades_on_pool() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup(&mut sc);
    let custody_id = init_custody(&mut sc);

    // Real DeepBook pool (whitelisted → no DEEP fees in test).
    ts::next_tx(&mut sc, ADMIN);
    let registry_id = registry::test_registry(sc.ctx());
    ts::next_tx(&mut sc, ADMIN);
    let mut db_registry = ts::take_shared_by_id<Registry>(&sc, registry_id);
    let db_admin = registry::get_admin_cap_for_testing(sc.ctx());
    let pool_id = pool::create_pool_admin<BASE, USDC>(
        &mut db_registry,
        1000, // tick
        1000, // lot
        10000, // min size
        true, // whitelisted
        false,
        &db_admin,
        sc.ctx(),
    );
    ts::return_shared(db_registry);
    destroy(db_admin);

    // Admin vets the pool for curators.
    ts::next_tx(&mut sc, ADMIN);
    let admin_cap = ts::take_from_sender<AdminCap>(&sc);
    let mut list = ts::take_shared<PoolAllowlist>(&sc);
    adapter::allow_pool(&admin_cap, &mut list, pool_id);
    ts::return_shared(list);
    ts::return_to_sender(&sc, admin_cap);

    // Fund the manager and place a resting bid through the wrapped BM.
    ts::next_tx(&mut sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let list = ts::take_shared<PoolAllowlist>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let mut db_pool = ts::take_shared_by_id<Pool<BASE, USDC>>(&sc, pool_id);
    adapter::deposit<USDC>(&mut v, &cap, &ireg, custody_id, 500_000_000, sc.ctx());
    adapter::place_limit_order<BASE, USDC>(
        &mut v,
        &cap,
        &ireg,
        &list,
        custody_id,
        &mut db_pool,
        1, // client order id
        constants::no_restriction(),
        constants::self_matching_allowed(),
        1_000_000, // price (multiple of tick)
        100_000, // quantity (multiple of lot, ≥ min)
        true, // bid
        false, // pay_with_deep
        constants::max_u64(),
        &clock,
        sc.ctx(),
    );

    // The bid locked quote value in the pool against our wrapped BM.
    {
        let custody: &DeepBookCustody = vault::borrow_position(&v, custody_id);
        assert!(adapter::custody_active_pools(custody).contains(&pool_id));
        // Appraisal covers manager balance + locked, all USDC → 1:1.
        let mut appraisal = vault::begin_appraisal<USDC>(&v);
        let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
        let mut ca = adapter::begin_custody_appraisal(&v, custody_id);
        adapter::value_asset<USDC>(&v, &cfg, &mut ca, option::none(), &clock);
        adapter::value_pool_locked<BASE, USDC>(
            &v,
            &cfg,
            &mut ca,
            &db_pool,
            option::none(),
            option::none(),
            option::none(),
            &clock,
        );
        adapter::finalize_custody_appraisal(&v, &mut appraisal, ca);
        // Nothing left the closed system: free 500 + custody-valued 500.
        assert!(vault::appraisal_value(&appraisal) == 1_000_000_000);
        let _consumed = appraisal; // dropped via deposit path in other tests
        ts::return_shared(cfg);
        destroy(_consumed);
    };

    // Cancel and retire: locked balance returns to the manager.
    adapter::cancel_all_orders<BASE, USDC>(
        &mut v,
        &cap,
        &ireg,
        custody_id,
        &mut db_pool,
        &clock,
        sc.ctx(),
    );
    adapter::retire_pool<BASE, USDC>(&mut v, &cap, &ireg, custody_id, &db_pool);
    adapter::withdraw<USDC>(&mut v, &cap, &ireg, custody_id, 500_000_000, sc.ctx());
    assert!(vault::free_balance_of<USDC>(&v) == 1_000_000_000);

    ts::return_shared(db_pool);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(list);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = deepbook_adapter::deepbook_adapter)]
fun unvetted_pool_rejected() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup(&mut sc);
    let custody_id = init_custody(&mut sc);

    ts::next_tx(&mut sc, ADMIN);
    let registry_id = registry::test_registry(sc.ctx());
    ts::next_tx(&mut sc, ADMIN);
    let mut db_registry = ts::take_shared_by_id<Registry>(&sc, registry_id);
    let db_admin = registry::get_admin_cap_for_testing(sc.ctx());
    let pool_id = pool::create_pool_admin<BASE, USDC>(
        &mut db_registry,
        1000,
        1000,
        10000,
        true,
        false,
        &db_admin,
        sc.ctx(),
    );
    ts::return_shared(db_registry);
    destroy(db_admin);

    ts::next_tx(&mut sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let list = ts::take_shared<PoolAllowlist>(&sc); // pool NOT allowed
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let mut db_pool = ts::take_shared_by_id<Pool<BASE, USDC>>(&sc, pool_id);
    adapter::place_limit_order<BASE, USDC>(
        &mut v,
        &cap,
        &ireg,
        &list,
        custody_id,
        &mut db_pool,
        1,
        constants::no_restriction(),
        constants::self_matching_allowed(),
        1_000_000,
        100_000,
        true,
        false,
        constants::max_u64(),
        &clock,
        sc.ctx(),
    );
    abort 0
}
