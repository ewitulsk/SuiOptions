#[test_only]
/// Exchange-adapter tests against REAL exchange code: custody lifecycle,
/// a live fill crediting the vault's cap-owned BalanceManager with the
/// appraisal picking it up from chain state, the closed donation lever,
/// and dead-curator force recovery.
module exchange_adapter::exchange_adapter_tests;

use sui::clock::{Self, Clock};
use sui::coin;
use sui::event;
use sui::sui::SUI;
use sui::test_scenario::{Self as ts, Scenario};

use exchange::admin;
use exchange::balance_manager::{Self as bm, BalanceManager};
use exchange::order;
use exchange::registry::{Self as ereg, SettlementRegistry};
use exchange::settlement;

use options_core::admin::ProtocolConfig as CoreProtocolConfig;

use trading_vault::events as tv_events;
use trading_vault::registry::{Self as vreg, IntegrationRegistry};
use trading_vault::test_helpers as th;
use trading_vault::vault::{Self, CuratorCap, TradingVault};

use exchange_adapter::exchange_adapter as adapter;

const TAKER: address = @0xC1;
const STRANGER: address = @0xE7;
const HOT: address = @0xB07;
const RELAYER: address = @0xD1;

const NOW: u64 = 1_000_000;
const EXPIRY: u64 = 2_000_000;

/// 1 SUI-raw = 2 USDC-raw.
const SUI_PRICE: u128 = 2_000_000_000_000;

