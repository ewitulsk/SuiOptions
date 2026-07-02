//! Exercise the real `EvmProbe` against a running EVM node (anvil).
//!
//! Args: <rpc_url> <outbox_addr> <digest_hex> <confirmations>
//! Prints the `Commitment` verdict (NotFound | Pending | Final) so a harness can
//! assert the full eth_getLogs + eth_blockNumber + finality path end to end.

use bridge_signer_service::probe::EvmProbe;
use bridge_signer_service::verifier::CommitmentProbe;

fn parse<const N: usize>(s: &str) -> [u8; N] {
    hex::decode(s.trim_start_matches("0x")).unwrap().try_into().unwrap()
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (rpc, outbox, digest, conf) =
        (a[1].clone(), parse::<20>(&a[2]), parse::<32>(&a[3]), a[4].parse::<u64>().unwrap());

    // Large lookback so the anvil scan starts at genesis (no public-RPC range cap).
    let probe = EvmProbe::new(reqwest::Client::new(), rpc, outbox, conf, u64::MAX);
    match probe.check(&digest).await {
        Ok(c) => println!("{c:?}"),
        Err(e) => {
            eprintln!("ERROR {e:#}");
            std::process::exit(1);
        }
    }
}
