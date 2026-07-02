//! Drive the real `EvmSourceWatcher` against a running EVM node (anvil).
//!
//! Args: <rpc_url> <outbox_addr> <deployment_salt_hex> <confirmations>
//! Prints how many committed messages the watcher reconstructed + hash-checked,
//! so a harness can assert the eth_getLogs → decode → digest-check path and the
//! confirmation-depth gate.

use bridge_relayer::evm_source::EvmSourceWatcher;
use bridge_relayer::relay::SourceWatcher;
use bridge_types::message::derive_domain_sep;

fn parse32(s: &str) -> [u8; 32] {
    hex::decode(s.trim_start_matches("0x")).unwrap().try_into().unwrap()
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (rpc, outbox, salt, conf) = (&a[1], &a[2], &a[3], a[4].parse::<u64>().unwrap());
    let ds = derive_domain_sep(&parse32(salt));

    let mut w = EvmSourceWatcher::connect(rpc, outbox, ds, conf, 0).await.unwrap();
    let msgs = w.poll().await.unwrap();
    println!("polled {}", msgs.len());
    for m in &msgs {
        println!("  nonce={} src={} dst={} payload=0x{}", m.nonce, m.src_chain_id, m.dst_chain_id, hex::encode(&m.payload));
    }
}