fun setup(): (Scenario, Clock) {
    let mut sc = ts::begin(th::admin_addr());
    let mut clk = th::init_protocol(&mut sc);
    clk.set_for_testing(NOW);

    // Allowlist the adapter witness.
    ts::next_tx(&mut sc, th::admin_addr());
    let admin_cap = th::take_admin_cap(&sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(&sc);
    vreg::allow_adapter(
        &admin_cap,
        &mut ireg,
        std::type_name::with_defining_ids<adapter::ExchangeAdapter>(),
    );
    ts::return_shared(ireg);
    th::return_admin_cap(&sc, admin_cap);

    th::new_default_vault(&mut sc);
    th::simple_deposit(&mut sc, th::alice_addr(), 1_000_000, &clk);
    (sc, clk)
}

fun init_custody(sc: &mut Scenario): (ID, ID) {
    ts::next_tx(sc, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    let custody_id = adapter::init_custody(&mut v, &cap, &ireg, sc.ctx());
    let custody: &adapter::ExchangeCustody = vault::borrow_position(&v, custody_id);
    let bm_id = adapter::custody_bm_id(custody);
    ts::return_to_sender(sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
    (custody_id, bm_id)
}

fun fund_usdc(sc: &mut Scenario, custody_id: ID, bm_id: ID, amount: u64) {
    ts::next_tx(sc, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    let mut m = ts::take_shared_by_id<BalanceManager>(sc, bm_id);
    adapter::fund<th::USDC>(&mut v, &cap, &ireg, &mut m, custody_id, amount, sc.ctx());
    ts::return_shared(m);
    ts::return_to_sender(sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
}

#[test]
fun custody_fund_appraise_defund() {
    let (mut sc, clk) = setup();
    let (custody_id, bm_id) = init_custody(&mut sc);
    fund_usdc(&mut sc, custody_id, bm_id, 600_000);

    ts::next_tx(&mut sc, STRANGER);
    {
        let v = ts::take_shared<TradingVault>(&sc);
        let m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        assert!(bm::balance_of<th::USDC>(&m) == 600_000);
        assert!(vault::free_balance_of<th::USDC>(&v) == 400_000);
        assert!(vault::position_count(&v) == 1);

        // Permissionless appraisal from the SHARED manager's live state.
        let cfg = th::take_protocol_config(&sc);
        let mut appraisal = vault::begin_appraisal<th::USDC>(&v);
        let mut ca = adapter::begin_custody_appraisal(&v, custody_id);
        adapter::value_asset<th::USDC>(&v, &cfg, &mut ca, &m, option::none(), &clk);
        adapter::finalize_custody_appraisal(&v, &mut appraisal, ca);
        vault::crank_appraisal(&v, appraisal);
        let navs = event::events_by_type<tv_events::VaultAppraised>();
        let (_, nav, positions) = tv_events::vault_appraised_fields(&navs[navs.length() - 1]);
        assert!(nav == 1_000_000); // 400k free + 600k in the manager
        assert!(positions == 1);
        ts::return_shared(cfg);
        ts::return_shared(m);
        ts::return_shared(v);
    };

    // Defund everything back; the tracked-asset entry prunes.
    ts::next_tx(&mut sc, th::curator_addr());
    {
        let mut v = ts::take_shared<TradingVault>(&sc);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let cap = ts::take_from_sender<CuratorCap>(&sc);
        let mut m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        adapter::defund<th::USDC>(&mut v, &cap, &ireg, &mut m, custody_id, 600_000, sc.ctx());
        assert!(vault::free_balance_of<th::USDC>(&v) == 1_000_000);
        assert!(bm::balance_of<th::USDC>(&m) == 0);
        let custody: &adapter::ExchangeCustody = vault::borrow_position(&v, custody_id);
        assert!(adapter::custody_assets(custody).is_empty());
        ts::return_shared(m);
        ts::return_to_sender(&sc, cap);
        ts::return_shared(ireg);
        ts::return_shared(v);
    };

    clk.destroy_for_testing();
    sc.end();
}

#[test]
fun fill_credits_manager_and_appraisal_reflects() {
    let (mut sc, clk) = setup();
    let (custody_id, bm_id) = init_custody(&mut sc);
    fund_usdc(&mut sc, custody_id, bm_id, 500_000);

    // Zero-fee SUI/USDC market.
    ts::next_tx(&mut sc, th::admin_addr());
    {
        let ecap = admin::mint_for_testing(sc.ctx());
        ereg::create_market<SUI, th::USDC>(&ecap, 1, 1, 0, sc.ctx());
        admin::burn_for_testing(ecap);
    };

    // Track the base asset fills will bring in, and delegate a signer.
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_addr = {
        let mut v = ts::take_shared<TradingVault>(&sc);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let cap = ts::take_from_sender<CuratorCap>(&sc);
        let mut m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        adapter::track_asset<SUI>(&mut v, &cap, &ireg, &m, custody_id);
        adapter::add_signer(&mut v, &cap, &ireg, &mut m, custody_id, HOT);
        assert!(bm::is_approved_signer(&m, HOT));
        let addr = object::id(&v).to_address();
        ts::return_shared(m);
        ts::return_to_sender(&sc, cap);
        ts::return_shared(ireg);
        ts::return_shared(v);
        addr
    };

    // The vault's maker bid: buy 50_000 SUI for 100_000 USDC.
    let order_bytes = order::to_bytes(&order::new_for_testing(
        order::canonical_type<th::USDC>(), // maker sells quote
        order::canonical_type<SUI>(),
        100_000,
        50_000,
        0,
        vault_addr,
        bm_id,
        @0x0,
        @0x0,
        EXPIRY,
        1,
    ));

    // A taker fills it against the SHARED manager — no vault in the tx.
    ts::next_tx(&mut sc, TAKER);
    {
        let mut reg = ts::take_shared<SettlementRegistry<SUI, th::USDC>>(&sc);
        let mut m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        let (quote_out, base_change) = settlement::fill_limit_order_reverse_for_testing<
            SUI,
            th::USDC,
        >(
            &mut reg,
            &mut m,
            order_bytes,
            coin::mint_for_testing<SUI>(50_000, sc.ctx()),
            50_000,
            0,
            &clk,
            sc.ctx(),
        );
        assert!(quote_out.value() == 100_000);
        assert!(base_change.value() == 0);
        coin::burn_for_testing(quote_out);
        coin::burn_for_testing(base_change);
        assert!(bm::balance_of<th::USDC>(&m) == 400_000);
        assert!(bm::balance_of<SUI>(&m) == 50_000);
        ts::return_shared(m);
        ts::return_shared(reg);
    };

    // NAV picks the fill up from chain state: 500k free + 400k USDC +
    // 50k SUI × 2 = 1_000_000.
    ts::next_tx(&mut sc, STRANGER);
    {
        let v = ts::take_shared<TradingVault>(&sc);
        let m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        let cfg = th::take_protocol_config(&sc);
        let att = th::attest<SUI, th::USDC>(&sc, SUI_PRICE, clk.timestamp_ms());
        let mut appraisal = vault::begin_appraisal<th::USDC>(&v);
        let mut ca = adapter::begin_custody_appraisal(&v, custody_id);
        adapter::value_asset<th::USDC>(&v, &cfg, &mut ca, &m, option::none(), &clk);
        adapter::value_asset<SUI>(&v, &cfg, &mut ca, &m, option::some(att), &clk);
        adapter::finalize_custody_appraisal(&v, &mut appraisal, ca);
        vault::crank_appraisal(&v, appraisal);
        let navs = event::events_by_type<tv_events::VaultAppraised>();
        let (_, nav, _) = tv_events::vault_appraised_fields(&navs[navs.length() - 1]);
        assert!(nav == 1_000_000);
        ts::return_shared(cfg);
        ts::return_shared(m);
        ts::return_shared(v);
    };

    clk.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 6, location = exchange::balance_manager)]
fun stranger_deposit_into_vault_bm_aborts() {
    // The donation lever the share-inflation defense depends on staying
    // closed: a third party cannot push value into the vault's manager.
    let (mut sc, clk) = setup();
    let (_, bm_id) = init_custody(&mut sc);

    ts::next_tx(&mut sc, STRANGER);
    let mut m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
    bm::deposit(&mut m, coin::mint_for_testing<th::USDC>(1_000_000, sc.ctx()), sc.ctx());
    clk.destroy_for_testing();
    abort 0
}

#[test]
fun force_recovery_and_close_with_dead_curator() {
    let (mut sc, clk) = setup();
    let (custody_id, bm_id) = init_custody(&mut sc);
    fund_usdc(&mut sc, custody_id, bm_id, 600_000);

    // Curator delegates a bot key, then goes dark; admin closes.
    ts::next_tx(&mut sc, th::curator_addr());
    {
        let mut v = ts::take_shared<TradingVault>(&sc);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let cap = ts::take_from_sender<CuratorCap>(&sc);
        let mut m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        adapter::add_signer(&mut v, &cap, &ireg, &mut m, custody_id, HOT);
        vault::initiate_close(&mut v, &cap);
        ts::return_shared(m);
        ts::return_to_sender(&sc, cap);
        ts::return_shared(ireg);
        ts::return_shared(v);
    };

    // A stranger force-recovers: void the bot key, pull escrow home.
    ts::next_tx(&mut sc, STRANGER);
    {
        let mut v = ts::take_shared<TradingVault>(&sc);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let mut m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        adapter::force_remove_signer(&mut v, &ireg, &mut m, custody_id, HOT, &clk);
        assert!(!bm::is_approved_signer(&m, HOT));
        adapter::force_defund_all<th::USDC>(&mut v, &ireg, &mut m, custody_id, &clk, sc.ctx());
        assert!(bm::balance_of<th::USDC>(&m) == 0);
        assert!(vault::free_balance_of<th::USDC>(&v) == 1_000_000);
        ts::return_shared(m);
        ts::return_shared(ireg);
        ts::return_shared(v);
    };

    // The curator (or cap holder) ejects the empty shell; close lands.
    ts::next_tx(&mut sc, th::curator_addr());
    {
        let mut v = ts::take_shared<TradingVault>(&sc);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let cap = ts::take_from_sender<CuratorCap>(&sc);
        adapter::eject_empty_custody(&mut v, &cap, &ireg, custody_id, th::curator_addr());
        assert!(vault::position_count(&v) == 0);
        vault::finalize_close(&mut v);
        assert!(vault::is_closed(&v));
        ts::return_to_sender(&sc, cap);
        ts::return_shared(ireg);
        ts::return_shared(v);
    };

    clk.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 2, location = exchange_adapter::exchange_adapter)]
fun fund_through_wrong_manager_aborts() {
    let (mut sc, clk) = setup();
    let (custody_id, _) = init_custody(&mut sc);

    // A manager the custody does not own.
    ts::next_tx(&mut sc, STRANGER);
    let foreign_bm = bm::new(sc.ctx());

    ts::next_tx(&mut sc, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let mut m = ts::take_shared_by_id<BalanceManager>(&sc, foreign_bm);
    adapter::fund<th::USDC>(&mut v, &cap, &ireg, &mut m, custody_id, 1, sc.ctx());
    clk.destroy_for_testing();
    abort 0
}

// ═══════════════════ direct vault escrow (SO-372) ═══════════════════

fun enable_quote(sc: &mut Scenario, vault_id: ID) {
    ts::next_tx(sc, th::curator_addr());
    let mut v = ts::take_shared_by_id<TradingVault>(sc, vault_id);
    let cap = ts::take_from_sender_by_id<CuratorCap>(sc, vault::curator_cap_id(&v));
    vault::add_quote_adapter<adapter::ExchangeAdapter>(&mut v, &cap);
    ts::return_to_sender(sc, cap);
    ts::return_shared(v);
}

fun init_direct(sc: &mut Scenario, vault_id: ID): (ID, ID) {
    ts::next_tx(sc, th::curator_addr());
    let mut v = ts::take_shared_by_id<TradingVault>(sc, vault_id);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let cap = ts::take_from_sender_by_id<CuratorCap>(sc, vault::curator_cap_id(&v));
    let custody_id = adapter::init_direct_custody(&mut v, &cap, &ireg, sc.ctx());
    let custody: &adapter::ExchangeCustody = vault::borrow_position(&v, custody_id);
    let bm_id = adapter::custody_bm_id(custody);
    ts::return_to_sender(sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
    (custody_id, bm_id)
}

fun create_market_zero_fee(sc: &mut Scenario) {
    ts::next_tx(sc, th::admin_addr());
    let ecap = admin::mint_for_testing(sc.ctx());
    ereg::create_market<SUI, th::USDC>(&ecap, 1, 1, 0, sc.ctx());
    admin::burn_for_testing(ecap);
}

/// Maker sells `ma` SUI (base) for `ta` USDC.
fun ask_order(maker: address, bm_id: ID, ma: u64, ta: u64, salt: u64): vector<u8> {
    order::to_bytes(&order::new_for_testing(
        order::canonical_type<SUI>(),
        order::canonical_type<th::USDC>(),
        ma, ta, 0, maker, bm_id, @0x0, @0x0, EXPIRY, salt,
    ))
}

/// Maker sells `ma` USDC (quote) for `ta` SUI.
fun bid_order(maker: address, bm_id: ID, ma: u64, ta: u64, salt: u64): vector<u8> {
    order::to_bytes(&order::new_for_testing(
        order::canonical_type<th::USDC>(),
        order::canonical_type<SUI>(),
        ma, ta, 0, maker, bm_id, @0x0, @0x0, EXPIRY, salt,
    ))
}

#[test]
fun direct_taker_fill_settles_from_vault_free_balances() {
    let (mut sc, clk) = setup();
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_id = {
        let v = ts::take_shared<TradingVault>(&sc);
        let id = object::id(&v);
        ts::return_shared(v);
        id
    };
    let (custody_id, bm_id) = init_direct(&mut sc, vault_id);
    enable_quote(&mut sc, vault_id);
    create_market_zero_fee(&mut sc);

    // The vault bids: buy 50_000 SUI for 100_000 USDC, escrowed by the
    // VAULT — the identity manager never holds a unit.
    let ord = bid_order(vault_id.to_address(), bm_id, 100_000, 50_000, 1);

    ts::next_tx(&mut sc, TAKER);
    {
        let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_id);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let mut reg = ts::take_shared<SettlementRegistry<SUI, th::USDC>>(&sc);
        let m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
        let (quote_out, base_change) = adapter::fill_vault_order_reverse_for_testing<
            SUI,
            th::USDC,
        >(
            &mut v,
            &ireg,
            &mut reg,
            &m,
            custody_id,
            ord,
            coin::mint_for_testing<SUI>(50_000, sc.ctx()),
            50_000,
            0,
            &clk,
            sc.ctx(),
        );
        assert!(quote_out.value() == 100_000);
        assert!(base_change.value() == 0);
        coin::burn_for_testing(quote_out);
        coin::burn_for_testing(base_change);
        // Escrow was the vault itself.
        assert!(vault::free_balance_of<th::USDC>(&v) == 900_000);
        assert!(vault::free_balance_of<SUI>(&v) == 50_000);
        assert!(bm::balance_of<th::USDC>(&m) == 0);
        assert!(bm::balance_of<SUI>(&m) == 0);
        let quote_fills = event::events_by_type<adapter::VaultQuoteFilled>();
        assert!(quote_fills.length() == 1);
        ts::return_shared(m);
        ts::return_shared(reg);
        ts::return_shared(ireg);
        ts::return_shared(v);
    };

    clk.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 113, location = trading_vault::vault)]
fun direct_fill_refused_without_quote_opt_in() {
    let (mut sc, clk) = setup();
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_id = {
        let v = ts::take_shared<TradingVault>(&sc);
        let id = object::id(&v);
        ts::return_shared(v);
        id
    };
    let (custody_id, bm_id) = init_direct(&mut sc, vault_id);
    create_market_zero_fee(&mut sc);
    let ord = bid_order(vault_id.to_address(), bm_id, 100_000, 50_000, 1);

    ts::next_tx(&mut sc, TAKER);
    let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_id);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut reg = ts::take_shared<SettlementRegistry<SUI, th::USDC>>(&sc);
    let m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
    let (a, b) = adapter::fill_vault_order_reverse_for_testing<SUI, th::USDC>(
        &mut v, &ireg, &mut reg, &m, custody_id, ord,
        coin::mint_for_testing<SUI>(50_000, sc.ctx()), 50_000, 0, &clk, sc.ctx(),
    );
    coin::burn_for_testing(a);
    coin::burn_for_testing(b);
    abort 0
}

#[test]
#[expected_failure(abort_code = 8, location = exchange_adapter::exchange_adapter)]
fun fund_refused_on_direct_custody() {
    let (mut sc, clk) = setup();
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_id = {
        let v = ts::take_shared<TradingVault>(&sc);
        let id = object::id(&v);
        ts::return_shared(v);
        id
    };
    let (custody_id, bm_id) = init_direct(&mut sc, vault_id);

    ts::next_tx(&mut sc, th::curator_addr());
    let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_id);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender_by_id<CuratorCap>(&sc, vault::curator_cap_id(&v));
    let mut m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
    adapter::fund<th::USDC>(&mut v, &cap, &ireg, &mut m, custody_id, 1, sc.ctx());
    clk.destroy_for_testing();
    abort 0
}

#[test]
#[expected_failure(abort_code = 9, location = exchange_adapter::exchange_adapter)]
fun starved_base_escrow_aborts_side_tagged() {
    let (mut sc, clk) = setup();
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_id = {
        let v = ts::take_shared<TradingVault>(&sc);
        let id = object::id(&v);
        ts::return_shared(v);
        id
    };
    let (custody_id, bm_id) = init_direct(&mut sc, vault_id);
    enable_quote(&mut sc, vault_id);
    create_market_zero_fee(&mut sc);
    // The vault quotes an ask but holds no SUI at all.
    let ord = ask_order(vault_id.to_address(), bm_id, 50_000, 100_000, 1);

    ts::next_tx(&mut sc, TAKER);
    let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_id);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut reg = ts::take_shared<SettlementRegistry<SUI, th::USDC>>(&sc);
    let m = ts::take_shared_by_id<BalanceManager>(&sc, bm_id);
    let (a, b) = adapter::fill_vault_order_for_testing<SUI, th::USDC>(
        &mut v, &ireg, &mut reg, &m, custody_id, ord,
        coin::mint_for_testing<th::USDC>(100_000, sc.ctx()), 100_000, 0, &clk, sc.ctx(),
    );
    coin::burn_for_testing(a);
    coin::burn_for_testing(b);
    abort 0
}

