#[test_only]
module whitelist::whitelist_tests;

use sui::test_scenario::{Self as ts};

use whitelist::whitelist::{Self as wl, AdminCap, Whitelist};

const ADMIN: address = @0xA1;
const MEMBER: address = @0xB2;
const STRANGER: address = @0xF6;

fun setup(sc: &mut ts::Scenario) {
    ts::next_tx(sc, ADMIN);
    wl::init_for_testing(sc.ctx());
}

#[test]
fun gate_semantics() {
    let mut sc = ts::begin(ADMIN);
    setup(&mut sc);

    ts::next_tx(&mut sc, ADMIN);
    let cap = ts::take_from_sender<AdminCap>(&sc);
    let mut w = ts::take_shared<Whitelist>(&sc);
    assert!(w.whitelist_enabled() && !w.ingress_paused());

    // Member passes; stranger is restricted.
    wl::add_member(&cap, &mut w, MEMBER);
    wl::assert_ingress_allowed(&w, MEMBER);
    assert!(w.is_member(MEMBER) && !w.is_member(STRANGER));

    // Go public: stranger passes; membership retained.
    wl::set_whitelist_enabled(&cap, &mut w, false);
    wl::assert_ingress_allowed(&w, STRANGER);
    wl::set_whitelist_enabled(&cap, &mut w, true);
    assert!(w.is_member(MEMBER));

    // Revocation is instant.
    wl::remove_member(&cap, &mut w, MEMBER);
    assert!(!w.is_member(MEMBER));

    // Events: one of each so far except enabled (2).
    assert!(sui::event::events_by_type<wl::MemberAdded>().length() == 1);
    assert!(sui::event::events_by_type<wl::MemberRemoved>().length() == 1);
    assert!(sui::event::events_by_type<wl::WhitelistEnabledSet>().length() == 2);

    ts::return_shared(w);
    ts::return_to_sender(&sc, cap);
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun stranger_restricted() {
    let mut sc = ts::begin(ADMIN);
    let w = wl::new_open_for_testing(sc.ctx());
    let mut w = w;
    wl::set_enabled_for_testing(&mut w, true);
    wl::assert_ingress_allowed(&w, STRANGER);
    wl::destroy_for_testing(w);
    sc.end();
}

#[test]
#[expected_failure(abort_code = 2, location = whitelist::whitelist)] // EIngressPaused
fun pause_blocks_even_members() {
    let mut sc = ts::begin(ADMIN);
    let mut w = wl::new_open_for_testing(sc.ctx());
    wl::add_member_for_testing(&mut w, MEMBER);
    wl::set_paused_for_testing(&mut w, true);
    wl::assert_ingress_allowed(&w, MEMBER);
    wl::destroy_for_testing(w);
    sc.end();
}

#[test]
fun pause_event_emitted() {
    let mut sc = ts::begin(ADMIN);
    setup(&mut sc);
    ts::next_tx(&mut sc, ADMIN);
    let cap = ts::take_from_sender<AdminCap>(&sc);
    let mut w = ts::take_shared<Whitelist>(&sc);
    wl::set_ingress_paused(&cap, &mut w, true);
    assert!(w.ingress_paused());
    assert!(sui::event::events_by_type<wl::IngressPauseSet>().length() == 1);
    ts::return_shared(w);
    ts::return_to_sender(&sc, cap);
    sc.end();
}
