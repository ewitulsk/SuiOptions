//! Browser-side FROST 2-of-2 threshold-ed25519 participant (curator half).
//!
//! Counterpart of `rust-backend/services/hedge-signer` (the service half):
//! the two halves exchange serialized `frost_ed25519` packages over the
//! signer's `/frost/*` HTTP surface, so this crate MUST pin the exact same
//! `frost-ed25519` version. Participant identifiers are fixed by the
//! protocol: curator = 1, service = 2.
//!
//! Everything is base64-in / base64-out to match the HTTP API. Sessions
//! (`KeygenSession` / `SignSession`) hold the secret intermediates in wasm
//! memory only — nothing secret ever crosses the wasm boundary except the
//! finished curator `KeyPackage`, which the TS wrapper encrypts before it is
//! stored anywhere.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use frost_ed25519 as frost;
use frost::keys::dkg;
use frost::keys::{KeyPackage, PublicKeyPackage};
use frost::Identifier;
use rand::rngs::OsRng;
use wasm_bindgen::prelude::*;

type Blake2b256 = Blake2b<U32>;

/// Curator's fixed FROST participant identifier.
pub const CURATOR_ID: u16 = 1;
/// The hedge-signer service's fixed FROST participant identifier.
pub const SERVICE_ID: u16 = 2;

fn curator_id() -> Identifier {
    Identifier::try_from(CURATOR_ID).expect("nonzero identifier")
}

fn service_id() -> Identifier {
    Identifier::try_from(SERVICE_ID).expect("nonzero identifier")
}

fn b64d(field: &str, s: &str) -> Result<Vec<u8>, JsError> {
    B64.decode(s.trim())
        .map_err(|_| JsError::new(&format!("{field} is not valid base64")))
}

fn blake2b256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2b256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Sui address of an ed25519 public key: `blake2b256( 0x00 flag || pubkey )`.
fn sui_address_of(pubkey: &[u8]) -> String {
    format!("0x{}", hex::encode(blake2b256(&[&[0x00u8], pubkey])))
}

/// Minimal ULEB128 encoder for the BCS `Vec<u8>` length prefix.
fn uleb128(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
    out
}

/// The 32-byte digest a Sui ed25519 key signs for a personal message:
/// `blake2b256( [3,0,0] || bcs(PersonalMessage{ message }) )`. Mirrors
/// hedge-signer's `policy::bluefin::personal_message_digest`.
#[wasm_bindgen]
pub fn personal_message_digest(message: &[u8]) -> String {
    hex::encode(blake2b256(&[
        &[3u8, 0, 0],
        &uleb128(message.len() as u64),
        message,
    ]))
}

/// The 32-byte digest a Sui ed25519 key signs for a transaction:
/// `blake2b256( [0,0,0] || bcs(TransactionData) )`.
#[wasm_bindgen]
pub fn transaction_digest(tx_bytes: &[u8]) -> String {
    hex::encode(blake2b256(&[&[0u8, 0, 0], tx_bytes]))
}

// ------------------------------------------------------------------- keygen

/// Result of a completed DKG: the curator's share material and the group
/// identity. `key_package_b64` is the curator's long-lived secret share —
/// the TS wrapper must encrypt it before persisting.
#[wasm_bindgen(getter_with_clone)]
pub struct KeygenResult {
    pub key_package_b64: String,
    pub public_key_package_b64: String,
    /// 32-byte group ed25519 public key, hex (no 0x).
    pub group_public_key_hex: String,
    /// Sui address derived from the group key — the parent account address.
    pub sui_address: String,
}

enum KeygenStage {
    Round1(dkg::round1::SecretPackage),
    Round2 {
        secret: dkg::round2::SecretPackage,
        service_round1: dkg::round1::Package,
    },
    Done,
}

/// Curator side of the two-round DKG. One instance per ceremony:
/// `new()` → send `round1_package_b64()` to the service →
/// `round2(service_round1)` → send the result to the service →
/// `finish(service_round2)`.
#[wasm_bindgen]
pub struct KeygenSession {
    stage: KeygenStage,
    round1_package_b64: String,
}

