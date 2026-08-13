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

/// The two ingress whitelists of one deployment (guarded launch): the core
/// `ProtocolConfig` member list (module `admin`) and the exchange's shared
/// `Whitelist` (module `whitelist`). The exchange package deliberately has
/// no dependency on options_core, so it carries its own copy under its own
/// AdminCap — admin tooling treats the two as ONE logical list, and every
/// mutation here rides a single PTB touching both.
pub struct IngressWhitelists {
    pub core_package: ObjectID,
    pub core_admin_cap: ObjectID,
    pub core_config: ObjectID,
    pub exchange_package: ObjectID,
    pub exchange_admin_cap: ObjectID,
    pub exchange_whitelist: ObjectID,
}

/// The caps + shared lists resolved into a PTB's inputs.
struct ResolvedWhitelists {
    core_admin: Argument,
    core_config: Argument,
    exchange_admin: Argument,
    exchange_whitelist: Argument,
}

impl IngressWhitelists {
    async fn resolve(
        &self,
        client: &ChainClient,
        pt: &mut ProgrammableTransactionBuilder,
    ) -> Result<ResolvedWhitelists> {
        Ok(ResolvedWhitelists {
            core_admin: pt.obj(
                client
                    .object_arg(self.core_admin_cap, false)
                    .await
                    .context("resolving core AdminCap")?,
            )?,
            core_config: pt.obj(
                client
                    .object_arg(self.core_config, true)
                    .await
                    .context("resolving core ProtocolConfig")?,
            )?,
            exchange_admin: pt.obj(
                client
                    .object_arg(self.exchange_admin_cap, false)
                    .await
                    .context("resolving exchange AdminCap")?,
            )?,
            exchange_whitelist: pt.obj(
                client
                    .object_arg(self.exchange_whitelist, true)
                    .await
                    .context("resolving exchange Whitelist")?,
            )?,
        })
    }
}

/// One PTB: `admin::add_member` / `remove_member` on the core
/// ProtocolConfig AND `whitelist::<function>` on the exchange Whitelist.
async fn whitelist_member_op(
    client: &ChainClient,
    signer: &Signer,
    whitelists: &IngressWhitelists,
    member: SuiAddress,
    function: &'static str,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let r = whitelists.resolve(client, &mut pt).await?;
    let addr = pt.pure(member)?;
    pt.programmable_move_call(
        whitelists.core_package,
        Identifier::new("admin")?,
        Identifier::new(function)?,
        vec![],
        vec![r.core_admin, r.core_config, addr],
    );
    pt.programmable_move_call(
        whitelists.exchange_package,
        Identifier::new("whitelist")?,
        Identifier::new(function)?,
        vec![],
        vec![r.exchange_admin, r.exchange_whitelist, addr],
    );
    submit_ptb(client, signer, pt, gas_budget, &format!("ingress whitelist {function}")).await
}

/// Adds `member` to BOTH ingress whitelists in one PTB.
pub async fn whitelist_add_member(
    client: &ChainClient,
    signer: &Signer,
    whitelists: &IngressWhitelists,
    member: SuiAddress,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    whitelist_member_op(client, signer, whitelists, member, "add_member", gas_budget).await
}

/// Removes `member` from BOTH ingress whitelists in one PTB.
pub async fn whitelist_remove_member(
    client: &ChainClient,
    signer: &Signer,
    whitelists: &IngressWhitelists,
    member: SuiAddress,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    whitelist_member_op(client, signer, whitelists, member, "remove_member", gas_budget).await
}

/// One PTB flipping `set_whitelist_enabled` on both lists — the go-public
/// lever. Membership is retained on-chain, so re-enabling restores the
/// prior cohort.
pub async fn set_whitelist_enabled(
    client: &ChainClient,
    signer: &Signer,
    whitelists: &IngressWhitelists,
    enabled: bool,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let r = whitelists.resolve(client, &mut pt).await?;
    let flag = pt.pure(enabled)?;
    pt.programmable_move_call(
        whitelists.core_package,
        Identifier::new("admin")?,
        Identifier::new("set_whitelist_enabled")?,
        vec![],
        vec![r.core_admin, r.core_config, flag],
    );
    pt.programmable_move_call(
        whitelists.exchange_package,
        Identifier::new("whitelist")?,
        Identifier::new("set_whitelist_enabled")?,
        vec![],
        vec![r.exchange_admin, r.exchange_whitelist, flag],
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

/// The big red button, ONE PTB: `set_ingress_paused` on both whitelists,
/// trading-vault `registry::set_paused` on the VaultProtocolConfig (gated
/// by the CORE AdminCap), and exchange `registry::set_paused<Base, Quote>`
/// on every market. Exits (withdrawals/cancels) are never gated on-chain,
/// so flipping this strands nobody.
#[allow(clippy::too_many_arguments)]
pub async fn set_ingress_paused(
    client: &ChainClient,
    signer: &Signer,
    whitelists: &IngressWhitelists,
    trading_vault_package: ObjectID,
    vault_protocol_config: ObjectID,
    markets: &[MarketPauseTarget],
    paused: bool,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let r = whitelists.resolve(client, &mut pt).await?;
    let flag = pt.pure(paused)?;
    pt.programmable_move_call(
        whitelists.core_package,
        Identifier::new("admin")?,
        Identifier::new("set_ingress_paused")?,
        vec![],
        vec![r.core_admin, r.core_config, flag],
    );
    pt.programmable_move_call(
        whitelists.exchange_package,
        Identifier::new("whitelist")?,
        Identifier::new("set_ingress_paused")?,
        vec![],
        vec![r.exchange_admin, r.exchange_whitelist, flag],
    );
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
        vec![r.core_admin, vault_cfg, flag],
    );
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
            whitelists.exchange_package,
            Identifier::new("registry")?,
            Identifier::new("set_paused")?,
            vec![base, quote],
            vec![r.exchange_admin, reg, flag],
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
