//! Initializes the options programs on a Solana cluster and records every
//! important on-chain id into `solana-deployments.json`.
//!
//! Program binaries are deployed with the anchor/solana CLI beforehand
//! (`solana-contracts/scripts/deploy-devnet.sh`) — program ids are fixed by
//! their deploy keypairs, so unlike the Sui twin there is nothing to
//! publish here. Pipeline per run (idempotent — re-runs converge):
//!   1. Resolve program ids (flags → Anchor.toml).
//!   2. `initialize` options_core (skip when Config exists with our admin).
//!   3. `--deploy-tokens`: create the test SPL mints that are missing or
//!      stale, keep the live ones.
//!   4. Rebuild `token_info` preserving existing pythFeedIds.
//!   5. Upsert the env slot, other envs untouched.

use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use clap::Parser;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;

use solana_deployment_manager::json_store::Deployments;
use solana_deployment_manager::plan::{
    build_token_info, plan_initialize, plan_token, InitAction, TokenAction, TEST_TOKENS,
};
use solana_deployment_manager::{anchor_toml, tokens, Cli, Command};
use solana_deployments::{ProgramInfo, SolanaNetworkDeployment, TestToken};
use solana_tx::{ix, pda, Network, Signer, SolanaClientWrapper};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Show)) {
        return show(&cli);
    }
    deploy(cli).await
}

/// `show`: print the env slot (or the whole file) and exit.
fn show(cli: &Cli) -> Result<()> {
    if !cli.output.exists() {
        bail!("{} does not exist", cli.output.display());
    }
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&cli.output)?)
        .with_context(|| format!("parsing {}", cli.output.display()))?;
    let value = match &cli.env {
        Some(env) => raw
            .get(env.to_ascii_lowercase())
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        None => raw,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn deploy(cli: Cli) -> Result<()> {
    let env_key = cli
        .env
        .as_deref()
        .context("--env is required")?
        .to_ascii_lowercase();
    let network = cli.network.context("--network is required")?;

    // Program ids: flags win, Anchor.toml supplies the rest.
    let defaults = if cli.core_program_id.is_some()
        && cli.venue_program_id.is_some()
        && cli.vault_program_id.is_some()
    {
        None
    } else {
        let path = cli.contracts.join("Anchor.toml");
        Some(anchor_toml::load_program_ids(&path)?)
    };
    let resolve = |flag: &Option<String>, default: Option<&String>, name: &str| -> Result<Pubkey> {
        let s = flag
            .as_deref()
            .or(default.map(String::as_str))
            .with_context(|| format!("no {name} program id (flag or Anchor.toml)"))?;
        Pubkey::from_str(s).with_context(|| format!("parsing {name} program id {s:?}"))
    };
    let core_id = resolve(&cli.core_program_id, defaults.as_ref().map(|d| &d.core), "core")?;
    let venue_id = resolve(&cli.venue_program_id, defaults.as_ref().map(|d| &d.venue), "venue")?;
    let vault_id = resolve(&cli.vault_program_id, defaults.as_ref().map(|d| &d.vault), "vault")?;

    // The initialize instruction is encoded against the linked program
    // crate, whose declared id is what the Anchor runtime enforces — a
    // divergent id can't be initialized by this build.
    if !cli.skip_init && core_id != options_core::ID {
        bail!(
            "--core-program-id {core_id} differs from the linked options_core crate's declared \
             id {}; rebuild the crate with the new declare_id! or pass --skip-init",
            options_core::ID
        );
    }
    if cli.deploy_tokens && network == Network::MainnetBeta {
        bail!("--deploy-tokens is refused on mainnet-beta (test mints are non-mainnet only)");
    }
    let faucet_authority = cli
        .faucet_authority
        .as_deref()
        .map(|s| Pubkey::from_str(s).with_context(|| format!("parsing --faucet-authority {s:?}")))
        .transpose()?;

    // Secrets → signer (the admin key) → RPC client. Precedence for the
    // endpoint: --rpc flag, then the solana.rpc_url secret, then public.
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let signer = Signer::from_secrets(&secrets, network).context("loading signer")?;
    let rpc_url = cli
        .rpc
        .clone()
        .unwrap_or_else(|| secrets.resolve_solana_rpc_url(network.rpc_url()));
    // Log the host only — the secret override carries an API key.
    let rpc_host = rpc_url
        .split("://")
        .nth(1)
        .and_then(|s| s.split(['/', '?']).next())
        .unwrap_or("<unparseable>")
        .to_string();
    let admin = signer.pubkey();
    tracing::info!(env = %env_key, %network, rpc_host, %admin, "starting deployment");
    let client = SolanaClientWrapper {
        client: RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed()),
        signer,
        network,
    };

    // Carry the previous env slot forward so re-runs without
    // --deploy-tokens don't wipe the mints or the pythFeedIds.
    let mut store = Deployments::load_or_default(&cli.output)?;
    let previous = store.envs.get(&env_key);
    let previous_test_tokens = previous.and_then(|d| d.program_info.test_tokens.clone());
    let previous_token_info = previous.map(|d| d.token_info.clone()).unwrap_or_default();
    let previous_init_sig = previous.and_then(|d| d.program_info.initialize_signature.clone());

    // 1. Idempotent initialize.
    let config_pda = pda::config(&core_id);
    let initialize_signature = if cli.skip_init {
        tracing::info!("--skip-init: not calling initialize");
        previous_init_sig
    } else {
        let existing = match maybe_account(&client, &config_pda).await? {
            Some(account) => Some(
                <options_core::state::Config as anchor_lang::AccountDeserialize>::try_deserialize(
                    &mut account.data.as_slice(),
                )
                .with_context(|| format!("deserializing Config PDA {config_pda}"))?,
            ),
            None => None,
        };
        match plan_initialize(existing.as_ref(), &admin)? {
            InitAction::AlreadyInitialized => {
                tracing::info!(config = %config_pda, "Config already initialized with our admin; skipping");
                previous_init_sig
            }
            InitAction::Initialize => {
                let sig = client
                    .send_and_confirm(&[ix::initialize(&admin)], &[], "initialize")
                    .await?;
                tracing::info!(config = %config_pda, signature = %sig, "options_core initialized");
                Some(sig.to_string())
            }
        }
    };

    // 2. Test mints.
    let test_tokens = if cli.deploy_tokens {
        let final_authority = faucet_authority.unwrap_or(admin);
        Some(
            deploy_test_tokens(&client, previous_test_tokens, &final_authority)
                .await
                .context("deploying test tokens")?,
        )
    } else if let Some(prev) = previous_test_tokens {
        tracing::info!(
            count = prev.len(),
            "preserving existing testTokens record (use --deploy-tokens to refresh)"
        );
        Some(prev)
    } else {
        None
    };

    // 3. Off-chain catalog: rebuilt from this run's mints when tokens were
    //    deployed (existing pythFeedIds win), carried forward otherwise.
    let token_info = if cli.deploy_tokens {
        build_token_info(previous_token_info, test_tokens.as_ref().unwrap())
    } else {
        previous_token_info
    };

    // 4. Upsert the env slot.
    let record = SolanaNetworkDeployment {
        program_info: ProgramInfo {
            options_core_program_id: core_id.to_string(),
            auction_venue_program_id: venue_id.to_string(),
            options_vault_program_id: vault_id.to_string(),
            config_pda: config_pda.to_string(),
            treasury_pda: pda::treasury(&core_id).to_string(),
            admin: admin.to_string(),
            network: network.as_str().to_owned(),
            deployed_at: chrono::Utc::now().to_rfc3339(),
            initialize_signature,
            test_tokens,
        },
        token_info,
    };
    record.validate().context("validating the record before writing")?;
    store.upsert(&env_key, record);
    store.save(&cli.output)?;

    tracing::info!(path = %cli.output.display(), env = %env_key, "deployment written");
    Ok(())
}