#[test]
#[expected_failure(abort_code = 11, location = exchange_adapter::exchange_adapter)]
fun self_cross_via_own_funded_manager_rejected() {
    let (mut sc, clk) = setup();
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_id = {
        let v = ts::take_shared<TradingVault>(&sc);
        let id = object::id(&v);
        ts::return_shared(v);
        id
    };
    let (direct_custody, direct_bm) = init_direct(&mut sc, vault_id);
    // The vault's OTHER custody: funded, same owner address.
    let (_, funded_bm) = init_custody(&mut sc);
    enable_quote(&mut sc, vault_id);
    create_market_zero_fee(&mut sc);

    let ord_a = ask_order(vault_id.to_address(), direct_bm, 50_000, 100_000, 1);
    let ord_b = bid_order(vault_id.to_address(), funded_bm, 105_000, 50_000, 2);

    ts::next_tx(&mut sc, TAKER);
    let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_id);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut reg = ts::take_shared<SettlementRegistry<SUI, th::USDC>>(&sc);
    let ma = ts::take_shared_by_id<BalanceManager>(&sc, direct_bm);
    let mut mb = ts::take_shared_by_id<BalanceManager>(&sc, funded_bm);
    adapter::match_vault_vs_bm_for_testing<SUI, th::USDC>(
        &mut v, &ireg, &mut reg, &ma, direct_custody, &mut mb,
        ord_a, ord_b, 50_000, &clk, sc.ctx(),
    );
    abort 0
}

