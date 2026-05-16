//! Exchange / admin CLI.
//!
//! Drives the AdminCap-gated entrypoints of the on-chain protocol. All
//! commands sign as the deployer (the address that holds `AdminCap` per
//! `deployments.json`).
//!
//! ```text
//!   exchange create-buckets --underlying 0x2::sui::SUI --settlement 0x2::sui::SUI \
//!       --expiry-ms 1747500000000 --start-strike 1000000 \
//!       --strike-interval 250000 --count 4
//!   exchange set-fee --bps 50
//!   exchange withdraw-treasury --type 0x2::sui::SUI \
//!       --amount 1000000 --recipient 0xabc...
//! ```
//!
//! Network defaults to testnet (matches the deployment we have). Reads the
//! signing key from `SUI_PRIVATE_KEY_TESTNET` (or `SUI_PRIVATE_KEY`).

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use sui_types::base_types::SuiAddress;

use clients::deployments::Deployments;
use clients::sui_client::{Network, SuiClientWrapper};
use clients::tx::admin::{
    new_call_option, set_fee_bps, withdraw_treasury, NewCallOptionArgs,
};

#[derive(Parser)]
#[command(name = "exchange", about = "Admin CLI for the covered-call options protocol")]
struct Cli {
    /// Path to the deployments.json (defaults to the workspace copy).
    #[arg(short, long, default_value = "deployments.json")]
    deployments: PathBuf,

    /// Target network.
    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    network: Network,

    /// Gas budget per transaction (MIST).
    #[arg(long, default_value_t = 200_000_000)]
    gas_budget: u64,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Call `bucket::new_call_option<U, S>`. Creates `count` shared buckets at
    /// strikes `start_strike + i * strike_interval` for `i ∈ [0, count)`.
    CreateBuckets {
        #[arg(long, default_value = "0x2::sui::SUI")]
        underlying: String,
        #[arg(long, default_value = "0x2::sui::SUI")]
        settlement: String,
        /// Expiry as a Sui clock millisecond timestamp.
        #[arg(long)]
        expiry_ms: u64,
        #[arg(long)]
        start_strike: u64,
        #[arg(long)]
        strike_interval: u64,
        #[arg(long)]
        count: u64,
    },
    /// Call `admin::set_fee_bps`. Caps at 1000 on chain.
    SetFee {
        #[arg(long)]
        bps: u64,
    },
    /// Call `treasury::withdraw<T>`.
    WithdrawTreasury {
        #[arg(long, name = "type")]
        asset_type: String,
        #[arg(long)]
        amount: u64,
        #[arg(long)]
        recipient: SuiAddress,
    },
    /// Print the resolved package id, admin cap, protocol_id bytes.
    Info,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let dep = Deployments::load(&cli.deployments)
        .with_context(|| format!("loading {}", cli.deployments.display()))?;
    let net = dep.for_network(cli.network).with_context(|| {
        format!(
            "no deployment for {} in {}",
            cli.network,
            cli.deployments.display()
        )
    })?;
    let package = net.package()?;
    let admin_cap = net.admin_cap()?;
    let protocol_config = net.protocol_config()?;

    let wrap = SuiClientWrapper::connect(cli.network).await?;

    if wrap.signer.address != net.deployer_address()? {
        return Err(anyhow!(
            "configured signer {} ≠ deployer {} from deployments.json — only the deployer holds AdminCap",
            wrap.signer.address,
            net.deployer
        ));
    }

    match cli.cmd {
        Command::CreateBuckets {
            underlying,
            settlement,
            expiry_ms,
            start_strike,
            strike_interval,
            count,
        } => {
            let resp = new_call_option(
                &wrap.client,
                &wrap.signer,
                &NewCallOptionArgs {
                    package,
                    admin_cap,
                    underlying_type: &underlying,
                    settlement_type: &settlement,
                    expiry_ms,
                    start_strike,
                    strike_interval,
                    count,
                },
                cli.gas_budget,
            )
            .await?;
            println!("✓ create-buckets digest: {}", resp.digest);
            if let Some(changes) = &resp.object_changes {
                for c in changes {
                    if let sui_json_rpc_types::ObjectChange::Created {
                        object_id,
                        object_type,
                        ..
                    } = c
                    {
                        if object_type.module.as_str() == "bucket"
                            && object_type.name.as_str() == "Bucket"
                        {
                            println!("  bucket: {object_id}");
                        }
                    }
                }
            }
        }
        Command::SetFee { bps } => {
            let resp = set_fee_bps(
                &wrap.client,
                &wrap.signer,
                package,
                admin_cap,
                protocol_config,
                bps,
                cli.gas_budget,
            )
            .await?;
            println!("✓ set-fee {bps} bps digest: {}", resp.digest);
        }
        Command::WithdrawTreasury {
            asset_type,
            amount,
            recipient,
        } => {
            let treasury = net.treasury().context("treasury_id missing")?;
            let resp = withdraw_treasury(
                &wrap.client,
                &wrap.signer,
                package,
                admin_cap,
                treasury,
                &asset_type,
                amount,
                recipient,
                cli.gas_budget,
            )
            .await?;
            println!("✓ withdraw-treasury digest: {}", resp.digest);
        }
        Command::Info => {
            let protocol_id_bytes = net.protocol_id_bytes()?;
            println!("network         : {}", cli.network);
            println!("package         : {}", net.package_id);
            println!("admin_cap       : {}", net.admin_cap_id);
            println!("protocol_config : {}", net.protocol_config_id);
            println!(
                "treasury        : {}",
                net.treasury_id.as_deref().unwrap_or("(missing)")
            );
            println!("deployer        : {}", net.deployer);
            println!("protocol_id     : 0x{}", hex::encode(&protocol_id_bytes));
        }
    }
    Ok(())
}
