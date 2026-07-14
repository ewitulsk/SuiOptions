module options_core::errors;

public fun quote_expired(): u64 { 1 }
public fun quote_nonce_used(): u64 { 2 }
public fun quote_signature_invalid(): u64 { 3 }
public fun quote_protocol_mismatch(): u64 { 4 }
public fun quote_bucket_mismatch(): u64 { 5 }
public fun quote_account_mismatch(): u64 { 6 }
public fun quote_recipient_mismatch(): u64 { 7 }
public fun bucket_expired(): u64 { 8 }
public fun bucket_not_expired(): u64 { 9 }
public fun bucket_not_drained(): u64 { 10 }
public fun insufficient_account_balance(): u64 { 11 }
public fun amount_mismatch(): u64 { 12 }
public fun settlement_amount_mismatch(): u64 { 13 }
public fun cursor_overflow(): u64 { 14 }
public fun not_owner(): u64 { 15 }
public fun position_bucket_mismatch(): u64 { 16 }
public fun call_option_bucket_mismatch(): u64 { 17 }
public fun fee_too_high(): u64 { 18 }
public fun nonce_still_valid(): u64 { 19 }
public fun insufficient_treasury_balance(): u64 { 20 }
public fun zero_amount(): u64 { 21 }
public fun count_must_be_positive(): u64 { 22 }
public fun invalid_signing_scheme(): u64 { 23 }
public fun invalid_pubkey_length(): u64 { 24 }
public fun strike_scale_too_large(): u64 { 25 }
public fun bucket_invalidated(): u64 { 26 }
public fun bucket_not_invalidated(): u64 { 27 }
public fun treasury_cap_not_fresh(): u64 { 28 }
// 29-34 moved to the auction / options_rfq packages (rfq venue codes).
// 35-55 moved to the options_vault package (vault + oracle codes).
// 56-58 retired (session custody codes).
public fun put_collateral_mismatch(): u64 { 59 }
