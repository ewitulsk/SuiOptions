//! Three-tier policy engine (doc 04 §3b) for the DeepBook Margin posture.
//!
//! Classifies the `ProgrammableTransaction` inside a `TransactionData` the
//! curator asks us to countersign:
//!
//! - **Auto-approve** — value stays inside the margin perimeter: every Move
//!   call is an allowlisted `deepbook_margin` target (manager lifecycle,
//!   borrow/repay, `pool_proxy` trading, TPSL) or neutral (`0x2::coin`,
//!   `0x1` stdlib), borrows are capped, shared-object inputs are
//!   allowlisted, and any `TransferObjects` pays the external account
//!   itself.
//! - **Strict** — the tx moves value out of the perimeter
//!   (`margin_manager::withdraw`, a transfer to a non-self address, or a
//!   `trading_vault::vault::return_external`): approve only the sweep
//!   shape, where every transfer recipient is the vault address and every
//!   call is perimeter or `return_external`.
//! - **Emergency** — `margin_manager::deposit`-only top-ups (restore the
//!   risk ratio): a subset of auto-approve, tagged separately for audit.
//!
//! Everything else — unknown packages, publish/upgrade, non-programmable
//! kinds — is denied. `SplitCoins`/`MergeCoins`/`MakeMoveVec` are neutral
//! plumbing.
//!
//! Classification order: emergency → strict trigger → auto → deny.
//!
//! The [`bluefin`] submodule is the sibling policy for the FROST-signed
//! Bluefin parent account (doc 03 §3b): it classifies detached venue
//! payloads (login / authorize_account / withdraw / sui_tx) before the
//! service contributes a threshold-signature share.

pub mod bluefin;

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result};
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableMoveCall, ProgrammableTransaction,
};
use sui_types::SUI_CLOCK_OBJECT_ID;

use crate::config::VaultConfig;

/// Approval tier a transaction was classified into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Auto,
    Strict,
    Emergency,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Auto => "auto",
            Tier::Strict => "strict",
            Tier::Emergency => "emergency",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The policy engine's verdict on one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Approve { tier: Tier },
    Deny { reason: String },
}

fn deny(reason: impl Into<String>) -> Decision {
    Decision::Deny {
        reason: reason.into(),
    }
}

/// Per-vault policy, parsed from [`VaultConfig`] at boot.
#[derive(Debug, Clone)]
pub struct VaultPolicy {
    pub vault_id: String,
    /// The 2-of-2 multisig address. Every tx sender must equal this.
    pub external_account: SuiAddress,
    /// Strict-tier sweep destination (= vault_id as an address).
    pub vault_address: SuiAddress,
    pub max_borrow_amount: u64,
    /// Canonical `deepbook_margin` package the perimeter targets live in.
    pub deepbook_margin_package: ObjectID,
    /// trading_vault package (from token-info) — pins `vault::return_external`.
    pub trading_vault_package: ObjectID,
    /// Allowed shared-object inputs: config `allowed_pools` (pools +
    /// registry + margin pools + the MarginManager) plus the vault object
    /// itself. The clock (0x6) is allowed implicitly.
    pub allowed_shared: HashSet<ObjectID>,
    /// Kept for the /policy summary only.
    pub allowed_pools: Vec<ObjectID>,
    pub curator_pubkey_b64: Option<String>,
    /// The only wallet a Bluefin `authorize_account` payload may authorize.
    /// `None` → every authorize payload is denied.
    pub curator_wallet: Option<SuiAddress>,
    /// Optional Bluefin internal-data-store pin (`ids` of authorize payloads).
    pub bluefin_ids: Option<ObjectID>,
    /// Optional Bluefin external-data-store pin (`eds` of withdraw payloads
    /// and `deposit_to_asset_bank` transactions).
    pub bluefin_eds: Option<ObjectID>,
    /// Bluefin Pro package pin for parent-address `deposit_to_asset_bank`
    /// transactions. `None` → every such deposit is denied.
    pub bluefin_package: Option<ObjectID>,
}

