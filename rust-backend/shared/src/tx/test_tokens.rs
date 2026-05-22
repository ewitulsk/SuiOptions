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
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, Transaction, TransactionData};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::shared_object_arg;

/// Standalone `mint_to_sender` PTB. Single command, single tx. Used by the
/// exchange CLI's `mint` subcommand.
pub async fn mint_to_sender(
    client: &SuiClient,
    signer: &Signer,
    tokens_package: ObjectID,
    module: &str,
    faucet_id: ObjectID,
    amount: u64,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
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

/// Mint + deposit into an Account in one PTB. The MM bot calls this on
/// first run so the freshly-created Account has settlement-asset balance
/// to pay premiums from.
///
/// Returns the [`SuiTransactionBlockResponse`] so the caller can log digest
/// / object changes.
pub async fn mint_and_deposit_into_account(
    client: &SuiClient,
    signer: &Signer,
    tokens_package: ObjectID,
    module: &str,
    faucet_id: ObjectID,
    coin_type: &str,
    account_id: ObjectID,
    options_package: ObjectID,
    amount: u64,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(%tokens_package, module, %account_id, amount, "minting and depositing into account");
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

    // account::deposit<T>(&mut Account, Coin<T>)
    let account = pt.obj(shared_object_arg(client, account_id, true).await?)?;
    let coin_tag = TypeTag::from_str(coin_type)
        .with_context(|| format!("parsing coin type {coin_type}"))?;
    pt.programmable_move_call(
        options_package,
        Identifier::new("account").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![coin_tag],
        vec![account, coin],
    );

    submit(client, signer, pt, gas_budget).await
}

async fn submit(
    client: &SuiClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    let programmable = pt.finish();
    let gas_coin = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(5))
        .await
        .context("listing gas coins")?
        .data
        .into_iter()
        .max_by_key(|c| c.balance)
        .ok_or_else(|| anyhow!("no SUI coins to pay gas for {}", signer.address))?;
    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("fetching reference gas price")?;
    let tx_data = TransactionData::new_programmable(
        signer.address,
        vec![gas_coin.object_ref()],
        programmable,
        gas_budget,
        gas_price,
    );
    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting tx")?;
    let effects = resp.effects.as_ref().context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("tx reverted: {:?}", effects.status());
    }
    debug!(digest = %resp.digest, "test token tx succeeded");
    // `Argument` is unused at this scope but keeps the borrow checker
    // honest about move semantics in earlier builds; explicitly drop.
    let _ = std::marker::PhantomData::<Argument>;
    Ok(resp)
}
