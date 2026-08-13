/// Guarded-launch ingress whitelist — the protocol's single access-control
/// surface. One shared `Whitelist`, one `AdminCap`, three levers:
///
/// - `members`: the vetted-cohort allowlist, instantly revocable
/// - `whitelist_enabled`: the go-public lever — `false` skips the member
///   check entirely (membership is retained, so re-enabling restores the
///   prior cohort)
/// - `ingress_paused`: the kill switch — blocks ALL gated ingress
///   regardless of membership or `whitelist_enabled`
///
/// Checked only where net-new money enters the protocol (option writes,
/// vault deposits + creation, exchange deposits + fills). Exits —
/// exercise, redeem, withdrawals, cancels, cranks, force sessions — must
/// NEVER call `assert_ingress_allowed`: gating ingress can't strand
/// funds; gating exits can.
module whitelist::whitelist;

use sui::event;
use sui::vec_set::{Self, VecSet};

const EIngressRestricted: u64 = 1;
const EIngressPaused: u64 = 2;

public struct AdminCap has key, store {
    id: UID,
}

public struct Whitelist has key {
    id: UID,
    members: VecSet<address>,
    whitelist_enabled: bool,
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
    transfer::public_transfer(AdminCap { id: object::new(ctx) }, ctx.sender());
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

/// The gate. Call with `ctx.sender()` at every net-new-money entry point;
/// never from an exit path.
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
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx);
}

/// Owned open-mode instance (member check off) for tests that predate the
/// gate: every sender passes; the pause still bites if set.
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
