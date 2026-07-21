/// End-to-end test of the computed DBM equity leg: a real MarginManager
/// (owned by the vault's registered external account) with deposits is
/// valued into a trading-vault appraisal via price-attestation legs.
#[test_only]
module dbm_oracle::dbm_oracle_tests;

use std::type_name;
use sui::clock::Clock;
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use deepbook::pool::Pool;
use deepbook::registry::Registry;
use deepbook_margin::margin_manager::{Self, MarginManager};
use deepbook_margin::margin_registry::MarginRegistry;
use deepbook_margin::test_constants::{Self, SUI, USDC as MUSDC};
use deepbook_margin::test_helpers as mth;

use trading_vault::registry::{Self as tv_registry, OracleRegistry};
use trading_vault::test_helpers::{Self as th, USDC};
use trading_vault::vault::{Self, TradingVault};

use dbm_oracle::dbm_oracle::{Self as dbm, DbmOracle};

fun external_addr(): address { @0xF00D }

/// TestOracle attestation Asset→Quote against a HELD registry reference
/// (`th::attest` takes/returns the shared registry itself, so it cannot
/// be called while holding it or twice in one tx).
fun attest<Asset, Quote>(
    oreg: &OracleRegistry,
    price_scaled: u128,
    timestamp_ms: u64,
): trading_vault::price::PriceAttestation {
    trading_vault::price::attest(
        th::test_oracle(),
        oreg,
        type_name::with_defining_ids<Asset>(),
        type_name::with_defining_ids<Quote>(),
        price_scaled,
        timestamp_ms,
    )
}

