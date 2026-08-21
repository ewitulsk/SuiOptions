//! Dakota wallet intents: canonicalization and ES256 signing.
//!
//! Dakota's wallets are non-custodial. We hold an ECDSA P-256 key; Dakota holds
//! only its public half, registered as an `ES256` signer. Every privileged
//! wallet or policy operation is an **endorsed request** — a signed statement of
//! intent — rather than a plain API call.
//!
//! The chain is exact and unforgiving:
//!
//! ```text
//! intent  ──RFC 8785 JCS──▶ canonical bytes ──SHA-256──▶ digest
//!         ──ECDSA P-256──▶ signature ──ASN.1 DER──▶ base64
//! ```
//!
//! Dakota **re-canonicalizes server-side** before verifying, so if our
//! canonical form differs from theirs by a single byte the signature simply
//! fails to verify and the error says nothing about why. Three rules keep the
//! two in step, and all three are enforced by construction below:
//!
//! - `snake_case` field names;
//! - amounts as **strings**, never JSON numbers (`"1.50"`, not `1.5`);
//! - unset fields **omitted**, never serialized as `null`.
//!
//! Nine endpoints take an endorsed request, not just transaction submission —
//! attaching or detaching a policy or signer group is equally privileged. See
//! [`Intent`].

use anyhow::{Context, Result};
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{DerSignature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use serde::Serialize;

/// The treasury signing key.
pub struct WalletSigner {
    key: SigningKey,
}

/// A signed statement of intent, in the shape Dakota's `EndorsedRequest`
/// expects.
#[derive(Debug, Clone, Serialize)]
pub struct EndorsedRequest<T: Serialize> {
    /// Base64 ASN.1 DER ECDSA signatures over the canonical intent. One per
    /// signer; a policy's `approval_threshold` decides how many are needed.
    pub signatures: Vec<String>,
    pub intent: T,
}

/// `transfer` operation inside a [`SendTransactionIntent`].
#[derive(Debug, Clone, Serialize)]
pub struct TransferOperation {
    /// Always `"transfer"` for a send.
    pub kind: String,
    pub from: String,
    pub to: String,
    /// Decimal **string**. A JSON number here would canonicalize differently
    /// on each side and break the signature.
    pub amount: String,
    pub asset_id: String,
}

/// Intent for `POST /wallets/{id}/transactions`.
#[derive(Debug, Clone, Serialize)]
pub struct SendTransactionIntent {
    pub wallet_id: String,
    /// CAIP-2 chain id, e.g. `eip155:84532` for Base Sepolia.
    pub caip2: String,
    pub operation: TransferOperation,
    pub idempotency_key: String,
}

impl WalletSigner {
    /// Load from a PKCS#8 PEM private key (what `openssl ... -genkey` writes).
    pub fn from_pem(pem: &str) -> Result<Self> {
        let key = SigningKey::from_pkcs8_pem(pem.trim())
            .context("parsing the P-256 wallet key (expected a PKCS#8 PEM private key)")?;
        Ok(Self { key })
    }

    /// Base64 DER SubjectPublicKeyInfo — exactly what `POST /signers` wants as
    /// `public_key` alongside `key_type: "ES256"`.
    pub fn public_key_b64(&self) -> Result<String> {
        use p256::pkcs8::EncodePublicKey;
        let der = self
            .key
            .verifying_key()
            .to_public_key_der()
            .context("encoding the P-256 public key")?;
        Ok(base64::engine::general_purpose::STANDARD.encode(der.as_bytes()))
    }

    /// Canonicalize, hash and sign an intent.
    ///
    /// The SHA-256 step is implicit: `p256`'s ECDSA signer prehashes with
    /// SHA-256, which is what ES256 means.
    pub fn sign<T: Serialize>(&self, intent: &T) -> Result<String> {
        let canonical = canonicalize(intent)?;
        let sig: DerSignature = self.key.sign(&canonical);
        Ok(base64::engine::general_purpose::STANDARD.encode(sig.as_bytes()))
    }

    /// Sign an intent and wrap it for submission **in its canonical form**.
    ///
    /// The returned `intent` is deliberately a `serde_json::Value` rebuilt from
    /// the canonical bytes, not the original struct. `serde_json::Value` orders
    /// object keys, so re-serializing it reproduces the exact bytes that were
    /// signed.
    ///
    /// This is load-bearing, and the failure it prevents is subtle: a struct
    /// serializes in *declaration* order, so transmitting one sends JSON whose
    /// key order differs from the canonical form we signed. Dakota then answers
    /// `endorsement validation failed` with no hint as to why. Sending the
    /// canonical form makes the wire bytes and the signed bytes identical by
    /// construction, so the two can never drift.
    pub fn endorse<T: Serialize>(&self, intent: T) -> Result<EndorsedRequest<serde_json::Value>> {
        let canonical = canonicalize(&intent)?;
        let signature = self.sign(&intent)?;
        let intent = serde_json::from_slice(&canonical)
            .context("re-reading the canonical intent")?;
        Ok(EndorsedRequest { signatures: vec![signature], intent })
    }
}

/// RFC 8785 JCS canonical bytes for `value`.
pub fn canonicalize<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_jcs::to_vec(value).context("canonicalizing intent (RFC 8785 JCS)")
}

