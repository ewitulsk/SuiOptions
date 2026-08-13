//! Admin-cap-gated PTBs: `new_call_option`, `set_fee_bps`, `withdraw_treasury`.
//!
//! All three are simple Move calls — no coin manipulation. They used to go
//! through the JSON-RPC `transaction_builder().move_call(..)` helper, which
//! resolved object args and pure args from JSON. That builder only exists on
//! the retired JSON-RPC client, so the calls are now assembled explicitly:
//! `ChainClient::object_arg` does the shared-vs-owned resolution the old
//! builder did, and pure args are BCS-encoded directly.

use anyhow::{Context, Result};
use std::str::FromStr;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;
use sui_types::{Identifier, TypeTag};
use tracing::info;

use crate::chain::{ChainClient, ExecutedTransaction};
use crate::sui_client::Signer;
use super::submit_ptb;

/// One argument to an admin Move call: either an on-chain object (resolved
/// by reading its owner) or a pure BCS value.
pub enum CallArg {
    /// Object id + whether the call takes it by `&mut`.
    Object(ObjectID, bool),
    Pure(Vec<u8>),
}

/// Build, sign, submit, and wait for the on-chain effects of a Move call.
async fn execute_move_call(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    module: &'static str,
    function: &'static str,
    type_args: Vec<&str>,
    args: Vec<CallArg>,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    info!(%package, module, function, gas_budget, "submitting move call");
    let type_arguments: Vec<TypeTag> = type_args
        .into_iter()
        .map(|s| TypeTag::from_str(s).with_context(|| format!("parsing type tag {s}")))
        .collect::<Result<_>>()?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let mut arguments: Vec<Argument> = Vec::with_capacity(args.len());
    for arg in args {
        let a = match arg {
            CallArg::Object(id, mutable) => {
                let oa = client
                    .object_arg(id, mutable)
                    .await
                    .with_context(|| format!("resolving object arg {id}"))?;
                pt.obj(oa)?
            }
            CallArg::Pure(bytes) => pt.pure_bytes(bytes, /* force_separate */ false),
        };
        arguments.push(a);
    }
    pt.programmable_move_call(
        package,
        Identifier::new(module)?,
        Identifier::new(function)?,
        type_arguments,
        arguments,
    );

    submit_ptb(client, signer, pt, gas_budget, &format!("{module}::{function}")).await
}

/// Calls `admin::set_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)`.
pub async fn set_fee_bps(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    protocol_config: ObjectID,
    new_bps: u64,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    execute_move_call(
        client,
        signer,
        package,
        "admin",
        "set_fee_bps",
        vec![],
        vec![
            CallArg::Object(admin_cap, false),
            CallArg::Object(protocol_config, true),
            CallArg::Pure(bcs::to_bytes(&new_bps)?),
        ],
        gas_budget,
    )
    .await
}

/// The ONE ingress whitelist of a deployment (guarded launch): the
/// standalone `whitelist` package, its shared `Whitelist` object, and the
/// `AdminCap` that gates every mutation. Every gated package (core,
/// trading-vault, exchange, exchange-adapter) checks this same object, so
/// admin tooling mutates exactly one list.
pub struct IngressWhitelist {
    /// The standalone whitelist package id.
    pub package: ObjectID,
    /// Owned `whitelist::AdminCap`.
    pub admin_cap: ObjectID,
    /// Shared `whitelist::Whitelist` object.
    pub whitelist: ObjectID,
}

/// The cap + shared list resolved into a PTB's inputs.
struct ResolvedWhitelist {
    admin: Argument,
    whitelist: Argument,
}

impl IngressWhitelist {
    async fn resolve(
        &self,
        client: &ChainClient,
        pt: &mut ProgrammableTransactionBuilder,
    ) -> Result<ResolvedWhitelist> {
        Ok(ResolvedWhitelist {
            admin: pt.obj(
                client
                    .object_arg(self.admin_cap, false)
                    .await
                    .context("resolving whitelist AdminCap")?,
            )?,
            whitelist: pt.obj(
                client
                    .object_arg(self.whitelist, true)
                    .await
                    .context("resolving shared Whitelist")?,
            )?,
        })
    }
}

/// One PTB: `whitelist::<function>(cap, wl, member)`.
async fn whitelist_member_op(
    client: &ChainClient,
    signer: &Signer,
    wl: &IngressWhitelist,
    member: SuiAddress,
    function: &'static str,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let r = wl.resolve(client, &mut pt).await?;
    let addr = pt.pure(member)?;
    pt.programmable_move_call(
        wl.package,
        Identifier::new("whitelist")?,
        Identifier::new(function)?,
        vec![],
        vec![r.admin, r.whitelist, addr],
    );
    submit_ptb(client, signer, pt, gas_budget, &format!("ingress whitelist {function}")).await
}

