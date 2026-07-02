//! Manual-relay helper: reconstruct a committed message, sign it with the
//! round-trip demo group keys, and emit everything needed to submit on the
//! destination — the digest (to cross-check the on-chain messageHash), the
//! signature, and the Move-BCS bytes for a Sui `bridge_receive` PTB.
//!
//! Seeds are the demo group keys: ed25519 [0x42;32] (pubkey 2152f8…),
//! secp256k1 [0x11;32] (address 19e7…).
//!
//! Args: <src_chain_id> <dst_chain_id> <nonce> <src_app_hex> <dst_app_hex>
//!       <payload_hex> <salt_hex> <group_pubkey_id>

use bridge_signer::ThresholdSigner;
use bridge_types::message::derive_domain_sep;
use bridge_types::CrossChainMessage;

fn b32(s: &str) -> [u8; 32] {
    let mut v = hex::decode(s.trim_start_matches("0x")).unwrap();
    // left-pad to 32 (EVM addresses arrive as 20/variable)
    while v.len() < 32 {
        v.insert(0, 0);
    }
    v.try_into().unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let src: u32 = a[1].parse().unwrap();
    let dst: u32 = a[2].parse().unwrap();
    let nonce: u64 = a[3].parse().unwrap();
    let src_app = b32(&a[4]);
    let dst_app = b32(&a[5]);
    let payload = hex::decode(a[6].trim_start_matches("0x")).unwrap();
    let salt = b32(&a[7]);
    let group_pubkey_id: u32 = a[8].parse().unwrap();

    let ds = derive_domain_sep(&salt);
    let msg = CrossChainMessage::new(src, dst, nonce, src_app, dst_app, payload);
    let signer = ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap();
    let env = signer.sign(&msg, &ds, group_pubkey_id).unwrap();

    println!("DIGEST=0x{}", hex::encode(msg.digest(&ds)));
    println!("SCHEME={}", env.scheme_tag);
    println!("SIG=0x{}", hex::encode(&env.signature));
    println!("MSG_BCS=0x{}", hex::encode(msg.to_move_bcs()));
    println!("ENV_BCS=0x{}", hex::encode(env.to_move_bcs()));
    // EVM-shaped fields (for cast receiveMessage tuple), too:
    println!("SRC_APP=0x{}", hex::encode(src_app));
    println!("DST_APP=0x{}", hex::encode(dst_app));
}
