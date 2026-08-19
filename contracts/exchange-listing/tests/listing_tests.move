#[test_only]
module exchange_listing::listing_tests;

use sui::clock;
use sui::coin;
use sui::test_scenario as ts;

use exchange::admin;
use exchange::registry::SettlementRegistry;
use exchange_listing::exchange_listing::{Self as listing, AdminCap, ListingAuthority};
use options_core::bucket::{Self, Bucket};
use options_core::enc0::B00;
use options_core::option_coin::{OptionCall, OptionPut};
use options_core::put_bucket::{Self, PutBucket};

public struct UNDER has drop {}
public struct QUOTE has drop {}
public struct QUOTE2 has drop {}

const ADMIN: address = @0xAD;
const EXPIRY_MS: u64 = 2_000_000;
const NOW_MS: u64 = 1_000;

/// init + take the admin cap + deposit a ListingCap into the authority.
fun setup(s: &mut ts::Scenario): (AdminCap, ListingAuthority) {
    listing::init_for_testing(s.ctx());
    s.next_tx(ADMIN);
    let cap = s.take_from_sender<AdminCap>();
    let mut auth = s.take_shared<ListingAuthority>();
    let lcap = admin::mint_listing_for_testing(s.ctx());
    listing::deposit_cap(&cap, &mut auth, lcap);
    (cap, auth)
}

fun make_call_bucket<S>(s: &mut ts::Scenario) {
    let tcap = coin::create_treasury_cap_for_testing<
        OptionCall<UNDER, S, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>,
    >(s.ctx());
    bucket::create_bucket_for_testing<
        UNDER,
        S,
        OptionCall<UNDER, S, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>,
    >(tcap, EXPIRY_MS, 100, 0, s.ctx());
}

fun teardown(s: ts::Scenario, cap: AdminCap, auth: ListingAuthority) {
    ts::return_shared(auth);
    transfer::public_transfer(cap, ADMIN);
    s.end();
}

#[test]
fun list_call_market_happy_path() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 25);
    make_call_bucket<QUOTE>(&mut s);
    s.next_tx(ADMIN);
    let bucket = s.take_shared<
        Bucket<UNDER, QUOTE, OptionCall<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>>,
    >();
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(NOW_MS);

    let id = listing::create_call_market(&mut auth, &bucket, &clk, s.ctx());
    let base = exchange::order::canonical_type<
        OptionCall<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>,
    >();
    assert!(listing::is_listed(&auth, base));
    assert!(listing::market_for(&auth, base) == option::some(id));

    s.next_tx(ADMIN);
    let reg = s.take_shared_by_id<
        SettlementRegistry<
            OptionCall<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>,
            QUOTE,
        >,
    >(id);
    assert!(reg.tick_size() == 5);
    assert!(reg.min_size() == 10);
    assert!(reg.current_fee_bps() == 25);
    ts::return_shared(reg);

    ts::return_shared(bucket);
    clk.destroy_for_testing();
    teardown(s, cap, auth);
}

#[test]
fun list_put_market_happy_path() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 25);
    let tcap = coin::create_treasury_cap_for_testing<
        OptionPut<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>,
    >(s.ctx());
    put_bucket::create_put_bucket_for_testing<
        UNDER,
        QUOTE,
        OptionPut<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>,
    >(tcap, EXPIRY_MS, 100, 0, s.ctx());
    s.next_tx(ADMIN);
    let bucket = s.take_shared<
        PutBucket<UNDER, QUOTE, OptionPut<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>>,
    >();
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(NOW_MS);

    let id = listing::create_put_market(&mut auth, &bucket, &clk, s.ctx());
    let base = exchange::order::canonical_type<
        OptionPut<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>,
    >();
    assert!(listing::market_for(&auth, base) == option::some(id));

    ts::return_shared(bucket);
    clk.destroy_for_testing();
    teardown(s, cap, auth);
}

#[test, expected_failure(abort_code = listing::EAlreadyListed)]
fun duplicate_listing_rejected() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 25);
    make_call_bucket<QUOTE>(&mut s);
    s.next_tx(ADMIN);
    let bucket = s.take_shared<
        Bucket<UNDER, QUOTE, OptionCall<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>>,
    >();
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(NOW_MS);
    listing::create_call_market(&mut auth, &bucket, &clk, s.ctx());
    listing::create_call_market(&mut auth, &bucket, &clk, s.ctx());
    abort 0
}

#[test, expected_failure(abort_code = listing::EQuoteNotEnabled)]
fun unlisted_quote_rejected() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 25);
    // Bucket settles in QUOTE2, which has no defaults.
    make_call_bucket<QUOTE2>(&mut s);
    s.next_tx(ADMIN);
    let bucket = s.take_shared<
        Bucket<UNDER, QUOTE2, OptionCall<UNDER, QUOTE2, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>>,
    >();
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(NOW_MS);
    listing::create_call_market(&mut auth, &bucket, &clk, s.ctx());
    abort 0
}

#[test, expected_failure(abort_code = listing::EExpiredSeries)]
fun expired_series_rejected() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 25);
    make_call_bucket<QUOTE>(&mut s);
    s.next_tx(ADMIN);
    let bucket = s.take_shared<
        Bucket<UNDER, QUOTE, OptionCall<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>>,
    >();
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(EXPIRY_MS);
    listing::create_call_market(&mut auth, &bucket, &clk, s.ctx());
    abort 0
}

#[test, expected_failure(abort_code = listing::ENoCap)]
fun no_cap_rejected() {
    let mut s = ts::begin(ADMIN);
    listing::init_for_testing(s.ctx());
    s.next_tx(ADMIN);
    let cap = s.take_from_sender<AdminCap>();
    let mut auth = s.take_shared<ListingAuthority>();
    // No ListingCap deposited.
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 25);
    make_call_bucket<QUOTE>(&mut s);
    s.next_tx(ADMIN);
    let bucket = s.take_shared<
        Bucket<UNDER, QUOTE, OptionCall<UNDER, QUOTE, B00, B00, B00, B00, B00, B00, B00, B00, B00, B00>>,
    >();
    let mut clk = clock::create_for_testing(s.ctx());
    clk.set_for_testing(NOW_MS);
    listing::create_call_market(&mut auth, &bucket, &clk, s.ctx());
    abort 0
}

#[test]
fun cap_withdraw_and_redeposit() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    assert!(listing::has_cap(&auth));
    let lcap = listing::withdraw_cap(&cap, &mut auth);
    assert!(!listing::has_cap(&auth));
    listing::deposit_cap(&cap, &mut auth, lcap);
    assert!(listing::has_cap(&auth));
    teardown(s, cap, auth);
}

#[test]
fun quote_defaults_lifecycle() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    assert!(!listing::quote_enabled<QUOTE>(&auth));
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 25);
    assert!(listing::quote_enabled<QUOTE>(&auth));
    // Update in place.
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 7, 20, 30);
    listing::clear_quote_defaults<QUOTE>(&cap, &mut auth);
    assert!(!listing::quote_enabled<QUOTE>(&auth));
    teardown(s, cap, auth);
}

#[test, expected_failure(abort_code = listing::EFeeTooHigh)]
fun defaults_fee_ceiling_enforced() {
    let mut s = ts::begin(ADMIN);
    let (cap, mut auth) = setup(&mut s);
    listing::set_quote_defaults<QUOTE>(&cap, &mut auth, 5, 10, 51);
    abort 0
}
