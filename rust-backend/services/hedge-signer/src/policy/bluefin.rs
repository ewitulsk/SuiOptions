//! Bluefin payload policy for the FROST-signed parent account (doc 03 §3b).
//!
//! Every `/frost/sign` request declares a payload kind and carries the raw
//! bytes the curator wants threshold-signed. This module decides whether the
//! service may contribute its signature share, and — crucially — derives the
//! exact 32-byte digest the ceremony will sign FROM the classified bytes, so
//! a caller can never have one payload classified and a different one signed.
//!
//! Kinds:
//! - `login` — Bluefin JWT auth payload (`LoginRequest` JSON). Allowed, but
//!   parsed strictly and pinned to the parent address: all Bluefin payloads
//!   are personal-message-signed JSON, so a lax "allow anything" here would
//!   let any other payload kind be smuggled through as a login.
//! - `authorize_account` — allowed only when the wallet being authorized is
//!   the vault's configured `curator_wallet` (deauthorize of that same
//!   wallet is also allowed — it only ever reduces access).
//! - `withdraw` — allowed; Bluefin withdrawals have no destination field
//!   (funds can only land at the parent address), so the real policy gate is
//!   the Sui-side sweep. Amount/asset are surfaced for the audit log.
//! - `sui_tx` — a Sui transaction from the parent address. Two shapes are
//!   signed, everything else is denied:
//!   1. the sweep: the [`classify`](super::classify) engine — every
//!      transfer output pays the vault;
//!   2. the venue deposit: a transaction whose single Move call is Bluefin
//!      `exchange::deposit_to_asset_bank` against the pinned package + eds,
//!      crediting the parent account itself (the released funds arrive at
//!      the parent address as plain coins, so materializing the Bluefin
//!      account needs the parent's own signature — doc 03 §2 step 2).
//!
//! Payload formats: mirrored from the `bluefin-pro` crate v1.13.0
//! (`src/signature.rs` `conversion::signable` structs and the
//! `bluefin_api::models::LoginRequest` model). The crate itself is NOT a
//! dependency: its signable payload types are `Serialize`-only (it is a
//! client SDK for constructing payloads, not parsing them), so depending on
//! it would buy none of the deserialization we need while dragging in its
//! whole HTTP/signing stack. The structs below are field-for-field copies
//! with `deny_unknown_fields`, which fails closed on any drift.
//!
//! TODO(mainnet): before any mainnet ceremony, re-verify the payload shapes
//! (field names, `type` strings, personal-message wrapping) against the
//! pinned bluefin-pro SDK release actually used by the curator client, on
//! Bluefin staging (doc 03 Phase 0.4).

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use serde::Deserialize;
use std::str::FromStr;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::transaction::{
    Argument, CallArg, Command, ObjectArg, ProgrammableTransaction, TransactionData,
    TransactionDataAPI, TransactionKind,
};
use sui_types::SUI_CLOCK_OBJECT_ID;

use super::{classify, Decision, Tier, VaultPolicy};
use sui_tx::tx::template::describe_ptb;

type Blake2b256 = Blake2b<U32>;

/// `type` discriminator bluefin-pro puts in authorize payloads.
const AUTHORIZE_TYPE: &str = "Bluefin Pro Authorize Account";
/// `type` discriminator bluefin-pro puts in withdraw payloads.
const WITHDRAW_TYPE: &str = "Bluefin Pro Withdrawal";

/// Sui intent prefix for personal messages (scope=3, version=0, app=0) —
/// what every Bluefin request payload is signed under.
const INTENT_PERSONAL_MESSAGE: [u8; 3] = [3, 0, 0];
/// Sui intent prefix for transaction data (scope=0, version=0, app=0).
const INTENT_TRANSACTION_DATA: [u8; 3] = [0, 0, 0];

/// The declared kind of a `/frost/sign` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    Login,
    AuthorizeAccount,
    Withdraw,
    SuiTx,
}

impl PayloadKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "login" => Some(Self::Login),
            "authorize_account" => Some(Self::AuthorizeAccount),
            "withdraw" => Some(Self::Withdraw),
            "sui_tx" => Some(Self::SuiTx),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::AuthorizeAccount => "authorize_account",
            Self::Withdraw => "withdraw",
            Self::SuiTx => "sui_tx",
        }
    }
}

