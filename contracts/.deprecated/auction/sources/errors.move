module auction::errors;

public fun zero_amount(): u64 { 1 }
public fun duration_too_short(): u64 { 2 }
public fun auction_closed(): u64 { 3 }
public fun auction_not_closed(): u64 { 4 }
public fun bid_too_low(): u64 { 5 }
public fun not_settle_authority(): u64 { 6 }
public fun settle_coupled(): u64 { 7 }
