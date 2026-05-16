#[test_only]
module options_protocol::admin_tests;

use sui::test_scenario::{Self as ts};

use options_protocol::admin::{Self, AdminCap, ProtocolConfig};
use options_protocol::test_helpers as th;

#[test]
fun test_init_creates_admin_cap_and_config() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = ts::take_from_sender<AdminCap>(&scenario);
    let config = ts::take_shared<ProtocolConfig>(&scenario);

    assert!(admin::fee_bps(&config) == 0, 0);
    assert!(!admin::protocol_id(&config).is_empty(), 0);

    ts::return_to_sender(&scenario, cap);
    ts::return_shared(config);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_set_fee_bps_updates_value() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = ts::take_from_sender<AdminCap>(&scenario);
    let mut config = ts::take_shared<ProtocolConfig>(&scenario);

    admin::set_fee_bps(&cap, &mut config, 50);
    assert!(admin::fee_bps(&config) == 50, 0);

    admin::set_fee_bps(&cap, &mut config, 1000);
    assert!(admin::fee_bps(&config) == 1000, 0);

    ts::return_to_sender(&scenario, cap);
    ts::return_shared(config);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 18, location = options_protocol::admin)] // fee_too_high
fun test_set_fee_bps_too_high_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = ts::take_from_sender<AdminCap>(&scenario);
    let mut config = ts::take_shared<ProtocolConfig>(&scenario);

    admin::set_fee_bps(&cap, &mut config, 1001);

    ts::return_to_sender(&scenario, cap);
    ts::return_shared(config);
    clock.destroy_for_testing();
    ts::end(scenario);
}
