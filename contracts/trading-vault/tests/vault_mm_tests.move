#[test_only]
module trading_vault::vault_mm_tests;

use std::string;
use sui::balance;
use sui::coin;
use sui::test_scenario as ts;

use std::type_name;

use options_core::collateral::{Self, CollateralRequest};
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
