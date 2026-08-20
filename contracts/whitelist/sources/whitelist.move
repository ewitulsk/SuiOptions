/// Guarded-launch ingress whitelist — the protocol's single access-control
/// surface. One shared `Whitelist`, one `AdminCap`, four independent
/// membership domains, each with three levers:
///
/// - `members`: the vetted-cohort allowlist, instantly revocable
/// - `enabled`: the go-public lever — `false` skips the member check
///   entirely (membership is retained, so re-enabling restores the prior
///   cohort)
/// - `paused`: the kill switch — blocks ALL gated ingress on that domain
///   regardless of membership or `enabled`
///
/// Domains (each gate names its domain as a compile-time constant — the
/// domain is never a caller-supplied argument, so membership on one domain
/// can never satisfy another's gate):
///
/// - `DOMAIN_OPTIONS`: writing/buying options through core (writer/trader
///   flows, self-writes, spreads) + any-strike bucket creation
/// - `DOMAIN_EXCHANGE`: exchange BalanceManager deposits + every
///   fill/match path
/// - `DOMAIN_VAULT_CREATE`: trading-vault creation
/// - `DOMAIN_VAULT_LP`: trading-vault deposits, curator commitment
///   funding, junior-reset recapitalization
///
/// Checked only where net-new money enters the protocol. Exits —
/// exercise, redeem, withdrawals, cancels, cranks, force sessions — must
/// NEVER call `assert_ingress_allowed`: gating ingress can't strand
/// funds; gating exits can.
module whitelist::whitelist;

use sui::event;
use sui::vec_set::{Self, VecSet};

const EIngressRestricted: u64 = 1;
const EIngressPaused: u64 = 2;
const EInvalidDomain: u64 = 3;

const DOMAIN_OPTIONS: u8 = 0;
const DOMAIN_EXCHANGE: u8 = 1;
const DOMAIN_VAULT_CREATE: u8 = 2;
const DOMAIN_VAULT_LP: u8 = 3;

public struct AdminCap has key, store {
    id: UID,
}

/// One membership domain: its cohort plus the two per-domain levers.
public struct DomainState has store {
    members: VecSet<address>,
    enabled: bool,
    paused: bool,
}

public struct Whitelist has key {
    id: UID,
    options: DomainState,
    exchange: DomainState,
    vault_create: DomainState,
    vault_lp: DomainState,
}

public struct MemberAdded has copy, drop {
    domain: u8,
    member: address,
}

public struct MemberRemoved has copy, drop {
    domain: u8,
    member: address,
}

public struct WhitelistEnabledSet has copy, drop {
    domain: u8,
    enabled: bool,
}

public struct IngressPauseSet has copy, drop {
    domain: u8,
    paused: bool,
}

fun new_domain(): DomainState {
    DomainState { members: vec_set::empty(), enabled: true, paused: false }
}

fun init(ctx: &mut TxContext) {
    transfer::public_transfer(AdminCap { id: object::new(ctx) }, ctx.sender());
    transfer::share_object(Whitelist {
        id: object::new(ctx),
        options: new_domain(),
        exchange: new_domain(),
        vault_create: new_domain(),
        vault_lp: new_domain(),
    });
}

fun domain_state(wl: &Whitelist, domain: u8): &DomainState {
    if (domain == DOMAIN_OPTIONS) &wl.options
    else if (domain == DOMAIN_EXCHANGE) &wl.exchange
    else if (domain == DOMAIN_VAULT_CREATE) &wl.vault_create
    else if (domain == DOMAIN_VAULT_LP) &wl.vault_lp
    else abort EInvalidDomain
}

fun domain_state_mut(wl: &mut Whitelist, domain: u8): &mut DomainState {
    if (domain == DOMAIN_OPTIONS) &mut wl.options
    else if (domain == DOMAIN_EXCHANGE) &mut wl.exchange
    else if (domain == DOMAIN_VAULT_CREATE) &mut wl.vault_create
    else if (domain == DOMAIN_VAULT_LP) &mut wl.vault_lp
    else abort EInvalidDomain
}

public fun add_member(_: &AdminCap, wl: &mut Whitelist, domain: u8, member: address) {
    wl.domain_state_mut(domain).members.insert(member);
    event::emit(MemberAdded { domain, member });
}

public fun remove_member(_: &AdminCap, wl: &mut Whitelist, domain: u8, member: address) {
    wl.domain_state_mut(domain).members.remove(&member);
    event::emit(MemberRemoved { domain, member });
}

