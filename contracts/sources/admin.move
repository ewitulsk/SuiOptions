module options_protocol::admin;

use options_protocol::errors;
use options_protocol::events;

const MAX_FEE_BPS: u64 = 1000;

public struct AdminCap has key, store {
    id: UID,
}

public struct ProtocolConfig has key {
    id: UID,
    /// Protocol-level skim (basis points of gross premium), routed to the
    /// global Treasury on every write. Orgs charge their own fee on top.
    fee_bps: u64,
    protocol_id: vector<u8>,
    /// Emergency brake: blocks new writes across ALL orgs' buckets.
    /// Exercises, redeems, burns, and cleanups are never blocked.
    paused: bool,
}

fun init(ctx: &mut TxContext) {
    let admin_cap = AdminCap { id: object::new(ctx) };
    let protocol_id = object::id_to_bytes(&object::id(&admin_cap));
    let config = ProtocolConfig {
        id: object::new(ctx),
        fee_bps: 0,
        protocol_id,
        paused: false,
    };
    transfer::public_transfer(admin_cap, ctx.sender());
    transfer::share_object(config);
}

public fun set_protocol_fee_bps(_: &AdminCap, config: &mut ProtocolConfig, new_bps: u64) {
    assert!(new_bps <= MAX_FEE_BPS, errors::fee_too_high());
    let old_bps = config.fee_bps;
    config.fee_bps = new_bps;
    events::emit_protocol_fee_updated(old_bps, new_bps);
}

public fun set_pause(_: &AdminCap, config: &mut ProtocolConfig, paused: bool, ctx: &TxContext) {
    config.paused = paused;
    events::emit_protocol_pause_set(paused, ctx.sender());
}

public fun protocol_fee_bps(config: &ProtocolConfig): u64 { config.fee_bps }

public fun is_paused(config: &ProtocolConfig): bool { config.paused }

public fun protocol_id(config: &ProtocolConfig): &vector<u8> { &config.protocol_id }

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx);
}
