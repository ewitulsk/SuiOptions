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
    let dom = wl::domain_options();
    assert!(w.whitelist_enabled(dom) && !w.ingress_paused(dom));

    // Member passes; stranger is restricted.
    wl::add_member(&cap, &mut w, dom, MEMBER);
    wl::assert_ingress_allowed(&w, MEMBER, dom);
    assert!(w.is_member(dom, MEMBER) && !w.is_member(dom, STRANGER));

    // Go public: stranger passes; membership retained.
    wl::set_whitelist_enabled(&cap, &mut w, dom, false);
    wl::assert_ingress_allowed(&w, STRANGER, dom);
    wl::set_whitelist_enabled(&cap, &mut w, dom, true);
    assert!(w.is_member(dom, MEMBER));

    // Revocation is instant.
    wl::remove_member(&cap, &mut w, dom, MEMBER);
    assert!(!w.is_member(dom, MEMBER));

    // Events: one of each so far except enabled (2).
    assert!(sui::event::events_by_type<wl::MemberAdded>().length() == 1);
    assert!(sui::event::events_by_type<wl::MemberRemoved>().length() == 1);
    assert!(sui::event::events_by_type<wl::WhitelistEnabledSet>().length() == 2);

    ts::return_shared(w);
    ts::return_to_sender(&sc, cap);
    sc.end();
}

#[test]
fun domains_are_isolated() {
    let mut sc = ts::begin(ADMIN);
    setup(&mut sc);

    ts::next_tx(&mut sc, ADMIN);
    let cap = ts::take_from_sender<AdminCap>(&sc);
    let mut w = ts::take_shared<Whitelist>(&sc);

    // Membership on one domain never satisfies another's gate.
    wl::add_member(&cap, &mut w, wl::domain_vault_lp(), MEMBER);
    wl::assert_ingress_allowed(&w, MEMBER, wl::domain_vault_lp());
    assert!(w.is_member(wl::domain_vault_lp(), MEMBER));
    assert!(!w.is_member(wl::domain_options(), MEMBER));
    assert!(!w.is_member(wl::domain_exchange(), MEMBER));
    assert!(!w.is_member(wl::domain_vault_create(), MEMBER));

    // Same address can join a second domain independently.
    wl::add_member(&cap, &mut w, wl::domain_exchange(), MEMBER);
    wl::assert_ingress_allowed(&w, MEMBER, wl::domain_exchange());

    // Levers are per-domain: going public on options leaves exchange gated.
    wl::set_whitelist_enabled(&cap, &mut w, wl::domain_options(), false);
    wl::assert_ingress_allowed(&w, STRANGER, wl::domain_options());
    assert!(!w.whitelist_enabled(wl::domain_options()));
    assert!(w.whitelist_enabled(wl::domain_exchange()));

    // Pausing one domain leaves the others live.
    wl::set_ingress_paused(&cap, &mut w, wl::domain_vault_create(), true);
    assert!(w.ingress_paused(wl::domain_vault_create()));
    assert!(!w.ingress_paused(wl::domain_vault_lp()));
    wl::assert_ingress_allowed(&w, MEMBER, wl::domain_vault_lp());

    ts::return_shared(w);
    ts::return_to_sender(&sc, cap);
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun member_of_other_domain_is_restricted() {
    let mut sc = ts::begin(ADMIN);
    setup(&mut sc);
    ts::next_tx(&mut sc, ADMIN);
    let cap = ts::take_from_sender<AdminCap>(&sc);
    let mut w = ts::take_shared<Whitelist>(&sc);
    wl::add_member(&cap, &mut w, wl::domain_vault_lp(), MEMBER);
    wl::assert_ingress_allowed(&w, MEMBER, wl::domain_options());
    abort 0
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun stranger_restricted() {
    let mut sc = ts::begin(ADMIN);
    let mut w = wl::new_open_for_testing(sc.ctx());
    wl::set_enabled_for_testing(&mut w, true);
    wl::assert_ingress_allowed(&w, STRANGER, wl::domain_options());
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
    wl::assert_ingress_allowed(&w, MEMBER, wl::domain_exchange());
    wl::destroy_for_testing(w);
    sc.end();
}

#[test]
#[expected_failure(abort_code = 3, location = whitelist::whitelist)] // EInvalidDomain
fun unknown_domain_aborts() {
    let mut sc = ts::begin(ADMIN);
    let w = wl::new_open_for_testing(sc.ctx());
    wl::assert_ingress_allowed(&w, MEMBER, 4);
    wl::destroy_for_testing(w);
    sc.end();
}

#[test]
fun pause_all_hits_every_domain() {
    let mut sc = ts::begin(ADMIN);
    setup(&mut sc);
    ts::next_tx(&mut sc, ADMIN);
    let cap = ts::take_from_sender<AdminCap>(&sc);
    let mut w = ts::take_shared<Whitelist>(&sc);
    wl::set_ingress_paused_all(&cap, &mut w, true);
    assert!(w.ingress_paused(wl::domain_options()));
    assert!(w.ingress_paused(wl::domain_exchange()));
    assert!(w.ingress_paused(wl::domain_vault_create()));
    assert!(w.ingress_paused(wl::domain_vault_lp()));
    assert!(sui::event::events_by_type<wl::IngressPauseSet>().length() == 4);
    wl::set_ingress_paused_all(&cap, &mut w, false);
    assert!(!w.ingress_paused(wl::domain_options()));
    assert!(!w.ingress_paused(wl::domain_vault_lp()));
    ts::return_shared(w);
    ts::return_to_sender(&sc, cap);
    sc.end();
}
