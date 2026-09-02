/// End-to-end tests for the hub-side multichain flow: bind → deposit
/// notice → ack; withdraw request → ack → payout receipt; state sync;
/// appraisal spoke legs (contribution + payables liability); ordering,
/// gating, and drain/close guards.
#[test_only]
module vault_v2::multichain_tests;

use sui::clock::Clock;
use sui::test_scenario::{Self as ts, Scenario};

use vault_v2::asset_markers::USDG;
use vault_v2::capital;
use vault_v2::endpoint::{Self, EndpointRegistry};
use vault_v2::endpoint_relayer::{Self, RelayerEndpoint};
use vault_v2::multichain;
use vault_v2::spoke;
use vault_v2::test_helpers as th;
use vault_v2::test_helpers::USDC;
use vault_v2::vault::{Self, CuratorCap, TradingVault};
use vault_v2::wire;

const HUB_CHAIN: u64 = 1;
const SPOKE_CHAIN: u64 = 0x101;
const SPOKE_ID: u64 = 3;
const SPOKE_VAULT: address = @0x51;
const ASSET_USDG: u8 = 1;
const ENDPOINT_CODE: u8 = 1;
const MAX_SYNC_AGE_MS: u64 = 86_400_000; // 1 day
const ACK_DEADLINE_MS: u64 = 43_200_000; // 12 h
const CURATOR_ON_SPOKE: address = @0xCC;
const DEPOSITOR: address = @0xD1;

const PAR: u128 = 1_000_000_000_000; // price scale, 1.0