/// Adds `member` to the ingress whitelist.
pub async fn whitelist_add_member(
    client: &ChainClient,
    signer: &Signer,
    wl: &IngressWhitelist,
    member: SuiAddress,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    whitelist_member_op(client, signer, wl, member, "add_member", gas_budget).await
}

/// Removes `member` from the ingress whitelist.
pub async fn whitelist_remove_member(
    client: &ChainClient,
    signer: &Signer,
    wl: &IngressWhitelist,
    member: SuiAddress,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    whitelist_member_op(client, signer, wl, member, "remove_member", gas_budget).await
}

/// `whitelist::set_whitelist_enabled` — the go-public lever. Membership is
/// retained on-chain, so re-enabling restores the prior cohort.
pub async fn set_whitelist_enabled(
    client: &ChainClient,
    signer: &Signer,
    wl: &IngressWhitelist,
    enabled: bool,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let r = wl.resolve(client, &mut pt).await?;
    let flag = pt.pure(enabled)?;
    pt.programmable_move_call(
        wl.package,
        Identifier::new("whitelist")?,
        Identifier::new("set_whitelist_enabled")?,
        vec![],
        vec![r.admin, r.whitelist, flag],
    );
    submit_ptb(client, signer, pt, gas_budget, "ingress set_whitelist_enabled").await
}

/// One exchange market's pause target: the shared SettlementRegistry plus
/// the Base/Quote type args its `set_paused<Base, Quote>` call needs.
pub struct MarketPauseTarget {
    pub registry: ObjectID,
    pub base: String,
    pub quote: String,
}

/// The big red button, ONE PTB: `whitelist::set_ingress_paused` on the
/// shared Whitelist (whitelist AdminCap), trading-vault
/// `registry::set_paused` on the VaultProtocolConfig (still gated by the
/// CORE AdminCap), and exchange `registry::set_paused<Base, Quote>` on
/// every market (EXCHANGE AdminCap) — three caps total. Exits
/// (withdrawals/cancels) are never gated on-chain, so flipping this
/// strands nobody.
#[allow(clippy::too_many_arguments)]
pub async fn set_ingress_paused(
    client: &ChainClient,
    signer: &Signer,
    wl: &IngressWhitelist,
    core_admin_cap: ObjectID,
    trading_vault_package: ObjectID,
    vault_protocol_config: ObjectID,
    exchange_package: ObjectID,
    exchange_admin_cap: ObjectID,
    markets: &[MarketPauseTarget],
    paused: bool,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let r = wl.resolve(client, &mut pt).await?;
    let flag = pt.pure(paused)?;
    pt.programmable_move_call(
        wl.package,
        Identifier::new("whitelist")?,
        Identifier::new("set_ingress_paused")?,
        vec![],
        vec![r.admin, r.whitelist, flag],
    );
    let core_admin = pt.obj(
        client
            .object_arg(core_admin_cap, false)
            .await
            .context("resolving core AdminCap")?,
    )?;
    let vault_cfg = pt.obj(
        client
            .object_arg(vault_protocol_config, true)
            .await
            .context("resolving VaultProtocolConfig")?,
    )?;
    pt.programmable_move_call(
        trading_vault_package,
        Identifier::new("registry")?,
        Identifier::new("set_paused")?,
        vec![],
        vec![core_admin, vault_cfg, flag],
    );
    let exchange_admin = pt.obj(
        client
            .object_arg(exchange_admin_cap, false)
            .await
            .context("resolving exchange AdminCap")?,
    )?;
    for m in markets {
        let base = TypeTag::from_str(&m.base)
            .with_context(|| format!("parsing market base type {}", m.base))?;
        let quote = TypeTag::from_str(&m.quote)
            .with_context(|| format!("parsing market quote type {}", m.quote))?;
        let reg = pt.obj(
            client
                .object_arg(m.registry, true)
                .await
                .with_context(|| format!("resolving market registry {}", m.registry))?,
        )?;
        pt.programmable_move_call(
            exchange_package,
            Identifier::new("registry")?,
            Identifier::new("set_paused")?,
            vec![base, quote],
            vec![exchange_admin, reg, flag],
        );
    }
    submit_ptb(client, signer, pt, gas_budget, "ingress set_ingress_paused").await
}

/// Calls `treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, recipient,
/// ctx)`.
pub async fn withdraw_treasury(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    treasury: ObjectID,
    asset_type: &str,
    amount: u64,
    recipient: SuiAddress,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    execute_move_call(
        client,
        signer,
        package,
        "treasury",
        "withdraw",
        vec![asset_type],
        vec![
            CallArg::Object(admin_cap, false),
            CallArg::Object(treasury, true),
            CallArg::Pure(bcs::to_bytes(&amount)?),
            CallArg::Pure(bcs::to_bytes(&recipient)?),
        ],
        gas_budget,
    )
    .await
}
