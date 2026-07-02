//! Derive on-chain group keys from signer seeds, and (with no args) print the
//! canonical cross-language test vectors so Rust, Move, and Solidity stay in
//! lockstep after any digest change.
//!
//! Usage:
//!   cargo run -p bridge-signer --example group_keys
//!       → prints the parity vectors (TEST_SALT, digest, Ed25519 sig) to bake
//!         into the Rust / Move / Solidity known-vector tests.
//!   cargo run -p bridge-signer --example group_keys -- <ed25519_seed_hex> <secp256k1_seed_hex>
//!       → prints the two group keys to register on-chain.

use bridge_signer::ThresholdSigner;
use bridge_types::chain_id;
use bridge_types::message::derive_domain_sep;
use bridge_types::CrossChainMessage;

fn parse(s: &str) -> [u8; 32] {
    hex::decode(s.trim_start_matches("0x")).expect("hex").try_into().expect("32 bytes")
}

fn print_group_keys(ed_seed: [u8; 32], k1_seed: [u8; 32]) {
    let signer = ThresholdSigner::from_seeds(ed_seed, k1_seed).unwrap();
    println!("ed25519_group_pubkey 0x{}", hex::encode(signer.ed25519_group_pubkey()));
    println!("ecdsa_group_address  0x{}", hex::encode(signer.ecdsa_group_address()));
}

fn print_vectors() {
    // Must match the tests in bridge-types/message.rs and bridge-signer/lib.rs.
    let salt = [0x01u8; 32];
    let seed = [0x42u8; 32];
    let ds = derive_domain_sep(&salt);

    let to_sui = CrossChainMessage::new(
        chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(),
        chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),
        7,
        [0xab; 32],
        [0xcd; 32],
        b"hello-bridge".to_vec(),
    );
    let signer = ThresholdSigner::from_seeds(seed, [0x11u8; 32]).unwrap();
    let env = signer.sign(&to_sui, &ds, 1).unwrap();

    println!("== cross-language parity vectors ==");
    println!("TEST_SALT           0x{}", hex::encode(salt));
    println!("DOMAIN_SEP          0x{}", hex::encode(ds));
    println!("known digest        {}", hex::encode(to_sui.digest(&ds)));
    println!("ed25519 group pk    {}", hex::encode(signer.ed25519_group_pubkey()));
    println!("ed25519 signature   {}", hex::encode(&env.signature));
    println!("ecdsa group address 0x{}", hex::encode(signer.ecdsa_group_address()));
    // BCS bytes the relayer passes to the Sui `bridge_receive` (message::from_bcs
    // / envelope::from_bcs must decode these back to the same digest + signature).
    println!("message to_move_bcs {}", hex::encode(to_sui.to_move_bcs()));
    println!("envelope to_move_bcs {}", hex::encode(env.to_move_bcs()));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.len() {
        1 => print_vectors(),
        3 => print_group_keys(parse(&args[1]), parse(&args[2])),
        _ => panic!("usage: group_keys [<ed25519_seed_hex> <secp256k1_seed_hex>]"),
    }
}