/// Protocol + untranched USDC vault + endpoint registry with the dev
/// relayer transport, hub chain id set, spoke bound.
fun setup(scenario: &mut Scenario): Clock {
    let clock = th::init_protocol(scenario);
    th::new_default_vault(scenario, &clock);

    ts::next_tx(scenario, th::admin_addr());
    endpoint::init_for_testing(scenario.ctx());

    ts::next_tx(scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(scenario);
    let mut reg = ts::take_shared<EndpointRegistry>(scenario);
    endpoint::allow_endpoint<RelayerEndpoint>(&admin_cap, &mut reg);
    endpoint::add_relayer(&admin_cap, &mut reg, th::admin_addr());
    endpoint::set_hub_chain_id(&admin_cap, &mut reg, HUB_CHAIN);
    ts::return_shared(reg);
    th::return_admin_cap(scenario, admin_cap);
    clock
}

fun bind_spoke(scenario: &mut Scenario, clock: &Clock) {
    ts::next_tx(scenario, th::curator_addr());
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    ts::next_tx(scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(scenario);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let reg = ts::take_shared<EndpointRegistry>(scenario);
    multichain::bind_spoke<RelayerEndpoint, USDG>(
        &admin_cap,
        &cap,
        &mut v,
        &reg,
        SPOKE_ID,
        SPOKE_CHAIN,
        SPOKE_VAULT,
        ENDPOINT_CODE,
        ASSET_USDG,
        MAX_SYNC_AGE_MS,
        ACK_DEADLINE_MS,
        CURATOR_ON_SPOKE,
        clock,
        scenario.ctx(),
    );
    ts::return_shared(reg);
    ts::return_shared(v);
    th::return_admin_cap(scenario, admin_cap);
    transfer::public_transfer(cap, th::curator_addr());
}

fun vault_addr(v: &TradingVault): address { object::id(v).to_address() }

/// Build a complete appraisal including the spoke leg, using a
/// pre-minted USDG attestation (`PriceAttestation` is copyable, so one
/// mint per transaction serves both the leg and the handler).
fun full_appraisal(
    v: &TradingVault,
    cfg: &vault_v2::registry::VaultProtocolConfig,
    att: vault_v2::price::PriceAttestation,
    clock: &Clock,
): vault::Appraisal {
    let mut a = vault::begin_appraisal<USDC>(v);
    if (vault::has_spoke(v, SPOKE_ID)) {
        multichain::record_spoke_state(v, cfg, &mut a, SPOKE_ID, vector[att], clock);
    };
    a
}

/// Deliver a spoke→hub DepositNotice as the relayer and return whether
/// it was accepted (from vault state).
fun deliver_deposit(
    scenario: &mut Scenario,
    clock: &Clock,
    seq: u64,
    deposit_seq: u64,
    amount: u128,
    tranche: u8,
) {
    ts::next_tx(scenario, th::admin_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let reg = ts::take_shared<EndpointRegistry>(scenario);
    let cfg = th::take_protocol_config(scenario);
    let bytes = wire::encode_deposit_notice_for_testing(
        SPOKE_CHAIN,
        HUB_CHAIN,
        SPOKE_VAULT,
        vault_addr(&v),
        seq,
        SPOKE_ID,
        deposit_seq,
        DEPOSITOR,
        ASSET_USDG,
        amount,
        tranche,
        clock.timestamp_ms(),
    );
    let msg = endpoint_relayer::deliver(&reg, bytes, scenario.ctx());
    let att = th::attest<USDG, USDC>(scenario, PAR, clock.timestamp_ms());
    let appraisal = full_appraisal(&v, &cfg, att, clock);
    let out = multichain::handle_deposit_notice(
        &mut v,
        &cfg,
        &reg,
        msg,
        appraisal,
        att,
        clock,
        scenario.ctx(),
    );
    endpoint_relayer::send(&reg, out, scenario.ctx());
    ts::return_shared(cfg);
    ts::return_shared(reg);
    ts::return_shared(v);
}

fun deliver_withdraw(
    scenario: &mut Scenario,
    clock: &Clock,
    seq: u64,
    request_seq: u64,
    shares: u128,
    all: bool,
    tranche: u8,
) {
    ts::next_tx(scenario, th::admin_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let reg = ts::take_shared<EndpointRegistry>(scenario);
    let cfg = th::take_protocol_config(scenario);
    let bytes = wire::encode_withdraw_request_for_testing(
        SPOKE_CHAIN,
        HUB_CHAIN,
        SPOKE_VAULT,
        vault_addr(&v),
        seq,
        SPOKE_ID,
        request_seq,
        DEPOSITOR,
        tranche,
        shares,
        all,
    );
    let msg = endpoint_relayer::deliver(&reg, bytes, scenario.ctx());
    let att = th::attest<USDG, USDC>(scenario, PAR, clock.timestamp_ms());
    let appraisal = full_appraisal(&v, &cfg, att, clock);
    let out = multichain::handle_withdraw_request(
        &mut v,
        &cfg,
        &reg,
        msg,
        appraisal,
        att,
        clock,
        scenario.ctx(),
    );
    endpoint_relayer::send(&reg, out, scenario.ctx());
    ts::return_shared(cfg);
    ts::return_shared(reg);
    ts::return_shared(v);
}

fun deliver_receipt(scenario: &mut Scenario, seq: u64, request_seq: u64, amount: u128) {
    ts::next_tx(scenario, th::admin_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let reg = ts::take_shared<EndpointRegistry>(scenario);
    let bytes = wire::encode_payout_receipt_for_testing(
        SPOKE_CHAIN,
        HUB_CHAIN,
        SPOKE_VAULT,
        vault_addr(&v),
        seq,
        SPOKE_ID,
        request_seq,
        amount,
    );
    let msg = endpoint_relayer::deliver(&reg, bytes, scenario.ctx());
    multichain::handle_payout_receipt(&mut v, &reg, msg);
    ts::return_shared(reg);
    ts::return_shared(v);
}

fun deliver_state_sync(
    scenario: &mut Scenario,
    seq: u64,
    free: u128,
    reserved: u128,
    ts_ms: u64,
) {
    ts::next_tx(scenario, th::admin_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let reg = ts::take_shared<EndpointRegistry>(scenario);
    let bytes = wire::encode_state_sync_for_testing(
        SPOKE_CHAIN,
        HUB_CHAIN,
        SPOKE_VAULT,
        vault_addr(&v),
        seq,
        SPOKE_ID,
        vector[ASSET_USDG],
        vector[free],
        vector[reserved],
        5_000_000_000_000_000,
        vector[],
        ts_ms,
    );
    let msg = endpoint_relayer::deliver(&reg, bytes, scenario.ctx());
    multichain::handle_state_sync(&mut v, &reg, msg);
    ts::return_shared(reg);
    ts::return_shared(v);
}

// ═══════════════════════════════ tests ═══════════════════════════════

#[test]
fun deposit_mints_ledger_shares_and_recognizes_funds() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);

    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    assert!(spoke::free_total(s, ASSET_USDG) == 1_000_000);
    assert!(spoke::holdings_count(s) == 1);
    let (shares, basis, _, _) = spoke::holding_fields(s, DEPOSITOR, 0);
    assert!(basis == 1_000_000);
    assert!(shares > 0);
    assert!(vault::total_shares(&v) == shares);
    // NAV via a fresh complete appraisal: pure spoke funds at par.
    let cfg = th::take_protocol_config(&scenario);
    let att = th::attest<USDG, USDC>(&scenario, PAR, clock.timestamp_ms());
    let a = full_appraisal(&v, &cfg, att, &clock);
    vault::crank_appraisal(&v, a);
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun deposit_rejected_while_deposits_paused() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);

    // Curator pauses vault deposits.
    ts::next_tx(&mut scenario, th::curator_addr());
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    vault::set_deposits_paused(&mut v, &cap, true);
    ts::return_shared(v);
    ts::return_to_sender(&scenario, cap);

    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    // Rejected: nothing recognized, nothing minted, but the lane moved.
    assert!(spoke::free_total(s, ASSET_USDG) == 0);
    assert!(spoke::holdings_count(s) == 0);
    assert!(vault::total_shares(&v) == 0);
    assert!(spoke::inbound_seq(s) == 1);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 143, location = vault_v2::spoke)]
fun replayed_sequence_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);
    // Same lane seq again — replay.
    deliver_deposit(&mut scenario, &clock, 1, 2, 1_000_000, 0);
    abort 0
}

