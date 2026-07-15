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

use anyhow::{anyhow, Context, Result};
use clap::Parser;

use token_info_client::{Snapshot, TokenInfoClient};
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::admin::{set_fee_bps, withdraw_treasury};
use sui_tx::tx::test_tokens::{mint_and_deposit_into_collateral, mint_to_sender};

use option_scheduler::roller::{self, ProductType, RollPlan};
use option_scheduler::strike_grid::StrikeGrid;

use exchange::{Cli, Command, Product};

/// Resolves either a ticker (looked up via the `/tokens` catalog) or a
/// fully-qualified Move type string. Lets every command that needs a type
/// arg accept either form. Catalog-backed so it works on every network
/// (testTokens is empty on mainnet).
fn resolve_coin_type(snapshot: &Snapshot, input: &str) -> Result<String> {
    if input.contains("::") {
        return Ok(input.to_owned());
    }
    Ok(snapshot.token_spec(input)?.coin_type.clone())
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
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, std::time::Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching token-info from {}", cli.token_info_url))?;
    let package = snapshot.package()?;
    let admin_cap = snapshot.admin_cap()?;
    let protocol_config = snapshot.protocol_config()?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;

    let needs_admin = matches!(
        cli.cmd,
        Command::CreateBuckets { .. } | Command::SetFee { .. } | Command::WithdrawTreasury { .. }
    );
    if needs_admin && wrap.signer.address != snapshot.deployer_address()? {
        return Err(anyhow!(
            "configured signer {} ≠ deployer {} from token-info — only the deployer holds AdminCap",
            wrap.signer.address,
            snapshot.package_info.deployer
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
            strike_scale,
            product,
        } => {
            let u_spec = snapshot
                .token_spec(&underlying)
                .with_context(|| format!("underlying {underlying} not in token-info catalog"))?;
            let s_type = resolve_coin_type(&snapshot, &settlement)?;
            // Settlement decimals feed the DeepBook pool grid; catalog
            // tokens carry them, raw coin-type inputs fall back to the
            // 9-decimal Sui convention.
            let settlement_decimals = snapshot
                .token_spec(&settlement)
                .map(|s| s.decimals)
                .unwrap_or(9);
            // Drive the same per-roll codegen→publish→create_bucket pipeline
            // the scheduler uses, so manual buckets get per-bucket option coins.
            let grid = StrikeGrid {
                start_strike,
                strike_interval,
                count,
                strike_scale,
            };
            // Route to the call/put codegen + create path inside the roller.
            let product_type = match product {
                Product::Call => ProductType::Call,
                Product::Put => ProductType::Put,
            };
            let plan = RollPlan {
                underlying_symbol: underlying.clone(),
                settlement_symbol: settlement.clone(),
                underlying_type: u_spec.coin_type.clone(),
                settlement_type: s_type,
                underlying_decimals: u_spec.decimals,
                settlement_decimals,
                expiry_ms,
                strikes: grid.strikes(),
                strike_scale,
                product_type,
            };
            // The manual tool rolls buckets only; pool creation stays with
            // the scheduler (pass None).
            let out =
                roller::submit(&wrap, package, admin_cap, &plan, None, cli.gas_budget).await?;
            println!("✓ create-buckets digest: {}", out.digest);
            for id in &out.bucket_ids {
                println!("  bucket: {id}");
            }
        }
        Command::Mint { token, amount } => {
            let tokens = snapshot.test_tokens()?;
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
            collateral_package,
            token,
            amount,
        } => {
            let tokens = snapshot.test_tokens()?;
            let info = tokens.get(&token)?;
            let (pkg, module) = info.module_path()?;
            let resp = mint_and_deposit_into_collateral(
                &wrap.client,
                &wrap.signer,
                pkg,
                &module,
                info.faucet()?,
                &info.coin_type,
                account,
                collateral_package,
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
            let asset_type = resolve_coin_type(&snapshot, &token)?;
            let treasury = snapshot.treasury().context("treasury_id missing")?;
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
            let protocol_id_bytes = snapshot.protocol_id_bytes()?;
            let pi = &snapshot.package_info;
            println!("network         : {}", cli.network);
            println!("package         : {}", pi.package_id);
            println!("admin_cap       : {}", pi.admin_cap_id);
            println!("protocol_config : {}", pi.protocol_config_id);
            println!(
                "treasury        : {}",
                pi.treasury_id.as_deref().unwrap_or("(missing)")
            );
            println!("deployer        : {}", pi.deployer);
            println!("protocol_id     : 0x{}", hex::encode(&protocol_id_bytes));
            println!("signer          : {}", wrap.signer.address);
            if let Some(tt) = snapshot.maybe_test_tokens() {
                println!();
                println!("test_tokens.package: {}", tt.package_id);
                for (sym, info) in &tt.tokens {
                    println!(
                        "  {:5} dec={} faucet={} type={}",
                        sym, info.decimals, info.faucet_id, info.coin_type
                    );
                }
            }
            if !snapshot.tokens().is_empty() {
                println!();
                println!("token_info:");
                for spec in snapshot.tokens() {
                    println!(
                        "  {:5} dec={} pyth={} type={}",
                        spec.ticker,
                        spec.decimals,
                        spec.pyth_feed_id.as_deref().unwrap_or("(none)"),
                        spec.coin_type
                    );
                }
            }
        }
    }
    Ok(())
}