#[test]
fun vault_vs_vault_match_settles_both_sides() {
    let (mut sc, clk) = setup(); // vault A + 1M USDC deposit
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_a = {
        let v = ts::take_shared<TradingVault>(&sc);
        let id = object::id(&v);
        ts::return_shared(v);
        id
    };
    // Vault A sells SUI: give it inventory via a curator session.
    ts::next_tx(&mut sc, th::curator_addr());
    {
        let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_a);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let cap = ts::take_from_sender_by_id<CuratorCap>(&sc, vault::curator_cap_id(&v));
        let mut s = vault::begin_session(&v, &cap, &ireg, th::test_adapter());
        vault::put<SUI>(&mut v, &mut s, sui::balance::create_for_testing<SUI>(50_000));
        vault::end_session(&v, s);
        ts::return_to_sender(&sc, cap);
        ts::return_shared(ireg);
        ts::return_shared(v);
    };

    // Vault B: fresh vault with 105_000 USDC.
    let vault_b = th::new_default_vault(&mut sc);
    ts::next_tx(&mut sc, h_bob());
    {
        let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_b);
        let cfg = th::take_protocol_config(&sc);
        let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
        let appraisal = vault::begin_appraisal<th::USDC>(&v);
        vault::deposit<th::USDC>(
            &mut v,
            &cfg,
            &core_cfg,
            appraisal,
            coin::from_balance(th::mint<th::USDC>(105_000), sc.ctx()),
            option::none(),
            &clk,
            sc.ctx(),
        );
        ts::return_shared(core_cfg);
        ts::return_shared(cfg);
        ts::return_shared(v);
    };

    let (custody_a, bm_a) = init_direct(&mut sc, vault_a);
    let (custody_b, bm_b) = init_direct(&mut sc, vault_b);
    enable_quote(&mut sc, vault_a);
    enable_quote(&mut sc, vault_b);
    create_market_zero_fee(&mut sc);

    // A resting (salt 1) sells 50_000 SUI at 2.0; B (salt 2) bids up to
    // 2.1 → executes at A's price: 100_000 USDC for 50_000 SUI.
    let ord_a = ask_order(vault_a.to_address(), bm_a, 50_000, 100_000, 1);
    let ord_b = bid_order(vault_b.to_address(), bm_b, 105_000, 50_000, 2);

    ts::next_tx(&mut sc, RELAYER);
    {
        let mut va = ts::take_shared_by_id<TradingVault>(&sc, vault_a);
        let mut vb = ts::take_shared_by_id<TradingVault>(&sc, vault_b);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let mut reg = ts::take_shared<SettlementRegistry<SUI, th::USDC>>(&sc);
        let ma = ts::take_shared_by_id<BalanceManager>(&sc, bm_a);
        let mb = ts::take_shared_by_id<BalanceManager>(&sc, bm_b);
        adapter::match_vault_vs_vault_for_testing<SUI, th::USDC>(
            &mut va, custody_a, &ma, &mut vb, custody_b, &mb,
            &ireg, &mut reg, ord_a, ord_b, 50_000, &clk, sc.ctx(),
        );
        // A: −50_000 SUI, +100_000 USDC. B: −100_000 USDC, +50_000 SUI.
        assert!(vault::free_balance_of<SUI>(&va) == 0);
        assert!(vault::free_balance_of<th::USDC>(&va) == 1_100_000);
        assert!(vault::free_balance_of<SUI>(&vb) == 50_000);
        assert!(vault::free_balance_of<th::USDC>(&vb) == 5_000);
        // Neither identity manager ever held funds.
        assert!(bm::balance_of<SUI>(&ma) == 0 && bm::balance_of<th::USDC>(&ma) == 0);
        assert!(bm::balance_of<SUI>(&mb) == 0 && bm::balance_of<th::USDC>(&mb) == 0);
        let quote_fills = event::events_by_type<adapter::VaultQuoteFilled>();
        assert!(quote_fills.length() == 2);
        ts::return_shared(mb);
        ts::return_shared(ma);
        ts::return_shared(reg);
        ts::return_shared(ireg);
        ts::return_shared(vb);
        ts::return_shared(va);
    };

    clk.destroy_for_testing();
    sc.end();
}