/// Put a decimal amount into the form Dakota signs over.
///
/// **This is not cosmetic.** Dakota normalizes the amount before rebuilding the
/// intent it verifies against, so a signature over `"1.00"` is checked against
/// `"1"` and fails as `endorsement validation failed` — an error that says
/// nothing about formatting. Verified against the live sandbox:
///
/// | sent     | result                        |
/// |----------|-------------------------------|
/// | `"1"`    | accepted (insufficient balance) |
/// | `"1.00"` | endorsement validation failed |
/// | `"0.50"` | endorsement validation failed |
/// | `"0.01"` | accepted (insufficient balance) |
///
/// So: strip trailing zeros from the fraction, and drop the point entirely if
/// nothing is left. `"1.00"` → `"1"`, `"0.50"` → `"0.5"`, `"0.01"` → `"0.01"`.
///
/// Anything that is not a plain decimal is returned untouched — better to let
/// Dakota reject an odd input than to silently rewrite it into a different
/// number.
pub fn normalize_amount(amount: &str) -> String {
    let s = amount.trim();
    let Some((whole, frac)) = s.split_once('.') else {
        return s.to_string();
    };
    if whole.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return s.to_string();
    }
    let trimmed = frac.trim_end_matches('0');
    if trimmed.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{trimmed}")
    }
}

#[cfg(test)]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The intent from Dakota's signing guide, field-for-field.
    fn doc_intent() -> SendTransactionIntent {
        SendTransactionIntent {
            wallet_id: "2LfZm5KMnRvLFtRP7nJJug4zJEP".into(),
            caip2: "eip155:1".into(),
            operation: TransferOperation {
                kind: "transfer".into(),
                from: "0xYourWalletAddress".into(),
                to: "0xDestinationAddress".into(),
                amount: "10.5".into(),
                asset_id: "USDC".into(),
            },
            idempotency_key: "a6f8c8c0-6f0a-4a24-a3a3-9e8a0cf2f7c0".into(),
        }
    }

    /// Generated once with:
    ///   openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt
    /// Test-only; the real key lives in Secrets Manager.
    const TEST_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgevZzL1gdAFr88hb2\n\
