use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use bridge_types::{chain_id, CrossChainMessage, Scheme, SignatureEnvelope};

use crate::sessions::{Admit, SignStatus};
use crate::state::AppState;
use crate::verifier::VerifyError;

#[derive(Debug, Deserialize)]
pub struct SignRequest {
    pub message: CrossChainMessage,
}

/// Async session status returned by POST (202) and GET.
#[derive(Debug, Serialize)]
pub struct SignRequestResponse {
    /// `0x`-prefixed keccak256 digest the signature is (or will be) over.
    pub message_hash: String,
    /// `"pending"` | `"signed"`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<SignatureEnvelope>,
}

fn to_response(message_hash: String, status: &SignStatus) -> SignRequestResponse {
    match status {
        SignStatus::Pending => SignRequestResponse { message_hash, status: "pending", envelope: None },
        SignStatus::Signed(env) => SignRequestResponse {
            message_hash,
            status: "signed",
            envelope: Some((**env).clone()),
        },
    }
}

/// POST /sign_requests (spec §5.3). Idempotent per message hash: a duplicate
/// coalesces onto the existing session. A newly-admitted request is verified
/// against the §5.4 source boundary *before* any signing work — an uncommitted
/// message is rejected at the door (422) and never queued. At M1 signing runs
/// inline, so the 202 usually already carries `status: "signed"`.
pub async fn create_sign_request(
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Json(req): Json<SignRequest>,
) -> Result<(StatusCode, Json<SignRequestResponse>), ApiError> {
    // Per-IP rate limit (skipped when the peer addr is absent, e.g. in tests).
    if let Some(ConnectInfo(addr)) = peer {
        if !state.rate_limiter.check(addr.ip()) {
            return Err(ApiError::RateLimited);
        }
    }

    let message = req.message;
    let family = chain_id::family(message.dst_chain_id);
    let scheme = Scheme::for_family(family).ok_or(ApiError::UnsupportedFamily(family))?;
    let hash = format!("0x{}", hex::encode(message.digest(&state.domain_sep)));

    // Claim the session (or coalesce). In-flight dedup: a concurrent duplicate
    // sees the Pending marker and returns without re-verifying/re-signing.
    match state.sessions.admit(&hash) {
        Admit::Existing(status) => return Ok((StatusCode::ACCEPTED, Json(to_response(hash, &status)))),
        Admit::Full => return Err(ApiError::Busy),
        Admit::New => {}
    }

    // §5.4 boundary. On failure, abandon the session (don't cache a rejection —
    // the message may reach finality later) and 422 at the door.
    if let Err(e) = state.verifier.verify_committed(&message).await {
        state.sessions.abandon(&hash);
        return Err(ApiError::from(e));
    }

    let group_pubkey_id = match scheme {
        Scheme::Ed25519 => state.ed25519_group_pubkey_id,
        Scheme::EcdsaSecp256k1 => state.ecdsa_group_pubkey_id,
    };
    match state.signer.sign(&message, &state.domain_sep, group_pubkey_id) {
        Ok(envelope) => {
            state.sessions.finalize(&hash, envelope.clone());
            Ok((StatusCode::ACCEPTED, Json(to_response(hash, &SignStatus::Signed(Box::new(envelope))))))
        }
        Err(e) => {
            state.sessions.abandon(&hash);
            Err(ApiError::Sign(e.to_string()))
        }
    }
}

/// GET /sign_requests/{message_hash} — poll a session by its `0x`-hash.
pub async fn get_sign_request(
    State(state): State<Arc<AppState>>,
    Path(message_hash): Path<String>,
) -> Result<Json<SignRequestResponse>, ApiError> {
    let hash = normalize_hash(&message_hash);
    match state.sessions.get(&hash) {
        Some(status) => Ok(Json(to_response(hash, &status))),
        None => Err(ApiError::NotFound),
    }
}

fn normalize_hash(h: &str) -> String {
    let h = h.trim().to_lowercase();
    if h.starts_with("0x") {
        h
    } else {
        format!("0x{h}")
    }
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
    RateLimited,
    Busy,
    NotFound,
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
            ApiError::Verify(e @ VerifyError::UnknownRoute(_)) => {
                (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
            }
            ApiError::Verify(e @ VerifyError::Unavailable(_)) => {
                (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
            }
            ApiError::Sign(m) => (StatusCode::INTERNAL_SERVER_ERROR, format!("signing failed: {m}")),
            ApiError::RateLimited => {
                (StatusCode::TOO_MANY_REQUESTS, "per-IP sign-request rate limit exceeded".into())
            }
            ApiError::Busy => {
                (StatusCode::SERVICE_UNAVAILABLE, "signer session capacity reached".into())
            }
            ApiError::NotFound => (StatusCode::NOT_FOUND, "no signing session for that hash".into()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