/// A payload the policy cleared for threshold signing.
#[derive(Debug, Clone)]
pub struct ApprovedPayload {
    /// The exact 32-byte digest the FROST ceremony must sign — derived here
    /// from the classified bytes, never taken from the caller.
    pub message: [u8; 32],
    pub kind: PayloadKind,
    /// Human summary for the audit log (includes amount/asset on withdraws).
    pub description: String,
    /// True for value-exiting payloads (withdraw / sui_tx sweep) — these get
    /// an alert-tagged log line when co-signed.
    pub is_exit: bool,
    /// `TransactionData` digest, `sui_tx` only.
    pub tx_digest: Option<String>,
}

fn blake2b256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    for p in parts {
        h.update(p);
    }
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

/// Minimal ULEB128 encoder for the BCS `Vec<u8>` length prefix.
fn uleb128(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
    out
}

/// The 32-byte digest a Sui ed25519 key signs for a personal message:
/// `blake2b256( [3,0,0] || bcs(PersonalMessage{ message }) )`. This is what
/// bluefin-pro's `sign_personal_message` signs for every request payload.
pub fn personal_message_digest(message: &[u8]) -> [u8; 32] {
    blake2b256(&[
        &INTENT_PERSONAL_MESSAGE,
        &uleb128(message.len() as u64),
        message,
    ])
}

/// The 32-byte digest a Sui ed25519 key signs for a transaction:
/// `blake2b256( [0,0,0] || bcs(TransactionData) )`.
pub fn transaction_digest(tx_bytes: &[u8]) -> [u8; 32] {
    blake2b256(&[&INTENT_TRANSACTION_DATA, tx_bytes])
}

// ------------------------------------------------------------------ payloads
// Field-for-field mirrors of bluefin-pro 1.13.0's signable payload JSON.
// `deny_unknown_fields` + all-required fields = deny on any shape drift.

/// `bluefin_api::models::LoginRequest` — signed as compact JSON.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginPayload {
    #[serde(rename = "accountAddress")]
    account_address: String,
    #[serde(rename = "signedAtMillis")]
    signed_at_millis: i64,
    #[serde(rename = "audience")]
    audience: String,
}

/// `conversion::signable::AuthorizeAccountRequest` /
/// `DeauthorizeAccountRequest` (identical wire shape; `status` flips).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthorizePayload {
    #[serde(rename = "type")]
    r#type: String,
    ids: String,
    account: String,
    user: String,
    status: bool,
    salt: String,
    signed_at: String,
}

/// `conversion::signable::WithdrawRequest`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WithdrawPayload {
    #[serde(rename = "type")]
    r#type: String,
    eds: String,
    asset_symbol: String,
    account: String,
    amount: String,
    salt: String,
    signed_at: String,
}

fn parse_addr(field: &str, s: &str) -> Result<SuiAddress, String> {
    SuiAddress::from_str(s).map_err(|e| format!("{field} {s:?} is not a valid address: {e}"))
}

fn parse_object_id(field: &str, s: &str) -> Result<ObjectID, String> {
    ObjectID::from_hex_literal(s).map_err(|e| format!("{field} {s:?} is not a valid object id: {e}"))
}

fn parse_u64_str(field: &str, s: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|_| format!("{field} {s:?} is not a u64"))
}

/// True when any Move call targets the vault's pinned Bluefin package.
/// (No pin configured ⇒ nothing can "touch" it — such transactions fall
/// through to the sweep classifier and are judged there.)
fn touches_bluefin(p: &VaultPolicy, pt: &ProgrammableTransaction) -> bool {
    let Some(pkg) = p.bluefin_package else {
        return false;
    };
    pt.commands.iter().any(|cmd| match cmd {
        Command::MoveCall(c) => c.package == pkg,
        _ => false,
    })
}

