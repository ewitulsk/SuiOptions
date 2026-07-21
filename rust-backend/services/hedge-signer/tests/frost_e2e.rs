//! In-process e2e for the FROST substrate: DKG keygen and the two-round
//! signing ceremony run against the real `/frost/*` handlers (service
//! side), with the curator side simulated directly with the frost crate.
//! The aggregated signature must verify as a PLAIN ed25519 signature under
//! the group public key — that is the whole point of the substrate
//! (Bluefin's Move verifier and Sui only ever see standard ed25519).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use base64::Engine;
use frost_ed25519 as frost;
use frost::keys::dkg;
use rand::rngs::OsRng;
use serde_json::json;

use hedge_signer::audit::AuditLog;
use hedge_signer::config::VaultConfig;
use hedge_signer::frost::{curator_id, service_id, Ceremonies, ShareStore};
use hedge_signer::frost_handlers::{self, KeygenRound1Req, KeygenRound2Req, SignRound1Req, SignRound2Req};
use hedge_signer::policy::bluefin::personal_message_digest;
use hedge_signer::policy::VaultPolicy;
use hedge_signer::state::FrostState;

const VAULT_ID: &str = "0x00000000000000000000000000000000000000000000000000000000000000aa";
const CURATOR_WALLET: &str =
    "0x00000000000000000000000000000000000000000000000000000000000000c1";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64d(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

struct TestEnv {
    state: Arc<FrostState>,
    audit_path: PathBuf,
    shares_path: PathBuf,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        std::fs::remove_file(&self.audit_path).ok();
        std::fs::remove_file(&self.shares_path).ok();
    }
}

fn test_env(name: &str) -> TestEnv {
    let dir = std::env::temp_dir();
    let audit_path = dir.join(format!("hedge-frost-audit-{}-{name}.jsonl", std::process::id()));
    let shares_path = dir.join(format!("hedge-frost-shares-{}-{name}.toml", std::process::id()));
    std::fs::remove_file(&audit_path).ok();
    std::fs::remove_file(&shares_path).ok();

    let vault_cfg = VaultConfig {
        vault_id: VAULT_ID.to_string(),
        external_account: "0x00000000000000000000000000000000000000000000000000000000000000ee"
            .to_string(),
        vault_address: VAULT_ID.to_string(),
        curator_pubkey_b64: None,
        max_borrow_amount: 1_000_000,
        allowed_pools: vec![],
        deepbook_margin_package: "0xdb".to_string(),
        curator_wallet: Some(CURATOR_WALLET.to_string()),
        bluefin: None,
    };
    let policy = VaultPolicy::from_config(
        &vault_cfg,
        sui_types::base_types::ObjectID::from_hex_literal("0x77").unwrap(),
    )
    .unwrap();
    let mut vaults = HashMap::new();
    vaults.insert(VAULT_ID.to_string(), policy);

    let state = Arc::new(FrostState {
        vaults,
        audit: Arc::new(AuditLog::open(&audit_path).unwrap()),
        ceremonies: Ceremonies::new(ShareStore::open(&shares_path).unwrap()),
    });
    TestEnv {
        state,
        audit_path,
        shares_path,
    }
}

/// Curator-side keygen against the service handlers. Returns the curator's
/// key material and the group pubkey hex + parent address the service
/// reported.
async fn run_keygen(
    state: &Arc<FrostState>,
) -> (
    frost::keys::KeyPackage,
    frost::keys::PublicKeyPackage,
    String,
    String,
) {
    // Curator round 1 → service round 1.
    let (cur_r1_secret, cur_r1_pkg) = dkg::part1(curator_id(), 2, 2, OsRng).unwrap();
    let Json(r1) = frost_handlers::keygen_round1(
        State(state.clone()),
        Json(KeygenRound1Req {
            vault_id: VAULT_ID.to_string(),
            curator_round1_b64: b64(&cur_r1_pkg.serialize().unwrap()),
        }),
    )
    .await
    .expect("keygen round1");

    // Curator round 2 → service round 2 + finalize; curator part3.
    let svc_r1 = dkg::round1::Package::deserialize(&b64d(&r1.service_round1_b64)).unwrap();
    let r1_map = BTreeMap::from([(service_id(), svc_r1)]);
    let (cur_r2_secret, cur_r2_pkgs) = dkg::part2(cur_r1_secret, &r1_map).unwrap();
    let for_service = cur_r2_pkgs.get(&service_id()).unwrap();
    let Json(r2) = frost_handlers::keygen_round2(
        State(state.clone()),
        Json(KeygenRound2Req {
            vault_id: VAULT_ID.to_string(),
            curator_round2_b64: b64(&for_service.serialize().unwrap()),
        }),
    )
    .await
    .expect("keygen round2");

    let svc_r2 = dkg::round2::Package::deserialize(&b64d(&r2.service_round2_b64)).unwrap();
    let r2_map = BTreeMap::from([(service_id(), svc_r2)]);
    let (cur_key_pkg, cur_pub_pkg) = dkg::part3(&cur_r2_secret, &r1_map, &r2_map).unwrap();

    // Both sides must agree on the group key.
    assert_eq!(
        hex::encode(cur_pub_pkg.verifying_key().serialize().unwrap()),
        r2.group_public_key_hex,
        "curator and service derived different group keys"
    );
    (cur_key_pkg, cur_pub_pkg, r2.group_public_key_hex, r2.sui_address)
}