/// Margin fixture: registry + margin pools + SUI/MUSDC deepbook pool with
/// margin enabled, and a MarginManager owned by `external_addr` holding
/// 100 raw SUI + 200 raw MUSDC (no debt). Returns (scenario, clock,
/// pool_id, manager_id).
fun setup_margin(): (Scenario, Clock, ID, ID) {
    let (mut scenario, clock, admin_cap, maintainer_cap) = mth::setup_margin_registry();

    let _sui_mp = mth::create_margin_pool<SUI>(
        &mut scenario,
        &maintainer_cap,
        mth::default_protocol_config(),
        &clock,
    );
    let _usdc_mp = mth::create_margin_pool<MUSDC>(
        &mut scenario,
        &maintainer_cap,
        mth::default_protocol_config(),
        &clock,
    );

    let (pool_id, _registry_id) = mth::create_pool_for_testing<SUI, MUSDC>(&mut scenario);

    ts::next_tx(&mut scenario, test_constants::admin());
    let mut margin_registry = ts::take_shared<MarginRegistry>(&scenario);
    mth::enable_deepbook_margin_on_pool<SUI, MUSDC>(
        pool_id,
        &mut margin_registry,
        &admin_cap,
        &clock,
        &mut scenario,
    );
    ts::return_shared(margin_registry);

    // The external account creates and funds its manager.
    ts::next_tx(&mut scenario, external_addr());
    let pool = ts::take_shared_by_id<Pool<SUI, MUSDC>>(&scenario, pool_id);
    let db_registry = ts::take_shared<Registry>(&scenario);
    let mut margin_registry = ts::take_shared<MarginRegistry>(&scenario);
    let manager_id = margin_manager::new<SUI, MUSDC>(
        &pool,
        &db_registry,
        &mut margin_registry,
        &clock,
        scenario.ctx(),
    );

    ts::next_tx(&mut scenario, external_addr());
    let mut manager = ts::take_shared_by_id<MarginManager<SUI, MUSDC>>(&scenario, manager_id);
    let base_oracle = mth::build_price_info_for_type<SUI>(&mut scenario, &clock);
    let quote_oracle = mth::build_price_info_for_type<MUSDC>(&mut scenario, &clock);
    ts::next_tx(&mut scenario, external_addr());
    manager.deposit<SUI, MUSDC, SUI>(
        &margin_registry,
        &base_oracle,
        &quote_oracle,
        coin::mint_for_testing<SUI>(100, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    manager.deposit<SUI, MUSDC, MUSDC>(
        &margin_registry,
        &base_oracle,
        &quote_oracle,
        coin::mint_for_testing<MUSDC>(200, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    std::unit_test::destroy(base_oracle);
    std::unit_test::destroy(quote_oracle);
    ts::return_shared(manager);
    ts::return_shared(margin_registry);
    ts::return_shared(db_registry);
    ts::return_shared(pool);
    std::unit_test::destroy(admin_cap);
    std::unit_test::destroy(maintainer_cap);

    (scenario, clock, pool_id, manager_id)
}

/// Vault fixture inside the same scenario: protocol init, DbmOracle
/// allowlisted, USDC vault with alice's 1000, external account registered
/// with the DbmOracle witness pinned. Destroys the vault-side clock and
/// keeps using the margin clock.
fun setup_vault(scenario: &mut Scenario) {
    let vclock = th::init_protocol(scenario);
    vclock.destroy_for_testing();

    ts::next_tx(scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(scenario);
    let mut oreg = ts::take_shared<OracleRegistry>(scenario);
    tv_registry::allow_oracle(&admin_cap, &mut oreg, type_name::with_defining_ids<DbmOracle>());
    ts::return_shared(oreg);
    th::return_admin_cap(scenario, admin_cap);

    th::new_default_vault(scenario);

    ts::next_tx(scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(scenario);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    vault::set_external_account(
        &admin_cap,
        &mut v,
        &oreg,
        external_addr(),
        type_name::with_defining_ids<DbmOracle>(),
        5_000,
        2_500,
    );
    ts::return_shared(oreg);
    ts::return_shared(v);
    th::return_admin_cap(scenario, admin_cap);
}

#[test]
fun computed_equity_prices_deposits_at_true_nav() {
    let (mut scenario, clock, pool_id, manager_id) = setup_margin();
    setup_vault(&mut scenario);
    // Alice funds 1000 with the equity leg (manager already holds assets).
    let now = clock.timestamp_ms();

    ts::next_tx(&mut scenario, th::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let pool = ts::take_shared_by_id<Pool<SUI, MUSDC>>(&scenario, pool_id);
    let manager = ts::take_shared_by_id<MarginManager<SUI, MUSDC>>(&scenario, manager_id);

    // SUI→USDC at 3.0, MUSDC→USDC at 1.0 (1e12 scale):
    // equity = 100×3 + 200×1 = 500.
    let att_sui = attest<SUI, USDC>(&oreg, 3_000_000_000_000, now);
    let att_musdc = attest<MUSDC, USDC>(&oreg, 1_000_000_000_000, now);
    let att_sui2 = attest<SUI, USDC>(&oreg, 3_000_000_000_000, now);
    let att_musdc2 = attest<MUSDC, USDC>(&oreg, 1_000_000_000_000, now);

    let mut a = vault::begin_appraisal<USDC>(&v);
    dbm::record_no_debt<SUI, MUSDC>(
        &v,
        &oreg,
        &cfg,
        &mut a,
        &manager,
        &pool,
        option::some(att_sui),
        option::some(att_musdc),
        &clock,
    );
    assert!(vault::appraisal_value(&a) == 500, 0);
    vault::deposit<USDC>(
        &mut v,
        &cfg,
        a,
        coin::from_balance(th::mint<USDC>(1_000), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    // First deposit into an empty vault mints 1:1 regardless of the leg.
    let (alice_shares, _, _) = vault::stake_of(&v, th::alice_addr());
    assert!(alice_shares == 1_000, 0);

    // Second depositor pays true NAV: 1000 cash + 500 equity.
    let mut a2 = vault::begin_appraisal<USDC>(&v);
    dbm::record_no_debt<SUI, MUSDC>(
        &v,
        &oreg,
        &cfg,
        &mut a2,
        &manager,
        &pool,
        option::some(att_sui2),
        option::some(att_musdc2),
        &clock,
    );
    assert!(vault::appraisal_value(&a2) == 1_500, 0);
    ts::next_tx(&mut scenario, th::bob_addr());
    vault::deposit<USDC>(
        &mut v,
        &cfg,
        a2,
        coin::from_balance(th::mint<USDC>(300), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    let (bob_shares, _, _) = vault::stake_of(&v, th::bob_addr());
    // 300 × 1000 / 1500 = 200.
    assert!(bob_shares == 200, 0);

    ts::return_shared(manager);
    ts::return_shared(pool);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 1, location = dbm_oracle::dbm_oracle)] // E_NOT_VAULT_ACCOUNT
fun manager_not_owned_by_registered_account_aborts() {
    let (mut scenario, clock, pool_id, manager_id) = setup_margin();
    setup_vault(&mut scenario);

    // Re-register the external account to a different address: the
    // manager's owner no longer matches.
    ts::next_tx(&mut scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    vault::set_external_account(
        &admin_cap,
        &mut v,
        &oreg,
        @0xBEEF,
        type_name::with_defining_ids<DbmOracle>(),
        5_000,
        2_500,
    );
    ts::return_shared(oreg);
    ts::return_shared(v);
    th::return_admin_cap(&scenario, admin_cap);

    ts::next_tx(&mut scenario, th::alice_addr());
    let now = clock.timestamp_ms();
    let v = ts::take_shared<TradingVault>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let pool = ts::take_shared_by_id<Pool<SUI, MUSDC>>(&scenario, pool_id);
    let manager = ts::take_shared_by_id<MarginManager<SUI, MUSDC>>(&scenario, manager_id);
    let att_sui = attest<SUI, USDC>(&oreg, 3_000_000_000_000, now);
    let att_musdc = attest<MUSDC, USDC>(&oreg, 1_000_000_000_000, now);
    let mut a = vault::begin_appraisal<USDC>(&v);
    dbm::record_no_debt<SUI, MUSDC>(
        &v,
        &oreg,
        &cfg,
        &mut a,
        &manager,
        &pool,
        option::some(att_sui),
        option::some(att_musdc),
        &clock,
    );
    abort 999
}

#[test]
#[expected_failure(abort_code = 3, location = dbm_oracle::dbm_oracle)] // E_MISSING_ATTESTATION
fun missing_leg_attestation_aborts() {
    let (mut scenario, clock, pool_id, manager_id) = setup_margin();
    setup_vault(&mut scenario);

    ts::next_tx(&mut scenario, th::alice_addr());
    let now = clock.timestamp_ms();
    let v = ts::take_shared<TradingVault>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let pool = ts::take_shared_by_id<Pool<SUI, MUSDC>>(&scenario, pool_id);
    let manager = ts::take_shared_by_id<MarginManager<SUI, MUSDC>>(&scenario, manager_id);
    let att_musdc = attest<MUSDC, USDC>(&oreg, 1_000_000_000_000, now);
    let mut a = vault::begin_appraisal<USDC>(&v);
    dbm::record_no_debt<SUI, MUSDC>(
        &v,
        &oreg,
        &cfg,
        &mut a,
        &manager,
        &pool,
        option::none(),
        option::some(att_musdc),
        &clock,
    );
    abort 999
}