OF/2NxApJCzGCEDdfSp6VQO30hyhRANCAAQRWz+jn65BtOMvdyHKcvjBeBSDZH2r\n\
1RTwjmYSi9R/zpBnuQ4EiMnCqfMPWiZqB4QdbAd0E7oH50VpuZ1P087G\n\
-----END PRIVATE KEY-----";

    #[test]
    fn jcs_sorts_keys_and_strips_whitespace() {
        // JCS orders object members by their UTF-16 code units, so the output
        // is independent of declaration order. This is the property that lets
        // Dakota re-canonicalize and land on identical bytes.
        let a = serde_json::json!({ "b": 1, "a": 2, "c": { "z": 1, "y": 2 } });
        let out = String::from_utf8(canonicalize(&a).unwrap()).unwrap();
        assert_eq!(out, r#"{"a":2,"b":1,"c":{"y":2,"z":1}}"#);
    }

    #[test]
    fn canonical_form_is_order_independent() {
        // Same intent, different struct field order in the source JSON.
        let one = serde_json::json!({
            "wallet_id": "w", "caip2": "eip155:1",
            "operation": { "kind": "transfer", "amount": "1.50" },
        });
        let two = serde_json::json!({
            "operation": { "amount": "1.50", "kind": "transfer" },
            "caip2": "eip155:1", "wallet_id": "w",
        });
        assert_eq!(canonicalize(&one).unwrap(), canonicalize(&two).unwrap());
    }

    #[test]
    fn intent_serializes_snake_case_with_string_amounts() {
        let bytes = canonicalize(&doc_intent()).unwrap();
        let s = String::from_utf8(bytes).unwrap();
        // Amount must be quoted: a bare 10.5 would canonicalize via JCS number
        // rules and diverge from what Dakota reconstructs from its own record.
        assert!(s.contains(r#""amount":"10.5""#), "got {s}");
        assert!(s.contains(r#""asset_id":"USDC""#));
        assert!(s.contains(r#""idempotency_key":"a6f8c8c0-6f0a-4a24-a3a3-9e8a0cf2f7c0""#));
        assert!(s.contains(r#""wallet_id":"2LfZm5KMnRvLFtRP7nJJug4zJEP""#));
        // No camelCase leaked in.
        assert!(!s.contains("assetId") && !s.contains("walletId"));
        // No whitespace.
        assert!(!s.contains(' ') || s.contains(r#"" ""#));
    }

    #[test]
    fn key_loads_and_public_key_is_der_spki() {
        let signer = WalletSigner::from_pem(TEST_PEM).unwrap();
        let b64 = signer.public_key_b64().unwrap();
        let der = base64::engine::general_purpose::STANDARD.decode(&b64).unwrap();
        // 91-byte SPKI for an uncompressed P-256 point, starting with the
        // SEQUENCE tag. This is the exact encoding `POST /signers` accepts —
        // sandbox echoed it back unchanged.
        assert_eq!(der.len(), 91, "unexpected SPKI length");
        assert_eq!(der[0], 0x30, "DER SEQUENCE tag");
    }

    #[test]
    fn signature_verifies_against_the_canonical_digest() {
        use p256::ecdsa::signature::Verifier;

        let signer = WalletSigner::from_pem(TEST_PEM).unwrap();
        let intent = doc_intent();
        let sig_b64 = signer.sign(&intent).unwrap();

        let der = base64::engine::general_purpose::STANDARD.decode(&sig_b64).unwrap();
        let sig = DerSignature::from_bytes(&der).unwrap();
        let canonical = canonicalize(&intent).unwrap();

        // Verifying over the canonical bytes is exactly what Dakota does after
        // re-canonicalizing the intent it received.
        signer
            .key
            .verifying_key()
            .verify(&canonical, &sig)
            .expect("signature must verify over the canonical form");
    }

    #[test]
    fn signature_does_not_verify_over_a_tampered_intent() {
        use p256::ecdsa::signature::Verifier;

        let signer = WalletSigner::from_pem(TEST_PEM).unwrap();
        let sig_b64 = signer.sign(&doc_intent()).unwrap();
        let der = base64::engine::general_purpose::STANDARD.decode(&sig_b64).unwrap();
        let sig = DerSignature::from_bytes(&der).unwrap();

        // Someone rewrites the amount in flight.
        let mut tampered = doc_intent();
        tampered.operation.amount = "9999.00".into();

        assert!(signer
            .key
            .verifying_key()
            .verify(&canonicalize(&tampered).unwrap(), &sig)
            .is_err());
    }

    #[test]
    fn signature_is_der_not_p1363() {
        let signer = WalletSigner::from_pem(TEST_PEM).unwrap();
        let der = base64::engine::general_purpose::STANDARD
            .decode(signer.sign(&doc_intent()).unwrap())
            .unwrap();
        // A raw P1363 r||s would be exactly 64 bytes with no tag. DER is
        // SEQUENCE-wrapped and 70-72 bytes for P-256; browsers get this wrong,
        // which is why the docs call it out.
        assert_eq!(der[0], 0x30, "expected an ASN.1 SEQUENCE tag");
        assert!(der.len() >= 68 && der.len() <= 72, "got {} bytes", der.len());
    }

    #[test]
    fn endorse_wraps_signature_and_intent() {
        let signer = WalletSigner::from_pem(TEST_PEM).unwrap();
        let req = signer.endorse(doc_intent()).unwrap();
        assert_eq!(req.signatures.len(), 1);
        let body = serde_json::to_value(&req).unwrap();
        assert!(body.get("signatures").unwrap().is_array());
        assert!(body.get("intent").unwrap().is_object());
    }

    #[test]
    fn the_transmitted_intent_is_byte_identical_to_what_was_signed() {
        // Dakota verifies over the JSON as transmitted. A struct serializes in
        // declaration order, so shipping one sends bytes that differ from the
        // canonical form we signed, and Dakota answers "endorsement validation
        // failed" without saying why. Confirmed against the live sandbox: this
        // exact mismatch is what it rejects.
        let signer = WalletSigner::from_pem(TEST_PEM).unwrap();
        let req = signer.endorse(doc_intent()).unwrap();

        let on_the_wire = serde_json::to_vec(&req.intent).unwrap();
        let signed = canonicalize(&doc_intent()).unwrap();
        assert_eq!(
            String::from_utf8(on_the_wire).unwrap(),
            String::from_utf8(signed).unwrap(),
            "the wire form must equal the signed form"
        );
    }

    #[test]
    fn a_struct_would_not_have_matched() {
        // Guards the reasoning above. If serde ever started emitting sorted
        // keys for structs this fails, and the canonical round-trip inside
        // `endorse` could be simplified away.
        assert_ne!(
            serde_json::to_vec(&doc_intent()).unwrap(),
            canonicalize(&doc_intent()).unwrap(),
            "struct order still differs from canonical order"
        );
    }

    #[test]
    fn amounts_are_normalized_the_way_dakota_expects() {
        // Each of these was checked against the live sandbox: the left-hand
        // forms are rejected as "endorsement validation failed" when signed
        // verbatim, and accepted once normalized.
        assert_eq!(normalize_amount("1.00"), "1");
        assert_eq!(normalize_amount("2.00"), "2");
        assert_eq!(normalize_amount("0.50"), "0.5");
        assert_eq!(normalize_amount("0.10"), "0.1");
        // Already normalized — must pass through untouched.
        assert_eq!(normalize_amount("0.01"), "0.01");
        assert_eq!(normalize_amount("1"), "1");
        assert_eq!(normalize_amount("1.5"), "1.5");
        assert_eq!(normalize_amount(" 1.20 "), "1.2");
    }

    #[test]
    fn normalize_leaves_odd_input_alone() {
        // Rewriting something we do not understand risks changing the number.
        // Let Dakota reject it instead.
        assert_eq!(normalize_amount("abc"), "abc");
        assert_eq!(normalize_amount("1.2.3"), "1.2.3");
        assert_eq!(normalize_amount("-1.00"), "-1.00");
        assert_eq!(normalize_amount(".5"), ".5");
        assert_eq!(normalize_amount(""), "");
    }

    #[test]
    fn zero_normalizes_without_losing_the_whole_part() {
        assert_eq!(normalize_amount("0.00"), "0");
        assert_eq!(normalize_amount("10.00"), "10");
    }

    #[test]
    fn a_bad_pem_is_a_clear_error_not_a_panic() {
        assert!(WalletSigner::from_pem("not a pem").is_err());
        assert!(WalletSigner::from_pem("").is_err());
    }
}
