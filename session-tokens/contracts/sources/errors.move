/// Central error-code registry. Numbers are stable and referenced by the SDK
/// when surfacing failures to the user. See spec §1.8.
module siws_session::errors;

public fun expired(): u64 { 1 }
public fun nonce_used(): u64 { 2 }
public fun bad_sig(): u64 { 3 }
public fun wrong_holder(): u64 { 4 }
public fun wrong_account(): u64 { 5 }
public fun revoked(): u64 { 6 }
public fun over_per_tx(): u64 { 7 }
public fun over_total(): u64 { 8 }
public fun not_allowed(): u64 { 9 }
public fun not_owner(): u64 { 10 }
public fun invalid_pubkey_length(): u64 { 11 }
public fun invalid_signature_length(): u64 { 12 }
