//! PTB builders for the hybrid exchange (`contracts/exchange`, SO-368):
//! the maker bot's `BalanceManager` lifecycle, faucet-funded deposits, and
//! the salt-watermark dead-man switch.

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use std::str::FromStr;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use tracing::info;

use crate::chain::{created_objects, ChainClient, ExecutedTransaction};
use crate::sui_client::Signer;
use crate::tx::shared_object_arg;

/// `balance_manager::new` — create and share a manager owned by the sender.
/// Returns the created `BalanceManager` object id.
pub async fn create_balance_manager(
    client: &ChainClient,
    signer: &Signer,
    exchange_package: ObjectID,
    gas_budget: u64,
) -> Result<ObjectID> {
    info!(%exchange_package, "creating exchange BalanceManager");
    let mut pt = ProgrammableTransactionBuilder::new();
    pt.programmable_move_call(
        exchange_package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("new").unwrap(),
        vec![],
        vec![],
    );
    let resp =
        super::submit_ptb(client, signer, pt, gas_budget, "balance_manager::new").await?;
    created_objects(&resp)
        .into_iter()
        .find(|o| o.object_type.ends_with("::balance_manager::BalanceManager"))
        .map(|o| o.object_id)
        .ok_or_else(|| anyhow!("balance_manager::new created no BalanceManager object"))
}

/// Faucet `mint` + `balance_manager::deposit<T>` in one PTB — the staging
/// maker bot's funding path. Testnet only by construction: there are no
/// faucets on mainnet.
pub async fn mint_and_deposit_into_balance_manager(
    client: &ChainClient,
    signer: &Signer,
    tokens_package: ObjectID,
    module: &str,
    faucet_id: ObjectID,
    exchange_package: ObjectID,
    manager_id: ObjectID,
    coin_type: &str,
    amount: u64,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    info!(%tokens_package, module, %manager_id, amount, "minting and depositing into BalanceManager");
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

    // balance_manager::deposit<T>(&mut BalanceManager, Coin<T>)
    let bm = pt.obj(shared_object_arg(client, manager_id, true).await?)?;
    let coin_tag = TypeTag::from_str(coin_type)
        .with_context(|| format!("parsing coin type {coin_type}"))?;
    pt.programmable_move_call(
        exchange_package,
        Identifier::new("balance_manager").unwrap(),
        Identifier::new("deposit").unwrap(),
        vec![coin_tag],
        vec![bm, coin],
    );

    super::submit_ptb(client, signer, pt, gas_budget, "mint+deposit into BalanceManager").await
}

/// `settlement::cancel_up_to<Base, Quote>` — the maker dead-man switch: one
/// cheap transaction voids ALL of the sender's orders in this market with
/// `salt <= min_valid_salt`. New orders must carry salts above the watermark.
pub async fn cancel_up_to(
    client: &ChainClient,
    signer: &Signer,
    exchange_package: ObjectID,
    registry_id: ObjectID,
    base_type: &str,
    quote_type: &str,
    min_valid_salt: u64,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    info!(%registry_id, min_valid_salt, "raising salt watermark (cancel_up_to)");
    let mut pt = ProgrammableTransactionBuilder::new();
    let reg = pt.obj(shared_object_arg(client, registry_id, true).await?)?;
    let salt_arg = pt.pure(&min_valid_salt)?;
    let base_tag = TypeTag::from_str(base_type)
        .with_context(|| format!("parsing base type {base_type}"))?;
    let quote_tag = TypeTag::from_str(quote_type)
        .with_context(|| format!("parsing quote type {quote_type}"))?;
    pt.programmable_move_call(
        exchange_package,
        Identifier::new("settlement").unwrap(),
        Identifier::new("cancel_up_to").unwrap(),
        vec![base_tag, quote_tag],
        vec![reg, salt_arg],
    );
    super::submit_ptb(client, signer, pt, gas_budget, "settlement::cancel_up_to").await
}