impl VaultPolicy {
    pub fn from_config(cfg: &VaultConfig, trading_vault_package: ObjectID) -> Result<Self> {
        let vault_object = ObjectID::from_hex_literal(&cfg.vault_id)
            .with_context(|| format!("vault_id {}", cfg.vault_id))?;
        let external_account = SuiAddress::from_str(&cfg.external_account)
            .with_context(|| format!("external_account for vault {}", cfg.vault_id))?;
        let vault_address = SuiAddress::from_str(&cfg.vault_address)
            .with_context(|| format!("vault_address for vault {}", cfg.vault_id))?;
        let deepbook_margin_package = ObjectID::from_hex_literal(&cfg.deepbook_margin_package)
            .with_context(|| format!("deepbook_margin_package for vault {}", cfg.vault_id))?;
        let mut allowed_pools = Vec::with_capacity(cfg.allowed_pools.len());
        for p in &cfg.allowed_pools {
            allowed_pools.push(
                ObjectID::from_hex_literal(p)
                    .with_context(|| format!("allowed_pools entry {p} for vault {}", cfg.vault_id))?,
            );
        }
        let mut allowed_shared: HashSet<ObjectID> = allowed_pools.iter().copied().collect();
        allowed_shared.insert(vault_object);
        let curator_wallet = cfg
            .curator_wallet
            .as_deref()
            .map(|w| {
                SuiAddress::from_str(w)
                    .with_context(|| format!("curator_wallet for vault {}", cfg.vault_id))
            })
            .transpose()?;
        let (bluefin_ids, bluefin_eds, bluefin_package) = match &cfg.bluefin {
            Some(b) => (
                b.ids_id
                    .as_deref()
                    .map(|s| {
                        ObjectID::from_hex_literal(s)
                            .with_context(|| format!("bluefin.ids_id for vault {}", cfg.vault_id))
                    })
                    .transpose()?,
                b.eds_id
                    .as_deref()
                    .map(|s| {
                        ObjectID::from_hex_literal(s)
                            .with_context(|| format!("bluefin.eds_id for vault {}", cfg.vault_id))
                    })
                    .transpose()?,
                b.package_id
                    .as_deref()
                    .map(|s| {
                        ObjectID::from_hex_literal(s).with_context(|| {
                            format!("bluefin.package_id for vault {}", cfg.vault_id)
                        })
                    })
                    .transpose()?,
            ),
            None => (None, None, None),
        };
        Ok(Self {
            vault_id: cfg.vault_id.clone(),
            external_account,
            vault_address,
            max_borrow_amount: cfg.max_borrow_amount,
            deepbook_margin_package,
            trading_vault_package,
            allowed_shared,
            allowed_pools,
            curator_pubkey_b64: cfg.curator_pubkey_b64.clone(),
            curator_wallet,
            bluefin_ids,
            bluefin_eds,
            bluefin_package,
        })
    }
}

/// What one Move call is, under this vault's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallKind {
    /// `0x2::coin::*` / `0x1::*` — value-neutral plumbing.
    Neutral,
    /// In-perimeter deepbook_margin call (manager lifecycle, repay,
    /// pool_proxy trading, tpsl).
    Perimeter,
    /// `margin_manager::deposit` — perimeter, and the emergency-tier anchor.
    Deposit,
    /// `margin_manager::borrow_base/quote` — perimeter, amount-capped.
    Borrow,
    /// `margin_manager::withdraw` — strict trigger.
    Withdraw,
    /// `trading_vault::vault::return_external` — strict-only sweep call.
    ReturnExternal,
    /// Anything else: default deny.
    Unknown,
}

fn framework() -> ObjectID {
    ObjectID::from_hex_literal("0x2").expect("0x2 is a valid ObjectID")
}

fn move_stdlib() -> ObjectID {
    ObjectID::from_hex_literal("0x1").expect("0x1 is a valid ObjectID")
}

fn kind_of(p: &VaultPolicy, call: &ProgrammableMoveCall) -> CallKind {
    if call.package == framework() && call.module.as_str() == "coin" {
        return CallKind::Neutral;
    }
    if call.package == move_stdlib() {
        return CallKind::Neutral;
    }
    if call.package == p.deepbook_margin_package {
        return match call.module.as_str() {
            "margin_manager" => match call.function.as_str() {
                "new" | "repay_base" | "repay_quote" => CallKind::Perimeter,
                "deposit" => CallKind::Deposit,
                "borrow_base" | "borrow_quote" => CallKind::Borrow,
                "withdraw" => CallKind::Withdraw,
                _ => CallKind::Unknown,
            },
            // Order placement/cancel/modify, *_and_repay_loan,
            // update_current_price — the whole proxy surface trades against
            // allowlisted pools with value staying in the manager.
            "pool_proxy" => CallKind::Perimeter,
            // Take-profit / stop-loss management.
            "tpsl" => CallKind::Perimeter,
            _ => CallKind::Unknown,
        };
    }
    if call.package == p.trading_vault_package
        && call.module.as_str() == "vault"
        && call.function.as_str() == "return_external"
    {
        return CallKind::ReturnExternal;
    }
    CallKind::Unknown
}