#[test]
#[expected_failure(abort_code = 143, location = vault_v2::spoke)]
fun sequence_gap_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 2, 1, 1_000_000, 0);
    abort 0
}

#[test]
#[expected_failure(abort_code = 150, location = vault_v2::multichain)]
fun wrong_lane_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let reg = ts::take_shared<EndpointRegistry>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    // src_app is not the bound spoke vault.
    let bytes = wire::encode_deposit_notice_for_testing(
        SPOKE_CHAIN,
        HUB_CHAIN,
        @0xBAD,
        vault_addr(&v),
        1,
        SPOKE_ID,
        1,
        DEPOSITOR,
        ASSET_USDG,
        1_000_000,
        0,
        clock.timestamp_ms(),
    );
    let msg = endpoint_relayer::deliver(&reg, bytes, scenario.ctx());
    let att = th::attest<USDG, USDC>(&scenario, PAR, clock.timestamp_ms());
    let appraisal = full_appraisal(&v, &cfg, att, &clock);
    let out = multichain::handle_deposit_notice(
        &mut v, &cfg, &reg, msg, appraisal, att, &clock, scenario.ctx(),
    );
    endpoint_relayer::send(&reg, out, scenario.ctx());
    abort 0
}

#[test]
fun withdraw_burns_books_payable_and_receipt_clears_it() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);

    // Past lockup (1h default), still inside the sync freshness window.
    clock.increment_for_testing(2 * 3_600_000);

    deliver_withdraw(&mut scenario, &clock, 2, 1, 0, true, 0);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    // Full exit at par with no profit: no fee, payable == deposit.
    let payable = spoke::payable(s, ASSET_USDG);
    assert!(payable == 1_000_000);
    assert!(spoke::free_total(s, ASSET_USDG) == 1_000_000);
    assert!(spoke::holdings_count(s) == 0);
    assert!(vault::total_shares(&v) == 0);
    // NAV nets to zero across contribution and liability.
    let cfg = th::take_protocol_config(&scenario);
    let att = th::attest<USDG, USDC>(&scenario, PAR, clock.timestamp_ms());
    let a = full_appraisal(&v, &cfg, att, &clock);
    vault::crank_appraisal(&v, a);
    ts::return_shared(cfg);
    ts::return_shared(v);

    deliver_receipt(&mut scenario, 3, 1, 1_000_000);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    assert!(spoke::payable(s, ASSET_USDG) == 0);
    assert!(spoke::free_total(s, ASSET_USDG) == 0);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun withdraw_rejected_while_locked_leaves_ledger_intact() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);

    // Immediately (inside lockup) — must reject, not burn.
    deliver_withdraw(&mut scenario, &clock, 2, 1, 0, true, 0);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    assert!(spoke::payable(s, ASSET_USDG) == 0);
    assert!(spoke::holdings_count(s) == 1);
    assert!(vault::total_shares(&v) > 0);
    assert!(spoke::inbound_seq(s) == 2);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun withdraw_crystallizes_fees_on_profit() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);

    // Hub-side profit: curator session gain doubles NAV.
    th::session_gain<USDC>(&mut scenario, 1_000_000);

    clock.increment_for_testing(2 * 3_600_000);
    deliver_withdraw(&mut scenario, &clock, 2, 1, 0, true, 0);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    // Exit value ~2_000_000, profit ~1_000_000, 10% curator fee →
    // payable ~1_900_000; fee shares keep the books non-empty.
    let payable = spoke::payable(s, ASSET_USDG);
    assert!(payable > 1_890_000 && payable < 1_910_000);
    assert!(vault::total_shares(&v) > 0);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 145, location = vault_v2::multichain)]
