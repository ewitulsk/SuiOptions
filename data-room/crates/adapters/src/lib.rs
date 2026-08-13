//! Per-exchange adapters: exchange-native payloads in, canonical events
//! out (spec §7.1). Adapters are pure parse functions — no IO — so golden
//! fixtures pin every venue format in unit tests.

pub mod binance_vision;
pub mod coinbase;
pub mod hyperliquid;

/// A parse failure carrying enough context for the rejects file.
#[derive(Debug, thiserror::Error)]
#[error("{src_file}:{src_line}: {reason}")]
pub struct Reject {
    pub src_file: String,
    pub src_line: i32,
    pub reason: String,
}
