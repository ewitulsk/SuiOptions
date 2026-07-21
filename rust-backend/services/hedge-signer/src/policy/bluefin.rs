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
//! - `sui_tx` — a Sui transaction from the parent address. Reuses the
//!   existing three-tier [`classify`](super::classify) engine and accepts
//!   ONLY the strict tier: the sweep shape where every transfer output pays
//!   the vault. (The parent's only legitimate Sui txs are sweeps back to the
//!   vault; deposits into Bluefin's AssetBank are pushed from the vault side
//!   and never need the parent's signature.)
//! - anything else — denied.
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
use sui_types::transaction::{TransactionData, TransactionDataAPI, TransactionKind};

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
            // The parent account's only legitimate Sui transactions are
            // sweeps back to the vault — the strict tier's exact shape.
            // Auto/emergency verdicts (margin-perimeter trading) belong to
            // the native-multisig external account, not this parent.
            match classify(p, pt) {
                Decision::Approve { tier: Tier::Strict } => Ok(ApprovedPayload {
                    message: transaction_digest(payload),
                    kind,
                    description: summary,
                    is_exit: true,
                    tx_digest: Some(tx_data.digest().to_string()),
                }),
                Decision::Approve { tier } => Err(format!(
                    "sui_tx classified {tier}, but only the strict sweep shape \
                     (every output pays the vault) is signed for the parent account"
                )),
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

    fn policy() -> VaultPolicy {
        VaultPolicy::from_config(
            &VaultConfig {
                vault_id: VAULT_ID.to_string(),
                external_account:
                    "0x00000000000000000000000000000000000000000000000000000000000000ee"
                        .to_string(),
                vault_address: VAULT_ID.to_string(),
                curator_pubkey_b64: None,
                max_borrow_amount: 1_000_000,
                allowed_pools: vec![],
                deepbook_margin_package: "0xdb".to_string(),
                curator_wallet: Some(CURATOR.to_string()),
                bluefin: Some(BluefinVaultConfig {
                    ids_id: Some(IDS.to_string()),
                    eds_id: Some(EDS.to_string()),
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
