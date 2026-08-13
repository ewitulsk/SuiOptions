module options_core::admin;

use options_core::errors;
use options_core::events;
use sui::vec_set::{Self, VecSet};

const MAX_FEE_BPS: u64 = 1000;

public struct AdminCap has key, store {
    id: UID,
}

public struct ProtocolConfig has key {
    id: UID,
    fee_bps: u64,
    protocol_id: vector<u8>,
    /// Guarded-launch ingress whitelist. Checked only where net-new money
    /// enters the protocol (writes, deposits) — never on exits.
    members: VecSet<address>,
    /// When false the member check is skipped entirely (go-public lever).
    /// Membership is retained, so re-enabling restores the prior cohort.
    whitelist_enabled: bool,
    /// Kill switch: blocks all gated ingress regardless of membership or
    /// `whitelist_enabled`. Exits are unaffected.
    ingress_paused: bool,
}

fun init(ctx: &mut TxContext) {
    let admin_cap = AdminCap { id: object::new(ctx) };
    let protocol_id = object::id_to_bytes(&object::id(&admin_cap));
    let config = ProtocolConfig {
        id: object::new(ctx),
        fee_bps: 0,
        protocol_id,
        members: vec_set::empty(),
        whitelist_enabled: true,
        ingress_paused: false,
    };
    transfer::public_transfer(admin_cap, ctx.sender());
    transfer::share_object(config);
}

public fun set_fee_bps(_: &AdminCap, config: &mut ProtocolConfig, new_bps: u64) {
    assert!(new_bps <= MAX_FEE_BPS, errors::fee_too_high());
    let old_bps = config.fee_bps;
    config.fee_bps = new_bps;
    events::emit_fee_updated(old_bps, new_bps);
}

public fun add_member(_: &AdminCap, config: &mut ProtocolConfig, member: address) {
    config.members.insert(member);
    events::emit_member_added(member);
}

public fun remove_member(_: &AdminCap, config: &mut ProtocolConfig, member: address) {
    config.members.remove(&member);
    events::emit_member_removed(member);
}

public fun set_whitelist_enabled(_: &AdminCap, config: &mut ProtocolConfig, enabled: bool) {
    config.whitelist_enabled = enabled;
    events::emit_whitelist_enabled_set(enabled);
}

public fun set_ingress_paused(_: &AdminCap, config: &mut ProtocolConfig, paused: bool) {
    config.ingress_paused = paused;
    events::emit_ingress_pause_set(paused);
}

/// Gate for every function that lets net-new money into the protocol.
/// Exits (exercise / redeem / withdraw / unwind) must never call this.
public fun assert_ingress_allowed(config: &ProtocolConfig, who: address) {
    assert!(!config.ingress_paused, errors::ingress_paused());
    assert!(
        !config.whitelist_enabled || config.members.contains(&who),
        errors::ingress_restricted(),
    );
}

public fun fee_bps(config: &ProtocolConfig): u64 { config.fee_bps }

public fun protocol_id(config: &ProtocolConfig): &vector<u8> { &config.protocol_id }

public fun is_member(config: &ProtocolConfig, who: address): bool {
    config.members.contains(&who)
}

public fun whitelist_enabled(config: &ProtocolConfig): bool { config.whitelist_enabled }

public fun ingress_paused(config: &ProtocolConfig): bool { config.ingress_paused }

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx);
}
