//! Derive the on-chain group keys from the M1 signer seeds.
//!
//! Usage: cargo run -p bridge-signer --example group_keys -- <ed25519_seed_hex> <secp256k1_seed_hex>
//!   ed25519_group_pubkey → register as the Sui group key (registerGroupKey).
//!   ecdsa_group_address  → register as the EVM group key / Deploy.s.sol GROUP_ADDRESS.

use bridge_signer::ThresholdSigner;

fn parse(s: &str) -> [u8; 32] {
    hex::decode(s.trim_start_matches("0x"))
        .expect("hex")
        .try_into()
        .expect("32 bytes")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    assert!(args.len() == 3, "usage: group_keys <ed25519_seed_hex> <secp256k1_seed_hex>");
    let signer = ThresholdSigner::from_seeds(parse(&args[1]), parse(&args[2])).unwrap();
    println!("ed25519_group_pubkey 0x{}", hex::encode(signer.ed25519_group_pubkey()));
    println!("ecdsa_group_address  0x{}", hex::encode(signer.ecdsa_group_address()));
}