#[wasm_bindgen]
impl KeygenSession {
    /// Run DKG part 1 for the curator (participant 1 of a 2-of-2).
    #[wasm_bindgen(constructor)]
    pub fn new() -> Result<KeygenSession, JsError> {
        let (secret, package) = dkg::part1(curator_id(), 2, 2, OsRng)
            .map_err(|e| JsError::new(&format!("dkg part1: {e}")))?;
        let round1_package_b64 = B64.encode(
            package
                .serialize()
                .map_err(|e| JsError::new(&format!("serializing round1 package: {e}")))?,
        );
        Ok(Self {
            stage: KeygenStage::Round1(secret),
            round1_package_b64,
        })
    }

    /// The curator's round-1 package for the service.
    #[wasm_bindgen(getter)]
    pub fn round1_package_b64(&self) -> String {
        self.round1_package_b64.clone()
    }

    /// Consume the service's round-1 package, produce the curator's round-2
    /// package addressed to the service.
    pub fn round2(&mut self, service_round1_b64: &str) -> Result<String, JsError> {
        let secret = match std::mem::replace(&mut self.stage, KeygenStage::Done) {
            KeygenStage::Round1(secret) => secret,
            _ => return Err(JsError::new("keygen session is not at round 2")),
        };
        let service_round1 =
            dkg::round1::Package::deserialize(&b64d("service_round1_b64", service_round1_b64)?)
                .map_err(|e| JsError::new(&format!("service round1 package: {e}")))?;
        let round1_packages = BTreeMap::from([(service_id(), service_round1.clone())]);
        let (round2_secret, round2_packages) = dkg::part2(secret, &round1_packages)
            .map_err(|e| JsError::new(&format!("dkg part2: {e}")))?;
        let out = round2_packages
            .get(&service_id())
            .ok_or_else(|| JsError::new("dkg part2 produced no package for the service"))?
            .serialize()
            .map_err(|e| JsError::new(&format!("serializing round2 package: {e}")))?;
        self.stage = KeygenStage::Round2 {
            secret: round2_secret,
            service_round1,
        };
        Ok(B64.encode(out))
    }

    /// Consume the service's round-2 package and finalize (part 3).
    pub fn finish(&mut self, service_round2_b64: &str) -> Result<KeygenResult, JsError> {
        let (secret, service_round1) = match std::mem::replace(&mut self.stage, KeygenStage::Done)
        {
            KeygenStage::Round2 {
                secret,
                service_round1,
            } => (secret, service_round1),
            _ => return Err(JsError::new("keygen session is not at finish")),
        };
        let service_round2 =
            dkg::round2::Package::deserialize(&b64d("service_round2_b64", service_round2_b64)?)
                .map_err(|e| JsError::new(&format!("service round2 package: {e}")))?;
        let round1_packages = BTreeMap::from([(service_id(), service_round1)]);
        let round2_packages = BTreeMap::from([(service_id(), service_round2)]);
        let (key_package, public_key_package) =
            dkg::part3(&secret, &round1_packages, &round2_packages)
                .map_err(|e| JsError::new(&format!("dkg part3: {e}")))?;
        let group_pubkey = public_key_package
            .verifying_key()
            .serialize()
            .map_err(|e| JsError::new(&format!("serializing group key: {e}")))?;
        Ok(KeygenResult {
            key_package_b64: B64.encode(
                key_package
                    .serialize()
                    .map_err(|e| JsError::new(&format!("serializing key package: {e}")))?,
            ),
            public_key_package_b64: B64.encode(
                public_key_package
                    .serialize()
                    .map_err(|e| JsError::new(&format!("serializing public key package: {e}")))?,
            ),
            sui_address: sui_address_of(&group_pubkey),
            group_public_key_hex: hex::encode(group_pubkey),
        })
    }
}

/// Re-derive the group identity from a stored `PublicKeyPackage` (used when
/// resuming: verify a cached share still matches the vault's parent).
#[wasm_bindgen]
pub fn group_identity(public_key_package_b64: &str) -> Result<KeygenResult, JsError> {
    let pkg = PublicKeyPackage::deserialize(&b64d(
        "public_key_package_b64",
        public_key_package_b64,
    )?)
    .map_err(|e| JsError::new(&format!("public key package: {e}")))?;
    let group_pubkey = pkg
        .verifying_key()
        .serialize()
        .map_err(|e| JsError::new(&format!("serializing group key: {e}")))?;
    Ok(KeygenResult {
        key_package_b64: String::new(),
        public_key_package_b64: public_key_package_b64.to_string(),
        sui_address: sui_address_of(&group_pubkey),
        group_public_key_hex: hex::encode(group_pubkey),
    })
}

