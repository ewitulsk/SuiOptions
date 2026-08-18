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
use std::str::FromStr;
use sui_types::base_types::ObjectID;

use token_info_client::{Snapshot, TokenInfoClient};
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::admin::{
    set_fee_bps, set_ingress_paused, set_whitelist_enabled, whitelist_add_member,
    whitelist_remove_member, withdraw_treasury, IngressWhitelist, MarketPauseTarget,
};
use sui_tx::tx::test_tokens::{mint_and_deposit_into_collateral, mint_to_sender};

use exchange::roller::{self, ProductType, RollPlan};
use exchange::strike_grid::StrikeGrid;

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

/// The standalone ingress whitelist's (package, AdminCap, Whitelist)
/// triple out of the token-info snapshot. Errors when the deployment
/// predates the standalone whitelist package.
fn ingress_whitelist(snapshot: &Snapshot) -> Result<IngressWhitelist> {
    Ok(IngressWhitelist {
        package: snapshot
            .whitelist_package()?
            .context("no whitelist block in token-info snapshot — redeploy the protocol")?,
        admin_cap: snapshot
            .whitelist_admin_cap()?
            .context("no whitelist block in token-info snapshot — redeploy the protocol")?,
        whitelist: snapshot
            .whitelist_object()?
            .context("no whitelist block in token-info snapshot — redeploy the protocol")?,
    })
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
        Command::CreateBuckets { .. }
            | Command::SetFee { .. }
            | Command::WithdrawTreasury { .. }
            | Command::WhitelistAdd { .. }
            | Command::WhitelistRemove { .. }
            | Command::WhitelistList
            | Command::WhitelistEnable
            | Command::WhitelistDisable
            | Command::PauseIngress
            | Command::UnpauseIngress
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
            // Publish-free any-strike create path (runtime currencies via
            // coin_registry) — the same one the frontend's create-bucket PTB
            // uses, driven here from an explicit strike grid.
            let grid = StrikeGrid {
                start_strike,
                strike_interval,
                count,
                strike_scale,
            };
            // Route to the call/put create path inside the roller.
            let product_type = match product {
                Product::Call => ProductType::Call,
                Product::Put => ProductType::Put,
            };
            let plan = RollPlan {
                underlying_type: u_spec.coin_type.clone(),
                settlement_type: s_type,
                underlying_decimals: u_spec.decimals,
                expiry_ms,
                strikes: grid.strikes(),
                strike_scale,
                product_type,
            };
            // Buckets only — no DeepBook pool is created here (pass None).
            let roll_ctx = sui_tx::tx::coin_pkg::AnyStrikeContext {
                package,
                bucket_registry: snapshot.bucket_registry()?,
                whitelist: snapshot
                    .whitelist_object()?
                    .context("whitelist missing from token-info")?,
            };
            let out = roller::submit(&wrap, &roll_ctx, &plan, None, None, cli.gas_budget).await?;
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
            println!("✓ mint {amount} {token} digest: {}", sui_tx::tx::tx_digest(&resp));
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
                sui_tx::tx::tx_digest(&resp)
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
            println!("✓ set-fee {bps} bps digest: {}", sui_tx::tx::tx_digest(&resp));
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
            println!("✓ withdraw-treasury digest: {}", sui_tx::tx::tx_digest(&resp));
        }
        Command::WhitelistAdd { address } => {
            let wl = ingress_whitelist(&snapshot)?;
            let resp =
                whitelist_add_member(&wrap.client, &wrap.signer, &wl, address, cli.gas_budget)
                    .await?;
            println!("✓ whitelist-add {address} digest: {}", sui_tx::tx::tx_digest(&resp));
        }
        Command::WhitelistRemove { address } => {
            let wl = ingress_whitelist(&snapshot)?;
            let resp =
                whitelist_remove_member(&wrap.client, &wrap.signer, &wl, address, cli.gas_budget)
                    .await?;
            println!("✓ whitelist-remove {address} digest: {}", sui_tx::tx::tx_digest(&resp));
        }
        Command::WhitelistList => {
            let wl = ingress_whitelist(&snapshot)?;
            {
                let id = wl.whitelist;
                let (_, json) = wrap
                    .client
                    .get_object_json(id)
                    .await
                    .with_context(|| format!("fetching Whitelist {id}"))?;
                println!("Whitelist ({id})");
                match json {
                    Some(v) => {
                        // VecSet<address> renders as { "contents": [...] }.
                        let enabled = v.pointer("/whitelist_enabled").and_then(|b| b.as_bool());
                        let paused = v.pointer("/ingress_paused").and_then(|b| b.as_bool());
                        let members = v.pointer("/members/contents").and_then(|m| m.as_array());
                        match (enabled, paused, members) {
                            (Some(enabled), Some(paused), Some(members)) => {
                                println!("  whitelist_enabled: {enabled}");
                                println!("  ingress_paused   : {paused}");
                                println!("  members          : {}", members.len());
                                for m in members {
                                    println!("    {}", m.as_str().unwrap_or_default());
                                }
                            }
                            // Unexpected JSON rendering — dump it raw
                            // rather than guessing at the shape.
                            _ => println!("  {v}"),
                        }
                    }
                    None => println!("  (node returned no JSON rendering for this object)"),
                }
            }
        }
        Command::WhitelistEnable | Command::WhitelistDisable => {
            let enabled = matches!(cli.cmd, Command::WhitelistEnable);
            let wl = ingress_whitelist(&snapshot)?;
            let resp =
                set_whitelist_enabled(&wrap.client, &wrap.signer, &wl, enabled, cli.gas_budget)
                    .await?;
            println!(
                "✓ whitelist_enabled={enabled} digest: {}",
                sui_tx::tx::tx_digest(&resp)
            );
        }
        Command::ListMarkets { api_url, dry_run } => {
            let listing = snapshot
                .exchange_listing()
                .context("no exchangeListing block in token-info snapshot — redeploy")?;
            let listing_pkg = ObjectID::from_str(&listing.package_id)?;
            let authority = ObjectID::from_str(&listing.listing_authority_id)?;
            let resp: serde_json::Value = reqwest::get(format!("{api_url}/buckets"))
                .await
                .context("fetching /buckets")?
                .error_for_status()?
                .json()
                .await
                .context("decoding /buckets")?;
            let series = resp
                .get("series")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let (mut listed, mut skipped, mut failed) = (0u32, 0u32, 0u32);
            for s in &series {
                for b in s.get("buckets").and_then(|v| v.as_array()).into_iter().flatten() {
                    let Some(bucket_id) = b.get("bucket_id").and_then(|v| v.as_str()) else {
                        continue; // never created on-chain
                    };
                    let Some(coin_type) = b.get("option_coin_type").and_then(|v| v.as_str())
                    else {
                        continue;
                    };
                    if b.get("exchange_market_id").and_then(|v| v.as_str()).is_some() {
                        skipped += 1;
                        continue; // already listed
                    }
                    if dry_run {
                        println!("would list {bucket_id} ({coin_type})");
                        listed += 1;
                        continue;
                    }
                    match sui_tx::tx::exchange::create_option_market(
                        &wrap.client,
                        &wrap.signer,
                        listing_pkg,
                        authority,
                        ObjectID::from_str(bucket_id)?,
                        coin_type,
                        cli.gas_budget,
                    )
                    .await
                    {
                        Ok(resp) => {
                            listed += 1;
                            println!(
                                "listed {bucket_id} digest: {}",
                                sui_tx::tx::tx_digest(&resp)
                            );
                        }
                        Err(e) => {
                            // Dedup/expiry aborts are expected on re-runs;
                            // report and continue.
                            failed += 1;
                            eprintln!("skip {bucket_id}: {e:#}");
                        }
                    }
                }
            }
            println!("listed {listed}, already-listed {skipped}, failed {failed}");
        }
        Command::PauseIngress | Command::UnpauseIngress => {
            let paused = matches!(cli.cmd, Command::PauseIngress);
            let wl = ingress_whitelist(&snapshot)?;
            let trading_vault = snapshot
                .trading_vault()
                .context("no tradingVault block in token-info snapshot")?
                .package()?;
            let vault_config = snapshot
                .trading_vault_objects()
                .context("no tradingVaultObjects block in token-info snapshot")?
                .vault_protocol_config()?;
            let exchange = snapshot
                .package_info
                .exchange
                .as_ref()
                .context("no exchange block in token-info snapshot")?;
            let markets = exchange
                .markets
                .iter()
                .map(|(sym, m)| {
                    Ok(MarketPauseTarget {
                        registry: m
                            .registry()
                            .with_context(|| format!("parsing registry id for {sym}"))?,
                        base: m.base.clone(),
                        quote: m.quote.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let resp = set_ingress_paused(
                &wrap.client,
                &wrap.signer,
                &wl,
                admin_cap,
                trading_vault,
                vault_config,
                exchange.package()?,
                exchange.admin_cap()?,
                &markets,
                paused,
                cli.gas_budget,
            )
            .await?;
            println!(
                "✓ ingress_paused={paused} (whitelist + vault registry + {} markets) digest: {}",
                markets.len(),
                sui_tx::tx::tx_digest(&resp)
            );
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
