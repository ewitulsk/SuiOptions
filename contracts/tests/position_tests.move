#[test_only]
module options_protocol::position_tests;

use sui::test_scenario::{Self as ts};

use options_protocol::position;
use options_protocol::test_helpers as th;

#[test]
fun test_mint_accessors_and_burn() {
    let mut scenario = ts::begin(th::writer_addr());
    let bid = object::id_from_address(@0xBEEF);
    let p = position::mint(bid, 100, 250, scenario.ctx());
    assert!(position::bucket_id(&p) == bid, 0);
    assert!(position::range_start(&p) == 100, 0);
    assert!(position::range_end(&p) == 250, 0);
    assert!(position::amount(&p) == 150, 0);
    let (_pid, pbid, rs, re) = position::burn(p);
    assert!(pbid == bid, 0);
    assert!(rs == 100, 0);
    assert!(re == 250, 0);
    ts::end(scenario);
}
