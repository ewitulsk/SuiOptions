/// Config-surface tests. The receive/send paths require live CCIP
/// objects (`CCIPObjectRef`, OffRamp-built `Any2SuiMessage`, OnRamp
/// state) and are exercised on testnet per the rollout plan — not
/// simulatable here.
#[test_only]
module endpoint_ccip::endpoint_ccip_tests;

use sui::test_scenario as ts;

use options_core::admin::{Self, AdminCap};

use endpoint_ccip::endpoint_ccip::{Self as ep, CcipTransport};

const ADMIN: address = @0xA1;

#[test]
fun init_and_chain_mapping() {
    let mut scenario = ts::begin(ADMIN);
    admin::init_for_testing(scenario.ctx());
    ep::init_for_testing(scenario.ctx());

    ts::next_tx(&mut scenario, ADMIN);
    let cap = ts::take_from_sender<AdminCap>(&scenario);
    let mut t = ts::take_shared<CcipTransport>(&scenario);
    ep::map_chain(&cap, &mut t, 0x101, 999_888_777, x"00aa");
    assert!(ep::selector_for_chain(&t, 0x101) == 999_888_777);
    ep::unmap_chain(&cap, &mut t, 0x101);
    ep::map_chain(&cap, &mut t, 0x101, 111, x"00bb");
    assert!(ep::selector_for_chain(&t, 0x101) == 111);
    ts::return_shared(t);
    ts::return_to_sender(&scenario, cap);
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 1, location = endpoint_ccip::endpoint_ccip)]
fun unmapped_chain_aborts() {
    let mut scenario = ts::begin(ADMIN);
    admin::init_for_testing(scenario.ctx());
    ep::init_for_testing(scenario.ctx());
    ts::next_tx(&mut scenario, ADMIN);
    let t = ts::take_shared<CcipTransport>(&scenario);
    let _ = ep::selector_for_chain(&t, 42);
    abort 0
}
