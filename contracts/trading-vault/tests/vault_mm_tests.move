#[test_only]
module trading_vault::vault_mm_tests;

use std::string;
use sui::balance;
use sui::coin;
use sui::test_scenario as ts;

use std::type_name;

use whitelist::whitelist::Whitelist;
use options_core::bucket::{Self, Bucket};
use options_core::collateral::{Self, CollateralRequest};
use options_core::position::{Self, Position};
use options_core::put_bucket::{Self, PutBucket};
use options_core::quote;

use trading_vault::registry as tv_registry;
use trading_vault::registry::{IntegrationRegistry, OracleRegistry};

use trading_vault::test_helpers as h;
use trading_vault::vault::{Self, CuratorCap, TradingVault};
use trading_vault::vault_mm;

fun request_for(
    vault: &TradingVault,
    source: ID,
    recipient: address,
    amount: u64,
): CollateralRequest<h::USDC> {
    let q = quote::new_quote(
        b"proto",
        object::id(vault), // signer id (unused here)
        source,
        @0x0,
        string::utf8(b"vault_mm"),
        recipient,
        object::id(vault), // bucket id placeholder
        amount,
        0,
        0,
        1,
    );
    collateral::new_request_for_testing<h::USDC>(q, amount, false)
}

#[test]
fun release_pulls_collateral_when_enabled_and_bound_to_vault() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Curator opts in.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_mm_release_enabled(&mut v, &cap, true);
    ts::return_to_sender(&sc, cap);

    // A core-minted request naming this vault as source AND recipient
    // releases collateral.
    ts::next_tx(&mut sc, h::bob_addr());
    let vault_id = object::id(&v);
    let req = request_for(&v, vault_id, vault_id.to_address(), 250_000);
    let funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    assert!(funds.value() == 250_000);
    assert!(vault::free_balance_of<h::USDC>(&v) == 750_000);
    balance::destroy_for_testing(funds);
    collateral::destroy_for_testing(req);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 3, location = trading_vault::vault_mm)]
fun release_rejected_when_disabled() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let vault_id = object::id(&v);
    let req = request_for(&v, vault_id, vault_id.to_address(), 250_000);
    let _funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    abort 0
}

#[test]
#[expected_failure(abort_code = 2, location = trading_vault::vault_mm)]
fun release_rejected_when_outputs_routed_elsewhere() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_mm_release_enabled(&mut v, &cap, true);
    ts::return_to_sender(&sc, cap);

    // The curator's bot signs a quote routing outputs to the CURATOR:
    // theft attempt, refused.
    ts::next_tx(&mut sc, h::curator_addr());
    let vault_id = object::id(&v);
    let req = request_for(&v, vault_id, h::curator_addr(), 250_000);
    let _funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    abort 0
}

#[test]
#[expected_failure(abort_code = 1, location = trading_vault::vault_mm)]
fun release_rejected_for_wrong_source() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_mm_release_enabled(&mut v, &cap, true);
    ts::return_to_sender(&sc, cap);

    ts::next_tx(&mut sc, h::bob_addr());
    let vault_id = object::id(&v);
    let other = object::id_from_address(@0xBEEF);
    let req = request_for(&v, other, vault_id.to_address(), 250_000);
    let _funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    abort 0
}

/// Per-bucket put coin marker for the held-coin appraisal test.
public struct PUTX has drop {}

