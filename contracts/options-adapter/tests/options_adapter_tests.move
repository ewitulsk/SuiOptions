#[test_only]
module options_adapter::options_adapter_tests;

use std::type_name;
use sui::balance;
use sui::clock::{Self, Clock};
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use auction::auction::{Self as auctions, Auction};
use options_core::admin::{Self, AdminCap, ProtocolConfig};
use options_core::bucket::{Self, Bucket};
use options_core::treasury::{Self, Treasury};

use trading_vault::price as tv_price;
use trading_vault::registry as tv_registry;
use trading_vault::registry::{IntegrationRegistry, OracleRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, CuratorCap, TradingVault};
use trading_vault::vault_mm;

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
/// Curator of the BIDDING vault in the cross-vault test.
const CURATOR2: address = @0xF6;

const ESCROW: u64 = 100_000;
const EXPIRY_MS: u64 = 10_000_000;
/// The desk-side bid escrowed in the bid-ticket tests.
const BID: u64 = 50_000;

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

// ═══════════════════ vault-funded auction bids ═══════════════════

/// Minimal world for the bid-ticket tests: registries + a QUOTE-deposit
/// vault seeded with 1M (bid asset == deposit asset, so appraisals need
/// no attestation legs). No bucket/treasury — the swap auction sells
/// CALL coins directly.
fun setup_bid(sc: &mut Scenario): Clock {
    ts::next_tx(sc, ADMIN);
    admin::init_for_testing(sc.ctx());
    tv_registry::init_for_testing(sc.ctx());

    ts::next_tx(sc, ADMIN);
    let admin_cap = ts::take_from_sender<AdminCap>(sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(sc);
    tv_registry::allow_adapter(
        &admin_cap,
        &mut ireg,
        type_name::with_defining_ids<adapter::OptionsAdapter>(),
    );
    ts::return_shared(ireg);
    ts::return_to_sender(sc, admin_cap);

    ts::next_tx(sc, CREATOR);
    let cfg = ts::take_shared<VaultProtocolConfig>(sc);
    vault::create_vault<QUOTE>(&cfg, CURATOR, 0, 1_000, 2, 8, 3_600_000, sc.ctx());
    ts::return_shared(cfg);

    ts::next_tx(sc, ADMIN);
    let clock = clock::create_for_testing(sc.ctx());

    ts::next_tx(sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(sc);
    let appraisal = vault::begin_appraisal<QUOTE>(&v);
    vault::deposit<QUOTE>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(balance::create_for_testing<QUOTE>(1_000_000), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock
}

/// MM opens an uncoupled swap auction selling ESCROW CALL coins for
/// QUOTE (proceeds/refund to MM).
fun open_swap_auction(sc: &mut Scenario, clock: &Clock): ID {
    ts::next_tx(sc, MM);
    auctions::create<CALL, QUOTE>(
        coin::from_balance(balance::create_for_testing<CALL>(ESCROW), sc.ctx()),
        10_000, // reserve
        600_000, // duration
        0,
        0,
        0,
        100,
        MM,
        MM,
        object::id_from_address(@0xFACE),
        clock,
        sc.ctx(),
    )
}

/// The curator bids BID from vault funds; win pinned to ESCROW CALLs.
fun place_vault_bid(sc: &mut Scenario, clock: &Clock, auction_id: ID): ID {
    ts::next_tx(sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    let mut auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(sc, auction_id);
    let ticket_id = adapter::bid_on_auction<CALL, QUOTE, CALL>(
        &mut v,
        &cap,
        &ireg,
        &mut auction,
        BID,
        ESCROW,
        object::id_from_address(@0xB0C4),
        false,
        clock,
        sc.ctx(),
    );
    ts::return_shared(auction);
    ts::return_to_sender(sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
    ticket_id
}

#[test]
fun bid_ticket_outbid_reclaim_returns_escrow() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    // Escrow left the vault; the ticket appraises at cost (QUOTE 1:1),
    // and the appraisal REQUIRES the leg (position accounting).
    ts::next_tx(&mut sc, ALICE);
    {
        let v = ts::take_shared<TradingVault>(&sc);
        let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
        assert!(vault::free_balance_of<QUOTE>(&v) == 1_000_000 - BID);
        assert!(vault::position_count(&v) == 1);
        let mut appraisal = vault::begin_appraisal<QUOTE>(&v);
        adapter::appraise_bid_ticket<QUOTE>(
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

    // MM outbids (100 bps over BID): the refund push-transfers to the
    // TICKET's address.
    ts::next_tx(&mut sc, MM);
    {
        let mut auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
        auctions::bid(
            &mut auction,
            coin::from_balance(balance::create_for_testing<QUOTE>(50_500), sc.ctx()),
            MM,
            &clock,
            sc.ctx(),
        );
        ts::return_shared(auction);
    };

    // Permissionless reclaim: receive the refund, burn the ticket.
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
    let refund = ts::most_recent_receiving_ticket<Coin<QUOTE>>(&ticket_id);
    adapter::reclaim_outbid_ticket<CALL, QUOTE>(&mut v, &ireg, ticket_id, &auction, refund);
    assert!(vault::free_balance_of<QUOTE>(&v) == 1_000_000);
    assert!(vault::position_count(&v) == 0);
    ts::return_shared(auction);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun bid_ticket_win_settle_redeem_absorbs_coins() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    // Deadline passes; a third party settles the swap: CALL escrow →
    // ticket address, QUOTE proceeds → MM. The auction object is GONE.
    clock.set_for_testing(601_000);
    ts::next_tx(&mut sc, MM);
    {
        let auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
        auctions::settle<CALL, QUOTE>(auction, &clock, sc.ctx());
    };
    ts::next_tx(&mut sc, MM);
    {
        let proceeds = ts::take_from_address<Coin<QUOTE>>(&sc, MM);
        assert!(proceeds.value() == BID);
        ts::return_to_address(MM, proceeds);
    };

    // Permissionless redeem: receive the winnings, burn the ticket.
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let winnings = ts::most_recent_receiving_ticket<Coin<CALL>>(&ticket_id);
    adapter::redeem_won_ticket<CALL>(&mut v, &ireg, ticket_id, winnings);
    assert!(vault::free_balance_of<QUOTE>(&v) == 1_000_000 - BID);
    assert!(vault::free_balance_of<CALL>(&v) == ESCROW);
    assert!(vault::position_count(&v) == 0);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun bid_ticket_reclaim_after_auction_deleted() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    // Outbid, then the auction settles (deleted) before the reclaim
    // crank ran — the no-auction variant still burns against the coin.
    ts::next_tx(&mut sc, MM);
    {
        let mut auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
        auctions::bid(
            &mut auction,
            coin::from_balance(balance::create_for_testing<QUOTE>(50_500), sc.ctx()),
            MM,
            &clock,
            sc.ctx(),
        );
        ts::return_shared(auction);
    };
    clock.set_for_testing(601_000);
    ts::next_tx(&mut sc, MM);
    {
        let auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
        auctions::settle<CALL, QUOTE>(auction, &clock, sc.ctx());
    };

    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let refund = ts::most_recent_receiving_ticket<Coin<QUOTE>>(&ticket_id);
    adapter::reclaim_refunded_ticket<QUOTE>(&mut v, &ireg, ticket_id, refund);
    assert!(vault::free_balance_of<QUOTE>(&v) == 1_000_000);
    assert!(vault::position_count(&v) == 0);
    ts::return_shared(ireg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 10, location = options_adapter::options_adapter)]
fun bid_ticket_reclaim_while_best_bidder_aborts() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    // Adversary donates an exact-amount coin to the ticket address while
    // the vault's escrow is still LIVE as the best bid.
    ts::next_tx(&mut sc, MM);
    transfer::public_transfer(
        coin::from_balance(balance::create_for_testing<QUOTE>(BID), sc.ctx()),
        ticket_id.to_address(),
    );

    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
    let fake = ts::most_recent_receiving_ticket<Coin<QUOTE>>(&ticket_id);
    adapter::reclaim_outbid_ticket<CALL, QUOTE>(&mut v, &ireg, ticket_id, &auction, fake);
    abort 0
}

#[test]
#[expected_failure(abort_code = 11, location = options_adapter::options_adapter)]
fun bid_ticket_reclaim_wrong_amount_aborts() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    // Genuinely outbid…
    ts::next_tx(&mut sc, MM);
    let mut auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
    auctions::bid(
        &mut auction,
        coin::from_balance(balance::create_for_testing<QUOTE>(50_500), sc.ctx()),
        MM,
        &clock,
        sc.ctx(),
    );
    ts::return_shared(auction);
    // …but the crank is fed a donated dust coin instead of the refund.
    ts::next_tx(&mut sc, MM);
    let dust = coin::from_balance(balance::create_for_testing<QUOTE>(7), sc.ctx());
    let dust_id = object::id(&dust);
    transfer::public_transfer(dust, ticket_id.to_address());

    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
    let fake = ts::receiving_ticket_by_id<Coin<QUOTE>>(dust_id);
    adapter::reclaim_outbid_ticket<CALL, QUOTE>(&mut v, &ireg, ticket_id, &auction, fake);
    abort 0
}

#[test]
#[expected_failure(abort_code = 13, location = options_adapter::options_adapter)]
fun bid_ticket_redeem_short_winnings_aborts() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    // A donated sliver of the win asset cannot force an early burn.
    ts::next_tx(&mut sc, MM);
    transfer::public_transfer(
        coin::from_balance(balance::create_for_testing<CALL>(1), sc.ctx()),
        ticket_id.to_address(),
    );

    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let fake = ts::most_recent_receiving_ticket<Coin<CALL>>(&ticket_id);
    adapter::redeem_won_ticket<CALL>(&mut v, &ireg, ticket_id, fake);
    abort 0
}

#[test]
#[expected_failure(abort_code = 12, location = options_adapter::options_adapter)]
fun bid_ticket_redeem_wrong_type_aborts() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    ts::next_tx(&mut sc, MM);
    transfer::public_transfer(
        coin::from_balance(balance::create_for_testing<QUOTE>(ESCROW), sc.ctx()),
        ticket_id.to_address(),
    );

    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let fake = ts::most_recent_receiving_ticket<Coin<QUOTE>>(&ticket_id);
    adapter::redeem_won_ticket<QUOTE>(&mut v, &ireg, ticket_id, fake);
    abort 0
}

#[test]
#[expected_failure(abort_code = 86, location = trading_vault::vault)]
fun bid_ticket_double_reclaim_aborts() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    let ticket_id = place_vault_bid(&mut sc, &clock, auction_id);

    ts::next_tx(&mut sc, MM);
    let mut auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
    auctions::bid(
        &mut auction,
        coin::from_balance(balance::create_for_testing<QUOTE>(50_500), sc.ctx()),
        MM,
        &clock,
        sc.ctx(),
    );
    ts::return_shared(auction);
    // A second exact-amount coin sits at the ticket address alongside
    // the genuine refund.
    ts::next_tx(&mut sc, MM);
    transfer::public_transfer(
        coin::from_balance(balance::create_for_testing<QUOTE>(BID), sc.ctx()),
        ticket_id.to_address(),
    );

    ts::next_tx(&mut sc, ALICE);
    {
        let mut v = ts::take_shared<TradingVault>(&sc);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let refund = ts::most_recent_receiving_ticket<Coin<QUOTE>>(&ticket_id);
        adapter::reclaim_refunded_ticket<QUOTE>(&mut v, &ireg, ticket_id, refund);
        ts::return_shared(ireg);
        ts::return_shared(v);
    };

    // The ticket is gone: a second reclaim (fed the remaining coin)
    // aborts in take_position.
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let second = ts::most_recent_receiving_ticket<Coin<QUOTE>>(&ticket_id);
    adapter::reclaim_refunded_ticket<QUOTE>(&mut v, &ireg, ticket_id, second);
    abort 0
}

#[test]
#[expected_failure(abort_code = 9, location = options_adapter::options_adapter)]
fun bid_on_auction_closing_vault_aborts() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);

    ts::next_tx(&mut sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::initiate_close(&mut v, &cap);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut auction = ts::take_shared_by_id<Auction<CALL, QUOTE>>(&sc, auction_id);
    adapter::bid_on_auction<CALL, QUOTE, CALL>(
        &mut v,
        &cap,
        &ireg,
        &mut auction,
        BID,
        ESCROW,
        object::id_from_address(@0xB0C4),
        false,
        &clock,
        sc.ctx(),
    );
    abort 0
}