/// For each table entry: keep the recorded mint when it still exists
/// on-chain with the right decimals, otherwise create a fresh one (payer
/// as temporary authority, seed supply to the payer, authority handed to
/// the faucet — one tx per mint).
async fn deploy_test_tokens(
    client: &SolanaClientWrapper,
    previous: Option<BTreeMap<String, TestToken>>,
    final_authority: &Pubkey,
) -> Result<BTreeMap<String, TestToken>> {
    let payer = client.signer.pubkey();
    let rent = client
        .client
        .get_minimum_balance_for_rent_exemption(tokens::MINT_SPACE)
        .await
        .context("fetching mint rent exemption")?;

    let mut out = BTreeMap::new();
    for (symbol, decimals) in TEST_TOKENS {
        let recorded = previous.as_ref().and_then(|m| m.get(symbol));
        let on_chain_decimals = match recorded {
            Some(rec) => {
                let mint = Pubkey::from_str(&rec.mint)
                    .with_context(|| format!("parsing recorded {symbol} mint {:?}", rec.mint))?;
                maybe_account(client, &mint)
                    .await?
                    .and_then(|acc| tokens::mint_decimals(&acc.owner, &acc.data))
            }
            None => None,
        };
        match plan_token(recorded, on_chain_decimals, decimals) {
            TokenAction::Keep => {
                let rec = recorded.expect("Keep implies a recorded mint").clone();
                tracing::info!(symbol, mint = %rec.mint, "test mint alive and matching; keeping");
                out.insert(symbol.to_owned(), rec);
            }
            TokenAction::Create => {
                let mint_kp = Keypair::new();
                let mint = mint_kp.pubkey();
                let ixs = tokens::create_mint_ixs(&payer, &mint, decimals, final_authority, rent)?;
                let sig = client
                    .send_and_confirm(&ixs, &[&mint_kp], &format!("create test mint {symbol}"))
                    .await?;
                tracing::info!(
                    symbol, %mint, decimals, authority = %final_authority, signature = %sig,
                    "test mint created (seed supply minted to deployer)"
                );
                out.insert(
                    symbol.to_owned(),
                    TestToken {
                        mint: mint.to_string(),
                        decimals,
                        mint_authority: final_authority.to_string(),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// `get_account` that maps "no account" to `None` instead of an error.
async fn maybe_account(
    client: &SolanaClientWrapper,
    pubkey: &Pubkey,
) -> Result<Option<solana_sdk::account::Account>> {
    Ok(client
        .client
        .get_account_with_commitment(pubkey, CommitmentConfig::confirmed())
        .await
        .with_context(|| format!("fetching account {pubkey}"))?
        .value)
}
