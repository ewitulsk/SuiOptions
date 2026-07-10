use anchor_lang::prelude::*;

/// All protocol timestamps are in milliseconds for parity with the Sui
/// contracts and the off-chain stack; Solana's clock is seconds.
pub fn now_ms(clock: &Clock) -> u64 {
    clock.unix_timestamp as u64 * 1000
}