/// A held put coin marks at max(strike payout − spot cost, 0) while live
/// and at zero once expired.
#[test]
fun held_put_coin_appraises_at_intrinsic_then_zero_after_expiry() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Allowlist the VaultMm witness + create a BTC/USDC put bucket:
    // strike 2.0 USDC per BTC raw unit (scale 12), expiry 10_000s.
    ts::next_tx(&mut sc, h::admin_addr());
    let admin_cap = h::take_admin_cap(&sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(&sc);
    tv_registry::allow_adapter(
        &admin_cap,
        &mut ireg,
        type_name::with_defining_ids<vault_mm::VaultMm>(),
    );
    ts::return_shared(ireg);
    let tcap = coin::create_treasury_cap_for_testing<PUTX>(sc.ctx());
    put_bucket::create_put_bucket<h::BTC, h::USDC, PUTX>(
        &admin_cap,
        tcap,
        10_000_000,
        2_000_000_000_000,
        12,
        sc.ctx(),
    );
    h::return_admin_cap(&sc, admin_cap);

    // 100_000 put units land at the vault address (writer-flow shape) and
    // are swept into custody as a position.
    ts::next_tx(&mut sc, h::bob_addr());
    let put = coin::mint_for_testing<PUTX>(100_000, sc.ctx());
    let coin_id = object::id(&put);
    transfer::public_transfer(put, vault_id.to_address());

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let ticket = ts::most_recent_receiving_ticket<coin::Coin<PUTX>>(&vault_id);
    vault_mm::receive_mm_option_coin<PUTX>(&mut v, &ireg, ticket);
    assert!(vault::position_count(&v) == 1);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Live: spot 0.5 → payout 200_000 − spot cost 50_000 = 150_000.
    ts::next_tx(&mut sc, h::bob_addr());
    clock.set_for_testing(5_000);
    let v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let bucket = ts::take_shared<PutBucket<h::BTC, h::USDC, PUTX>>(&sc);
    let att = h::attest<h::BTC, h::USDC>(&sc, 500_000_000_000, 5_000);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault_mm::appraise_put_coin<h::BTC, h::USDC, PUTX>(
        &v,
        &cfg,
        &mut appraisal,
        &bucket,
        coin_id,
        option::some(att),
        option::none(),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 1_000_000 + 150_000);
    sui::test_utils::destroy(appraisal);

    // Expired: zero, no attestations needed.
    clock.set_for_testing(10_000_001);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault_mm::appraise_put_coin<h::BTC, h::USDC, PUTX>(
        &v,
        &cfg,
        &mut appraisal,
        &bucket,
        coin_id,
        option::none(),
        option::none(),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 1_000_000);
    sui::test_utils::destroy(appraisal);
    ts::return_shared(bucket);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

// ═══════════ curator exits (SO-299): exercise / offset / release ═══════════

/// Per-bucket call coin marker for the exit tests.
public struct CALLX has drop {}

fun allow_vault_mm(sc: &mut ts::Scenario) {
    ts::next_tx(sc, h::admin_addr());
    let admin_cap = h::take_admin_cap(sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(sc);
    tv_registry::allow_adapter(
        &admin_cap,
        &mut ireg,
        type_name::with_defining_ids<vault_mm::VaultMm>(),
    );
    ts::return_shared(ireg);
    h::return_admin_cap(sc, admin_cap);
}

/// BTC/USDC call bucket: strike 2.0 (scale 12), expiry 10_000s.
fun create_call_bucket(sc: &mut ts::Scenario) {
    ts::next_tx(sc, h::admin_addr());
    let admin_cap = h::take_admin_cap(sc);
    let tcap = coin::create_treasury_cap_for_testing<CALLX>(sc.ctx());
    bucket::create_bucket<h::BTC, h::USDC, CALLX>(
        &admin_cap,
        tcap,
        10_000_000,
        2_000_000_000_000,
        12,
        sc.ctx(),
    );
    h::return_admin_cap(sc, admin_cap);
}

/// BTC/USDC put bucket twin: strike 2.0 (scale 12), expiry 10_000s.
fun create_put_bucket(sc: &mut ts::Scenario) {
    ts::next_tx(sc, h::admin_addr());
    let admin_cap = h::take_admin_cap(sc);
    let tcap = coin::create_treasury_cap_for_testing<PUTX>(sc.ctx());
    put_bucket::create_put_bucket<h::BTC, h::USDC, PUTX>(
        &admin_cap,
        tcap,
        10_000_000,
        2_000_000_000_000,
        12,
        sc.ctx(),
    );
    h::return_admin_cap(sc, admin_cap);
}

/// Bob writes `amount` covered calls; the coin (and optionally the
/// Position) transfer to the vault address. Returns (position_id, coin_id).
fun write_calls_to_vault(
    sc: &mut ts::Scenario,
    vault_id: ID,
    amount: u64,
    send_position: bool,
    clock: &sui::clock::Clock,
): (ID, ID) {
    ts::next_tx(sc, h::bob_addr());
    let mut bucket = ts::take_shared<Bucket<h::BTC, h::USDC, CALLX>>(sc);
    let wl = ts::take_shared<Whitelist>(sc);
    let (pos, call) = bucket::write_collateralized(
        &mut bucket,
        &wl,
        coin::mint_for_testing<h::BTC>(amount, sc.ctx()),
        clock,
        sc.ctx(),
    );
    ts::return_shared(wl);
    let position_id = object::id(&pos);
    let coin_id = object::id(&call);
    transfer::public_transfer(call, vault_id.to_address());
    if (send_position) {
        transfer::public_transfer(pos, vault_id.to_address());
    } else {
        transfer::public_transfer(pos, h::bob_addr());
    };
    ts::return_shared(bucket);
    (position_id, coin_id)
}

/// Sweep the most recently transferred Coin<CALLX> (and optionally the
/// Position) into VaultMm custody.
fun sweep_call_coin(sc: &mut ts::Scenario, vault_id: ID, and_position: bool) {
    ts::next_tx(sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let ticket = ts::most_recent_receiving_ticket<coin::Coin<CALLX>>(&vault_id);
    vault_mm::receive_mm_option_coin<CALLX>(&mut v, &ireg, ticket);
    if (and_position) {
        let pticket = ts::most_recent_receiving_ticket<Position>(&vault_id);
        vault_mm::receive_mm_position(&mut v, &ireg, pticket);
    };
    ts::return_shared(ireg);
    ts::return_shared(v);
}

#[test]
fun exercise_call_coin_partial_then_appraisal_completes() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_vault_mm(&mut sc);
    create_call_bucket(&mut sc);
    let (_pos_id, coin_id) = write_calls_to_vault(&mut sc, vault_id, 100_000, false, &clock);
    sweep_call_coin(&mut sc, vault_id, false);

    // Curator exercises 40_000: pays 80_000 USDC strike, gains 40_000 BTC;
    // the 60_000 remainder re-stores under the SAME position id.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut bucket = ts::take_shared<Bucket<h::BTC, h::USDC, CALLX>>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault_mm::exercise_call_coin<h::BTC, h::USDC, CALLX>(
        &mut v,
        &cap,
        &ireg,
        &mut bucket,
        coin_id,
        40_000,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<h::USDC>(&v) == 920_000);
    assert!(vault::free_balance_of<h::BTC>(&v) == 40_000);
    assert!(vault::position_count(&v) == 1);
    let rem: &coin::Coin<CALLX> = vault::borrow_position(&v, coin_id);
    assert!(rem.value() == 60_000);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(bucket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Appraisal still completes afterwards (types + position counts line
    // up) — proven by CONSUMING it through a deposit. Spot 3.0: free
    // 920_000 + 40_000×3 BTC + 60_000×(3−2) coin intrinsic.
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let bucket = ts::take_shared<Bucket<h::BTC, h::USDC, CALLX>>(&sc);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    // PriceAttestation is copyable: one BTC mark serves both legs.
    let btc_att = h::attest<h::BTC, h::USDC>(&sc, 3_000_000_000_000, 0);
    vault::appraise_balance<h::BTC>(&v, &cfg, &mut appraisal, btc_att, &clock);
    vault_mm::appraise_call_coin<h::BTC, h::USDC, CALLX>(
        &v,
        &cfg,
        &mut appraisal,
        &bucket,
        coin_id,
        option::some(btc_att),
        option::none(),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 920_000 + 120_000 + 60_000);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    vault::deposit<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        appraisal,
        coin::mint_for_testing<h::USDC>(1_000, sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
    ts::return_shared(bucket);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 78, location = trading_vault::vault)]
fun exercise_call_coin_insufficient_settlement_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    // Not enough USDC for the 80_000 strike payment.
    h::simple_deposit(&mut sc, h::alice_addr(), 50_000, &clock);
    allow_vault_mm(&mut sc);
    create_call_bucket(&mut sc);
    let (_pos_id, coin_id) = write_calls_to_vault(&mut sc, vault_id, 100_000, false, &clock);
    sweep_call_coin(&mut sc, vault_id, false);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut bucket = ts::take_shared<Bucket<h::BTC, h::USDC, CALLX>>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault_mm::exercise_call_coin<h::BTC, h::USDC, CALLX>(
        &mut v,
        &cap,
        &ireg,
        &mut bucket,
        coin_id,
        40_000,
        &clock,
        sc.ctx(),
    );
    abort 0
}

#[test]
fun exercise_put_coin_delivers_underlying_and_collects_payout() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_vault_mm(&mut sc);
    create_put_bucket(&mut sc);
    // Deliverable underlying enters as strategy P&L.
    h::session_gain<h::BTC>(&mut sc, 100_000);

    // Bob writes 100_000 puts (200_000 cash collateral); the vault holds
    // the put coins.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut bucket = ts::take_shared<PutBucket<h::BTC, h::USDC, PUTX>>(&sc);
    let wl = ts::take_shared<Whitelist>(&sc);
    let (pos, put) = put_bucket::write_collateralized(
        &mut bucket,
        &wl,
        coin::mint_for_testing<h::USDC>(200_000, sc.ctx()),
        100_000,
        &clock,
        sc.ctx(),
    );
    ts::return_shared(wl);
    let coin_id = object::id(&put);
    transfer::public_transfer(put, vault_id.to_address());
    transfer::public_transfer(pos, h::bob_addr());
    ts::return_shared(bucket);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let ticket = ts::most_recent_receiving_ticket<coin::Coin<PUTX>>(&vault_id);
    vault_mm::receive_mm_option_coin<PUTX>(&mut v, &ireg, ticket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Exercise 40_000: deliver 40_000 BTC, receive floor(40_000×2) USDC.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut bucket = ts::take_shared<PutBucket<h::BTC, h::USDC, PUTX>>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault_mm::exercise_put_coin<h::BTC, h::USDC, PUTX>(
        &mut v,
        &cap,
        &ireg,
        &mut bucket,
        coin_id,
        40_000,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<h::BTC>(&v) == 60_000);
    assert!(vault::free_balance_of<h::USDC>(&v) == 1_080_000);
    assert!(vault::position_count(&v) == 1);
    let rem: &coin::Coin<PUTX> = vault::borrow_position(&v, coin_id);
    assert!(rem.value() == 60_000);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(bucket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun close_offset_position_partial_then_full() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_vault_mm(&mut sc);
    create_call_bucket(&mut sc);
    // The vault holds BOTH sides of the write.
    let (pos_id, coin_id) = write_calls_to_vault(&mut sc, vault_id, 100_000, true, &clock);
    sweep_call_coin(&mut sc, vault_id, true);

    // Partial close: 30_000 freed underlying, both positions re-stored.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut bucket = ts::take_shared<Bucket<h::BTC, h::USDC, CALLX>>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    assert!(vault::position_count(&v) == 2);
    vault_mm::close_offset_position<h::BTC, h::USDC, CALLX>(
        &mut v,
        &cap,
        &ireg,
        &mut bucket,
        pos_id,
        coin_id,
        30_000,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<h::BTC>(&v) == 30_000);
    assert!(vault::position_count(&v) == 2);
    assert!(bucket::closed_pending(&bucket) == 30_000);
    {
        let pos: &Position = vault::borrow_position(&v, pos_id);
        assert!(position::amount(pos) == 70_000);
    };

    // Full close of the remainder: position destroyed, coin exhausted.
    vault_mm::close_offset_position<h::BTC, h::USDC, CALLX>(
        &mut v,
        &cap,
        &ireg,
        &mut bucket,
        pos_id,
        coin_id,
        70_000,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<h::BTC>(&v) == 100_000);
    assert!(vault::position_count(&v) == 0);
    assert!(bucket::closed_pending(&bucket) == 100_000);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(bucket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun close_offset_put_position_returns_cash_collateral() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_vault_mm(&mut sc);
    create_put_bucket(&mut sc);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut bucket = ts::take_shared<PutBucket<h::BTC, h::USDC, PUTX>>(&sc);
    let wl = ts::take_shared<Whitelist>(&sc);
    let (pos, put) = put_bucket::write_collateralized(
        &mut bucket,
        &wl,
        coin::mint_for_testing<h::USDC>(200_000, sc.ctx()),
        100_000,
        &clock,
        sc.ctx(),
    );
    ts::return_shared(wl);
    let pos_id = object::id(&pos);
    let coin_id = object::id(&put);
    transfer::public_transfer(put, vault_id.to_address());
    transfer::public_transfer(pos, vault_id.to_address());
    ts::return_shared(bucket);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let ticket = ts::most_recent_receiving_ticket<coin::Coin<PUTX>>(&vault_id);
    vault_mm::receive_mm_option_coin<PUTX>(&mut v, &ireg, ticket);
    let pticket = ts::most_recent_receiving_ticket<Position>(&vault_id);
    vault_mm::receive_mm_position(&mut v, &ireg, pticket);
    assert!(vault::position_count(&v) == 2);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Full close: floor(100_000 × 2.0) cash collateral comes back.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut bucket = ts::take_shared<PutBucket<h::BTC, h::USDC, PUTX>>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault_mm::close_offset_put_position<h::BTC, h::USDC, PUTX>(
        &mut v,
        &cap,
        &ireg,
        &mut bucket,
        pos_id,
        coin_id,
        100_000,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<h::USDC>(&v) == 1_200_000);
    assert!(vault::position_count(&v) == 0);
    assert!(put_bucket::closed_pending(&bucket) == 100_000);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(bucket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun release_coin_to_balances_makes_coin_appraisable_as_balance() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_vault_mm(&mut sc);

    // An option coin arrives (writer flow) and is swept in as a position.
    ts::next_tx(&mut sc, h::bob_addr());
    let call = coin::mint_for_testing<CALLX>(100_000, sc.ctx());
    let coin_id = object::id(&call);
    transfer::public_transfer(call, vault_id.to_address());
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let ticket = ts::most_recent_receiving_ticket<coin::Coin<CALLX>>(&vault_id);
    vault_mm::receive_mm_option_coin<CALLX>(&mut v, &ireg, ticket);
    assert!(vault::position_count(&v) == 1);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Curator frees it into balances.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault_mm::release_coin_to_balances<CALLX>(&mut v, &cap, &ireg, coin_id);
    assert!(vault::position_count(&v) == 0);
    assert!(vault::free_balance_of<CALLX>(&v) == 100_000);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // The freed coin type prices as an ordinary balance (SO-297 shape: an
    // attested mark, here 0.5 USDC/unit) and the appraisal CONSUMES.
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::appraise_balance<CALLX>(
        &v,
        &cfg,
        &mut appraisal,
        h::attest<CALLX, h::USDC>(&sc, 500_000_000_000, 0),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 1_000_000 + 50_000);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    vault::deposit<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        appraisal,
        coin::mint_for_testing<h::USDC>(1_000, sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

/// The exits are cap-gated: a rotated-out (stale) cap is refused before
/// any session opens. (Forced/crank sessions cannot reach these at all —
/// every exit `take`s or `take_position`s, and forced sessions abort 91
/// on `take` by construction.)
#[test]
#[expected_failure(abort_code = 70, location = trading_vault::vault)]
fun stale_cap_cannot_release_coin() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    let vault_id = h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_vault_mm(&mut sc);

    ts::next_tx(&mut sc, h::bob_addr());
    let call = coin::mint_for_testing<CALLX>(100_000, sc.ctx());
    let coin_id = object::id(&call);
    transfer::public_transfer(call, vault_id.to_address());
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let ticket = ts::most_recent_receiving_ticket<coin::Coin<CALLX>>(&vault_id);
    vault_mm::receive_mm_option_coin<CALLX>(&mut v, &ireg, ticket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    // Rotate the role away; the old cap becomes a claim ticket only.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::rotate_curator_by_curator(&mut v, &cap, h::alice_addr(), sc.ctx());
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    vault_mm::release_coin_to_balances<CALLX>(&mut v, &cap, &ireg, coin_id);
    abort 0
}
