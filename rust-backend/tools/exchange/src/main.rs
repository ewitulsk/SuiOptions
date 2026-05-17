//! Exchange / admin CLI.
//!
//! Pulls every on-chain id (package, AdminCap, ProtocolConfig, Treasury,
//! test-tokens package + per-symbol Faucet) out of `deployments.json` —
//! nothing about tokens or addresses is hardcoded in this binary.
//!
//! ```text
//!   exchange create-buckets --underlying TBTC --settlement TUSDC \
//!       --expiry-ms 1763251200000 --start-strike 50000000000 \
//!       --strike-interval 5000000000 --count 4
//!   exchange mint --token TUSDC --amount 1000000000
//!   exchange fund-account --account 0x… --token TUSDC --amount 1000000000
//!   exchange set-fee --bps 50
//!   exchange info
//! ```
//!
//! `SUI_PRIVATE_KEY_TESTNET` (or `SUI_PRIVATE_KEY`) holds the deployer's
//! key; the `info` subcommand confirms it matches the deployer recorded in
//! `deployments.json` (the only address that holds AdminCap).

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use sui_types::base_types::SuiAddress;

use shared::deployments::{Deployments, NetworkDeployment};
use shared::sui_client::{Network, SuiClientWrapper};
use shared::tx::admin::{
    new_call_option, set_fee_bps, withdraw_treasury, NewCallOptionArgs,
};
use shared::tx::test_tokens::{mint_and_deposit_into_account, mint_to_sender};

#[derive(Parser)]
#[command(name = "exchange", about = "Admin CLI for the covered-call options protocol")]
struct Cli {
    #[arg(short, long, default_value = "deployments.json")]
    deployments: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key. No env-var
    /// fallback.
    #[arg(short = 's', long, default_value = "tools/exchange/config/secrets.toml")]
    secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    network: Network,

    #[arg(long, default_value_t = 200_000_000)]
    gas_budget: u64,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// `bucket::new_call_option<U, S>` — creates `count` buckets at
    /// `start_strike + i * strike_interval` for `i ∈ [0, count)`. Tokens
    /// are looked up by symbol from `deployments.testTokens`.
    CreateBuckets {
        #[arg(long, default_value = "TBTC")]
        underlying: String,
        #[arg(long, default_value = "TUSDC")]
        settlement: String,
        #[arg(long)]
        expiry_ms: u64,
        #[arg(long)]
        start_strike: u64,
        #[arg(long)]
        strike_interval: u64,
        #[arg(long)]
        count: u64,
    },
    /// Faucet-mint `amount` of a test token to the signer.
    Mint {
        #[arg(long)]
        token: String,
        #[arg(long)]
        amount: u64,
    },
    /// Mint a test token and deposit it into the given Account in one PTB.
    /// Use for fast-MM-bootstrap or any "give Account X some settlement
    /// asset to quote with" workflow.
    FundAccount {
        #[arg(long)]
        account: sui_types::base_types::ObjectID,
        #[arg(long)]
        token: String,
        #[arg(long)]
        amount: u64,
    },
    /// `admin::set_fee_bps`.
    SetFee {
        #[arg(long)]
        bps: u64,
    },
    /// `treasury::withdraw<T>`. `--token` accepts a symbol from
    /// `deployments.testTokens` or any fully-qualified Move type.
    WithdrawTreasury {
        #[arg(long)]
        token: String,
        #[arg(long)]
        amount: u64,
        #[arg(long)]
        recipient: SuiAddress,
    },
    /// Print every id resolvable from `deployments.json`.
    Info,
}

/// Resolves either an uppercase symbol (looked up via testTokens) or a
/// fully-qualified Move type string. Lets every command that needs a type
/// arg accept either form.
fn resolve_coin_type(net: &NetworkDeployment, input: &str) -> Result<String> {
    if input.contains("::") {
        return Ok(input.to_owned());
    }
    Ok(net.token(input)?.coin_type.clone())
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
    let net = dep.for_network(cli.network.as_str()).with_context(|| {
        format!(
            "no deployment for {} in {}",
            cli.network,
            cli.deployments.display()
        )
    })?;
    let package = net.package()?;
    let admin_cap = net.admin_cap()?;
    let protocol_config = net.protocol_config()?;

    let secrets = shared::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;

    let needs_admin = matches!(
        cli.cmd,
        Command::CreateBuckets { .. } | Command::SetFee { .. } | Command::WithdrawTreasury { .. }
    );
    if needs_admin && wrap.signer.address != net.deployer_address()? {
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
            let u_type = resolve_coin_type(net, &underlying)?;
            let s_type = resolve_coin_type(net, &settlement)?;
            let resp = new_call_option(
                &wrap.client,
                &wrap.signer,
                &NewCallOptionArgs {
                    package,
                    admin_cap,
                    underlying_type: &u_type,
                    settlement_type: &s_type,
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
        Command::Mint { token, amount } => {
            let tokens = net.test_tokens()?;
            let info = tokens.get(&token)?;
            let (pkg, module) = info.module_path()?;
            let resp = mint_to_sender(
                &wrap.client,
                &wrap.signer,
                pkg,
                &module,
                info.faucet()?,
                amount,
                cli.gas_budget,
            )
            .await?;
            println!("✓ mint {amount} {token} digest: {}", resp.digest);
        }
        Command::FundAccount {
            account,
            token,
            amount,
        } => {
            let tokens = net.test_tokens()?;
            let info = tokens.get(&token)?;
            let (pkg, module) = info.module_path()?;
            let resp = mint_and_deposit_into_account(
                &wrap.client,
                &wrap.signer,
                pkg,
                &module,
                info.faucet()?,
                &info.coin_type,
                account,
                package,
                amount,
                cli.gas_budget,
            )
            .await?;
            println!(
                "✓ fund-account {account} {amount} {token} digest: {}",
                resp.digest
            );
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
            token,
            amount,
            recipient,
        } => {
            let asset_type = resolve_coin_type(net, &token)?;
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
            println!("signer          : {}", wrap.signer.address);
            if let Some(tt) = &net.test_tokens {
                println!();
                println!("test_tokens.package: {}", tt.package_id);
                for (sym, info) in &tt.tokens {
                    println!(
                        "  {:5} dec={} faucet={} type={}",
                        sym, info.decimals, info.faucet_id, info.coin_type
                    );
                }
            }
        }
    }
    Ok(())
}
