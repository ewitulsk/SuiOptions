module options_core::admin;

use options_core::errors;
use options_core::events;

const MAX_FEE_BPS: u64 = 1000;

public struct AdminCap has key, store {
    id: UID,
}

public struct ProtocolConfig has key {
    id: UID,
    fee_bps: u64,
    protocol_id: vector<u8>,
}

fun init(ctx: &mut TxContext) {
    let admin_cap = AdminCap { id: object::new(ctx) };
    let protocol_id = object::id_to_bytes(&object::id(&admin_cap));
    let config = ProtocolConfig {
        id: object::new(ctx),
        fee_bps: 0,
        protocol_id,
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

public fun fee_bps(config: &ProtocolConfig): u64 { config.fee_bps }

public fun protocol_id(config: &ProtocolConfig): &vector<u8> { &config.protocol_id }

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx);
}
