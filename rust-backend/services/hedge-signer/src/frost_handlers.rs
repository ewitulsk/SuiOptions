//! HTTP handlers for the `/frost/*` surface: DKG keygen, the two-round
//! signing ceremony, and the group-pubkey lookup. Policy, audit and alert
//! conventions mirror the native-multisig `/sign` path in
//! [`crate::handlers`]; the ceremony mechanics live in [`crate::frost`].

use std::sync::Arc;

use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;
use sui_types::crypto::{PublicKey, Signer, SuiSignature};
use tracing::{error, info};

use crate::audit::{now_ms, AuditEntry};
use crate::chain::VaultLookup;
use crate::frost::{group_sui_address, SERVICE_ID};
use crate::policy::bluefin::classify_payload;
use crate::state::FrostState;

type ApiError = (StatusCode, String);

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

// -------------------------------------------------------------------- pubkey

#[derive(Debug, Serialize)]
pub struct FrostPubkeyResp {
    pub vault_id: String,
    /// The group ed25519 public key (32 bytes, hex). What Bluefin's Move
    /// verifier and Sui both see; the curator prefixes the 0x00 scheme flag
    /// when assembling full signatures.
    pub group_public_key_hex: String,
    /// Sui address derived from the group key — the parent account address.
    pub sui_address: String,
    pub scheme: String,
}

/// `GET /frost/pubkey/:vault_id` — the vault's group key + parent address.
pub async fn pubkey(
    State(s): State<Arc<FrostState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<FrostPubkeyResp>, ApiError> {
    let found = s
        .ceremonies
        .store
        .get(&vault_id, |share| {
            let pk = share.group_public_key_hex()?;
            let addr = group_sui_address(&share.public_key_package)?;
            anyhow::Ok((pk, addr))
        })
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("vault {vault_id} has no FROST share"),
            )
        })?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(FrostPubkeyResp {
        vault_id,
        group_public_key_hex: found.0,
        sui_address: found.1.to_string(),
        scheme: "ed25519".to_string(),
    }))
}

// -------------------------------------------------------- registration attest

/// Domain separator of the external-account registration attestation.
/// `vault::set_external_account_attested` rebuilds the same message on
/// chain, so these bytes are part of the interface. Byte-identical in
/// trading-vault v2 (`EXTERNAL_REG_DOMAIN` in vault.move) — completed
/// ceremonies survive the cutover with no re-registration.
pub const REGISTRATION_DOMAIN: &str = "tv_external_reg_v1";

#[derive(Debug, Serialize)]
pub struct FrostRegistrationResp {
    pub vault_id: String,
    /// The FROST group parent Sui address being attested.
    pub parent_address: String,
    /// The service key's raw ed25519 public key (32 bytes, hex, NO scheme
    /// flag) — what the vault is seeded with as its registrar.
    pub registrar_pubkey_hex: String,
    /// Raw ed25519 signature over `message_hex` (64 bytes, hex). Plain
    /// RFC 8032: no Sui intent, no blake2b prehash, no flag byte.
    pub signature_hex: String,
    /// `domain || vault_id(32) || parent_address(32)`, hex.
    pub message_hex: String,
    pub domain: String,
    pub scheme: String,
}

/// `GET /frost/registration/:vault_id` — an attestation that this service
/// co-holds the vault's FROST parent address.
///
/// Ungated, like keygen: the attestation is inert on chain unless it
/// verifies against the vault's seeded registrar pubkey, and the service
/// only ever attests a parent whose share it actually holds.
pub async fn registration(
    State(s): State<Arc<FrostState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<FrostRegistrationResp>, ApiError> {
    let vault_bytes = ObjectID::from_hex_literal(&vault_id)
        .map_err(|e| bad_request(format!("vault_id {vault_id} is not an object id: {e}")))?
        .into_bytes();
    let parent = s
        .ceremonies
        .store
        .get(&vault_id, |share| {
            group_sui_address(&share.public_key_package)
        })
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("vault {vault_id} has no FROST share"),
            )
        })?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let PublicKey::Ed25519(registrar_pk) = s.registrar.public() else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "service signing key is not ed25519; registration attestations cannot be verified \
             on chain"
                .to_string(),
        ));
    };

    let mut message = Vec::with_capacity(REGISTRATION_DOMAIN.len() + 64);
    message.extend_from_slice(REGISTRATION_DOMAIN.as_bytes());
    message.extend_from_slice(&vault_bytes);
    message.extend_from_slice(&parent.to_inner());
    // `Signer::sign` on an ed25519 SuiKeyPair is plain ed25519 over these
    // exact bytes; the flag + pubkey the Sui wrapper appends are dropped.
    let signature = Signer::sign(s.registrar.as_ref(), &message);

    info!(
        vault = %vault_id,
        parent = %parent,
        "issued external-account registration attestation"
    );
    Ok(Json(FrostRegistrationResp {
        vault_id,
        parent_address: parent.to_string(),
        registrar_pubkey_hex: hex::encode(registrar_pk.0),
        signature_hex: hex::encode(signature.signature_bytes()),
        message_hex: hex::encode(&message),
        domain: REGISTRATION_DOMAIN.to_string(),
        scheme: "ed25519".to_string(),
    }))
}

