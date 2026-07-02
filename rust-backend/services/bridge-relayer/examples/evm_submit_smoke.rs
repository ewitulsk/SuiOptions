//! End-to-end check of the EVM destination submitter against a local node.
//!
//! Args: <rpc_url> <inbox_addr> <recipient_addr(20-byte hex)> <relayer_key> <group_secp_seed_hex>
//!
//! Signs a Sui→EVM message with the group key, submits it via the real
//! `EvmDestSubmitter`, and asserts the Inbox marked it consumed (which only
//! happens if the ECDSA signature verified AND dispatch to the recipient
//! succeeded — the whole tx reverts otherwise).

use bridge_relayer::evm_submit::EvmDestSubmitter;
use bridge_relayer::relay::DestSubmitter;
use bridge_signer::ThresholdSigner;
use bridge_types::{chain_id, message::left_pad_address, CrossChainMessage};

fn parse32(s: &str) -> [u8; 32] {
    hex::decode(s.trim_start_matches("0x")).unwrap().try_into().unwrap()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (rpc, inbox, recipient, relayer_key, group_seed) =
        (&a[1], &a[2], &a[3], &a[4], &a[5]);

    let recipient_addr: [u8; 20] =
        hex::decode(recipient.trim_start_matches("0x")).unwrap().try_into().unwrap();

    // dst = HyperEVM internal id → the signer selects the ECDSA scheme.
    let message = CrossChainMessage::new(
        chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),
        chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(),
        0,
        [0xaa; 32],
        left_pad_address(recipient_addr),
        b"evm-submit-smoke".to_vec(),
    );

    let signer = ThresholdSigner::from_seeds([0x42; 32], parse32(group_seed))?;
    let envelope = signer.sign(&message, 1)?;
    let digest = message.digest();

    let submitter = EvmDestSubmitter::new(rpc, inbox, relayer_key)?;
    assert!(!submitter.is_delivered(&digest).await?, "should not be delivered yet");
    submitter.submit(&message, &envelope).await?;
    assert!(submitter.is_delivered(&digest).await?, "should be consumed after submit");

    println!("OK message_hash=0x{} delivered + consumed on EVM Inbox", hex::encode(digest));
    Ok(())
}
