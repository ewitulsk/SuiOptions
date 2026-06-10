use protocol_types::asset::AssetType;

#[derive(Clone, Debug)]
pub struct Bucket {
    /// Org that created (and administers) this bucket, hex object id.
    pub org_id: String,
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    /// Fully-qualified type of the per-bucket fungible option coin.
    pub call_type: AssetType,
    /// On-chain strike — real ratio is `strike / 10^strike_scale`.
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
    pub invalidated: bool,
    /// DeepBook pool trading this bucket's call coin (SO-153), hex object
    /// id. `None` until a venue is created.
    pub deepbook_pool_id: Option<String>,
}
