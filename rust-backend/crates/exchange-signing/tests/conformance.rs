//! Release-blocking conformance suite (§5.2): the checked-in fixture vectors
//! must verify and must re-derive byte-identically. The same vectors are
//! hard-coded in the Move test suite; if this file's assertions change, the
//! Move side must change with it (and vice versa) — that is a consensus break.

use exchange_signing::fixtures::{generate, FixtureVector};
use exchange_signing::{order_digest, personal_message_signing_digest, verify_signature};

fn load_checked_in() -> Vec<FixtureVector> {
    let raw = include_str!("../fixtures/conformance.json");
    serde_json::from_str(raw).expect("fixtures/conformance.json parses")
}

#[test]
fn checked_in_fixtures_match_regeneration() {
    let generated = serde_json::to_value(generate()).unwrap();
    let checked_in = serde_json::to_value(load_checked_in()).unwrap();
    assert_eq!(
        generated, checked_in,
        "fixtures drifted from the signing implementation — this is a consensus break"
    );
}

#[test]
fn fixtures_verify() {
    for v in load_checked_in() {
        let order_bcs = hex::decode(&v.order_bcs_hex).unwrap();
        assert_eq!(v.order.to_bcs(), order_bcs, "{}: BCS drift", v.name);

        let digest = order_digest(&v.order, &v.registry_id);
        assert_eq!(hex::encode(digest.0), v.digest_hex, "{}: digest drift", v.name);
        assert_eq!(
            hex::encode(personal_message_signing_digest(&digest.0)),
            v.signing_digest_hex,
            "{}: intent-wrapping drift",
            v.name
        );

        let sig = hex::decode(&v.signature_hex).unwrap();
        let pk = hex::decode(&v.public_key_hex).unwrap();
        let derived = verify_signature(v.scheme, &digest.0, &sig, &pk)
            .unwrap_or_else(|e| panic!("{}: signature invalid: {e}", v.name));
        assert_eq!(derived, v.signer_address, "{}: address derivation drift", v.name);
    }
}
