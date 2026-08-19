//! PTB builders for the test-tokens faucets.
//!
//! Each test-coin Move module exposes two helpers:
//!
//! ```move
//! public fun mint(faucet: &mut Faucet, amount: u64, ctx): Coin<T>
//! public fun mint_to_sender(faucet: &mut Faucet, amount: u64, ctx)
//! ```
//!
//! `mint` is composable in a PTB (returns the freshly minted coin), so the
//! writer and MM bot use it inline. `mint_to_sender` is the standalone
//! "give me X tokens to my own address" — useful for the exchange CLI's
//! `mint` command.

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;
use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::shared_object_arg;
use crate::chain::{ChainClient, ExecutedTransaction};

/// Standalone `mint_to_sender` PTB. Single command, single tx. Used by the
/// exchange CLI's `mint` subcommand.
pub async fn mint_to_sender(
    client: &ChainClient,
    signer: &Signer,
    tokens_package: ObjectID,
    module: &str,
    faucet_id: ObjectID,
    amount: u64,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    info!(%tokens_package, module, amount, "minting tokens to sender");
    let mut pt = ProgrammableTransactionBuilder::new();
    let faucet = pt.obj(shared_object_arg(client, faucet_id, true).await?)?;
    let amount_arg = pt.pure(&amount)?;
    pt.programmable_move_call(
        tokens_package,
        Identifier::new(module).map_err(|e| anyhow!("module name {module}: {e}"))?,
        Identifier::new("mint_to_sender").unwrap(),
        vec![],
        vec![faucet, amount_arg],
    );
    submit(client, signer, pt, gas_budget).await
}

/// Mint + deposit into the MM's own `mm_collateral::CollateralAccount` in one
/// PTB. The MM bot calls this to fund its collateral account (core's
/// `account::deposit` is gone — custody lives in the per-MM package).
///
/// Returns the [`SuiTransactionBlockResponse`] so the caller can log digest
/// / object changes.
pub async fn mint_and_deposit_into_collateral(
    client: &ChainClient,
    signer: &Signer,
    tokens_package: ObjectID,
    module: &str,
    faucet_id: ObjectID,
    coin_type: &str,
    collateral_account_id: ObjectID,
    collateral_package: ObjectID,
    amount: u64,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    info!(%tokens_package, module, %collateral_account_id, amount, "minting and depositing into collateral account");
    use move_core_types::language_storage::TypeTag;
    use std::str::FromStr;

    let mut pt = ProgrammableTransactionBuilder::new();

    // Mint -> Coin<T>
    let faucet = pt.obj(shared_object_arg(client, faucet_id, true).await?)?;
    let amount_arg = pt.pure(&amount)?;
    let coin = pt.programmable_move_call(
        tokens_package,
        Identifier::new(module).map_err(|e| anyhow!("module name {module}: {e}"))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, amount_arg],
    );

    // mm_collateral::deposit<T>(&mut CollateralAccount, Coin<T>)
    let account = pt.obj(shared_object_arg(client, collateral_account_id, true).await?)?;
    let coin_tag = TypeTag::from_str(coin_type)
        .with_context(|| format!("parsing coin type {coin_type}"))?;
    pt.programmable_move_call(
        collateral_package,
        Identifier::new("mm_collateral").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![coin_tag],
        vec![account, coin],
    );

    submit(client, signer, pt, gas_budget).await
}

/// Mint + deposit into a curated trading vault in one PTB — the testnet
/// seed for a vault the MM bot just created for itself (SO-345).
///
/// `vault::deposit` consumes an `Appraisal` that must cover every held
/// asset and position. Doing this at creation time is what keeps it to one
/// PTB: a brand-new vault holds nothing, so `begin_appraisal` is complete
/// on the spot with no attestation legs. Seed before setting an external
/// account, or the appraisal stops being trivial.
///
/// Testnet only by construction: there are no faucets on mainnet.
pub async fn mint_and_deposit_into_vault(
    client: &ChainClient,
    signer: &Signer,
    tokens_package: ObjectID,
    module: &str,
    faucet_id: ObjectID,
    refs: &crate::tx::trading_vault::TradingVaultRefs<'_>,
    whitelist_id: ObjectID,
    amount: u64,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    info!(
        %tokens_package,
        module,
        vault = %refs.vault_id,
        amount,
        "minting and depositing into trading vault"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    // Mint -> Coin<T>
    let faucet = pt.obj(shared_object_arg(client, faucet_id, true).await?)?;
    let amount_arg = pt.pure(&amount)?;
    let coin = pt.programmable_move_call(
        tokens_package,
        Identifier::new(module).map_err(|e| anyhow!("module name {module}: {e}"))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, amount_arg],
    );

    // begin_appraisal -> deposit. Both take the vault as a shared input;
    // the builder unions the mutability, so the mutable deposit leg wins.
    // v2: deposit mints a VaultPosition NFT — untranched (tranche 0),
    // transferred back to the depositing signer.
    let appraisal = crate::tx::trading_vault::build_begin_appraisal(client, &mut pt, refs).await?;
    crate::tx::trading_vault::build_deposit_and_transfer(
        client,
        &mut pt,
        refs,
        whitelist_id,
        appraisal,
        coin,
        0,
        signer.address,
    )
    .await?;

    submit(client, signer, pt, gas_budget).await
}

async fn submit(
    client: &ChainClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let resp = super::submit_ptb(client, signer, pt, gas_budget, "test token tx").await?;
    debug!(digest = %super::tx_digest(&resp), "test token tx succeeded");
    // `Argument` is unused at this scope but keeps the borrow checker
    // honest about move semantics in earlier builds; explicitly drop.
    let _ = std::marker::PhantomData::<Argument>;
    Ok(resp)
}