// ------------------------------------------------------------------ signing

/// Result of the curator's signing round 2: the `SigningPackage` to relay to
/// the service and the curator's own signature share.
#[wasm_bindgen(getter_with_clone)]
pub struct SignRound2Result {
    pub signing_package_b64: String,
    pub signature_share_b64: String,
}

/// Curator side of one two-round FROST signing ceremony:
/// `new(key_package)` → send `commitments_b64()` with the payload to the
/// service's `/frost/sign/round1` → `round2(message_hex, service
/// commitments)` → relay the signing package to `/frost/sign/round2` →
/// `aggregate(...)` with both shares.
#[wasm_bindgen]
pub struct SignSession {
    key_package: KeyPackage,
    nonces: Option<frost::round1::SigningNonces>,
    commitments_b64: String,
}

#[wasm_bindgen]
impl SignSession {
    #[wasm_bindgen(constructor)]
    pub fn new(key_package_b64: &str) -> Result<SignSession, JsError> {
        let key_package = KeyPackage::deserialize(&b64d("key_package_b64", key_package_b64)?)
            .map_err(|e| JsError::new(&format!("key package: {e}")))?;
        let (nonces, commitments) = frost::round1::commit(key_package.signing_share(), &mut OsRng);
        let commitments_b64 = B64.encode(
            commitments
                .serialize()
                .map_err(|e| JsError::new(&format!("serializing commitments: {e}")))?,
        );
        Ok(Self {
            key_package,
            nonces: Some(nonces),
            commitments_b64,
        })
    }

    /// The curator's nonce commitments for the service's round 1.
    #[wasm_bindgen(getter)]
    pub fn commitments_b64(&self) -> String {
        self.commitments_b64.clone()
    }

    /// Build the `SigningPackage` over the service-approved digest
    /// (`message_hex`, 32 bytes) and produce the curator's signature share.
    /// Nonces are single-use: a second call fails.
    pub fn round2(
        &mut self,
        message_hex: &str,
        service_commitments_b64: &str,
    ) -> Result<SignRound2Result, JsError> {
        let nonces = self
            .nonces
            .take()
            .ok_or_else(|| JsError::new("signing nonces already consumed"))?;
        let message =
            hex::decode(message_hex).map_err(|_| JsError::new("message_hex is not hex"))?;
        if message.len() != 32 {
            return Err(JsError::new("message_hex must be a 32-byte digest"));
        }
        let curator_commitments =
            frost::round1::SigningCommitments::deserialize(&b64d(
                "commitments",
                &self.commitments_b64.clone(),
            )?)
            .map_err(|e| JsError::new(&format!("curator commitments: {e}")))?;
        let service_commitments = frost::round1::SigningCommitments::deserialize(&b64d(
            "service_commitments_b64",
            service_commitments_b64,
        )?)
        .map_err(|e| JsError::new(&format!("service commitments: {e}")))?;
        let commitments = BTreeMap::from([
            (curator_id(), curator_commitments),
            (service_id(), service_commitments),
        ]);
        let signing_package = frost::SigningPackage::new(commitments, &message);
        let share = frost::round2::sign(&signing_package, &nonces, &self.key_package)
            .map_err(|e| JsError::new(&format!("frost round2 sign: {e}")))?;
        Ok(SignRound2Result {
            signing_package_b64: B64.encode(
                signing_package
                    .serialize()
                    .map_err(|e| JsError::new(&format!("serializing signing package: {e}")))?,
            ),
            signature_share_b64: B64.encode(share.serialize()),
        })
    }
}

