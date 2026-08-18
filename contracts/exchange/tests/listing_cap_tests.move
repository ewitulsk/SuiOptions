#[test_only]
module exchange::listing_cap_tests;

use sui::test_scenario as ts;
use exchange::admin;
use exchange::registry::{Self, SettlementRegistry};

public struct BASE has drop {}
public struct QUOTE has drop {}

const ADMIN: address = @0xAD;

#[test]
fun listing_cap_creates_market() {
    let mut s = ts::begin(ADMIN);
    let cap = admin::mint_listing_for_testing(s.ctx());
    let id = registry::create_market_listed<BASE, QUOTE>(&cap, 5, 10, 25, s.ctx());
    s.next_tx(ADMIN);
    let reg = s.take_shared_by_id<SettlementRegistry<BASE, QUOTE>>(id);
    assert!(reg.tick_size() == 5);
    assert!(reg.min_size() == 10);
    assert!(reg.current_fee_bps() == 25);
    assert!(!reg.is_paused());
    ts::return_shared(reg);
    admin::burn_listing_for_testing(cap);
    s.end();
}

#[test, expected_failure(abort_code = registry::EFeeTooHigh)]
fun listing_cap_respects_fee_ceiling() {
    let mut s = ts::begin(ADMIN);
    let cap = admin::mint_listing_for_testing(s.ctx());
    registry::create_market_listed<BASE, QUOTE>(&cap, 1, 1, 51, s.ctx());
    abort 0
}

#[test, expected_failure(abort_code = registry::ESameToken)]
fun listing_cap_rejects_same_token() {
    let mut s = ts::begin(ADMIN);
    let cap = admin::mint_listing_for_testing(s.ctx());
    registry::create_market_listed<BASE, BASE>(&cap, 1, 1, 0, s.ctx());
    abort 0
}
