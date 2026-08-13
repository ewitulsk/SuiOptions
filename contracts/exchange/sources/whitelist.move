/// Guarded-launch ingress whitelist for the exchange. The exchange package
/// deliberately has no dependency on options_core (audit-scope isolation),
/// so it carries its own copy of the whitelist under its own AdminCap; the
/// admin tooling treats the core and exchange lists as one logical list.
///
/// Checked only where net-new money enters the exchange: BalanceManager
/// deposits and fill validation (a taker's wallet coin can enter a fill
/// without ever touching a BalanceManager). Withdrawals and cancels are
/// never gated.
module exchange::whitelist;

use sui::event;
use sui::vec_set::{Self, VecSet};

use exchange::admin::AdminCap;

const EIngressRestricted: u64 = 1;
const EIngressPaused: u64 = 2;

public struct Whitelist has key {
    id: UID,
    members: VecSet<address>,
    /// When false the member check is skipped entirely (go-public lever).
    /// Membership is retained, so re-enabling restores the prior cohort.
    whitelist_enabled: bool,
    /// Kill switch: blocks all gated ingress regardless of membership or
    /// `whitelist_enabled`. Withdrawals and cancels are unaffected.
    ingress_paused: bool,
}

public struct MemberAdded has copy, drop {
    member: address,
}

public struct MemberRemoved has copy, drop {
    member: address,
}

public struct WhitelistEnabledSet has copy, drop {
    enabled: bool,
}

public struct IngressPauseSet has copy, drop {
    paused: bool,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(Whitelist {
        id: object::new(ctx),
        members: vec_set::empty(),
        whitelist_enabled: true,
        ingress_paused: false,
    });
}

public fun add_member(_: &AdminCap, wl: &mut Whitelist, member: address) {
    wl.members.insert(member);
    event::emit(MemberAdded { member });
}

public fun remove_member(_: &AdminCap, wl: &mut Whitelist, member: address) {
    wl.members.remove(&member);
    event::emit(MemberRemoved { member });
}

public fun set_whitelist_enabled(_: &AdminCap, wl: &mut Whitelist, enabled: bool) {
    wl.whitelist_enabled = enabled;
    event::emit(WhitelistEnabledSet { enabled });
}

public fun set_ingress_paused(_: &AdminCap, wl: &mut Whitelist, paused: bool) {
    wl.ingress_paused = paused;
    event::emit(IngressPauseSet { paused });
}

/// Gate for every function that lets net-new money into the exchange.
/// Withdrawals and cancels must never call this.
public fun assert_ingress_allowed(wl: &Whitelist, who: address) {
    assert!(!wl.ingress_paused, EIngressPaused);
    assert!(
        !wl.whitelist_enabled || wl.members.contains(&who),
        EIngressRestricted,
    );
}

public fun is_member(wl: &Whitelist, who: address): bool { wl.members.contains(&who) }

public fun whitelist_enabled(wl: &Whitelist): bool { wl.whitelist_enabled }

public fun ingress_paused(wl: &Whitelist): bool { wl.ingress_paused }

#[test_only]
public fun share_for_testing(ctx: &mut TxContext) {
    init(ctx);
}

/// A whitelist with the member check disabled, for tests that predate the
/// gate (every sender passes; pause still bites if set).
#[test_only]
public fun new_open_for_testing(ctx: &mut TxContext): Whitelist {
    Whitelist {
        id: object::new(ctx),
        members: vec_set::empty(),
        whitelist_enabled: false,
        ingress_paused: false,
    }
}

#[test_only]
public fun add_member_for_testing(wl: &mut Whitelist, member: address) {
    wl.members.insert(member);
}

#[test_only]
public fun set_enabled_for_testing(wl: &mut Whitelist, enabled: bool) {
    wl.whitelist_enabled = enabled;
}

#[test_only]
public fun set_paused_for_testing(wl: &mut Whitelist, paused: bool) {
    wl.ingress_paused = paused;
}

#[test_only]
public fun destroy_for_testing(wl: Whitelist) {
    let Whitelist { id, .. } = wl;
    id.delete();
}