// -------------------------------------------------------------------- keygen

#[derive(Deserialize)]
pub struct KeygenRound1Req {
    pub vault_id: String,
    /// base64 of the curator's serialized DKG round-1 package.
    pub curator_round1_b64: String,
}

#[derive(Debug, Serialize)]
pub struct KeygenRound1Resp {
    /// base64 of the service's serialized DKG round-1 package.
    pub service_round1_b64: String,
    /// The service's fixed FROST participant identifier (curator is 1).
    pub service_identifier: u16,
}

/// Body of the 409 a vault that already has a share gets back: enough for
/// the caller to tell "someone already ran this ceremony" from "my half is
/// lost", without another round-trip to `/frost/pubkey`.
#[derive(Debug, Serialize)]
pub struct KeygenConflictResp {
    pub error: String,
    pub parent_address: String,
    pub group_public_key_hex: String,
}

/// `POST /frost/keygen/round1` errors. The already-has-a-share conflict
/// answers JSON; everything else stays plain text like the other handlers.
#[derive(Debug)]
pub enum KeygenError {
    Plain(StatusCode, String),
    Conflict(KeygenConflictResp),
}

impl KeygenError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Plain(status, _) => *status,
            Self::Conflict(_) => StatusCode::CONFLICT,
        }
    }
}

impl IntoResponse for KeygenError {
    fn into_response(self) -> Response {
        match self {
            Self::Plain(status, msg) => (status, msg).into_response(),
            Self::Conflict(body) => (StatusCode::CONFLICT, Json(body)).into_response(),
        }
    }
}

