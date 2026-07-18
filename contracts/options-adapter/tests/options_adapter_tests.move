#[test_only]
module options_adapter::options_adapter_tests;

use std::type_name;
use sui::balance;
use sui::clock::{Self, Clock};
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use auction::auction::{Self as auctions, Auction};
use options_core::admin::{Self, AdminCap, ProtocolConfig};
use options_core::bucket::{Self, Bucket};
use options_core::treasury::{Self, Treasury};

use trading_vault::price as tv_price;
use trading_vault::registry as tv_registry;
use trading_vault::registry::{IntegrationRegistry, OracleRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, CuratorCap, TradingVault};

use options_adapter::options_adapter::{Self as adapter, RfqTicket};

/// Vault deposit asset == the call underlying.
public struct UND has drop {}
/// Settlement / premium asset.
public struct QUOTE has drop {}
/// Per-bucket option coin marker.
public struct CALL has drop {}

/// Local oracle witness for pricing QUOTE into UND.
public struct TestOracle has drop {}

const ADMIN: address = @0xA1;
const CREATOR: address = @0xB2;
const CURATOR: address = @0xC3;
const ALICE: address = @0xD4;
const MM: address = @0xE5;

const ESCROW: u64 = 100_000;
const EXPIRY_MS: u64 = 10_000_000;