#[test]
#[expected_failure(abort_code = 82, location = trading_vault::vault)]
fun appraisal_without_bid_ticket_leg_aborts() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup_bid(&mut sc);
    let auction_id = open_swap_auction(&mut sc, &clock);
    place_vault_bid(&mut sc, &clock, auction_id);

    // A live ticket makes an appraisal without its leg incomplete.
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
    let appraisal = vault::begin_appraisal<QUOTE>(&v);
    vault::deposit<QUOTE>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(balance::create_for_testing<QUOTE>(10), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    abort 0
}

/// End-to-end across two vaults: a writer vault opens a call RFQ
/// (coupled auction), the bidder vault funds its bid from custody, the
/// permissionless RFQ settle mints the CALL to the bid ticket's address,
/// and redeem absorbs it into the bidder vault.
#[test]
fun bid_ticket_wins_coupled_rfq_end_to_end() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    let rfq_ticket_id = open_rfq(&mut sc, &clock);

    // Bidder vault: QUOTE-denominated, curated by CURATOR2.
    ts::next_tx(&mut sc, CREATOR);
    let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
    let bidder_vault_id =
        vault::create_vault<QUOTE>(&cfg, CURATOR2, 0, 1_000, 2, 8, 3_600_000, sc.ctx());
    ts::return_shared(cfg);
    ts::next_tx(&mut sc, ALICE);
    {
        let mut v = ts::take_shared_by_id<TradingVault>(&sc, bidder_vault_id);
        let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
        let appraisal = vault::begin_appraisal<QUOTE>(&v);
        vault::deposit<QUOTE>(
            &mut v,
            &cfg,
            appraisal,
            coin::from_balance(balance::create_for_testing<QUOTE>(1_000_000), sc.ctx()),
            &clock,
            sc.ctx(),
        );
        ts::return_shared(cfg);
        ts::return_shared(v);
    };

    // CURATOR2 bids vault funds on the writer's premium auction. A call
    // RFQ mints 1:1 with its escrow, so the win is ESCROW CALLs.
    ts::next_tx(&mut sc, CURATOR2);
    let mut v2 = ts::take_shared_by_id<TradingVault>(&sc, bidder_vault_id);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let cap2 = ts::take_from_sender<CuratorCap>(&sc);
    let mut auction = ts::take_shared<Auction<UND, QUOTE>>(&sc);
    let writer_vault_id = auctions::origin(&auction);
    let bucket_id = {
        let bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
        let id = object::id(&bucket);
        ts::return_shared(bucket);
        id
    };
    let bid_ticket_id = adapter::bid_on_auction<UND, QUOTE, CALL>(
        &mut v2,
        &cap2,
        &ireg,
        &mut auction,
        BID,
        ESCROW,
        bucket_id,
        false,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<QUOTE>(&v2) == 1_000_000 - BID);
    ts::return_shared(auction);
    ts::return_to_sender(&sc, cap2);
    ts::return_shared(ireg);
    ts::return_shared(v2);

    // Deadline passes; the writer-side crank settles the RFQ: Position +
    // premium into the writer vault, CALL coins → the bid ticket.
    clock.set_for_testing(601_000);
    ts::next_tx(&mut sc, ALICE);
    {
        let mut v1 = ts::take_shared_by_id<TradingVault>(&sc, writer_vault_id);
        let ireg = ts::take_shared<IntegrationRegistry>(&sc);
        let auction = ts::take_shared<Auction<UND, QUOTE>>(&sc);
        let mut bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
        let config = ts::take_shared<ProtocolConfig>(&sc);
        let mut treasury = ts::take_shared<Treasury>(&sc);
        let mut pos_opt = adapter::settle_call_rfq<UND, QUOTE, CALL>(
            &mut v1,
            &ireg,
            rfq_ticket_id,
            auction,
            &mut bucket,
            &config,
            &mut treasury,
            &clock,
            sc.ctx(),
        );
        pos_opt.extract();
        pos_opt.destroy_none();
        assert!(vault::free_balance_of<QUOTE>(&v1) == BID);
        ts::return_shared(treasury);
        ts::return_shared(config);
        ts::return_shared(bucket);
        ts::return_shared(ireg);
        ts::return_shared(v1);
    };

    // Redeem the won CALLs into the bidder vault.
    ts::next_tx(&mut sc, ALICE);
    let mut v2 = ts::take_shared_by_id<TradingVault>(&sc, bidder_vault_id);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let winnings = ts::most_recent_receiving_ticket<Coin<CALL>>(&bid_ticket_id);
    adapter::redeem_won_ticket<CALL>(&mut v2, &ireg, bid_ticket_id, winnings);
    assert!(vault::free_balance_of<QUOTE>(&v2) == 1_000_000 - BID);
    assert!(vault::free_balance_of<CALL>(&v2) == ESCROW);
    assert!(vault::position_count(&v2) == 0);
    ts::return_shared(ireg);
    ts::return_shared(v2);

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