/// Validate the venue-deposit transaction shape: exactly one
/// `exchange::deposit_to_asset_bank` against the pinned package, its
/// `target_address` the parent itself, every shared input the pinned eds or
/// the clock, coin plumbing only around it, and no transfers or code
/// deployment anywhere. Returns the audit description on success.
///
/// Move signature (verified against the deployed package):
/// `deposit_to_asset_bank<T>(eds: &mut ExternalDataStore, asset_symbol:
/// String, target_address: address, amount: u64, coin: &mut Coin<T>, ctx)` —
/// argument 2 is the credited account, checked by POSITION below.
fn classify_bluefin_deposit(
    p: &VaultPolicy,
    parent: SuiAddress,
    pt: &ProgrammableTransaction,
) -> Result<String, String> {
    let pkg = p
        .bluefin_package
        .ok_or("no bluefin package_id pinned for this vault")?;
    let Some(eds) = p.bluefin_eds else {
        // The deposit's mandatory shared object can't be validated without
        // the pin — fail closed rather than sign against an unknown store.
        return Err("no bluefin eds_id pinned for this vault; refusing deposit".into());
    };

    // Shared inputs: only the pinned eds and the clock.
    for input in &pt.inputs {
        if let CallArg::Object(ObjectArg::SharedObject { id, .. }) = input {
            if *id != eds && *id != SUI_CLOCK_OBJECT_ID {
                return Err(format!(
                    "bluefin deposit: shared object {id} is not the pinned eds {eds}"
                ));
            }
        }
    }

    let mut deposits: Vec<&sui_types::transaction::ProgrammableMoveCall> = Vec::new();
    for cmd in &pt.commands {
        match cmd {
            Command::MoveCall(c) => {
                if c.package == pkg
                    && c.module.as_str() == "exchange"
                    && c.function.as_str() == "deposit_to_asset_bank"
                {
                    deposits.push(c.as_ref());
                } else if c.package == super::framework() && c.module.as_str() == "coin" {
                    // 0x2::coin plumbing around the deposit is fine.
                } else if c.package == super::move_stdlib() {
                    // 0x1 stdlib (e.g. string construction) is fine.
                } else {
                    return Err(format!(
                        "bluefin deposit: call {}::{}::{} is outside the deposit shape",
                        c.package, c.module, c.function
                    ));
                }
            }
            Command::SplitCoins(..) | Command::MergeCoins(..) | Command::MakeMoveVec(..) => {}
            Command::TransferObjects(..) => {
                return Err("bluefin deposit: TransferObjects is never part of the shape".into())
            }
            Command::Publish(..) | Command::Upgrade(..) => {
                return Err("publish/upgrade transactions are never signed".into())
            }
        }
    }
    let [call] = deposits.as_slice() else {
        return Err(format!(
            "bluefin deposit: expected exactly one deposit_to_asset_bank call, found {}",
            deposits.len()
        ));
    };

    // target_address (argument 2) must be a pure input equal to the parent —
    // deposit_to_asset_bank credits ANY address, so an unchecked target is a
    // value exit to an arbitrary Bluefin account.
    let target = match call.arguments.get(2) {
        Some(Argument::Input(i)) => match pt.inputs.get(*i as usize) {
            Some(CallArg::Pure(bytes)) => bcs::from_bytes::<SuiAddress>(bytes)
                .map_err(|_| "bluefin deposit: target_address is not a valid pure address")?,
            _ => return Err("bluefin deposit: target_address is not a pure input".into()),
        },
        _ => return Err("bluefin deposit: target_address argument missing".into()),
    };
    if target != parent {
        return Err(format!(
            "bluefin deposit: target_address {target} is not the parent account {parent}"
        ));
    }

    // amount (argument 3): surfaced for the audit log; not capped — the
    // on-chain release budget already bounds what ever reaches the parent.
    let amount = match call.arguments.get(3) {
        Some(Argument::Input(i)) => match pt.inputs.get(*i as usize) {
            Some(CallArg::Pure(bytes)) => bcs::from_bytes::<u64>(bytes).ok(),
            _ => None,
        },
        _ => None,
    };
    let amount_str = amount.map_or_else(|| "?".to_string(), |a| a.to_string());
    Ok(format!(
        "bluefin deposit_to_asset_bank {amount_str} crediting parent {parent}"
    ))
}

