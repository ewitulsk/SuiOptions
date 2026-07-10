use anchor_lang::prelude::*;

/// Milliseconds, for parity with the Sui contracts and off-chain stack.
pub fn now_ms(clock: &Clock) -> u64 {
    clock.unix_timestamp as u64 * 1000
}