fn withdraw_payload(parent: &str) -> Vec<u8> {
    serde_json::to_vec_pretty(&json!({
        "type": "Bluefin Pro Withdrawal",
        "eds": "0x0000000000000000000000000000000000000000000000000000000000000102",
        "assetSymbol": "USDC",
        "account": parent,
        "amount": "3500000000000",
        "salt": "1725930601205",
        "signedAt": "1725931543867",
    }))
    .unwrap()
}

#[tokio::test]
async fn keygen_then_two_round_sign_verifies_as_plain_ed25519() {
    let env = test_env("e2e");
    let state = &env.state;
    let (cur_key_pkg, cur_pub_pkg, group_pk_hex, parent) = run_keygen(state).await;

    // /frost/pubkey agrees with the ceremony output.
    let Json(pk) = frost_handlers::pubkey(State(state.clone()), Path(VAULT_ID.to_string()))
        .await
        .expect("pubkey");
    assert_eq!(pk.group_public_key_hex, group_pk_hex);
    assert_eq!(pk.sui_address, parent);

    // Re-keygen must be refused: the share already exists.
    let (_, again_r1) = dkg::part1(curator_id(), 2, 2, OsRng).unwrap();
    let err = frost_handlers::keygen_round1(
        State(state.clone()),
        Json(KeygenRound1Req {
            vault_id: VAULT_ID.to_string(),
            curator_round1_b64: b64(&again_r1.serialize().unwrap()),
        }),
    )
    .await
    .expect_err("re-keygen must be refused");
    assert_eq!(err.0, StatusCode::CONFLICT);

    // Signing round 1: a withdraw payload for the parent account.
    let payload = withdraw_payload(&parent);
    let Json(r1) = frost_handlers::sign_round1(
        State(state.clone()),
        Json(SignRound1Req {
            vault_id: VAULT_ID.to_string(),
            payload_kind: "withdraw".to_string(),
            payload_b64: b64(&payload),
        }),
    )
    .await
    .expect("sign round1");
    let digest = personal_message_digest(&payload);
    assert_eq!(r1.message_hex, hex::encode(digest), "service must sign the payload digest");

    // Curator commits, builds the SigningPackage from both commitments.
    let (cur_nonces, cur_commitments) =
        frost::round1::commit(cur_key_pkg.signing_share(), &mut OsRng);
    let svc_commitments =
        frost::round1::SigningCommitments::deserialize(&b64d(&r1.commitments_b64)).unwrap();
    let commitments = BTreeMap::from([
        (curator_id(), cur_commitments),
        (service_id(), svc_commitments),
    ]);
    let signing_package = frost::SigningPackage::new(commitments, &digest);

    // Signing round 2: the service contributes its share.
    let Json(r2) = frost_handlers::sign_round2(
        State(state.clone()),
        Json(SignRound2Req {
            session_id: r1.session_id.clone(),
            signing_package_b64: b64(&signing_package.serialize().unwrap()),
        }),
    )
    .await
    .expect("sign round2");
    let svc_share =
        frost::round2::SignatureShare::deserialize(&b64d(&r2.signature_share_b64)).unwrap();

    // Curator signs and aggregates.
    let cur_share = frost::round2::sign(&signing_package, &cur_nonces, &cur_key_pkg).unwrap();
    let shares = BTreeMap::from([(curator_id(), cur_share), (service_id(), svc_share)]);
    let signature = frost::aggregate(&signing_package, &shares, &cur_pub_pkg).unwrap();

    // Verifies under the frost group key…
    cur_pub_pkg
        .verifying_key()
        .verify(&digest, &signature)
        .expect("group verify");

    // …and, decisively, as a PLAIN ed25519 signature with a stock verifier.
    let pk_bytes: [u8; 32] = hex::decode(&group_pk_hex).unwrap().try_into().unwrap();
    let sig_bytes: [u8; 64] = signature.serialize().unwrap().try_into().unwrap();
    let dalek_pk = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes).unwrap();
    dalek_pk
        .verify_strict(&digest, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
        .expect("aggregated signature must be plain ed25519 under the group key");

    // The session was consumed: replaying round 2 is refused.
    let err = frost_handlers::sign_round2(
        State(state.clone()),
        Json(SignRound2Req {
            session_id: r1.session_id.clone(),
            signing_package_b64: b64(&signing_package.serialize().unwrap()),
        }),
    )
    .await
    .expect_err("session must be single-use");
    assert_eq!(err.0, StatusCode::FORBIDDEN);

    // Audit stream saw the approved withdraw with amount + asset.
    let audit = std::fs::read_to_string(&env.audit_path).unwrap();
    let approved: Vec<serde_json::Value> = audit
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &serde_json::Value| v["decision"] == "approved")
        .collect();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0]["tier"], "frost:withdraw");
    let summary = approved[0]["ptb_summary"].as_str().unwrap();
    assert!(summary.contains("3500000000000") && summary.contains("USDC"), "{summary}");
}