/// `POST /frost/keygen/round1` — service side of DKG round 1.
///
/// Open to any real vault: a fresh group address is inert until an admin
/// registers it with `vault::set_external_account`, so the config
/// registration that gates SIGNING is not required here. What is required
/// is that the vault exists on chain and has no external account yet — and
/// that we are not about to orphan a share we already hold.
pub async fn keygen_round1(
    State(s): State<Arc<FrostState>>,
    Json(req): Json<KeygenRound1Req>,
) -> Result<Json<KeygenRound1Resp>, KeygenError> {
    if let Some(existing) = s.ceremonies.store.get(&req.vault_id, |share| {
        let pk = share.group_public_key_hex()?;
        let addr = group_sui_address(&share.public_key_package)?;
        anyhow::Ok((pk, addr))
    }) {
        let (group_public_key_hex, parent_address) = existing
            .map_err(|e| KeygenError::Plain(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        return Err(KeygenError::Conflict(KeygenConflictResp {
            error: format!(
                "vault {} already has a FROST share; re-keygen would orphan its parent account",
                req.vault_id
            ),
            parent_address: parent_address.to_string(),
            group_public_key_hex,
        }));
    }
    // Fail closed: an RPC that will not answer is not an approval.
    match s.chain.resolve(&req.vault_id).await {
        Ok(VaultLookup::Vault { external: None }) => {}
        Ok(VaultLookup::Vault {
            external: Some(account),
        }) => {
            return Err(KeygenError::Plain(
                StatusCode::CONFLICT,
                format!(
                    "vault {} already has external account {account} registered on chain; \
                     a new parent address could never be registered for it",
                    req.vault_id
                ),
            ))
        }
        Ok(VaultLookup::NotAVault(why)) => {
            return Err(KeygenError::Plain(
                StatusCode::BAD_REQUEST,
                format!("refusing keygen: {why}"),
            ))
        }
        Err(e) => {
            return Err(KeygenError::Plain(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("cannot validate vault {} on chain: {e:#}", req.vault_id),
            ))
        }
    }
    let curator_round1 = b64().decode(req.curator_round1_b64.trim()).map_err(|_| {
        KeygenError::Plain(
            StatusCode::BAD_REQUEST,
            "curator_round1_b64 is not base64".into(),
        )
    })?;
    let service_round1 = s
        .ceremonies
        .keygen_round1(&req.vault_id, &curator_round1)
        .map_err(|e| KeygenError::Plain(StatusCode::BAD_REQUEST, format!("keygen round1: {e}")))?;
    info!(vault = %req.vault_id, "frost keygen round1 started");
    Ok(Json(KeygenRound1Resp {
        service_round1_b64: b64().encode(service_round1),
        service_identifier: SERVICE_ID,
    }))
}

#[derive(Deserialize)]
pub struct KeygenRound2Req {
    pub vault_id: String,
    /// base64 of the curator's serialized DKG round-2 package addressed to
    /// the service.
    pub curator_round2_b64: String,
}

#[derive(Debug, Serialize)]
pub struct KeygenRound2Resp {
    /// base64 of the service's serialized DKG round-2 package addressed to
    /// the curator (who then runs part3 and must arrive at the same group
    /// key).
    pub service_round2_b64: String,
    pub group_public_key_hex: String,
    pub sui_address: String,
}

/// `POST /frost/keygen/round2` — service side of DKG round 2 + finalize:
/// the service's share is persisted before this returns.
pub async fn keygen_round2(
    State(s): State<Arc<FrostState>>,
    Json(req): Json<KeygenRound2Req>,
) -> Result<Json<KeygenRound2Resp>, ApiError> {
    let curator_round2 = b64()
        .decode(req.curator_round2_b64.trim())
        .map_err(|_| bad_request("curator_round2_b64 is not base64"))?;
    let (service_round2, group_public_key_hex, address) = s
        .ceremonies
        .keygen_round2(&req.vault_id, &curator_round2)
        .map_err(|e| bad_request(format!("keygen round2: {e}")))?;
    info!(
        vault = %req.vault_id,
        group_pubkey = %group_public_key_hex,
        parent_address = %address,
        "frost keygen complete; service share persisted"
    );
    Ok(Json(KeygenRound2Resp {
        service_round2_b64: b64().encode(service_round2),
        group_public_key_hex,
        sui_address: address.to_string(),
    }))
}

// ---------------------------------------------------------------------- sign

#[derive(Deserialize)]
pub struct SignRound1Req {
    pub vault_id: String,
    /// `login` / `authorize_account` / `withdraw` / `sui_tx`.
    pub payload_kind: String,
    /// base64 of the raw payload bytes: the exact JSON a Bluefin request
    /// signs (personal-message wrapped), or `bcs(TransactionData)` for
    /// `sui_tx`.
    pub payload_b64: String,
}

#[derive(Debug, Serialize)]
pub struct SignRound1Resp {
    /// Ceremony session id for round 2.
    pub session_id: String,
    /// base64 of the service's serialized signing commitments.
    pub commitments_b64: String,
    pub service_identifier: u16,
    /// The 32-byte digest the ceremony will sign (hex) — derived by the
    /// service from the payload; round 2 rejects any other message.
    pub message_hex: String,
}

/// Audit + alert + count one FROST denial, and build the HTTP error.
async fn deny_frost(
    s: &FrostState,
    vault_id: &str,
    status: StatusCode,
    reason: String,
    sender: String,
    tx_digest: String,
    summary: String,
) -> ApiError {
    s.audit
        .record(&AuditEntry {
            ts_ms: now_ms(),
            vault_id: vault_id.to_string(),
            sender,
            tx_digest,
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
        payload = %summary,
        "refusing to contribute FROST signature share"
    );
    metrics::counter!("hedge_signer_frost_denials_total").increment(1);
    (status, reason)
}

/// `POST /frost/sign/round1` — classify the declared payload under the
/// vault's Bluefin policy and, on approval, open a signing session and
/// return the service's nonce commitments.
pub async fn sign_round1(
    State(s): State<Arc<FrostState>>,
    Json(req): Json<SignRound1Req>,
) -> Result<Json<SignRound1Resp>, ApiError> {
    let Some(vault) = s.vaults.get(&req.vault_id) else {
        return Err(deny_frost(
            &s,
            &req.vault_id,
            StatusCode::FORBIDDEN,
            format!("unknown vault {}", req.vault_id),
            String::new(),
            String::new(),
            String::new(),
        )
        .await);
    };
    let Some(parent) = s
        .ceremonies
        .store
        .get(&req.vault_id, |share| {
            group_sui_address(&share.public_key_package)
        })
        .transpose()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    else {
        return Err(deny_frost(
            &s,
            &req.vault_id,
            StatusCode::FORBIDDEN,
            format!("vault {} has no FROST share (run keygen first)", req.vault_id),
            String::new(),
            String::new(),
            String::new(),
        )
        .await);
    };
    let Ok(payload) = b64().decode(req.payload_b64.trim()) else {
        return Err(deny_frost(
            &s,
            &req.vault_id,
            StatusCode::BAD_REQUEST,
            "payload_b64 is not base64".into(),
            parent.to_string(),
            String::new(),
            String::new(),
        )
        .await);
    };

    let approved = match classify_payload(vault, parent, &req.payload_kind, &payload) {
        Ok(a) => a,
        Err(reason) => {
            return Err(deny_frost(
                &s,
                &req.vault_id,
                StatusCode::FORBIDDEN,
                reason,
                parent.to_string(),
                String::new(),
                format!("kind={}", req.payload_kind),
            )
            .await)
        }
    };
    let message_hex = hex::encode(approved.message);
    let (session_id, commitments) = s
        .ceremonies
        .sign_round1(&req.vault_id, approved)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    info!(
        vault = %req.vault_id,
        kind = %req.payload_kind,
        session_id = %session_id,
        message = %message_hex,
        "frost sign round1: payload approved, commitments issued"
    );
    Ok(Json(SignRound1Resp {
        session_id,
        commitments_b64: b64().encode(commitments),
        service_identifier: SERVICE_ID,
        message_hex,
    }))
}

#[derive(Deserialize)]
pub struct SignRound2Req {
    pub session_id: String,
    /// base64 of the curator-built serialized `SigningPackage` (both
    /// participants' commitments + the message).
    pub signing_package_b64: String,
}

#[derive(Debug, Serialize)]
pub struct SignRound2Resp {
    /// base64 of the service's serialized signature share. The curator
    /// aggregates it with their own; the result verifies as plain ed25519
    /// under the group public key.
    pub signature_share_b64: String,
    pub service_identifier: u16,
}

/// `POST /frost/sign/round2` — contribute the service's signature share for
/// a round-1-approved session. Sessions are single-use and expire.
pub async fn sign_round2(
    State(s): State<Arc<FrostState>>,
    Json(req): Json<SignRound2Req>,
) -> Result<Json<SignRound2Resp>, ApiError> {
    let Some(session) = s.ceremonies.take_sign_session(&req.session_id) else {
        return Err(deny_frost(
            &s,
            "",
            StatusCode::FORBIDDEN,
            format!(
                "signing session {} not found (unknown, expired, or already used)",
                req.session_id
            ),
            String::new(),
            String::new(),
            String::new(),
        )
        .await);
    };
    let Ok(signing_package) = b64().decode(req.signing_package_b64.trim()) else {
        return Err(deny_frost(
            &s,
            &session.vault_id,
            StatusCode::BAD_REQUEST,
            "signing_package_b64 is not base64".into(),
            String::new(),
            session.tx_digest.clone().unwrap_or_default(),
            session.description.clone(),
        )
        .await);
    };
    let share = match s.ceremonies.sign_round2(&session, &signing_package) {
        Ok(share) => share,
        Err(e) => {
            return Err(deny_frost(
                &s,
                &session.vault_id,
                StatusCode::FORBIDDEN,
                e.to_string(),
                String::new(),
                session.tx_digest.clone().unwrap_or_default(),
                session.description.clone(),
            )
            .await)
        }
    };

    let kind = session.kind.as_str();
    s.audit
        .record(&AuditEntry {
            ts_ms: now_ms(),
            vault_id: session.vault_id.clone(),
            sender: String::new(),
            tx_digest: session
                .tx_digest
                .clone()
                .unwrap_or_else(|| hex::encode(session.message)),
            decision: "approved".into(),
            tier: Some(format!("frost:{kind}")),
            reason: None,
            ptb_summary: session.description.clone(),
        })
        .await;
    if session.is_exit {
        // Value leaves the venue (withdraw) or the parent address (sweep):
        // alert-tagged so co-signed exits are trackable, like denials.
        info!(
            alert_id = "hedge-signer-exit-cosigned",
            vault = %session.vault_id,
            kind = %kind,
            payload = %session.description,
            "co-signed value-exit payload"
        );
    } else {
        info!(
            vault = %session.vault_id,
            kind = %kind,
            payload = %session.description,
            "co-signed venue payload"
        );
    }
    metrics::counter!("hedge_signer_frost_shares_total", "kind" => session.kind.as_str())
        .increment(1);
    Ok(Json(SignRound2Resp {
        signature_share_b64: b64().encode(share),
        service_identifier: SERVICE_ID,
    }))
}