/// Where a `TransferObjects` recipient argument points.
enum Recipient {
    /// A pure input decodable as an address.
    Pure(SuiAddress),
    /// A pure input that is NOT a valid address encoding.
    Malformed,
    /// Not a pure input (result of a command, gas coin, …).
    NotPure,
}

fn resolve_recipient(pt: &ProgrammableTransaction, arg: &Argument) -> Recipient {
    let Argument::Input(i) = arg else {
        return Recipient::NotPure;
    };
    match pt.inputs.get(*i as usize) {
        Some(CallArg::Pure(bytes)) => match bcs::from_bytes::<SuiAddress>(bytes) {
            Ok(addr) => Recipient::Pure(addr),
            Err(_) => Recipient::Malformed,
        },
        _ => Recipient::NotPure,
    }
}

/// Classify `pt` under `p`. See the module docs for the tier definitions.
pub fn classify(p: &VaultPolicy, pt: &ProgrammableTransaction) -> Decision {
    // Never sign code deployment, in any tier.
    for cmd in &pt.commands {
        if matches!(cmd, Command::Publish(..) | Command::Upgrade(..)) {
            return deny("publish/upgrade transactions are never signed");
        }
    }

    let calls: Vec<&ProgrammableMoveCall> = pt
        .commands
        .iter()
        .filter_map(|cmd| match cmd {
            Command::MoveCall(c) => Some(c.as_ref()),
            _ => None,
        })
        .collect();
    let kinds: Vec<CallKind> = calls.iter().map(|c| kind_of(p, c)).collect();

    // Shared-object allowlist (all tiers): every shared input must be a
    // configured pool/registry/manager id, the vault object, or the clock.
    for input in &pt.inputs {
        if let CallArg::Object(ObjectArg::SharedObject { id, .. }) = input {
            if *id != SUI_CLOCK_OBJECT_ID && !p.allowed_shared.contains(id) {
                return deny(format!("shared object {id} is not in the allowlist"));
            }
        }
    }

    // Borrow caps (all tiers). The borrow amount must be a resolvable pure
    // u64 input; we conservatively cap EVERY pure u64 argument of a borrow
    // call (the exact parameter position is not pinned here — over-checking
    // only errs toward denial).
    for (call, kind) in calls.iter().zip(&kinds) {
        if *kind != CallKind::Borrow {
            continue;
        }
        let mut amounts: Vec<u64> = Vec::new();
        for arg in &call.arguments {
            if let Argument::Input(i) = arg {
                if let Some(CallArg::Pure(bytes)) = pt.inputs.get(*i as usize) {
                    if let Ok(v) = bcs::from_bytes::<u64>(bytes) {
                        amounts.push(v);
                    }
                }
            }
        }
        if amounts.is_empty() {
            return deny(format!(
                "{}::{} amount is not a resolvable pure u64 input",
                call.module, call.function
            ));
        }
        if let Some(over) = amounts.iter().find(|a| **a > p.max_borrow_amount) {
            return deny(format!(
                "{}::{} amount {over} exceeds max_borrow_amount {}",
                call.module, call.function, p.max_borrow_amount
            ));
        }
    }

    // Transfer recipients: a pure-address transfer to anything but the
    // external account itself is an exit and routes to the strict tier.
    // Malformed pure recipients are denied outright; non-pure (computed)
    // recipients don't trigger strict on their own.
    let mut transfer_exits = false;
    for cmd in &pt.commands {
        if let Command::TransferObjects(_, recipient) = cmd {
            match resolve_recipient(pt, recipient) {
                Recipient::Pure(addr) if addr == p.external_account => {}
                Recipient::Pure(_) => transfer_exits = true,
                Recipient::Malformed => {
                    return deny("TransferObjects recipient is a malformed pure input")
                }
                Recipient::NotPure => {}
            }
        }
    }

    let has_withdraw = kinds.contains(&CallKind::Withdraw);
    let has_return = kinds.contains(&CallKind::ReturnExternal);
    let strict_trigger = has_withdraw || has_return || transfer_exits;

    if strict_trigger {
        // Strict tier: the sweep path (withdraw → return_external, or a
        // transfer of a coin to the vault address). Every call must be
        // perimeter/neutral/withdraw/return_external, and every transfer
        // recipient must be a pure input equal to the vault address (the
        // external account itself also passes — self-directed change).
        for (call, kind) in calls.iter().zip(&kinds) {
            if *kind == CallKind::Unknown {
                return deny(format!(
                    "call {}::{}::{} is outside the strict-tier allowlist",
                    call.package, call.module, call.function
                ));
            }
        }
        for cmd in &pt.commands {
            if let Command::TransferObjects(_, recipient) = cmd {
                match resolve_recipient(pt, recipient) {
                    Recipient::Pure(addr)
                        if addr == p.vault_address || addr == p.external_account => {}
                    Recipient::Pure(addr) => {
                        return deny(format!(
                            "strict tier: TransferObjects recipient {addr} is not the vault address"
                        ))
                    }
                    Recipient::Malformed | Recipient::NotPure => {
                        return deny(
                            "strict tier: TransferObjects recipient is not a pure address input",
                        )
                    }
                }
            }
        }
        return Decision::Approve { tier: Tier::Strict };
    }

    // Auto tier: every call inside the perimeter (or neutral).
    for (call, kind) in calls.iter().zip(&kinds) {
        match kind {
            CallKind::Neutral | CallKind::Perimeter | CallKind::Deposit | CallKind::Borrow => {}
            _ => {
                return deny(format!(
                    "call {}::{}::{} is outside the auto-approve perimeter",
                    call.package, call.module, call.function
                ))
            }
        }
    }

    // Emergency: deposit-only top-up (a tagged subset of auto-approve).
    let non_neutral: Vec<CallKind> = kinds
        .iter()
        .copied()
        .filter(|k| *k != CallKind::Neutral)
        .collect();
    if !non_neutral.is_empty() && non_neutral.iter().all(|k| *k == CallKind::Deposit) {
        return Decision::Approve {
            tier: Tier::Emergency,
        };
    }

    if calls.is_empty() {
        // No Move calls at all: nothing endorses this shape.
        return deny("transaction contains no recognizable Move calls");
    }

    Decision::Approve { tier: Tier::Auto }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::SharedObjectMutability;
    use sui_types::Identifier;

    fn dbm_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0xdb").unwrap()
    }
    fn tv_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0x77").unwrap()
    }
    fn pool_id() -> ObjectID {
        ObjectID::from_hex_literal("0xbeef").unwrap()
    }
    fn manager_id() -> ObjectID {
        ObjectID::from_hex_literal("0xcafe").unwrap()
    }

    fn policy() -> VaultPolicy {
        let vault_id = "0x00000000000000000000000000000000000000000000000000000000000000aa";
        VaultPolicy::from_config(
            &crate::config::VaultConfig {
                vault_id: vault_id.to_string(),
                external_account:
                    "0x00000000000000000000000000000000000000000000000000000000000000ee"
                        .to_string(),
                vault_address: vault_id.to_string(),
                curator_pubkey_b64: None,
                max_borrow_amount: 1_000_000,
                allowed_pools: vec![pool_id().to_hex_literal(), manager_id().to_hex_literal()],
                deepbook_margin_package: dbm_pkg().to_hex_literal(),
                curator_wallet: None,
                bluefin: None,
            },
            tv_pkg(),
        )
        .unwrap()
    }

    fn shared(b: &mut ProgrammableTransactionBuilder, id: ObjectID) -> Argument {
        b.obj(ObjectArg::SharedObject {
            id,
            initial_shared_version: 1.into(),
            mutability: SharedObjectMutability::Mutable,
        })
        .unwrap()
    }

    fn call(
        b: &mut ProgrammableTransactionBuilder,
        pkg: ObjectID,
        module: &str,
        function: &str,
        args: Vec<Argument>,
    ) {
        b.programmable_move_call(
            pkg,
            Identifier::new(module).unwrap(),
            Identifier::new(function).unwrap(),
            vec![],
            args,
        );
    }

    fn approve(tier: Tier) -> Decision {
        Decision::Approve { tier }
    }

    fn assert_denied(d: &Decision, needle: &str) {
        match d {
            Decision::Deny { reason } => {
                assert!(reason.contains(needle), "reason {reason:?} missing {needle:?}")
            }
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn borrow_and_place_order_within_caps_is_auto() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        let amount = b.pure(500_000u64).unwrap();
        call(&mut b, dbm_pkg(), "margin_manager", "borrow_base", vec![mgr, amount]);
        let pool = shared(&mut b, pool_id());
        call(&mut b, dbm_pkg(), "pool_proxy", "place_limit_order", vec![pool]);
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Auto));
    }

    #[test]
    fn borrow_above_cap_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        let amount = b.pure(2_000_000u64).unwrap();
        call(&mut b, dbm_pkg(), "margin_manager", "borrow_quote", vec![mgr, amount]);
        assert_denied(&classify(&p, &b.finish()), "exceeds max_borrow_amount");
    }

    #[test]
    fn borrow_without_resolvable_pure_amount_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        // Amount comes from a command result, not a pure input.
        call(&mut b, dbm_pkg(), "margin_manager", "borrow_base", vec![mgr, Argument::Result(0)]);
        assert_denied(&classify(&p, &b.finish()), "not a resolvable pure u64");
    }

    #[test]
    fn withdraw_with_transfer_to_vault_is_strict() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        call(&mut b, dbm_pkg(), "margin_manager", "withdraw", vec![mgr]);
        b.transfer_arg(p.vault_address, Argument::Result(0));
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Strict));
    }

    #[test]
    fn withdraw_swept_via_return_external_is_strict() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        call(&mut b, dbm_pkg(), "margin_manager", "withdraw", vec![mgr]);
        let vault_obj = ObjectID::from_hex_literal(&p.vault_id).unwrap();
        let vault = shared(&mut b, vault_obj);
        call(&mut b, tv_pkg(), "vault", "return_external", vec![vault, Argument::Result(0)]);
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Strict));
    }

    #[test]
    fn withdraw_with_transfer_to_foreign_address_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        call(&mut b, dbm_pkg(), "margin_manager", "withdraw", vec![mgr]);
        let attacker = SuiAddress::from_str(
            "0x00000000000000000000000000000000000000000000000000000000000000dd",
        )
        .unwrap();
        b.transfer_arg(attacker, Argument::Result(0));
        assert_denied(&classify(&p, &b.finish()), "not the vault address");
    }

    #[test]
    fn deposit_only_is_emergency() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        let coin = b.pure(1u8).unwrap(); // stand-in owned-coin arg
        call(&mut b, dbm_pkg(), "margin_manager", "deposit", vec![mgr, coin]);
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Emergency));
    }

    #[test]
    fn unknown_package_call_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        call(&mut b, ObjectID::from_hex_literal("0xdead").unwrap(), "drain", "all", vec![]);
        assert_denied(&classify(&p, &b.finish()), "outside the auto-approve perimeter");
    }

    #[test]
    fn publish_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        b.command(Command::Publish(vec![vec![0u8]], vec![]));
        assert_denied(&classify(&p, &b.finish()), "publish/upgrade");
    }

    #[test]
    fn unknown_shared_object_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let foreign = shared(&mut b, ObjectID::from_hex_literal("0xf00d").unwrap());
        call(&mut b, dbm_pkg(), "pool_proxy", "place_limit_order", vec![foreign]);
        assert_denied(&classify(&p, &b.finish()), "not in the allowlist");
    }

    #[test]
    fn margin_manager_withdraw_never_reaches_auto() {
        // A bare withdraw with no transfers still routes to strict (and
        // approves — the coin stays with the sender, i.e. the multisig).
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let mgr = shared(&mut b, manager_id());
        call(&mut b, dbm_pkg(), "margin_manager", "withdraw", vec![mgr]);
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Strict));
    }

    #[test]
    fn transfer_to_self_stays_auto() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let pool = shared(&mut b, pool_id());
        call(&mut b, dbm_pkg(), "pool_proxy", "place_market_order", vec![pool]);
        b.transfer_arg(p.external_account, Argument::Result(0));
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Auto));
    }
}