#[tokio::test]
async fn foreign_authorize_is_denied_and_audited() {
    let env = test_env("authz");
    let state = &env.state;
    let (_, _, _, parent) = run_keygen(state).await;

    let payload = serde_json::to_vec_pretty(&json!({
        "type": "Bluefin Pro Authorize Account",
        "ids": "0x0000000000000000000000000000000000000000000000000000000000000101",
        "account": parent,
        "user": "0x00000000000000000000000000000000000000000000000000000000000000dd",
        "status": true,
        "salt": "1725930601205",
        "signedAt": "1725931543867",
    }))
    .unwrap();
    let err = frost_handlers::sign_round1(
        State(state.clone()),
        Json(SignRound1Req {
            vault_id: VAULT_ID.to_string(),
            payload_kind: "authorize_account".to_string(),
            payload_b64: b64(&payload),
        }),
    )
    .await
    .expect_err("foreign authorize must be denied");
    assert_eq!(err.0, StatusCode::FORBIDDEN);
    assert!(err.1.contains("not the configured curator wallet"), "{}", err.1);

    let audit = std::fs::read_to_string(&env.audit_path).unwrap();
    let last: serde_json::Value =
        serde_json::from_str(audit.lines().last().unwrap()).unwrap();
    assert_eq!(last["decision"], "denied");
    assert_eq!(last["vault_id"], VAULT_ID);
}

#[tokio::test]
async fn unknown_payload_kind_is_denied() {
    let env = test_env("kind");
    let state = &env.state;
    let _ = run_keygen(state).await;

    let err = frost_handlers::sign_round1(
        State(state.clone()),
        Json(SignRound1Req {
            vault_id: VAULT_ID.to_string(),
            payload_kind: "order".to_string(),
            payload_b64: b64(b"{}"),
        }),
    )
    .await
    .expect_err("unknown kind must be denied");
    assert_eq!(err.0, StatusCode::FORBIDDEN);
    assert!(err.1.contains("unknown payload kind"), "{}", err.1);
}

#[tokio::test]
async fn tampered_signing_package_message_is_refused() {
    let env = test_env("tamper");
    let state = &env.state;
    let (cur_key_pkg, _, _, parent) = run_keygen(state).await;

    let payload = withdraw_payload(&parent);
    let Json(r1) = frost_handlers::sign_round1(
        State(state.clone()),
        Json(SignRound1Req {
            vault_id: VAULT_ID.to_string(),
            payload_kind: "withdraw".to_string(),
            payload_b64: b64(&payload),
        }),
    )
    .await
    .expect("sign round1");

    // Curator swaps in a DIFFERENT message after round-1 classification.
    let evil = [0x42u8; 32];
    let (_, cur_commitments) = frost::round1::commit(cur_key_pkg.signing_share(), &mut OsRng);
    let svc_commitments =
        frost::round1::SigningCommitments::deserialize(&b64d(&r1.commitments_b64)).unwrap();
    let commitments = BTreeMap::from([
        (curator_id(), cur_commitments),
        (service_id(), svc_commitments),
    ]);
    let package = frost::SigningPackage::new(commitments, &evil);

    let err = frost_handlers::sign_round2(
        State(state.clone()),
        Json(SignRound2Req {
            session_id: r1.session_id,
            signing_package_b64: b64(&package.serialize().unwrap()),
        }),
    )
    .await
    .expect_err("message swap must be refused");
    assert_eq!(err.0, StatusCode::FORBIDDEN);
    assert!(err.1.contains("not the policy-approved digest"), "{}", err.1);
}
