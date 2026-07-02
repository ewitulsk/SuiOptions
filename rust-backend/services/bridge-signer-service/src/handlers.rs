use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use bridge_types::{chain_id, CrossChainMessage, Scheme, SignatureEnvelope};

use crate::state::AppState;
use crate::verifier::VerifyError;

#[derive(Debug, Deserialize)]
pub struct SignRequest {
    pub message: CrossChainMessage,
}

#[derive(Debug, Serialize)]
pub struct SignResponse {
    /// `0x`-prefixed keccak256 digest the signature is over.
    pub message_hash: String,
    pub envelope: SignatureEnvelope,
}

/// POST /sign_message (spec §5.3). Enforces the §5.4 boundary, then signs with
/// the scheme the destination family verifies.
pub async fn sign_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SignRequest>,
) -> Result<Json<SignResponse>, ApiError> {
    let message = req.message;

    // (1) well-formed: destination must be a family we can sign for.
    let family = chain_id::family(message.dst_chain_id);
    let scheme = Scheme::for_family(family).ok_or(ApiError::UnsupportedFamily(family))?;

    // (2) §5.4: only sign messages the source Outbox committed at finality.
    state.verifier.verify_committed(&message).await?;

    // (3) reference the registered group key for this destination's scheme.
    let group_pubkey_id = match scheme {
        Scheme::Ed25519 => state.ed25519_group_pubkey_id,
        Scheme::EcdsaSecp256k1 => state.ecdsa_group_pubkey_id,
    };

    let message_hash = format!("0x{}", hex::encode(message.digest()));
    let envelope = state
        .signer
        .sign(&message, group_pubkey_id)
        .map_err(|e| ApiError::Sign(e.to_string()))?;

    Ok(Json(SignResponse { message_hash, envelope }))
}

#[derive(Debug, Serialize)]
pub struct GroupKeysResponse {
    /// 32-byte Ed25519 group pubkey to register as the Sui group key.
    pub ed25519_pubkey: String,
    /// 20-byte ECDSA group address to register as the EVM group key.
    pub ecdsa_address: String,
    pub ed25519_group_pubkey_id: u32,
    pub ecdsa_group_pubkey_id: u32,
}

/// GET /group_keys — the keys/ids operators register on-chain via
/// `registerGroupKey`. Not in the spec's endpoint list, but the natural wiring
/// surface for the 1-of-1 launch.
pub async fn group_keys(State(state): State<Arc<AppState>>) -> Json<GroupKeysResponse> {
    Json(GroupKeysResponse {
        ed25519_pubkey: format!("0x{}", hex::encode(state.signer.ed25519_group_pubkey())),
        ecdsa_address: format!("0x{}", hex::encode(state.signer.ecdsa_group_address())),
        ed25519_group_pubkey_id: state.ed25519_group_pubkey_id,
        ecdsa_group_pubkey_id: state.ecdsa_group_pubkey_id,
    })
}

/// GET /get_attestation (spec §5.3). M1 stub: real Nautilus remote attestation
/// (PCRs, enclave-bound ephemeral key) arrives at M3/M4. Returns the group keys
/// so the surface is still useful for wiring.
pub async fn get_attestation(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "attested": false,
        "note": "M1 stub — no Nautilus attestation yet (M3/M4)",
        "ed25519_group_pubkey": format!("0x{}", hex::encode(state.signer.ed25519_group_pubkey())),
        "ecdsa_group_address": format!("0x{}", hex::encode(state.signer.ecdsa_group_address())),
    }))
}

pub async fn health() -> &'static str {
    "ok"
}

/// Admin endpoints (spec §5.3): Seal key-load + share provisioning + DKG. All
/// stubbed at M1 (single-party keys come from config); implemented at M3.
pub async fn admin_not_implemented() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": "not implemented until M3 (Seal key-load / share provisioning / DKG)" })),
    )
        .into_response()
}

/// Maps signing/verification failures to HTTP status codes.
#[derive(Debug)]
pub enum ApiError {
    UnsupportedFamily(u8),
    Verify(VerifyError),
    Sign(String),
}

impl From<VerifyError> for ApiError {
    fn from(e: VerifyError) -> Self {
        ApiError::Verify(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::UnsupportedFamily(f) => (
                StatusCode::BAD_REQUEST,
                format!("destination family {f} has no supported signature scheme"),
            ),
            ApiError::Verify(e @ VerifyError::NotCommitted { .. }) => {
                (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
            }
            ApiError::Verify(e @ VerifyError::Unavailable(_)) => {
                (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
            }
            ApiError::Sign(m) => (StatusCode::INTERNAL_SERVER_ERROR, format!("signing failed: {m}")),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