/// Classify one declared payload under `p` for the parent account at
/// `parent` (the FROST group address). `Ok` carries the digest to sign;
/// `Err` is the denial reason. Conservative throughout: any parse failure,
/// unknown field, or ambiguity denies.
pub fn classify_payload(
    p: &VaultPolicy,
    parent: SuiAddress,
    kind: &str,
    payload: &[u8],
) -> Result<ApprovedPayload, String> {
    let kind = PayloadKind::parse(kind)
        .ok_or_else(|| format!("unknown payload kind {kind:?} is never signed"))?;
    match kind {
        PayloadKind::Login => {
            let login: LoginPayload = serde_json::from_slice(payload)
                .map_err(|e| format!("login payload does not parse as LoginRequest: {e}"))?;
            let account = parse_addr("login accountAddress", &login.account_address)?;
            if account != parent {
                return Err(format!(
                    "login accountAddress {account} is not the parent account {parent}"
                ));
            }
            if login.signed_at_millis <= 0 {
                return Err("login signedAtMillis must be positive".into());
            }
            Ok(ApprovedPayload {
                message: personal_message_digest(payload),
                kind,
                description: format!("bluefin login (audience {})", login.audience),
                is_exit: false,
                tx_digest: None,
            })
        }
        PayloadKind::AuthorizeAccount => {
            let auth: AuthorizePayload = serde_json::from_slice(payload).map_err(|e| {
                format!("authorize payload does not parse as AuthorizeAccountRequest: {e}")
            })?;
            if auth.r#type != AUTHORIZE_TYPE {
                return Err(format!(
                    "authorize payload type {:?} is not {AUTHORIZE_TYPE:?}",
                    auth.r#type
                ));
            }
            let account = parse_addr("authorize account", &auth.account)?;
            if account != parent {
                return Err(format!(
                    "authorize account {account} is not the parent account {parent}"
                ));
            }
            let user = parse_addr("authorize user", &auth.user)?;
            let Some(curator) = p.curator_wallet else {
                return Err(format!(
                    "vault {} has no curator_wallet configured; refusing to authorize {user}",
                    p.vault_id
                ));
            };
            if user != curator {
                return Err(format!(
                    "authorize user {user} is not the configured curator wallet {curator}"
                ));
            }
            let ids = parse_object_id("authorize ids", &auth.ids)?;
            if let Some(pinned) = p.bluefin_ids {
                if ids != pinned {
                    return Err(format!("authorize ids {ids} is not the pinned ids {pinned}"));
                }
            }
            parse_u64_str("authorize salt", &auth.salt)?;
            parse_u64_str("authorize signedAt", &auth.signed_at)?;
            Ok(ApprovedPayload {
                message: personal_message_digest(payload),
                kind,
                description: format!(
                    "bluefin {} curator wallet {user}",
                    if auth.status { "authorize" } else { "deauthorize" }
                ),
                is_exit: false,
                tx_digest: None,
            })
        }
        PayloadKind::Withdraw => {
            let w: WithdrawPayload = serde_json::from_slice(payload)
                .map_err(|e| format!("withdraw payload does not parse as WithdrawRequest: {e}"))?;
            if w.r#type != WITHDRAW_TYPE {
                return Err(format!(
                    "withdraw payload type {:?} is not {WITHDRAW_TYPE:?}",
                    w.r#type
                ));
            }
            let account = parse_addr("withdraw account", &w.account)?;
            if account != parent {
                return Err(format!(
                    "withdraw account {account} is not the parent account {parent}"
                ));
            }
            let eds = parse_object_id("withdraw eds", &w.eds)?;
            if let Some(pinned) = p.bluefin_eds {
                if eds != pinned {
                    return Err(format!("withdraw eds {eds} is not the pinned eds {pinned}"));
                }
            }
            let amount = parse_u64_str("withdraw amount", &w.amount)?;
            parse_u64_str("withdraw salt", &w.salt)?;
            parse_u64_str("withdraw signedAt", &w.signed_at)?;
            Ok(ApprovedPayload {
                message: personal_message_digest(payload),
                kind,
                description: format!(
                    "bluefin withdraw {amount} (e9) {} to parent {parent}",
                    w.asset_symbol
                ),
                is_exit: true,
                tx_digest: None,
            })
        }
        PayloadKind::SuiTx => {
            let tx_data: TransactionData = bcs::from_bytes(payload)
                .map_err(|e| format!("sui_tx payload does not decode as TransactionData: {e}"))?;
            let sender = tx_data.sender();
            if sender != parent {
                return Err(format!(
                    "sui_tx sender {sender} is not the parent account {parent}"
                ));
            }
            let TransactionKind::ProgrammableTransaction(pt) = tx_data.kind() else {
                return Err("sui_tx: only programmable transactions are signed".into());
            };
            let summary = describe_ptb(pt);
            // A transaction touching the Bluefin package is judged as a
            // venue deposit — its verdict is final (never falls through to
            // the sweep path, so a malformed deposit can't be laundered
            // into another shape).
            if touches_bluefin(p, pt) {
                let description = classify_bluefin_deposit(p, parent, pt)?;
                return Ok(ApprovedPayload {
                    message: transaction_digest(payload),
                    kind,
                    description,
                    is_exit: false,
                    tx_digest: Some(tx_data.digest().to_string()),
                });
            }
            // Otherwise the parent account's only legitimate Sui
            // transactions are sweeps back to the vault.
            match classify(p, pt) {
                Decision::Approve { tier: Tier::Strict } => Ok(ApprovedPayload {
                    message: transaction_digest(payload),
                    kind,
                    description: summary,
                    is_exit: true,
                    tx_digest: Some(tx_data.digest().to_string()),
                }),
                Decision::Deny { reason } => Err(reason),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BluefinVaultConfig, VaultConfig};
    use serde_json::json;
    use sui_types::digests::ObjectDigest;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::Argument;

    const VAULT_ID: &str = "0x00000000000000000000000000000000000000000000000000000000000000aa";
    const CURATOR: &str = "0x00000000000000000000000000000000000000000000000000000000000000c1";
    const PARENT: &str = "0x00000000000000000000000000000000000000000000000000000000000000f0";
    const IDS: &str = "0x0000000000000000000000000000000000000000000000000000000000000101";
    const EDS: &str = "0x0000000000000000000000000000000000000000000000000000000000000102";
    const BLUEFIN_PKG: &str =
        "0x0000000000000000000000000000000000000000000000000000000000000103";

    fn policy() -> VaultPolicy {
        VaultPolicy::from_config(
            &VaultConfig {
                vault_id: VAULT_ID.to_string(),
                external_account:
                    "0x00000000000000000000000000000000000000000000000000000000000000ee"
                        .to_string(),
                vault_address: VAULT_ID.to_string(),
                curator_pubkey_b64: None,
                allowed_shared: vec![],
                curator_wallet: Some(CURATOR.to_string()),
                bluefin: Some(BluefinVaultConfig {
                    ids_id: Some(IDS.to_string()),
                    eds_id: Some(EDS.to_string()),
                    package_id: Some(BLUEFIN_PKG.to_string()),
                }),
            },
            ObjectID::from_hex_literal("0x77").unwrap(),
        )
        .unwrap()
    }

    fn parent() -> SuiAddress {
        SuiAddress::from_str(PARENT).unwrap()
    }

    fn authorize_json(user: &str) -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "type": AUTHORIZE_TYPE,
            "ids": IDS,
            "account": PARENT,
            "user": user,
            "status": true,
            "salt": "1725930601205",
            "signedAt": "1725931543867",
        }))
        .unwrap()
    }

    fn withdraw_json() -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "type": WITHDRAW_TYPE,
            "eds": EDS,
            "assetSymbol": "USDC",
            "account": PARENT,
            "amount": "3500000000000",
            "salt": "1725930601205",
            "signedAt": "1725931543867",
        }))
        .unwrap()
    }

    #[test]
    fn authorize_of_curator_wallet_is_allowed() {
        let p = policy();
        let ok = classify_payload(&p, parent(), "authorize_account", &authorize_json(CURATOR))
            .expect("curator authorize must pass");
        assert_eq!(ok.kind, PayloadKind::AuthorizeAccount);
        assert!(!ok.is_exit);
        assert!(ok.description.contains("authorize"));
    }

    #[test]
    fn authorize_of_foreign_wallet_is_denied() {
        let p = policy();
        let foreign = "0x00000000000000000000000000000000000000000000000000000000000000dd";
        let err = classify_payload(&p, parent(), "authorize_account", &authorize_json(foreign))
            .unwrap_err();
        assert!(err.contains("not the configured curator wallet"), "{err}");
    }

    #[test]
    fn authorize_without_configured_curator_is_denied() {
        let mut p = policy();
        p.curator_wallet = None;
        let err = classify_payload(&p, parent(), "authorize_account", &authorize_json(CURATOR))
            .unwrap_err();
        assert!(err.contains("no curator_wallet configured"), "{err}");
    }

    #[test]
    fn authorize_with_foreign_ids_is_denied() {
        let p = policy();
        let mut v: serde_json::Value =
            serde_json::from_slice(&authorize_json(CURATOR)).unwrap();
        v["ids"] = json!("0x0000000000000000000000000000000000000000000000000000000000000999");
        let err = classify_payload(
            &p,
            parent(),
            "authorize_account",
            &serde_json::to_vec_pretty(&v).unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("not the pinned ids"), "{err}");
    }

    #[test]
    fn withdraw_is_allowed_and_describes_amount_asset() {
        let p = policy();
        let ok = classify_payload(&p, parent(), "withdraw", &withdraw_json())
            .expect("withdraw must pass");
        assert!(ok.is_exit);
        assert!(ok.description.contains("3500000000000"), "{}", ok.description);
        assert!(ok.description.contains("USDC"), "{}", ok.description);
        assert_eq!(ok.message, personal_message_digest(&withdraw_json()));
    }

    #[test]
    fn withdraw_for_foreign_account_is_denied() {
        let p = policy();
        let mut v: serde_json::Value = serde_json::from_slice(&withdraw_json()).unwrap();
        v["account"] =
            json!("0x00000000000000000000000000000000000000000000000000000000000000dd");
        let err = classify_payload(
            &p,
            parent(),
            "withdraw",
            &serde_json::to_vec_pretty(&v).unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("not the parent account"), "{err}");
    }

    #[test]
    fn withdraw_with_extra_fields_is_denied() {
        // deny_unknown_fields: payload-shape drift fails closed.
        let p = policy();
        let mut v: serde_json::Value = serde_json::from_slice(&withdraw_json()).unwrap();
        v["destination"] =
            json!("0x00000000000000000000000000000000000000000000000000000000000000dd");
        let err = classify_payload(
            &p,
            parent(),
            "withdraw",
            &serde_json::to_vec_pretty(&v).unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
    }

    #[test]
    fn login_for_parent_is_allowed() {
        let p = policy();
        let payload = serde_json::to_vec(&json!({
            "accountAddress": PARENT,
            "signedAtMillis": 1_725_931_543_867i64,
            "audience": "api",
        }))
        .unwrap();
        let ok = classify_payload(&p, parent(), "login", &payload).expect("login must pass");
        assert!(!ok.is_exit);
    }

    #[test]
    fn login_smuggling_a_withdraw_payload_is_denied() {
        // A withdraw payload declared as `login` must not slide through the
        // lenient branch — same personal-message signing domain.
        let p = policy();
        let err = classify_payload(&p, parent(), "login", &withdraw_json()).unwrap_err();
        assert!(err.contains("does not parse"), "{err}");
    }

    #[test]
    fn unknown_kind_is_denied() {
        let p = policy();
        let err = classify_payload(&p, parent(), "order", b"{}").unwrap_err();
        assert!(err.contains("unknown payload kind"), "{err}");
    }

    fn tx_bytes(recipient: SuiAddress) -> Vec<u8> {
        let mut b = ProgrammableTransactionBuilder::new();
        b.transfer_arg(recipient, Argument::GasCoin);
        let tx = TransactionData::new_programmable(
            parent(),
            vec![(
                ObjectID::random(),
                sui_types::base_types::SequenceNumber::from_u64(1),
                ObjectDigest::random(),
            )],
            b.finish(),
            1_000_000,
            1_000,
        );
        bcs::to_bytes(&tx).unwrap()
    }

    #[test]
    fn sui_tx_sweep_to_vault_is_allowed_strict() {
        let p = policy();
        let vault_addr = SuiAddress::from_str(VAULT_ID).unwrap();
        let bytes = tx_bytes(vault_addr);
        let ok = classify_payload(&p, parent(), "sui_tx", &bytes).expect("sweep must pass");
        assert!(ok.is_exit);
        assert!(ok.tx_digest.is_some());
        assert_eq!(ok.message, transaction_digest(&bytes));
    }

    #[test]
    fn sui_tx_paying_foreign_address_is_denied() {
        let p = policy();
        let attacker = SuiAddress::from_str(
            "0x00000000000000000000000000000000000000000000000000000000000000dd",
        )
        .unwrap();
        let err = classify_payload(&p, parent(), "sui_tx", &tx_bytes(attacker)).unwrap_err();
        assert!(err.contains("not the vault address"), "{err}");
    }

    #[test]
    fn sui_tx_from_foreign_sender_is_denied() {
        let p = policy();
        let vault_addr = SuiAddress::from_str(VAULT_ID).unwrap();
        let mut b = ProgrammableTransactionBuilder::new();
        b.transfer_arg(vault_addr, Argument::GasCoin);
        let tx = TransactionData::new_programmable(
            SuiAddress::from_str(CURATOR).unwrap(), // not the parent
            vec![(
                ObjectID::random(),
                sui_types::base_types::SequenceNumber::from_u64(1),
                ObjectDigest::random(),
            )],
            b.finish(),
            1_000_000,
            1_000,
        );
        let err =
            classify_payload(&p, parent(), "sui_tx", &bcs::to_bytes(&tx).unwrap()).unwrap_err();
        assert!(err.contains("not the parent account"), "{err}");
    }

    // ------------------------------------------------ deposit_to_asset_bank

    use sui_types::transaction::SharedObjectMutability;
    use sui_types::Identifier;

    /// A parent-sent deposit PTB: SplitCoins plumbing + the deposit call.
    /// `pkg`/`eds`/`target` parameterized so each pin can be violated;
    /// `extra_transfer` bolts a TransferObjects exit onto the shape.
    fn deposit_tx_bytes(
        pkg: &str,
        eds: &str,
        target: &str,
        extra_transfer: Option<SuiAddress>,
        extra_call_pkg: Option<&str>,
    ) -> Vec<u8> {
        let mut b = ProgrammableTransactionBuilder::new();
        let eds_arg = b
            .obj(sui_types::transaction::ObjectArg::SharedObject {
                id: ObjectID::from_hex_literal(eds).unwrap(),
                initial_shared_version: 1.into(),
                mutability: SharedObjectMutability::Mutable,
            })
            .unwrap();
        let symbol = b.pure("USDC".to_string()).unwrap();
        let target_arg = b.pure(SuiAddress::from_str(target).unwrap()).unwrap();
        let amount = b.pure(1_000_000u64).unwrap();
        let coin = b.pure(7u8).unwrap(); // stand-in owned-coin arg
        b.programmable_move_call(
            ObjectID::from_hex_literal(pkg).unwrap(),
            Identifier::new("exchange").unwrap(),
            Identifier::new("deposit_to_asset_bank").unwrap(),
            vec![],
            vec![eds_arg, symbol, target_arg, amount, coin],
        );
        if let Some(addr) = extra_transfer {
            b.transfer_arg(addr, Argument::GasCoin);
        }
        if let Some(extra) = extra_call_pkg {
            b.programmable_move_call(
                ObjectID::from_hex_literal(extra).unwrap(),
                Identifier::new("drain").unwrap(),
                Identifier::new("all").unwrap(),
                vec![],
                vec![],
            );
        }
        let tx = TransactionData::new_programmable(
            parent(),
            vec![(
                ObjectID::random(),
                sui_types::base_types::SequenceNumber::from_u64(1),
                ObjectDigest::random(),
            )],
            b.finish(),
            1_000_000,
            1_000,
        );
        bcs::to_bytes(&tx).unwrap()
    }

    #[test]
    fn bluefin_deposit_crediting_parent_is_allowed() {
        let p = policy();
        let bytes = deposit_tx_bytes(BLUEFIN_PKG, EDS, PARENT, None, None);
        let ok = classify_payload(&p, parent(), "sui_tx", &bytes).expect("deposit must pass");
        assert_eq!(ok.kind, PayloadKind::SuiTx);
        assert!(!ok.is_exit, "a venue deposit is not a value exit");
        assert!(ok.description.contains("deposit_to_asset_bank"), "{}", ok.description);
        assert!(ok.description.contains("1000000"), "{}", ok.description);
        assert_eq!(ok.message, transaction_digest(&bytes));
    }

    #[test]
    fn bluefin_deposit_crediting_foreign_target_is_denied() {
        let p = policy();
        let foreign = "0x00000000000000000000000000000000000000000000000000000000000000dd";
        let err = classify_payload(
            &p,
            parent(),
            "sui_tx",
            &deposit_tx_bytes(BLUEFIN_PKG, EDS, foreign, None, None),
        )
        .unwrap_err();
        assert!(err.contains("not the parent account"), "{err}");
    }

    #[test]
    fn bluefin_deposit_with_extra_transfer_is_denied() {
        let p = policy();
        let attacker = SuiAddress::from_str(
            "0x00000000000000000000000000000000000000000000000000000000000000dd",
        )
        .unwrap();
        let err = classify_payload(
            &p,
            parent(),
            "sui_tx",
            &deposit_tx_bytes(BLUEFIN_PKG, EDS, PARENT, Some(attacker), None),
        )
        .unwrap_err();
        assert!(err.contains("TransferObjects"), "{err}");
    }

    #[test]
    fn bluefin_deposit_with_extra_unknown_call_is_denied() {
        let p = policy();
        let err = classify_payload(
            &p,
            parent(),
            "sui_tx",
            &deposit_tx_bytes(BLUEFIN_PKG, EDS, PARENT, None, Some("0xdead")),
        )
        .unwrap_err();
        assert!(err.contains("outside the deposit shape"), "{err}");
    }

    #[test]
    fn bluefin_deposit_against_foreign_eds_is_denied() {
        let p = policy();
        let foreign_eds =
            "0x0000000000000000000000000000000000000000000000000000000000000999";
        let err = classify_payload(
            &p,
            parent(),
            "sui_tx",
            &deposit_tx_bytes(BLUEFIN_PKG, foreign_eds, PARENT, None, None),
        )
        .unwrap_err();
        assert!(err.contains("not the pinned eds"), "{err}");
    }

    #[test]
    fn bluefin_deposit_without_package_pin_is_denied() {
        // No pin ⇒ the tx never reads as "bluefin" and the sweep classifier
        // rejects the unknown call/shared object — fail closed either way.
        let mut p = policy();
        p.bluefin_package = None;
        let err = classify_payload(
            &p,
            parent(),
            "sui_tx",
            &deposit_tx_bytes(BLUEFIN_PKG, EDS, PARENT, None, None),
        )
        .unwrap_err();
        assert!(err.contains("not in the allowlist"), "{err}");
    }

    #[test]
    fn bluefin_deposit_without_eds_pin_is_denied() {
        let mut p = policy();
        p.bluefin_eds = None;
        let err = classify_payload(
            &p,
            parent(),
            "sui_tx",
            &deposit_tx_bytes(BLUEFIN_PKG, EDS, PARENT, None, None),
        )
        .unwrap_err();
        assert!(err.contains("no bluefin eds_id pinned"), "{err}");
    }

    #[test]
    fn bluefin_double_deposit_is_denied() {
        let p = policy();
        // Two deposit calls in one tx: shape requires exactly one.
        let mut b = ProgrammableTransactionBuilder::new();
        for _ in 0..2 {
            let eds_arg = b
                .obj(sui_types::transaction::ObjectArg::SharedObject {
                    id: ObjectID::from_hex_literal(EDS).unwrap(),
                    initial_shared_version: 1.into(),
                    mutability: SharedObjectMutability::Mutable,
                })
                .unwrap();
            let symbol = b.pure("USDC".to_string()).unwrap();
            let target_arg = b.pure(SuiAddress::from_str(PARENT).unwrap()).unwrap();
            let amount = b.pure(5u64).unwrap();
            b.programmable_move_call(
                ObjectID::from_hex_literal(BLUEFIN_PKG).unwrap(),
                Identifier::new("exchange").unwrap(),
                Identifier::new("deposit_to_asset_bank").unwrap(),
                vec![],
                vec![eds_arg, symbol, target_arg, amount],
            );
        }
        let tx = TransactionData::new_programmable(
            parent(),
            vec![(
                ObjectID::random(),
                sui_types::base_types::SequenceNumber::from_u64(1),
                ObjectDigest::random(),
            )],
            b.finish(),
            1_000_000,
            1_000,
        );
        let err =
            classify_payload(&p, parent(), "sui_tx", &bcs::to_bytes(&tx).unwrap()).unwrap_err();
        assert!(err.contains("exactly one deposit_to_asset_bank"), "{err}");
    }

    #[test]
    fn personal_message_digest_matches_manual_construction() {
        let msg = b"hello bluefin";
        let mut manual = Vec::new();
        manual.extend_from_slice(&[3u8, 0, 0]);
        manual.push(msg.len() as u8); // uleb128 of a small length is the byte
        manual.extend_from_slice(msg);
        let mut h = Blake2b256::new();
        h.update(&manual);
        let expect: [u8; 32] = h.finalize().into();
        assert_eq!(personal_message_digest(msg), expect);
    }
}
