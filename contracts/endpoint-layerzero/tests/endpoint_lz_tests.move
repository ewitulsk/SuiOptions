/// Config-surface tests. The receive/send paths require live LayerZero
/// endpoint objects (executor-built `Call`s, messaging channels) and are
/// exercised on testnet per the rollout plan — not simulatable here.
#[test_only]
module endpoint_lz::endpoint_lz_tests;

use sui::test_scenario as ts;

use options_core::admin::{Self, AdminCap};

use endpoint_lz::endpoint_lz::{Self as ep, LzTransport};

const ADMIN: address = @0xA1;

#[test]
fun init_and_chain_mapping() {
    let mut scenario = ts::begin(ADMIN);
    admin::init_for_testing(scenario.ctx());
    ep::init_for_testing(scenario.ctx());

    ts::next_tx(&mut scenario, ADMIN);
    let cap = ts::take_from_sender<AdminCap>(&scenario);
    let mut t = ts::take_shared<LzTransport>(&scenario);
    ep::map_chain(&cap, &mut t, 0x101, 30_101);
    assert!(ep::eid_for_chain(&t, 0x101) == 30_101);
    ep::unmap_chain(&cap, &mut t, 0x101);
    ep::map_chain(&cap, &mut t, 0x101, 30_202);
    assert!(ep::eid_for_chain(&t, 0x101) == 30_202);
    ts::return_shared(t);
    ts::return_to_sender(&scenario, cap);
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 2, location = endpoint_lz::endpoint_lz)]
fun unmapped_chain_aborts() {
    let mut scenario = ts::begin(ADMIN);
    admin::init_for_testing(scenario.ctx());
    ep::init_for_testing(scenario.ctx());
    ts::next_tx(&mut scenario, ADMIN);
    let t = ts::take_shared<LzTransport>(&scenario);
    let _ = ep::eid_for_chain(&t, 42);
    abort 0
}
