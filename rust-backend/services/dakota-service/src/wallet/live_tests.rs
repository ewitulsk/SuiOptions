//! Live sandbox check for the signing chain.
//!
//! Unit tests can only prove we verify our *own* signatures. The thing that
//! actually breaks in production is a canonicalization mismatch: Dakota
//! re-canonicalizes the intent server-side, and if their bytes differ from
//! ours by one character the signature fails to verify — with an error that
//! says nothing about canonicalization. Only the real API can tell us.
//!
//! Run with a sandbox key:
//!
//! ```sh
//! DAKOTA_TEST_API_KEY=... cargo test -p dakota-service -- --ignored live
//! ```
//!
//! Creates a throwaway signer/group/policy/wallet each run (Dakota has no
//! delete for these, so they accumulate in the sandbox — harmless).

use super::*;
use p256::ecdsa::SigningKey;
use p256::pkcs8::EncodePrivateKey;
use serde_json::Value;

const BASE: &str = "https://api.platform.sandbox.dakota.xyz";

fn api_key() -> String {
    std::env::var("DAKOTA_TEST_API_KEY").expect("set DAKOTA_TEST_API_KEY to run live tests")
}

async fn post(path: &str, key: &str, body: &Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{BASE}{path}"))
        .header("x-api-key", key)
        .header("x-idempotency-key", uuid::Uuid::new_v4().to_string())
        .header("content-type", "application/json")
        .json(body)
        .send()
        .await
        .expect("request");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, json)
}

/// End-to-end: register our public key, build a wallet around it, then submit
/// a signed transfer and assert Dakota accepted the **signature**.
///
/// The transfer itself is expected to fail — the wallet is empty. That is the
/// point: an empty-balance rejection proves the signature verified and policy
/// evaluation was reached, whereas a signature error would mean our canonical
/// form disagrees with Dakota's.
#[tokio::test]
#[ignore] // requires DAKOTA_TEST_API_KEY
async fn live_signature_is_accepted_by_dakota() {
    let key = api_key();

    // Fresh key per run so the test never depends on prior state.
    let sk = SigningKey::random(&mut rand::rngs::OsRng);
    let pem = sk.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap().to_string();
    let signer = WalletSigner::from_pem(&pem).unwrap();
    let public_key = signer.public_key_b64().unwrap();

    let (st, signer_resp) = post(
        "/signers",
        &key,
        &serde_json::json!({ "name": "live-test", "public_key": public_key, "key_type": "ES256" }),
    )
    .await;
    assert_eq!(st, 201, "POST /signers → {signer_resp}");
    // Dakota echoes the key type back in its own spelling.
    assert_eq!(signer_resp["key_type"], "KEY_TYPE_ES256");

    // member_keys takes PUBLIC KEYS, not signer ids.
    let (st, group) = post(
        "/signer-groups",
        &key,
        &serde_json::json!({ "name": "live-test-group", "member_keys": [public_key] }),
    )
    .await;
    assert_eq!(st, 201, "POST /signer-groups → {group}");
    let group_id = group["id"].as_str().unwrap().to_string();

    let (st, policy) = post(
        "/policies",
        &key,
        &serde_json::json!({
            "name": "live-test-policy",
            "signer_group_id": group_id,
            "rules": [{ "rule_type": "approval_threshold", "action": "allow",
                        "definition": { "threshold": 1 } }],
        }),
    )
    .await;
    assert_eq!(st, 201, "POST /policies → {policy}");

    let (st, wallet) = post(
        "/wallets",
        &key,
        &serde_json::json!({
            "name": "live-test-wallet", "family": "evm",
            "signer_groups": [group_id], "policies": [policy["id"].as_str().unwrap()],
        }),
    )
    .await;
    assert_eq!(st, 201, "POST /wallets → {wallet}");
    let wallet_id = wallet["id"].as_str().unwrap().to_string();
    let address = wallet["address"].as_str().unwrap().to_string();

    // Balances must be readable on a brand-new wallet.
    let bal = reqwest::Client::new()
        .get(format!("{BASE}/wallets/{wallet_id}/balances"))
        .header("x-api-key", &key)
        .send()
        .await
        .unwrap();
    assert_eq!(bal.status().as_u16(), 200);

    // The actual subject of the test.
    let intent = SendTransactionIntent {
        wallet_id: wallet_id.clone(),
        caip2: "eip155:84532".into(),
        operation: TransferOperation {
            kind: "transfer".into(),
            from: address.clone(),
            to: "0x000000000000000000000000000000000000dEaD".into(),
            amount: "0.01".into(),
            asset_id: "USDC".into(),
        },
        idempotency_key: uuid::Uuid::new_v4().to_string(),
    };
    let endorsed = signer.endorse(intent).unwrap();
    let body = serde_json::to_value(&endorsed).unwrap();

    let (status, resp) = post(
        &format!("/wallets/{wallet_id}/transactions"),
        &key,
        &body,
    )
    .await;

    let detail = resp["detail"].as_str().unwrap_or("").to_lowercase();
    let title = resp["title"].as_str().unwrap_or("").to_lowercase();
    let combined = format!("{title} {detail}");

    // The one outcome that means our canonicalization is wrong.
    assert!(
        !combined.contains("signature")
            && !combined.contains("endorse")
            && !combined.contains("unauthorized signer"),
        "Dakota rejected the SIGNATURE — canonical form disagrees with theirs.\n\
         status {status}, body: {resp}"
    );

    // Anything else (accepted, or refused for balance/policy reasons) means
    // the signature verified and Dakota got as far as evaluating the transfer.
    println!("live signing OK — status {status}, body: {resp}");

    // --- amount normalization -------------------------------------------
    //
    // Dakota normalizes the decimal before rebuilding the intent it verifies
    // against, so a signature over "1.00" is checked against "1" and fails.
    // Without `normalize_amount` every whole-dollar transfer the dashboard
    // sends is rejected as "endorsement validation failed", which names
    // nothing useful. These are the exact forms a person types.
    for raw in ["1.00", "0.50", "2.00", "0.01"] {
        let intent = SendTransactionIntent {
            wallet_id: wallet_id.clone(),
            caip2: "eip155:84532".into(),
            operation: TransferOperation {
                kind: "transfer".into(),
                from: address.clone(),
                to: "0x000000000000000000000000000000000000dEaD".into(),
                amount: crate::wallet::normalize_amount(raw),
                asset_id: "USDC".into(),
            },
            idempotency_key: uuid::Uuid::new_v4().to_string(),
        };
        let body = serde_json::to_value(&signer.endorse(intent).unwrap()).unwrap();
        let (_, resp) = post(&format!("/wallets/{wallet_id}/transactions"), &key, &body).await;
        let detail = resp["detail"].as_str().unwrap_or("").to_lowercase();
        assert!(
            !detail.contains("endorsement"),
            "amount {raw:?} was rejected as an endorsement failure: {resp}"
        );
    }
}
