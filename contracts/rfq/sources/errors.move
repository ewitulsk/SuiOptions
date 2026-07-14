module options_rfq::errors;

public fun rfq_auction_mismatch(): u64 { 1 }
public fun rfq_bucket_mismatch(): u64 { 2 }
public fun rfq_too_close_to_expiry(): u64 { 3 }
