#[test_only]
module options_protocol::call_option_tests;

use sui::test_scenario::{Self as ts};

use options_protocol::call_option;
use options_protocol::test_helpers as th;

fun fake_bucket_id(addr: address): ID {
    object::id_from_address(addr)
}

#[test]
fun test_mint_amount_and_bucket_id() {
    let mut scenario = ts::begin(th::writer_addr());
    let bid = fake_bucket_id(@0xBEEF);
    let opt = call_option::mint(bid, 100, scenario.ctx());
    assert!(call_option::amount(&opt) == 100, 0);
    assert!(call_option::bucket_id(&opt) == bid, 0);
    let burned = call_option::burn(opt);
    assert!(burned == 100, 0);
    ts::end(scenario);
}

#[test]
fun test_split_then_join_round_trip() {
    let mut scenario = ts::begin(th::writer_addr());
    let bid = fake_bucket_id(@0xBEEF);
    let mut opt = call_option::mint(bid, 100, scenario.ctx());

    let piece = call_option::split(&mut opt, 30, scenario.ctx());
    assert!(call_option::amount(&opt) == 70, 0);
    assert!(call_option::amount(&piece) == 30, 0);
    assert!(call_option::bucket_id(&piece) == bid, 0);

    call_option::join(&mut opt, piece);
    assert!(call_option::amount(&opt) == 100, 0);

    call_option::burn(opt);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 12, location = options_protocol::call_option)] // amount_mismatch — split too large
fun test_split_exceeding_amount_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    let mut opt = call_option::mint(fake_bucket_id(@0xBEEF), 10, scenario.ctx());
    let extra = call_option::split(&mut opt, 11, scenario.ctx());
    call_option::burn(extra);
    call_option::burn(opt);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 21, location = options_protocol::call_option)] // zero_amount — splitting zero
fun test_split_zero_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    let mut opt = call_option::mint(fake_bucket_id(@0xBEEF), 10, scenario.ctx());
    let extra = call_option::split(&mut opt, 0, scenario.ctx());
    call_option::burn(extra);
    call_option::burn(opt);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 17, location = options_protocol::call_option)] // call_option_bucket_mismatch
fun test_join_different_bucket_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    let mut a = call_option::mint(fake_bucket_id(@0xAAAA), 10, scenario.ctx());
    let b = call_option::mint(fake_bucket_id(@0xBBBB), 5, scenario.ctx());
    call_option::join(&mut a, b);
    call_option::burn(a);
    ts::end(scenario);
}
