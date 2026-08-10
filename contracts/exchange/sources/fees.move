/// Fee vault sweeps (spec §4.6/§4.9). Fees accrue to per-registry vaults at
/// fill time and are swept by admin — relayer compensation comes from the
/// vaults, keeping fill gas flat.
module exchange::fees;

use sui::coin::{Self, Coin};
use exchange::admin::AdminCap;
use exchange::registry::{Self, SettlementRegistry};

/// Sweep the entire base-token fee vault.
public fun sweep_base<Base, Quote>(
    _: &AdminCap,
    reg: &mut SettlementRegistry<Base, Quote>,
    ctx: &mut TxContext,
): Coin<Base> {
    coin::from_balance(registry::take_fee_base(reg), ctx)
}

/// Sweep the entire quote-token fee vault.
public fun sweep_quote<Base, Quote>(
    _: &AdminCap,
    reg: &mut SettlementRegistry<Base, Quote>,
    ctx: &mut TxContext,
): Coin<Quote> {
    coin::from_balance(registry::take_fee_quote(reg), ctx)
}
