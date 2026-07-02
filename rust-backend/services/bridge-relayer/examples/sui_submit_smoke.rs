//! Exercise the real `SuiDestSubmitter` write path end-to-end against testnet:
//! read `dst_app`'s type on chain → derive the `bridge_receive` target → build
//! the PTB → submit via sui-sdk. Delivers one already-committed EVM→Sui message.
//!
//! Args: <sui_rpc> <sui_key> <inbox_id> <keys_id> <src_app_hex> <dst_app_hex>
//!       <payload_hex> <sig_hex> <group_pubkey_id>

use bridge_relayer::relay::DestSubmitter;
use bridge_relayer::sui_dest::SuiDestSubmitter;
use bridge_types::{envelope::SCHEME_ED25519, CrossChainMessage, SignatureEnvelope};

fn b32(s: &str) -> [u8; 32] {
    let mut v = hex::decode(s.trim_start_matches("0x")).unwrap();
    while v.len() < 32 {
        v.insert(0, 0);
    }
    v.try_into().unwrap()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (rpc, key, inbox, keys) = (&a[1], &a[2], &a[3], &a[4]);
    let src_app = b32(&a[5]);
    let dst_app = b32(&a[6]);
    let payload = hex::decode(a[7].trim_start_matches("0x")).unwrap();
    let signature = hex::decode(a[8].trim_start_matches("0x")).unwrap();
    let group_pubkey_id: u32 = a[9].parse().unwrap();

    // HyperEVM(268436454) → Sui(134217728), nonce 1 (the second lock).
    let message = CrossChainMessage::new(268436454, 134217728, 1, src_app, dst_app, payload);
    let envelope = SignatureEnvelope { scheme_tag: SCHEME_ED25519, group_pubkey_id, signature };

    let submitter = SuiDestSubmitter::connect(rpc, key, inbox, keys, 200_000_000).await?;
    println!("submitting via real SuiDestSubmitter (type-derived bridge_receive)...");
    submitter.submit(&message, &envelope).await?;
    println!("OK — delivered to Sui");
    Ok(())
}
