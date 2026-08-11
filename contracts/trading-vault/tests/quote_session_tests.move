#[test_only]
/// Quote sessions (SO-372): the permissionless, take-capable settlement
/// session behind direct vault escrow. Gate matrix + curator management.
module trading_vault::quote_session_tests;

use sui::balance;
use sui::test_scenario as ts;

use trading_vault::registry::{Self, IntegrationRegistry};
use trading_vault::test_helpers as h;
use trading_vault::vault::{Self, CuratorCap, TradingVault};

fun enable_quote_adapter(sc: &mut ts::Scenario) {
    ts::next_tx(sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    vault::add_quote_adapter<h::TestAdapter>(&mut v, &cap);
    ts::return_to_sender(sc, cap);
    ts::return_shared(v);
}

#[test]
fun quote_session_takes_and_returns_permissionlessly() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    enable_quote_adapter(&mut sc);

    // A stranger (a taker filling the vault's quote) runs the session:
    // take the maker leg, return the proceeds — all in-transaction.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut s = vault::begin_quote_session(&v, &ireg, h::test_adapter());
    let owed = vault::take<h::USDC>(&mut v, &mut s, 100_000);
    balance::destroy_for_testing(owed); // "paid to the taker"
    vault::put<h::BTC>(&mut v, &mut s, h::mint<h::BTC>(50_000)); // "proceeds"
    vault::end_session(&v, s);
    assert!(vault::free_balance_of<h::USDC>(&v) == 900_000);
    assert!(vault::free_balance_of<h::BTC>(&v) == 50_000);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 113, location = trading_vault::vault)]
fun quote_session_refused_without_curator_opt_in() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // TestAdapter IS protocol-allowlisted; the curator never opted in.
    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let _s = vault::begin_quote_session(&v, &ireg, h::test_adapter());
    abort 0
}

#[test]
#[expected_failure(abort_code = 75, location = trading_vault::vault)]
fun quote_session_refused_for_delisted_adapter() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    enable_quote_adapter(&mut sc);

    // Protocol kill switch: delisting beats the curator opt-in.
    ts::next_tx(&mut sc, h::admin_addr());
    let admin_cap = h::take_admin_cap(&sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(&sc);
    registry::disallow_adapter(
        &admin_cap,
        &mut ireg,
        std::type_name::with_defining_ids<h::TestAdapter>(),
    );
    ts::return_shared(ireg);
    h::return_admin_cap(&sc, admin_cap);

    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let _s = vault::begin_quote_session(&v, &ireg, h::test_adapter());
    abort 0
}

#[test]
#[expected_failure(abort_code = 72, location = trading_vault::vault)]
fun quote_session_refused_while_closing() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    enable_quote_adapter(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::initiate_close(&mut v, &cap);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let _s = vault::begin_quote_session(&v, &ireg, h::test_adapter());
    abort 0
}

#[test]
#[expected_failure(abort_code = 113, location = trading_vault::vault)]
fun curator_removal_stops_new_quote_sessions() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    enable_quote_adapter(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::remove_quote_adapter<h::TestAdapter>(&mut v, &cap);
    assert!(
        !vault::is_quote_adapter(&v, &std::type_name::with_defining_ids<h::TestAdapter>()),
    );
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let _s = vault::begin_quote_session(&v, &ireg, h::test_adapter());
    abort 0
}

#[test]
#[expected_failure(abort_code = 70, location = trading_vault::vault)]
fun stale_cap_cannot_manage_quote_adapters() {
    let mut sc = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::rotate_curator_by_curator(&mut v, &cap, h::bob_addr(), sc.ctx());
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let old_cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::add_quote_adapter<h::TestAdapter>(&mut v, &old_cap);
    abort 0
}
