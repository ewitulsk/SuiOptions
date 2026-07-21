//! HTTP handlers: [`health`], [`pubkey`], [`policy`], [`sign`].

use std::sync::Arc;

use axum::extract::{Json, State};
use axum::http::StatusCode;
use base64::Engine;
use serde::{Deserialize, Serialize};
use shared_crypto::intent::Intent;
use sui_tx::tx::template::describe_ptb;
use sui_types::crypto::EncodeDecodeBase64;
use sui_types::transaction::{Transaction, TransactionData, TransactionDataAPI, TransactionKind};
use tracing::{error, info};

use crate::audit::{now_ms, AuditEntry};
use crate::policy::{classify, Decision};
use crate::state::AppState;

type ApiError = (StatusCode, String);

pub async fn health() -> &'static str {
    "ok"
}

// -------------------------------------------------------------------- pubkey

#[derive(Serialize)]
pub struct PubkeyResp {
    /// Flag-prefixed public key, base64 — the form `sui keytool
    /// multi-sig-address` takes, so operators can derive the 2-of-2 address.
    pub public_key_b64: String,
    /// The service key's own (single-key) Sui address.
    pub address: String,
    /// Signature scheme (`ed25519`, …).
    pub scheme: String,
}

/// `GET /pubkey` — the service's multisig member key.
pub async fn pubkey(State(s): State<Arc<AppState>>) -> Json<PubkeyResp> {
    let pk = s.sui.signer.keypair.public();
    Json(PubkeyResp {
        public_key_b64: pk.encode_base64(),
        address: s.sui.signer.address.to_string(),
        scheme: pk.scheme().to_string(),
    })
}

// -------------------------------------------------------------------- policy

#[derive(Serialize)]
pub struct VaultPolicyResp {
    pub vault_id: String,
    pub external_account: String,
    pub vault_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curator_pubkey_b64: Option<String>,
    pub max_borrow_amount: u64,
    pub allowed_pools: Vec<String>,
    pub deepbook_margin_package: String,
    pub trading_vault_package: String,
}

#[derive(Serialize)]
pub struct PolicyResp {
    pub vaults: Vec<VaultPolicyResp>,
}

/// `GET /policy` — summary of the loaded policy. No secrets.
pub async fn policy(State(s): State<Arc<AppState>>) -> Json<PolicyResp> {
    let mut vaults: Vec<VaultPolicyResp> = s
        .vaults
        .values()
        .map(|p| VaultPolicyResp {
            vault_id: p.vault_id.clone(),
            external_account: p.external_account.to_string(),
            vault_address: p.vault_address.to_string(),
            curator_pubkey_b64: p.curator_pubkey_b64.clone(),
            max_borrow_amount: p.max_borrow_amount,
            allowed_pools: p.allowed_pools.iter().map(|o| o.to_hex_literal()).collect(),
            deepbook_margin_package: p.deepbook_margin_package.to_hex_literal(),
            trading_vault_package: p.trading_vault_package.to_hex_literal(),
        })
        .collect();
    vaults.sort_by(|a, b| a.vault_id.cmp(&b.vault_id));
    Json(PolicyResp { vaults })
}

// ---------------------------------------------------------------------- sign

#[derive(Deserialize)]
pub struct SignReq {
    /// The vault whose external account is the tx sender.
    pub vault_id: String,
    /// base64(bcs(TransactionData)).
    pub tx_bytes_b64: String,
}

#[derive(Serialize)]
pub struct SignResp {
    /// base64 service signature over the exact `TransactionData` submitted.
    /// The curator's member signature over the same bytes completes the
    /// 2-of-2.
    pub signature_b64: String,
    /// `auto` / `strict` / `emergency`.
    pub tier: String,
    /// Command-sequence summary of what was signed.
    pub description: String,
}

