/// The bridge signer's instantiation of the generic enclave registry
/// (bridge_tickets/07). `BRIDGE_SIGNER` is the witness type that parameterizes
/// this signer's `EnclaveConfig`/`Enclave`/`Cap`, so only this module can mint
/// the governance capability.
module bridge_enclave::signer;

use bridge_enclave::enclave;

public struct BRIDGE_SIGNER has drop {}

/// On publish, mint the governance `Cap` for the bridge signer's enclave config
/// and hand it to the deployer (→ a governance multisig per ticket 10). Governance
/// then calls `enclave::create_enclave_config` with the approved PCRs, and each
/// node operator permissionlessly `register_enclave`s its attested instance.
fun init(ctx: &mut TxContext) {
    transfer::public_transfer(enclave::new_cap(BRIDGE_SIGNER {}, ctx), ctx.sender());
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}