fun setup(sc: &mut Scenario): Clock {
    ts::next_tx(sc, ADMIN);
    admin::init_for_testing(sc.ctx());
    tv_registry::init_for_testing(sc.ctx());

    ts::next_tx(sc, ADMIN);
    let admin_cap = ts::take_from_sender<AdminCap>(sc);
    treasury::create_and_share(&admin_cap, sc.ctx());
    let mut ireg = ts::take_shared<IntegrationRegistry>(sc);
    tv_registry::allow_adapter(
        &admin_cap,
        &mut ireg,
        type_name::with_defining_ids<adapter::OptionsAdapter>(),
    );
    ts::return_shared(ireg);
    let mut oreg = ts::take_shared<OracleRegistry>(sc);
    tv_registry::allow_oracle(&admin_cap, &mut oreg, type_name::with_defining_ids<TestOracle>());
    ts::return_shared(oreg);

    // Bucket: strike 2.0 QUOTE per UND (scale 12), expiry 10_000s.
    let tcap = coin::create_treasury_cap_for_testing<CALL>(sc.ctx());
    bucket::create_bucket<UND, QUOTE, CALL>(
        &admin_cap,
        tcap,
        EXPIRY_MS,
        2_000_000_000_000,
        12,
        sc.ctx(),
    );
    ts::return_to_sender(sc, admin_cap);

    // UND-denominated vault, Alice seeds 1_000_000.
    ts::next_tx(sc, CREATOR);
    let cfg = ts::take_shared<VaultProtocolConfig>(sc);
    vault::create_vault<UND>(&cfg, CURATOR, 0, 1_000, 2, 8, 3_600_000, sc.ctx());
    ts::return_shared(cfg);

    ts::next_tx(sc, ADMIN);
    let clock = clock::create_for_testing(sc.ctx());

    ts::next_tx(sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(sc);
    let appraisal = vault::begin_appraisal<UND>(&v);
    vault::deposit<UND>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(balance::create_for_testing<UND>(1_000_000), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock
}

fun open_rfq(sc: &mut Scenario, clock: &Clock): ID {
    ts::next_tx(sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    let bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(sc);
    let ticket_id = adapter::open_call_rfq<UND, QUOTE, CALL>(
        &mut v,
        &cap,
        &ireg,
        &bucket,
        ESCROW,
        10_000, // reserve premium
        600_000, // duration
        0,
        0,
        0,
        100,
        clock,
        sc.ctx(),
    );
    ts::return_shared(bucket);
    ts::return_to_sender(sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
    ticket_id
}

#[test]
fun rfq_fill_absorbs_position_and_premium() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    let ticket_id = open_rfq(&mut sc, &clock);

    // Escrow left the vault; the ticket appraises it at cost (UND 1:1).
    ts::next_tx(&mut sc, ALICE);
    {
        let v = ts::take_shared<TradingVault>(&sc);
        let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
        assert!(vault::free_balance_of<UND>(&v) == 1_000_000 - ESCROW);
        let mut appraisal = vault::begin_appraisal<UND>(&v);
        adapter::appraise_rfq_ticket<UND>(
            &v,
            &cfg,
            &mut appraisal,
            ticket_id,
            option::none(),
            &clock,
        );
        assert!(vault::appraisal_value(&appraisal) == 1_000_000);
        sui::test_utils::destroy(appraisal);
        ts::return_shared(cfg);
        ts::return_shared(v);
    };

    // MM bids 50_000 QUOTE premium.
    ts::next_tx(&mut sc, MM);
    let mut auction = ts::take_shared<Auction<UND, QUOTE>>(&sc);
    auctions::bid(
        &mut auction,
        coin::from_balance(balance::create_for_testing<QUOTE>(50_000), sc.ctx()),
        MM,
        &clock,
        sc.ctx(),
    );
    ts::return_shared(auction);

    // Deadline passes; anyone settles.
    clock.set_for_testing(601_000);
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let auction = ts::take_shared<Auction<UND, QUOTE>>(&sc);
    let mut bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    let config = ts::take_shared<ProtocolConfig>(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let mut pos_opt = adapter::settle_call_rfq<UND, QUOTE, CALL>(
        &mut v,
        &ireg,
        ticket_id,
        auction,
        &mut bucket,
        &config,
        &mut treasury,
        &clock,
        sc.ctx(),
    );
    let position_id = pos_opt.extract();
    pos_opt.destroy_none();
    // Vault: 900k UND free + 50k QUOTE premium + 1 option Position.
    assert!(vault::free_balance_of<UND>(&v) == 1_000_000 - ESCROW);
    assert!(vault::free_balance_of<QUOTE>(&v) == 50_000);
    assert!(vault::position_count(&v) == 1);
    ts::return_shared(treasury);
    ts::return_shared(config);
    ts::return_shared(bucket);

    // Appraise the position: strike value (200k QUOTE→100k UND at the
    // 0.5 cross) vs spot 100k UND → min is 100k; QUOTE premium needs
    // the same attestation. NAV = 900k + 100k + 25k = 1_025_000.
    ts::next_tx(&mut sc, ALICE);
    let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let att = tv_price::attest(
        TestOracle {},
        &oreg,
        type_name::with_defining_ids<QUOTE>(),
        type_name::with_defining_ids<UND>(),
        500_000_000_000, // 0.5 UND per QUOTE raw unit at 1e12
        clock.timestamp_ms(),
    );
    let mut appraisal = vault::begin_appraisal<UND>(&v);
    vault::appraise_balance<QUOTE>(&v, &cfg, &mut appraisal, att, &clock);
    let bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    adapter::appraise_call_position<UND, QUOTE, CALL>(
        &v,
        &cfg,
        &mut appraisal,
        &bucket,
        position_id,
        option::none(),
        option::some(att),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 900_000 + 100_000 + 25_000);
    sui::test_utils::destroy(appraisal);
    ts::return_shared(bucket);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun rfq_no_winner_refunds_escrow() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    let ticket_id = open_rfq(&mut sc, &clock);

    clock.set_for_testing(601_000);
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let auction = ts::take_shared<Auction<UND, QUOTE>>(&sc);
    let mut bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    let config = ts::take_shared<ProtocolConfig>(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let pos_opt = adapter::settle_call_rfq<UND, QUOTE, CALL>(
        &mut v,
        &ireg,
        ticket_id,
        auction,
        &mut bucket,
        &config,
        &mut treasury,
        &clock,
        sc.ctx(),
    );
    pos_opt.destroy_none();
    assert!(vault::free_balance_of<UND>(&v) == 1_000_000);
    assert!(vault::position_count(&v) == 0);
    ts::return_shared(treasury);
    ts::return_shared(config);
    ts::return_shared(bucket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun redeem_after_expiry_returns_funds() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    let ticket_id = open_rfq(&mut sc, &clock);

    ts::next_tx(&mut sc, MM);
    let mut auction = ts::take_shared<Auction<UND, QUOTE>>(&sc);
    auctions::bid(
        &mut auction,
        coin::from_balance(balance::create_for_testing<QUOTE>(50_000), sc.ctx()),
        MM,
        &clock,
        sc.ctx(),
    );
    ts::return_shared(auction);

    clock.set_for_testing(601_000);
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let auction = ts::take_shared<Auction<UND, QUOTE>>(&sc);
    let mut bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    let config = ts::take_shared<ProtocolConfig>(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let mut pos_opt = adapter::settle_call_rfq<UND, QUOTE, CALL>(
        &mut v,
        &ireg,
        ticket_id,
        auction,
        &mut bucket,
        &config,
        &mut treasury,
        &clock,
        sc.ctx(),
    );
    let position_id = pos_opt.extract();
    pos_opt.destroy_none();
    ts::return_shared(treasury);
    ts::return_shared(config);

    // No exercise; expiry passes; permissionless redeem returns all
    // escrowed underlying.
    clock.set_for_testing(EXPIRY_MS + 1);
    adapter::redeem_call_position<UND, QUOTE, CALL>(
        &mut v,
        &ireg,
        &mut bucket,
        position_id,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<UND>(&v) == 1_000_000);
    assert!(vault::free_balance_of<QUOTE>(&v) == 50_000);
    assert!(vault::position_count(&v) == 0);
    ts::return_shared(bucket);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}