public fun set_whitelist_enabled(_: &AdminCap, wl: &mut Whitelist, domain: u8, enabled: bool) {
    wl.domain_state_mut(domain).enabled = enabled;
    event::emit(WhitelistEnabledSet { domain, enabled });
}

public fun set_ingress_paused(_: &AdminCap, wl: &mut Whitelist, domain: u8, paused: bool) {
    wl.domain_state_mut(domain).paused = paused;
    event::emit(IngressPauseSet { domain, paused });
}

/// The big red button's whitelist leg: flip the pause on every domain in
/// one call.
public fun set_ingress_paused_all(cap: &AdminCap, wl: &mut Whitelist, paused: bool) {
    set_ingress_paused(cap, wl, DOMAIN_OPTIONS, paused);
    set_ingress_paused(cap, wl, DOMAIN_EXCHANGE, paused);
    set_ingress_paused(cap, wl, DOMAIN_VAULT_CREATE, paused);
    set_ingress_paused(cap, wl, DOMAIN_VAULT_LP, paused);
}

/// The gate. Call with `ctx.sender()` and the gate's own domain constant
/// at every net-new-money entry point; never from an exit path.
public fun assert_ingress_allowed(wl: &Whitelist, who: address, domain: u8) {
    let d = wl.domain_state(domain);
    assert!(!d.paused, EIngressPaused);
    assert!(!d.enabled || d.members.contains(&who), EIngressRestricted);
}

public fun domain_options(): u8 { DOMAIN_OPTIONS }

public fun domain_exchange(): u8 { DOMAIN_EXCHANGE }

public fun domain_vault_create(): u8 { DOMAIN_VAULT_CREATE }

public fun domain_vault_lp(): u8 { DOMAIN_VAULT_LP }

public fun is_member(wl: &Whitelist, domain: u8, who: address): bool {
    wl.domain_state(domain).members.contains(&who)
}

public fun whitelist_enabled(wl: &Whitelist, domain: u8): bool {
    wl.domain_state(domain).enabled
}

public fun ingress_paused(wl: &Whitelist, domain: u8): bool {
    wl.domain_state(domain).paused
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx);
}

#[test_only]
fun new_open_domain(): DomainState {
    DomainState { members: vec_set::empty(), enabled: false, paused: false }
}

/// Owned open-mode instance (member check off on every domain) for tests
/// that predate the gate: every sender passes; a pause still bites if set.
#[test_only]
public fun new_open_for_testing(ctx: &mut TxContext): Whitelist {
    Whitelist {
        id: object::new(ctx),
        options: new_open_domain(),
        exchange: new_open_domain(),
        vault_create: new_open_domain(),
        vault_lp: new_open_domain(),
    }
}

/// Adds to EVERY domain — keeps pre-split test fixtures working.
#[test_only]
public fun add_member_for_testing(wl: &mut Whitelist, member: address) {
    wl.options.members.insert(member);
    wl.exchange.members.insert(member);
    wl.vault_create.members.insert(member);
    wl.vault_lp.members.insert(member);
}

#[test_only]
public fun add_member_domain_for_testing(wl: &mut Whitelist, domain: u8, member: address) {
    wl.domain_state_mut(domain).members.insert(member);
}

#[test_only]
public fun remove_member_for_testing(wl: &mut Whitelist, member: address) {
    wl.options.members.remove(&member);
    wl.exchange.members.remove(&member);
    wl.vault_create.members.remove(&member);
    wl.vault_lp.members.remove(&member);
}

/// Flips the go-public lever on EVERY domain.
#[test_only]
public fun set_enabled_for_testing(wl: &mut Whitelist, enabled: bool) {
    wl.options.enabled = enabled;
    wl.exchange.enabled = enabled;
    wl.vault_create.enabled = enabled;
    wl.vault_lp.enabled = enabled;
}

/// Flips the pause on EVERY domain.
#[test_only]
public fun set_paused_for_testing(wl: &mut Whitelist, paused: bool) {
    wl.options.paused = paused;
    wl.exchange.paused = paused;
    wl.vault_create.paused = paused;
    wl.vault_lp.paused = paused;
}

#[test_only]
public fun destroy_for_testing(wl: Whitelist) {
    let Whitelist { id, options, exchange, vault_create, vault_lp } = wl;
    destroy_domain(options);
    destroy_domain(exchange);
    destroy_domain(vault_create);
    destroy_domain(vault_lp);
    id.delete();
}

#[test_only]
fun destroy_domain(d: DomainState) {
    let DomainState { .. } = d;
}