fun stale_spoke_blocks_nav() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);

    clock.increment_for_testing(MAX_SYNC_AGE_MS + 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let att = th::attest<USDG, USDC>(&scenario, PAR, clock.timestamp_ms());
    let a = full_appraisal(&v, &cfg, att, &clock);
    vault::crank_appraisal(&v, a);
    abort 0
}

#[test]
fun state_sync_refreshes_the_window() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);

    clock.increment_for_testing(MAX_SYNC_AGE_MS + 1);
    deliver_state_sync(&mut scenario, 2, 1_000_000, 0, clock.timestamp_ms());

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    assert!(spoke::last_sync_ms(s) == clock.timestamp_ms());
    assert!(spoke::fee_pot_balance(s) == 5_000_000_000_000_000);
    let cfg = th::take_protocol_config(&scenario);
    let att = th::attest<USDG, USDC>(&scenario, PAR, clock.timestamp_ms());
    let a = full_appraisal(&v, &cfg, att, &clock);
    vault::crank_appraisal(&v, a);
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 82, location = vault_v2::vault)]
fun appraisal_without_spoke_leg_is_incomplete() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let a = vault::begin_appraisal<USDC>(&v);
    vault::crank_appraisal(&v, a);
    abort 0
}

#[test]
#[expected_failure(abort_code = 151, location = vault_v2::vault)]
fun close_blocked_while_spoke_bound() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::curator_addr());
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    vault::initiate_close(&mut v, &cap);
    abort 0
}

#[test]
fun drained_spoke_unbinds_and_close_proceeds() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 0);
    clock.increment_for_testing(2 * 3_600_000);
    deliver_withdraw(&mut scenario, &clock, 2, 1, 0, true, 0);
    deliver_receipt(&mut scenario, 3, 1, 1_000_000);

    ts::next_tx(&mut scenario, th::curator_addr());
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    ts::next_tx(&mut scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    multichain::unbind_spoke(&admin_cap, &cap, &mut v, SPOKE_ID);
    assert!(vault::spoke_count(&v) == 0);
    vault::initiate_close(&mut v, &cap);
    assert!(vault::is_closing(&v));
    ts::return_shared(v);
    th::return_admin_cap(&scenario, admin_cap);
    transfer::public_transfer(cap, th::curator_addr());
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun tranched_vault_routes_spoke_deposits_by_tranche() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_tranched_vault(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::admin_addr());
    endpoint::init_for_testing(scenario.ctx());
    ts::next_tx(&mut scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(&scenario);
    let mut reg = ts::take_shared<EndpointRegistry>(&scenario);
    endpoint::allow_endpoint<RelayerEndpoint>(&admin_cap, &mut reg);
    endpoint::add_relayer(&admin_cap, &mut reg, th::admin_addr());
    endpoint::set_hub_chain_id(&admin_cap, &mut reg, HUB_CHAIN);
    ts::return_shared(reg);
    th::return_admin_cap(&scenario, admin_cap);
    bind_spoke(&mut scenario, &clock);

    // Junior deposit accepted; senior deposit through the same lane is
    // then allowed by the 20% target buffer only up to 4x junior.
    deliver_deposit(&mut scenario, &clock, 1, 1, 1_000_000, 2);
    deliver_deposit(&mut scenario, &clock, 2, 2, 1_000_000, 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(capital::junior_shares(vault::book(&v)) > 0);
    assert!(capital::senior_shares(vault::book(&v)) > 0);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    assert!(spoke::free_total(s, ASSET_USDG) == 2_000_000);
    assert!(spoke::holdings_count(s) == 2);

    // Untranched code 0 on a tranched vault → rejected, lane advances.
    ts::return_shared(v);
    deliver_deposit(&mut scenario, &clock, 3, 3, 1_000_000, 0);
    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let s = vault::spoke_ref(&v, SPOKE_ID);
    assert!(spoke::free_total(s, ASSET_USDG) == 2_000_000);
    assert!(spoke::inbound_seq(s) == 3);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun config_sync_builds_and_ships() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    bind_spoke(&mut scenario, &clock);

    ts::next_tx(&mut scenario, th::admin_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let reg = ts::take_shared<EndpointRegistry>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let out = multichain::build_config_sync(&mut v, &cfg, &reg, SPOKE_ID);
    endpoint_relayer::send(&reg, out, scenario.ctx());
    let s = vault::spoke_ref(&v, SPOKE_ID);
    assert!(spoke::outbound_seq(s) == 1);
    ts::return_shared(cfg);
    ts::return_shared(reg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    scenario.end();
}
