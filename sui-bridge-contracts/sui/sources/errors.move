/// Centralized abort codes for the Layer 1 messaging package, mirroring the
/// `options_protocol::errors` convention (one `public fun` per code).
module sui_bridge::errors;

// --- message / encoding ---
public fun bad_bytes32_length(): u64 { 1 }
public fun unsupported_version(): u64 { 2 }

// --- registry ---
public fun chain_already_registered(): u64 { 3 }
public fun chain_not_registered(): u64 { 4 }
public fun unknown_family(): u64 { 5 }
public fun chain_local_too_large(): u64 { 19 }
public fun group_key_already_registered(): u64 { 6 }
public fun group_key_not_registered(): u64 { 7 }
public fun bad_pubkey_length(): u64 { 8 }
public fun invalid_threshold(): u64 { 9 }

// --- outbox ---
public fun outbox_paused(): u64 { 10 }

// --- inbox ---
public fun inbox_paused(): u64 { 11 }
public fun wrong_dst_chain(): u64 { 12 }
public fun message_already_consumed(): u64 { 13 }
public fun nonce_below_window(): u64 { 14 }
public fun signature_invalid(): u64 { 15 }
public fun unsupported_scheme(): u64 { 16 }
public fun dst_app_mismatch(): u64 { 17 }
public fun scheme_key_mismatch(): u64 { 18 }
