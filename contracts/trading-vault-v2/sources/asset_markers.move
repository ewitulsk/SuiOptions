/// Pricing markers for spoke-chain assets that do not exist on Sui
/// (multichain plan §1). A marker is a pure type tag: the spoke binding
/// maps its local asset code to a marker `TypeName`, the oracle service
/// attests `marker → accounting-asset` prices through the normal
/// `OracleRegistry` pin, and the appraisal's spoke legs value spoke
/// balances with those attestations. No coin, no supply — just a name.
module vault_v2::asset_markers;

/// Paxos Global Dollar as held on spoke chains (Robinhood: native USDG;
/// testnet: the TUSDG mock — same marker, the price feed differs per
/// network config).
public struct USDG has drop {}
