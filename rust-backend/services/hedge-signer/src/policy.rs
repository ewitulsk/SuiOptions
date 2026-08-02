//! Sweep policy for the parent account's Sui transactions (doc 03 §3b).
//!
//! The parent account (the vault's registered external account) exists to
//! hold venue collateral, and the ONLY Sui transaction the service will
//! countersign from it is the **sweep**: value coming back to the vault.
//! A transaction qualifies when every Move call is neutral plumbing
//! (`0x2::coin`, `0x1` stdlib) or `trading_vault::vault::return_external`,
//! every shared input is allowlisted, and every `TransferObjects`
//! recipient is a pure address equal to the vault (or the account itself —
//! self-directed change).
//!
//! Everything else — unknown packages, publish/upgrade, non-programmable
//! kinds, a transfer to any other address — is denied.
//! `SplitCoins`/`MergeCoins`/`MakeMoveVec` are neutral plumbing.
//!
//! The [`bluefin`] submodule is the sibling policy for the FROST-signed
//! Bluefin parent account: it classifies detached venue payloads (login /
//! authorize_account / withdraw / sui_tx) before the service contributes a
//! threshold-signature share, and routes the `sui_tx` sweep shape through
//! [`classify`] here.

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

/// Approval tier a transaction was classified into. The sweep is the only
/// approvable shape; the enum is kept so the audit log and metrics carry a
/// stable label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Strict,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Strict => "strict",
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
    /// The parent account address. Every tx sender must equal this.
    pub external_account: SuiAddress,
    /// Sweep destination (= vault_id as an address).
    pub vault_address: SuiAddress,
    /// trading_vault package (from token-info) — pins `vault::return_external`.
    pub trading_vault_package: ObjectID,
    /// Allowed shared-object inputs: config `allowed_shared` plus the vault
    /// object itself. The clock (0x6) is allowed implicitly.
    pub allowed_shared: HashSet<ObjectID>,
    /// Kept for the /policy summary only.
    pub allowed_shared_list: Vec<ObjectID>,
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
        let mut allowed_shared_list = Vec::with_capacity(cfg.allowed_shared.len());
        for o in &cfg.allowed_shared {
            allowed_shared_list.push(ObjectID::from_hex_literal(o).with_context(|| {
                format!("allowed_shared entry {o} for vault {}", cfg.vault_id)
            })?);
        }
        let mut allowed_shared: HashSet<ObjectID> = allowed_shared_list.iter().copied().collect();
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
            trading_vault_package,
            allowed_shared,
            allowed_shared_list,
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
    /// `trading_vault::vault::return_external` — the sweep call.
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

/// Classify `pt` under `p`. See the module docs for the sweep shape.
pub fn classify(p: &VaultPolicy, pt: &ProgrammableTransaction) -> Decision {
    // Never sign code deployment.
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

    // Shared-object allowlist: every shared input must be a configured id,
    // the vault object, or the clock.
    for input in &pt.inputs {
        if let CallArg::Object(ObjectArg::SharedObject { id, .. }) = input {
            if *id != SUI_CLOCK_OBJECT_ID && !p.allowed_shared.contains(id) {
                return deny(format!("shared object {id} is not in the allowlist"));
            }
        }
    }

    // Every call must be neutral plumbing or the sweep call itself.
    for call in &calls {
        if kind_of(p, call) == CallKind::Unknown {
            return deny(format!(
                "call {}::{}::{} is outside the sweep allowlist",
                call.package, call.module, call.function
            ));
        }
    }

    // Every transfer must pay the vault (or the account itself — change).
    for cmd in &pt.commands {
        if let Command::TransferObjects(_, recipient) = cmd {
            match resolve_recipient(pt, recipient) {
                Recipient::Pure(addr)
                    if addr == p.vault_address || addr == p.external_account => {}
                Recipient::Pure(addr) => {
                    return deny(format!(
                        "TransferObjects recipient {addr} is not the vault address"
                    ))
                }
                Recipient::Malformed | Recipient::NotPure => {
                    return deny("TransferObjects recipient is not a pure address input")
                }
            }
        }
    }

    // A transaction that neither calls nor transfers anything endorses
    // nothing — refuse rather than sign an opaque shape.
    if calls.is_empty() && !pt.commands.iter().any(|c| matches!(c, Command::TransferObjects(..))) {
        return deny("transaction contains no recognizable Move calls");
    }

    Decision::Approve { tier: Tier::Strict }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::SharedObjectMutability;
    use sui_types::Identifier;

    fn tv_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0x77").unwrap()
    }
    fn venue_obj() -> ObjectID {
        ObjectID::from_hex_literal("0xbeef").unwrap()
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
                allowed_shared: vec![venue_obj().to_hex_literal()],
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
    fn transfer_to_vault_is_the_sweep() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let coin = b.pure(1u8).unwrap(); // stand-in owned-coin arg
        b.transfer_arg(p.vault_address, coin);
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Strict));
    }

    #[test]
    fn return_external_into_the_vault_is_the_sweep() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let vault_obj = ObjectID::from_hex_literal(&p.vault_id).unwrap();
        let vault = shared(&mut b, vault_obj);
        let coin = b.pure(1u8).unwrap();
        call(&mut b, tv_pkg(), "vault", "return_external", vec![vault, coin]);
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Strict));
    }

    #[test]
    fn transfer_to_self_is_allowed_change() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let coin = b.pure(1u8).unwrap();
        b.transfer_arg(p.external_account, coin);
        assert_eq!(classify(&p, &b.finish()), approve(Tier::Strict));
    }

    #[test]
    fn transfer_to_foreign_address_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        let attacker = SuiAddress::from_str(
            "0x00000000000000000000000000000000000000000000000000000000000000dd",
        )
        .unwrap();
        let coin = b.pure(1u8).unwrap();
        b.transfer_arg(attacker, coin);
        assert_denied(&classify(&p, &b.finish()), "not the vault address");
    }

    #[test]
    fn foreign_call_is_denied() {
        let p = policy();
        let mut b = ProgrammableTransactionBuilder::new();
        call(&mut b, ObjectID::from_hex_literal("0xdead").unwrap(), "drain", "all", vec![]);
        assert_denied(&classify(&p, &b.finish()), "outside the sweep allowlist");
    }

    /// The margin-perimeter surface the DBM posture used to auto-approve
    /// (borrow / trade / withdraw) is now foreign code: denied outright.
    #[test]
    fn margin_perimeter_calls_are_denied() {
        let p = policy();
        let margin = ObjectID::from_hex_literal("0xdb").unwrap();
        for (module, function) in
            [("margin_manager", "borrow_base"), ("pool_proxy", "place_limit_order"), ("margin_manager", "withdraw")]
        {
            let mut b = ProgrammableTransactionBuilder::new();
            let obj = shared(&mut b, venue_obj());
            call(&mut b, margin, module, function, vec![obj]);
            assert_denied(&classify(&p, &b.finish()), "outside the sweep allowlist");
        }
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
        call(&mut b, tv_pkg(), "vault", "return_external", vec![foreign]);
        assert_denied(&classify(&p, &b.finish()), "not in the allowlist");
    }

    #[test]
    fn empty_transaction_is_denied() {
        let p = policy();
        let b = ProgrammableTransactionBuilder::new();
        assert_denied(&classify(&p, &b.finish()), "no recognizable Move calls");
    }
}
