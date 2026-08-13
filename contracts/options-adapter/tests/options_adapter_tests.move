#[test_only]
module options_adapter::options_adapter_tests;

use std::type_name;
use sui::balance;
use sui::clock::{Self, Clock};
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin::{Self, AdminCap, ProtocolConfig as CoreProtocolConfig};
use options_core::bucket::{Self, Bucket};

use trading_vault::price as tv_price;
use trading_vault::registry as tv_registry;
use trading_vault::registry::{IntegrationRegistry, OracleRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, CuratorCap, TradingVault};
use trading_vault::vault_mm;

use options_adapter::options_adapter as adapter;

/// Vault deposit asset == the call underlying.
public struct UND has drop {}
/// Settlement / premium asset.
public struct QUOTE has drop {}
/// Per-bucket option coin marker.
public struct CALL has drop {}

/// Local oracle witness for pricing QUOTE into UND.
public struct TestOracle has drop {}

const ADMIN: address = @0xA1;
const CURATOR: address = @0xC3;
const ALICE: address = @0xD4;
const MM: address = @0xE5;

const WRITE: u64 = 100_000;
const EXPIRY_MS: u64 = 10_000_000;

fun setup(sc: &mut Scenario): Clock {
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
    tv_registry::allow_adapter(
        &admin_cap,
        &mut ireg,
        type_name::with_defining_ids<vault_mm::VaultMm>(),
    );
    ts::return_shared(ireg);
    let mut oreg = ts::take_shared<OracleRegistry>(sc);
    tv_registry::allow_oracle(&admin_cap, &mut oreg, type_name::with_defining_ids<TestOracle>());
    ts::return_shared(oreg);
    // Core ingress whitelist: every named test actor is a member.
    let mut core_cfg = ts::take_shared<CoreProtocolConfig>(sc);
    admin::add_member(&admin_cap, &mut core_cfg, ADMIN);
    admin::add_member(&admin_cap, &mut core_cfg, CURATOR);
    admin::add_member(&admin_cap, &mut core_cfg, ALICE);
    admin::add_member(&admin_cap, &mut core_cfg, MM);
    ts::return_shared(core_cfg);

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
    ts::next_tx(sc, CURATOR);
    let cfg = ts::take_shared<VaultProtocolConfig>(sc);
    vault::create_vault<UND>(&cfg, 0, 1_000, 3_600_000, sc.ctx());
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
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock
}

/// MM writes `WRITE` collateralized calls into the bucket and the
/// resulting `Position` lands in vault custody under the ADAPTER's tag
/// (the retired RFQ settle's custody shape). Returns the position id.
fun custody_written_position(sc: &mut Scenario, clock: &Clock): ID {
    ts::next_tx(sc, MM);
    let mut bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(sc);
    let (pos, call_coins) = bucket::write_collateralized<UND, QUOTE, CALL>(
        &mut bucket,
        &core_cfg,
        coin::from_balance(balance::create_for_testing<UND>(WRITE), sc.ctx()),
        clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
    transfer::public_transfer(call_coins, MM);
    let pos_id = object::id(&pos);
    transfer::public_transfer(pos, CURATOR);
    ts::return_shared(bucket);

    ts::next_tx(sc, CURATOR);
    let mut v = ts::take_shared<TradingVault>(sc);
    let ireg = ts::take_shared<IntegrationRegistry>(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    let pos = ts::take_from_sender<options_core::position::Position>(sc);
    adapter::custody_position_for_testing(&mut v, &cap, &ireg, pos);
    ts::return_to_sender(sc, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
    pos_id
}

#[test]
fun custodied_position_appraises_at_exercise_now_mark() {
    let mut sc = ts::begin(ADMIN);
    let clock = setup(&mut sc);
    let pos_id = custody_written_position(&mut sc, &clock);

    // No exercise: the whole range is unexercised and marks at
    // min(spot, strike). Spot 1 UND = 2 QUOTE (0.5 UND per QUOTE):
    // strike proceeds 200k QUOTE → 100k UND vs spot 100k UND → 100k.
    // NAV = 1M free + 100k.
    ts::next_tx(&mut sc, ALICE);
    let v = ts::take_shared<TradingVault>(&sc);
    let cfg = ts::take_shared<VaultProtocolConfig>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    let att = tv_price::attest(
        TestOracle {},
        &oreg,
        type_name::with_defining_ids<QUOTE>(),
        type_name::with_defining_ids<UND>(),
        500_000_000_000, // 0.5 UND per QUOTE raw unit at 1e12
        clock.timestamp_ms(),
    );
    let mut appraisal = vault::begin_appraisal<UND>(&v);
    adapter::appraise_call_position<UND, QUOTE, CALL>(
        &v,
        &cfg,
        &mut appraisal,
        &bucket,
        pos_id,
        option::none(),
        option::some(att),
        &clock,
    );
    assert!(vault::appraisal_value(&appraisal) == 1_000_000 + 100_000);
    sui::test_utils::destroy(appraisal);
    ts::return_shared(bucket);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun redeem_after_expiry_returns_funds() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    let pos_id = custody_written_position(&mut sc, &clock);

    // No exercise; expiry passes; permissionless redeem returns all
    // escrowed underlying into the vault.
    clock.set_for_testing(EXPIRY_MS + 1);
    ts::next_tx(&mut sc, ALICE);
    let mut v = ts::take_shared<TradingVault>(&sc);
    let ireg = ts::take_shared<IntegrationRegistry>(&sc);
    let mut bucket = ts::take_shared<Bucket<UND, QUOTE, CALL>>(&sc);
    adapter::redeem_call_position<UND, QUOTE, CALL>(
        &mut v,
        &ireg,
        &mut bucket,
        pos_id,
        &clock,
        sc.ctx(),
    );
    assert!(vault::free_balance_of<UND>(&v) == 1_000_000 + WRITE);
    assert!(vault::free_balance_of<QUOTE>(&v) == 0);
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
    let core_cfg = ts::take_shared<CoreProtocolConfig>(sc);
    let (long_pos, long_coins) = bucket::write_collateralized<UND, QUOTE, LCALL>(
        &mut long_bucket,
        &core_cfg,
        coin::from_balance(balance::create_for_testing<UND>(100_000), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    let (spread_pos, short_coins) = bucket::write_spread<UND, QUOTE, CALL, LCALL>(
        &mut short_bucket,
        &core_cfg,
        &long_bucket,
        long_coins,
        // Exactly required_settlement(long_bucket, 100k) = 100k QUOTE.
        coin::from_balance(balance::create_for_testing<QUOTE>(100_000), sc.ctx()),
        &clock,
        sc.ctx(),
    );
    ts::return_shared(core_cfg);
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
