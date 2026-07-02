//! End-to-end tests of the async signing API: `POST /sign_requests` +
//! `GET /sign_requests/{hash}`, idempotent per hash, verify-before-admit.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use bridge_signer::ThresholdSigner;
use bridge_signer_service::ratelimit::RateLimiter;
use bridge_signer_service::router::public_router;
use bridge_signer_service::sessions::SessionStore;
use bridge_signer_service::state::AppState;
use bridge_signer_service::verifier::{SourceVerifier, TrustAllVerifier, VerifyError};
use bridge_types::message::derive_domain_sep;
use bridge_types::{chain_id, CrossChainMessage};

/// A verifier that always refuses — to prove uncommitted messages 422 at the door.
struct RejectAllVerifier;
#[async_trait::async_trait]
impl SourceVerifier for RejectAllVerifier {
    async fn verify_committed(&self, m: &CrossChainMessage) -> Result<(), VerifyError> {
        Err(VerifyError::NotCommitted { src_chain_id: m.src_chain_id, nonce: m.nonce })
    }
}

fn state_with(verifier: Box<dyn SourceVerifier>) -> Arc<AppState> {
    Arc::new(AppState {
        signer: ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap(),
        verifier,
        domain_sep: derive_domain_sep(&[0x01; 32]),
        ed25519_group_pubkey_id: 1,
        ecdsa_group_pubkey_id: 2,
        sessions: SessionStore::new(300, 1000),
        rate_limiter: RateLimiter::new(60, 1000),
    })
}

async fn post(app: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    send(app, Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap()).await
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    send(app, Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()).await
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn known_message() -> CrossChainMessage {
    // HyperEVM → Sui, so the Ed25519 path is selected.
    CrossChainMessage::new(
        chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(),
        chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),
        7,
        [0xab; 32],
        [0xcd; 32],
        b"hello-bridge".to_vec(),
    )
}

const KNOWN_HASH: &str = "0x535392536947463d04988702a5480f431f34efed3cf557dc12aa434c2decd707";
const KNOWN_SIG: &str = "0x12bc85a949906a86bdea305aa6bc32ef704e77de62ea5fb65a3df3a39902e533\
98ca95da28a3c34aa8187edcf8f6936330c94016e1e1c4d3f2f7b80027190001";

#[tokio::test]
async fn sign_request_completes_and_is_pollable() {
    let state = state_with(Box::new(TrustAllVerifier));

    // POST → 202, signed inline at M1, carries the known-vector signature.
    let (status, body) =
        post(public_router(state.clone()), "/sign_requests", json!({ "message": known_message() })).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["message_hash"], KNOWN_HASH);
    assert_eq!(body["status"], "signed");
    assert_eq!(body["envelope"]["scheme_tag"], 0); // ED25519
    assert_eq!(body["envelope"]["signature"], KNOWN_SIG);

    // GET by hash returns the same completed session.
    let (status, polled) =
        get(public_router(state), &format!("/sign_requests/{KNOWN_HASH}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(polled["status"], "signed");
    assert_eq!(polled["envelope"]["signature"], KNOWN_SIG);
}

#[tokio::test]
async fn duplicate_post_coalesces_onto_one_session() {
    let state = state_with(Box::new(TrustAllVerifier));
    let m = json!({ "message": known_message() });

    let (s1, b1) = post(public_router(state.clone()), "/sign_requests", m.clone()).await;
    let (s2, b2) = post(public_router(state), "/sign_requests", m).await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    assert_eq!(s2, StatusCode::ACCEPTED);
    // Both resolve to the same signed session (idempotent per hash).
    assert_eq!(b1["envelope"]["signature"], b2["envelope"]["signature"]);
    assert_eq!(b1["message_hash"], b2["message_hash"]);
}

#[tokio::test]
async fn uncommitted_message_is_rejected_at_the_door() {
    let state = state_with(Box::new(RejectAllVerifier));
    let (status, _) =
        post(public_router(state.clone()), "/sign_requests", json!({ "message": known_message() })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // No session was cached — GET is a 404, so a later retry can re-admit.
    let (get_status, _) = get(public_router(state), &format!("/sign_requests/{KNOWN_HASH}")).await;
    assert_eq!(get_status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unsupported_destination_family_is_bad_request() {
    let state = state_with(Box::new(TrustAllVerifier));
    let message = CrossChainMessage::new(
        chain_id::encode(chain_id::FAMILY_EVM, 1).unwrap(),
        chain_id::encode(chain_id::FAMILY_SOLANA, 1).unwrap(), // no verifier here
        0,
        [0u8; 32],
        [0u8; 32],
        vec![],
    );
    let (status, _) =
        post(public_router(state), "/sign_requests", json!({ "message": message })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_unknown_hash_is_not_found() {
    let state = state_with(Box::new(TrustAllVerifier));
    let (status, _) = get(public_router(state), "/sign_requests/0xdeadbeef").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