// ═══════════════════ spread-position appraisal ═══════════════════

/// Long-leg option coin (strike-1.0 bucket) for the spread tests.
public struct LCALL has drop {}

/// World on top of `setup`: a strike-1.0 long bucket beside the standard
/// strike-2.0 short bucket; MM writes 100k long calls, compresses a
/// same-size short against them, and the spread `Position` is swept into
/// vault custody (VaultMm sweep — custody source is irrelevant to
/// appraisal). Returns (clock, spread position id).
fun setup_spread(sc: &mut Scenario): (Clock, ID) {
    let clock = setup(sc);

    ts::next_tx(sc, ADMIN);
    let admin_cap = ts::take_from_sender<AdminCap>(sc);
    let mut ireg = ts::take_shared<IntegrationRegistry>(sc);
    tv_registry::allow_adapter(
        &admin_cap,
        &mut ireg,
        type_name::with_defining_ids<vault_mm::VaultMm>(),
    );
    ts::return_shared(ireg);
    let tcap = coin::create_treasury_cap_for_testing<LCALL>(sc.ctx());
    bucket::create_bucket<UND, QUOTE, LCALL>(
        &admin_cap,
        tcap,
        EXPIRY_MS,
        1_000_000_000_000, // strike 1.0 QUOTE per UND
        12,
        sc.ctx(),
    );
    ts::return_to_sender(sc, admin_cap);

    ts::next_tx(sc, MM);
    let mut long_bucket = ts::take_shared<Bucket<UND, QUOTE, LCALL>>(sc);
    let mut short_bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(sc);
    let (long_pos, long_coins) = bucket::write_collateralized<UND, QUOTE, LCALL>(
        &mut long_bucket,
        coin::from_balance(balance::create_for_testing<UND>(100_000), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    let (spread_pos, short_coins) = bucket::write_spread<UND, QUOTE, CALL, LCALL>(
        &mut short_bucket,
        &long_bucket,
        long_coins,
        // Exactly required_settlement(long_bucket, 100k) = 100k QUOTE.
        coin::from_balance(balance::create_for_testing<QUOTE>(100_000), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    transfer::public_transfer(long_pos, MM);
    transfer::public_transfer(short_coins, MM);
    let v = ts::take_shared<TradingVault>(sc);
    let vault_id = object::id(&v);
    let pos_id = object::id(&spread_pos);
    transfer::public_transfer(spread_pos, vault_id.to_address());
    ts::return_shared(v);
    ts::return_shared(short_bucket);
    ts::return_shared(long_bucket);

    ts::next_tx(sc, MM);
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let ticket = ts::most_recent_receiving_ticket<options_core::position::Position>(&vault_id);
    vault_mm::receive_mm_position(&mut v, &ireg, ticket);
    ts::return_shared(ireg);
    ts::return_shared(v);
    (clock, pos_id)
}

#[test]
#[expected_failure(abort_code = 15, location = options_adapter::options_adapter)] // E_SPREAD_POSITION
fun physical_appraisal_rejects_spread_position() {
    let mut sc = ts::begin(ADMIN);
    let (clock, pos_id) = setup_spread(&mut sc);

    ts::next_tx(&mut sc, ALICE);
    let v = ts::take_shared<TradingVault>(&sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
    let bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    let mut appraisal = vault::begin_appraisal<UND>(&v);
    adapter::appraise_call_position<UND, QUOTE, CALL>(
        &v,
        &cfg,
        &mut appraisal,
        &bucket,
        pos_id,
        option::none(),
        option::none(),
        &clock,
    );
    abort 0
}

#[test]
fun spread_position_appraises_from_escrow() {
    let mut sc = ts::begin(ADMIN);
    let (clock, pos_id) = setup_spread(&mut sc);

    ts::next_tx(&mut sc, ALICE);
    let v = ts::take_shared<TradingVault>(&sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let short_bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    let long_bucket = ts::take_shared<Bucket<UND, QUOTE, LCALL>>(&sc);

    // Spot 1 UND = 2 QUOTE (0.5 UND per QUOTE): cash 100k QUOTE → 50k
    // UND, long intrinsic 100k − 50k = 50k, short intrinsic 0 at the
    // boundary → mark = 100k UND, the physical min(spot, strike). NAV
    // = 1M free + 100k.
    let att = tv_price::attest(
        TestOracle {},
        &oreg,
        type_name::with_defining_ids<QUOTE>(),
        type_name::with_defining_ids<UND>(),
        500_000_000_000,
        clock.timestamp_ms(),
    );
    let mut appraisal = vault::begin_appraisal<UND>(&v);
    vault_mm::appraise_call_spread_position<UND, QUOTE, CALL, LCALL>(
        &v,
        &cfg,
        &mut appraisal,
        &short_bucket,
        &long_bucket,
        pos_id,
        option::none(),
        option::some(att),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 1_000_000 + 100_000);
    sui::test_utils::destroy(appraisal);

    // Spot below the long strike (4.0 UND per QUOTE → spot 100k UND,
    // K_long 400k, K_short 800k): both intrinsics zero, mark floors at
    // the escrowed cash (100k QUOTE → 400k UND). NAV = 1M + 400k.
    let att_otm = tv_price::attest(
        TestOracle {},
        &oreg,
        type_name::with_defining_ids<QUOTE>(),
        type_name::with_defining_ids<UND>(),
        4_000_000_000_000,
        clock.timestamp_ms(),
    );
    let mut appraisal = vault::begin_appraisal<UND>(&v);
    vault_mm::appraise_call_spread_position<UND, QUOTE, CALL, LCALL>(
        &v,
        &cfg,
        &mut appraisal,
        &short_bucket,
        &long_bucket,
        pos_id,
        option::none(),
        option::some(att_otm),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 1_000_000 + 400_000);
    sui::test_utils::destroy(appraisal);

    ts::return_shared(long_bucket);
    ts::return_shared(short_bucket);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 10, location = trading_vault::vault_mm)] // E_LONG_BUCKET_MISMATCH
fun spread_appraisal_wrong_long_bucket_aborts() {
    let mut sc = ts::begin(ADMIN);
    let (clock, pos_id) = setup_spread(&mut sc);

    // A second LCALL bucket (same coin type, different object): escrow
    // types line up but the escrowed long bucket id does not.
    ts::next_tx(&mut sc, ADMIN);
    let admin_cap = ts::take_from_sender<AdminCap>(&sc);
    let tcap = coin::create_treasury_cap_for_testing<LCALL>(sc.ctx());
    bucket::create_bucket<UND, QUOTE, LCALL>(
        &admin_cap,
        tcap,
        EXPIRY_MS,
        1_000_000_000_000,
        12,
        sc.ctx(),
    );
    ts::return_to_sender(&sc, admin_cap);

    ts::next_tx(&mut sc, ALICE);
    let v = ts::take_shared<TradingVault>(&sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
    let short_bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    // most_recent: the wrong (fresh) LCALL bucket.
    let wrong_long = ts::take_shared<Bucket<UND, QUOTE, LCALL>>(&sc);
    let mut appraisal = vault::begin_appraisal<UND>(&v);
    vault_mm::appraise_call_spread_position<UND, QUOTE, CALL, LCALL>(
        &v,
        &cfg,
        &mut appraisal,
        &short_bucket,
        &wrong_long,
        pos_id,
        option::none(),
        option::none(),
        &clock,
    );
    abort 0
}