/// Aggregate both signature shares into the group's plain ed25519 signature
/// (64 bytes, hex). Verified against the group key before returning — an
/// invalid share fails here, never on-chain.
#[wasm_bindgen]
pub fn aggregate_signature(
    signing_package_b64: &str,
    curator_share_b64: &str,
    service_share_b64: &str,
    public_key_package_b64: &str,
) -> Result<String, JsError> {
    let signing_package =
        frost::SigningPackage::deserialize(&b64d("signing_package_b64", signing_package_b64)?)
            .map_err(|e| JsError::new(&format!("signing package: {e}")))?;
    let curator_share =
        frost::round2::SignatureShare::deserialize(&b64d("curator_share_b64", curator_share_b64)?)
            .map_err(|e| JsError::new(&format!("curator share: {e}")))?;
    let service_share =
        frost::round2::SignatureShare::deserialize(&b64d("service_share_b64", service_share_b64)?)
            .map_err(|e| JsError::new(&format!("service share: {e}")))?;
    let pubkeys = PublicKeyPackage::deserialize(&b64d(
        "public_key_package_b64",
        public_key_package_b64,
    )?)
    .map_err(|e| JsError::new(&format!("public key package: {e}")))?;
    let shares = BTreeMap::from([
        (curator_id(), curator_share),
        (service_id(), service_share),
    ]);
    let signature = frost::aggregate(&signing_package, &shares, &pubkeys)
        .map_err(|e| JsError::new(&format!("frost aggregate: {e}")))?;
    let sig_bytes = signature
        .serialize()
        .map_err(|e| JsError::new(&format!("serializing signature: {e}")))?;
    pubkeys
        .verifying_key()
        .verify(signing_package.message(), &signature)
        .map_err(|e| JsError::new(&format!("aggregated signature does not verify: {e}")))?;
    Ok(hex::encode(sig_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full 2-of-2 DKG + signing loop with this crate playing BOTH sides
    /// (the service side uses the same frost API hedge-signer does), then
    /// verifies the aggregate as plain ed25519 — mirrors
    /// hedge-signer/tests/frost_e2e.rs.
    #[test]
    fn dkg_and_sign_roundtrip() {
        // Service half (plain frost, as hedge-signer runs it).
        let (svc_r1_secret, svc_r1_pkg) = dkg::part1(service_id(), 2, 2, OsRng).unwrap();

        // Curator half through the wasm-facing session API.
        let mut kg = KeygenSession::new().unwrap();
        let curator_r1_b64 = kg.round1_package_b64();

        let curator_r1 =
            dkg::round1::Package::deserialize(&B64.decode(curator_r1_b64).unwrap()).unwrap();
        let svc_round1_map = BTreeMap::from([(curator_id(), curator_r1)]);
        let (svc_r2_secret, svc_r2_pkgs) = dkg::part2(svc_r1_secret, &svc_round1_map).unwrap();
        let svc_r2_for_curator = svc_r2_pkgs.get(&curator_id()).unwrap();

        let curator_r2_b64 = kg
            .round2(&B64.encode(svc_r1_pkg.serialize().unwrap()))
            .unwrap();
        let curator_r2 =
            dkg::round2::Package::deserialize(&B64.decode(curator_r2_b64).unwrap()).unwrap();
        let (svc_key_pkg, svc_pub_pkg) = dkg::part3(
            &svc_r2_secret,
            &svc_round1_map,
            &BTreeMap::from([(curator_id(), curator_r2)]),
        )
        .unwrap();

        let done = kg
            .finish(&B64.encode(svc_r2_for_curator.serialize().unwrap()))
            .unwrap();
        assert_eq!(
            done.group_public_key_hex,
            hex::encode(svc_pub_pkg.verifying_key().serialize().unwrap()),
            "both halves must derive the same group key"
        );

        // Signing: message = a personal-message digest.
        let message = blake2b256(&[&[3u8, 0, 0], &uleb128(5), b"hello"]);
        let mut ss = SignSession::new(&done.key_package_b64).unwrap();
        let (svc_nonces, svc_commitments) =
            frost::round1::commit(svc_key_pkg.signing_share(), &mut OsRng);
        let r2 = ss
            .round2(
                &hex::encode(message),
                &B64.encode(svc_commitments.serialize().unwrap()),
            )
            .unwrap();
        let signing_package = frost::SigningPackage::deserialize(
            &B64.decode(&r2.signing_package_b64).unwrap(),
        )
        .unwrap();
        let svc_share =
            frost::round2::sign(&signing_package, &svc_nonces, &svc_key_pkg).unwrap();

        let sig_hex = aggregate_signature(
            &r2.signing_package_b64,
            &r2.signature_share_b64,
            &B64.encode(svc_share.serialize()),
            &done.public_key_package_b64,
        )
        .unwrap();
        assert_eq!(sig_hex.len(), 128, "64-byte ed25519 signature");
    }
}