#[test]
fun direct_custody_appraises_empty_and_deposits_still_work() {
    let (mut sc, clk) = setup();
    ts::next_tx(&mut sc, th::curator_addr());
    let vault_id = {
        let v = ts::take_shared<TradingVault>(&sc);
        let id = object::id(&v);
        ts::return_shared(v);
        id
    };
    let (custody_id, _) = init_direct(&mut sc, vault_id);

    // The custody is a position, so appraisals must cover it — with an
    // empty (zero) custody appraisal: no double-count, no wedge.
    ts::next_tx(&mut sc, th::alice_addr());
    let mut v = ts::take_shared_by_id<TradingVault>(&sc, vault_id);
    let cfg = th::take_protocol_config(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let mut appraisal = vault::begin_appraisal<th::USDC>(&v);
    let ca = adapter::begin_custody_appraisal(&v, custody_id);
    adapter::finalize_custody_appraisal(&v, &mut appraisal, ca);
    vault::deposit<th::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        appraisal,
        coin::from_balance(th::mint<th::USDC>(500_000), sc.ctx()),
        option::none(),
        &clk,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
    // NAV was exactly the 1M free balance: the new stake matches the
    // genesis stake 1:2 in value.
    let (shares, _, _) = vault::stake_of(&v, th::alice_addr());
    assert!(shares > 0);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clk.destroy_for_testing();
    sc.end();
}

fun h_bob(): address { th::bob_addr() }
