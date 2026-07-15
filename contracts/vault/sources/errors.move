module options_vault::errors;

// Codes keep their historical values from the monolithic package so
// off-chain benign-abort classification carries over unchanged.
public fun vault_wrong_phase(): u64 { 35 }
public fun vault_bucket_not_selected(): u64 { 36 }
public fun vault_bucket_already_selected(): u64 { 37 }
public fun vault_selling_closed(): u64 { 38 }
public fun vault_positions_pending(): u64 { 39 }
public fun vault_rfqs_open(): u64 { 40 }
public fun vault_round_not_finalized(): u64 { 41 }
public fun vault_receipt_round_mismatch(): u64 { 42 }
public fun vault_strike_out_of_band(): u64 { 43 }
public fun vault_expiry_out_of_band(): u64 { 44 }
public fun vault_slice_too_large(): u64 { 45 }
public fun vault_too_many_rfqs(): u64 { 46 }
public fun vault_deposits_paused(): u64 { 47 }
public fun vault_wrong_origin(): u64 { 48 }
public fun oracle_feed_mismatch(): u64 { 49 }
public fun oracle_price_stale(): u64 { 50 }
public fun oracle_confidence(): u64 { 51 }
public fun oracle_price_invalid(): u64 { 52 }
public fun vault_proceeds_unswapped(): u64 { 53 }
public fun vault_config_invalid(): u64 { 54 }
