//! Smoke-test a Sui gRPC endpoint against everything this workspace needs.
//!
//! Point it at a candidate `[sui] grpc_url` before rolling that override out
//! to the fleet — a provider can serve `GetObject` and still not serve
//! `SimulateTransaction` or the owned-object listing, and finding that out
//! from a crash-looping keeper is expensive.
//!
//! Run: `cargo run -p sui-tx --example endpoint_check -- [GRPC_URL] [ADDRESS]`

use std::str::FromStr;

use sui_tx::chain::{decode_return_value, sui_coin_type, ChainClient};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::Identifier;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let url = args
        .nth(0)
        .unwrap_or_else(|| sui_tx::Network::Testnet.grpc_url().to_owned());
    // Optional: an address to exercise the coin/balance reads against.
    let addr = args.next().map(|a| SuiAddress::from_str(&a)).transpose()?;

    let c = ChainClient::new(&url)?;
    println!("endpoint      : {}", c.host());
    println!("chain id      : {}", c.chain_identifier().await?);
    println!("gas price     : {}", c.reference_gas_price().await?);
    println!("tip checkpoint: {}", c.latest_checkpoint().await?);

    // Object read + shared-object arg resolution (every PTB depends on this).
    let clock = sui_types::SUI_CLOCK_OBJECT_ID;
    c.shared_object_arg(clock, false).await?;
    println!("shared arg    : ok");

    // Gas-less simulation — the dev-inspect replacement. Calls
    // `0x2::clock::timestamp_ms(&Clock)` and decodes the BCS return value.
    let mut pt = ProgrammableTransactionBuilder::new();
    let clock_arg = pt.obj(c.shared_object_arg(clock, false).await?)?;
    pt.programmable_move_call(
        ObjectID::from_hex_literal("0x2")?,
        Identifier::new("clock")?,
        Identifier::new("timestamp_ms")?,
        vec![],
        vec![clock_arg],
    );
    let sim = c.dev_inspect_ptb(SuiAddress::ZERO, pt).await?;
    let ts: u64 = decode_return_value(&sim, 0)?;
    println!("dev-inspect   : ok (clock = {ts})");

    // Address-balance gas needs both of these: the expiration binds to the
    // chain id, and is only valid for the current epoch and the next.
    println!("epoch         : {}", c.current_epoch().await?);

    if let Some(addr) = addr {
        let coins = c.coins(addr, &sui_coin_type()).await?;
        let total = c.balance(addr, &sui_coin_type()).await?;
        let in_address = c.address_balance(addr, &sui_coin_type()).await?;
        println!("owned SUI     : {} coins", coins.len());
        println!("balance       : {total} MIST");
        // A node that serves address balances reports the split; one that
        // doesn't leaves the field unset, which reads as zero here. Funds
        // sitting in the address balance are spendable as gas but invisible to
        // a Coin<SUI> read — that asymmetry is what `gas_payment` exists for.
        println!("  in coins    : {} MIST", total - in_address as u128);
        println!("  address bal : {in_address} MIST");
        match c.gas_payment(addr, 50_000_000).await {
            Ok(p) => println!("gas for 0.05  : {p:?}"),
            Err(e) => println!("gas for 0.05  : unaffordable ({e})"),
        }
    } else {
        println!("owned SUI     : skipped (pass an address to exercise it)");
    }

    println!("\nendpoint serves everything the workspace needs.");
    Ok(())
}
