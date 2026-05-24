use protocol_types::asset::AssetType;

#[derive(Clone, Debug)]
pub struct Bucket {
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    pub strike: u64,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    pub cleaned: bool,
}
