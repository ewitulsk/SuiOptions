//! End-to-end test of the `/sign_message` route: drives the real public router
//! with a `TrustAllVerifier` and asserts the returned envelope matches the
//! signature the Sui Inbox verifies (same key + message as the on-chain vector).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use bridge_signer::ThresholdSigner;
use bridge_signer_service::router::public_router;
use bridge_signer_service::state::AppState;
use bridge_signer_service::verifier::TrustAllVerifier;
use bridge_types::{chain_id, CrossChainMessage};

fn state() -> Arc<AppState> {
    Arc::new(AppState {
        signer: ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap(),
        verifier: Box::new(TrustAllVerifier),
        ed25519_group_pubkey_id: 1,
        ecdsa_group_pubkey_id: 2,
    })
}

async fn post_json(app: axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn sign_message_returns_onchain_vector_signature() {
    // The known vector: HyperEVM → Sui, so the Ed25519 path is selected.
    let message = CrossChainMessage::new(
        chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(),
        chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),
        7,
        [0xab; 32],
        [0xcd; 32],
        b"hello-bridge".to_vec(),
    );

    let (status, body) =
        post_json(public_router(state()), "/sign_message", json!({ "message": message })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["message_hash"],
        "0x7b767c416104fbef99880be0416fa07353493afb6547ad67d700029ce09572af"
    );
    assert_eq!(body["envelope"]["scheme_tag"], 0); // ED25519
    assert_eq!(body["envelope"]["group_pubkey_id"], 1);
    assert_eq!(
        body["envelope"]["signature"],
        "0x3830227d4552e5a5864e7c16dbc69f0326a45a068cb98443fc4098824ce7afd0\
e0ccbe45043b269ae0b50c5bc54854451418d1803af9277c2262b9c0ad493b03"
    );
}

#[tokio::test]
async fn sign_message_rejects_unsupported_destination_family() {
    let message = CrossChainMessage::new(
        chain_id::encode(chain_id::FAMILY_EVM, 1).unwrap(),
        chain_id::encode(chain_id::FAMILY_SOLANA, 1).unwrap(), // no verifier here
        0,
        [0u8; 32],
        [0u8; 32],
        vec![],
    );

    let (status, _) =
        post_json(public_router(state()), "/sign_message", json!({ "message": message })).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}