/// Audit + alert + count one denial, and build the HTTP error. Denials that
/// never produced a decodable tx still get an audit line — one line per
/// /sign request, with whatever fields decoded.
async fn deny_request(
    s: &AppState,
    vault_id: &str,
    status: StatusCode,
    reason: String,
    sender: String,
    digest: String,
    summary: String,
) -> ApiError {
    s.audit
        .record(&AuditEntry {
            ts_ms: now_ms(),
            vault_id: vault_id.to_string(),
            sender,
            tx_digest: digest,
            decision: "denied".into(),
            tier: None,
            reason: Some(reason.clone()),
            ptb_summary: summary.clone(),
        })
        .await;
    error!(
        alert_id = "hedge-signer-denied",
        vault = %vault_id,
        reason = %reason,
        ptb = %summary,
        "refusing to co-sign"
    );
    metrics::counter!("hedge_signer_denials_total").increment(1);
    (status, reason)
}

/// `POST /sign` — classify a `TransactionData` under the vault's policy and,
/// on approval, return the service's signature over it.
pub async fn sign(
    State(s): State<Arc<AppState>>,
    Json(req): Json<SignReq>,
) -> Result<Json<SignResp>, ApiError> {
    let tx_bytes = match base64::engine::general_purpose::STANDARD.decode(req.tx_bytes_b64.trim()) {
        Ok(b) => b,
        Err(_) => {
            return Err(deny_request(
                &s,
                &req.vault_id,
                StatusCode::BAD_REQUEST,
                "tx_bytes_b64 is not base64".into(),
                String::new(),
                String::new(),
                String::new(),
            )
            .await)
        }
    };
    let tx_data: TransactionData = match bcs::from_bytes(&tx_bytes) {
        Ok(t) => t,
        Err(e) => {
            return Err(deny_request(
                &s,
                &req.vault_id,
                StatusCode::BAD_REQUEST,
                format!("decoding TransactionData: {e}"),
                String::new(),
                String::new(),
                String::new(),
            )
            .await)
        }
    };

    let sender = tx_data.sender();
    let digest = tx_data.digest().to_string();

    let Some(vault) = s.vaults.get(&req.vault_id) else {
        return Err(deny_request(
            &s,
            &req.vault_id,
            StatusCode::FORBIDDEN,
            format!("unknown vault {}", req.vault_id),
            sender.to_string(),
            digest,
            String::new(),
        )
        .await);
    };

    let TransactionKind::ProgrammableTransaction(pt) = tx_data.kind() else {
        return Err(deny_request(
            &s,
            &req.vault_id,
            StatusCode::FORBIDDEN,
            "only programmable transactions are signed".into(),
            sender.to_string(),
            digest,
            String::new(),
        )
        .await);
    };
    let summary = describe_ptb(pt);

    if sender != vault.external_account {
        return Err(deny_request(
            &s,
            &req.vault_id,
            StatusCode::FORBIDDEN,
            format!(
                "sender {sender} is not the external account {} for vault {}",
                vault.external_account, req.vault_id
            ),
            sender.to_string(),
            digest,
            summary,
        )
        .await);
    }

    match classify(vault, pt) {
        Decision::Deny { reason } => Err(deny_request(
            &s,
            &req.vault_id,
            StatusCode::FORBIDDEN,
            reason,
            sender.to_string(),
            digest,
            summary,
        )
        .await),
        Decision::Approve { tier } => {
            let sig = Transaction::signature_from_signer(
                tx_data.clone(),
                Intent::sui_transaction(),
                &s.sui.signer.keypair,
            );
            s.audit
                .record(&AuditEntry {
                    ts_ms: now_ms(),
                    vault_id: req.vault_id.clone(),
                    sender: sender.to_string(),
                    tx_digest: digest.clone(),
                    decision: "approved".into(),
                    tier: Some(tier.as_str().into()),
                    reason: None,
                    ptb_summary: summary.clone(),
                })
                .await;
            info!(
                vault = %req.vault_id,
                %sender,
                tx_digest = %digest,
                tier = %tier,
                ptb = %summary,
                "co-signed external-account transaction"
            );
            metrics::counter!("hedge_signer_signatures_total", "tier" => tier.as_str())
                .increment(1);
            Ok(Json(SignResp {
                signature_b64: sig.encode_base64(),
                tier: tier.as_str().to_string(),
                description: summary,
            }))
        }
    }
}
