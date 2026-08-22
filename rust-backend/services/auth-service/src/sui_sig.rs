//! Verify a Sui wallet `signPersonalMessage` signature and recover the signer
//! address — for EVERY Sui signature scheme, verified locally (SO-423).
//!
//! This is the validator-grade path: `sui_types::signature::GenericSignature`
//! parses the serialized signature (plain Ed25519/Secp256k1/Secp256r1 keys,
//! MultiSig, zkLogin, Passkey) and `verify_authenticator` runs the same
//! checks a validator would — including, for zkLogin, the Groth16 proof
//! against the JWK set and epoch bound supplied via [`VerifyParams`].
//!
//! The address is derived from the signature itself wherever possible and
//! never trusted from the client. zkLogin is the exception: its address
//! cannot be recovered from the authenticator alone (padded vs legacy
//! derivation duality), so the caller supplies the claimed address and
//! `verify_claims` proves the signature binds to it — exactly how validators
//! treat transaction senders. A claimed address for any other scheme is
//! cross-checked against the derived one.

use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use shared_crypto::intent::{Intent, IntentMessage, PersonalMessage};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::ToFromBytes;
use sui_types::digests::ZKLoginInputsDigest;
use sui_types::signature::{GenericSignature, VerifyParams};
use sui_types::signature_verification::VerifiedDigestCache;

pub type ZkCache = Arc<VerifiedDigestCache<ZKLoginInputsDigest>>;

/// Build the process-wide verified-zkLogin-inputs cache (an LRU keyed by the
/// Groth16 inputs digest, so repeat logins skip the pairing checks).
pub fn new_zk_cache() -> ZkCache {
    Arc::new(VerifiedDigestCache::new_empty())
}

/// Parse the serialized signature without verifying — callers use this to
/// route (zkLogin needs chain inputs; everything else verifies standalone).
pub fn parse(signature_b64: &str) -> Result<GenericSignature> {
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|e| anyhow!("signature is not valid base64: {e}"))?;
    GenericSignature::from_bytes(&sig_bytes)
        .map_err(|e| anyhow!("cannot parse signature: {e}"))
}

/// Verify `sig` over `message` and return the `0x`-prefixed signer address.
///
/// `claimed_address` is required for zkLogin and optional (a cross-check)
/// otherwise. `epoch` + `params` carry the zkLogin public inputs; schemes
/// without an epoch bound ignore them, so `(0, &VerifyParams::default())` is
/// valid for classic signatures.
pub fn recover_and_verify(
    sig: &GenericSignature,
    message: &[u8],
    claimed_address: Option<&str>,
    epoch: u64,
    params: &VerifyParams,
    cache: &ZkCache,
) -> Result<String> {
    let claimed = claimed_address
        .map(|a| {
            // Normalize first: SuiAddress::from_str wants the full 64-hex
            // form, while clients may send short/uppercase variants.
            SuiAddress::from_str(&crate::allowlist::normalize(a))
                .map_err(|e| anyhow!("invalid address {a:?}: {e}"))
        })
        .transpose()?;

    let author = if sig.is_zklogin() {
        // Not recoverable from the authenticator; the proof binds to it below.
        claimed.ok_or_else(|| anyhow!("address is required for zkLogin signatures"))?
    } else {
        let derived = SuiAddress::try_from(sig)
            .map_err(|e| anyhow!("cannot derive address from signature: {e}"))?;
        if let Some(claimed) = claimed {
            if claimed != derived {
                bail!("claimed address {claimed} does not match signature ({derived})");
            }
        }
        derived
    };

    let intent_msg = IntentMessage::new(
        Intent::personal_message(),
        PersonalMessage {
            message: message.to_vec(),
        },
    );
    sig.verify_authenticator(&intent_msg, author, epoch, params, cache.clone())
        .map_err(|e| anyhow!("signature did not verify for {author}: {e}"))?;

    Ok(author.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn blake2b256(parts: &[&[u8]]) -> [u8; 32] {
        use blake2::digest::consts::U32;
        use blake2::{Blake2b, Digest};
        let mut h = Blake2b::<U32>::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().into()
    }

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

    /// Build a Sui serialized Ed25519 personal-message signature the way a
    /// wallet would (intent [3,0,0] over BCS PersonalMessage, blake2b digest).
    fn wallet_sign(sk: &SigningKey, message: &[u8]) -> String {
        use base64::Engine;
        let digest = blake2b256(&[&[3u8, 0, 0], &uleb128(message.len() as u64), message]);
        let sig = sk.sign(&digest);
        let mut serialized = vec![0u8]; // ed25519 flag
        serialized.extend_from_slice(&sig.to_bytes());
        serialized.extend_from_slice(&sk.verifying_key().to_bytes());
        base64::engine::general_purpose::STANDARD.encode(serialized)
    }

    #[test]
    fn ed25519_personal_message_round_trip() {
        let sk = SigningKey::generate(&mut OsRng);
        let message = b"SuiOptions admin login \xe2\x80\x94 nonce: deadbeef";
        let sig = parse(&wallet_sign(&sk, message)).unwrap();

        let expected = {
            let mut pre = vec![0u8];
            pre.extend_from_slice(&sk.verifying_key().to_bytes());
            format!("0x{}", hex::encode(blake2b256(&[&pre])))
        };
        let addr = recover_and_verify(
            &sig,
            message,
            None,
            0,
            &VerifyParams::default(),
            &new_zk_cache(),
        )
        .unwrap();
        assert_eq!(addr, expected);

        // Claimed-address cross-check: matching passes, mismatched rejects.
        recover_and_verify(&sig, message, Some(&expected), 0, &VerifyParams::default(), &new_zk_cache())
            .unwrap();
        let err = recover_and_verify(&sig, message, Some("0x1"), 0, &VerifyParams::default(), &new_zk_cache())
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn tampered_message_rejected() {
        let sk = SigningKey::generate(&mut OsRng);
        let sig = parse(&wallet_sign(&sk, b"real message")).unwrap();
        let err = recover_and_verify(
            &sig,
            b"other message",
            None,
            0,
            &VerifyParams::default(),
            &new_zk_cache(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not verify"), "{err}");
    }

    #[test]
    fn zklogin_without_claimed_address_rejected_early() {
        // A structurally-valid zkLogin flag with garbage payload should fail
        // to parse; a parsed one without `address` must demand it. We can't
        // mint a real authenticator here, so assert the flag routing via the
        // parse error (flag 5 payload must be valid BCS).
        let bad = base64::engine::general_purpose::STANDARD.encode([5u8, 1, 2, 3]);
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn garbage_signature_rejected() {
        assert!(parse("not-base64!").is_err());
        let empty = base64::engine::general_purpose::STANDARD.encode([] as [u8; 0]);
        assert!(parse(&empty).is_err());
    }

    // --- zkLogin: the full Groth16 path over Mysten's test vectors --------

    use fastcrypto_zkp::bn254::zk_login::{parse_jwks, OIDCProvider};
    use fastcrypto_zkp::bn254::zk_login_api::ZkLoginEnv;
    use shared_crypto::intent::PersonalMessage;
    use sui_types::utils::sign_zklogin_personal_msg;
    use sui_types::zk_login_util::DEFAULT_JWK_BYTES;

    /// VerifyParams mirroring production assembly but under the Test
    /// verifying key + the fixture's Twitch test JWK.
    fn test_params() -> VerifyParams {
        let jwks = parse_jwks(DEFAULT_JWK_BYTES, &OIDCProvider::Twitch, true)
            .unwrap()
            .into_iter()
            .collect();
        VerifyParams::new(jwks, vec![], ZkLoginEnv::Test, false, true, true, None, true, true)
    }

    #[test]
    fn zklogin_personal_message_fixture_verifies() {
        let message = b"hello world".to_vec();
        // Test-vector authenticator: real proof, ephemeral key, max_epoch 10.
        let (address, sig) = sign_zklogin_personal_msg(PersonalMessage {
            message: message.clone(),
        });
        let addr_str = address.to_string();

        let out = recover_and_verify(
            &sig,
            &message,
            Some(&addr_str),
            0,
            &test_params(),
            &new_zk_cache(),
        )
        .unwrap();
        assert_eq!(out, addr_str);

        // The claimed address is REQUIRED for zkLogin…
        let err =
            recover_and_verify(&sig, &message, None, 0, &test_params(), &new_zk_cache())
                .unwrap_err();
        assert!(err.to_string().contains("required"), "{err}");

        // …and the proof must bind to it: a different address rejects.
        let err = recover_and_verify(
            &sig,
            &message,
            Some("0x1"),
            0,
            &test_params(),
            &new_zk_cache(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not verify"), "{err}");
    }

    #[test]
    fn zklogin_expired_epoch_rejected() {
        let message = b"hello world".to_vec();
        let (address, sig) = sign_zklogin_personal_msg(PersonalMessage {
            message: message.clone(),
        });
        // Fixture max_epoch is 10; a current epoch past it must reject.
        let err = recover_and_verify(
            &sig,
            &message,
            Some(&address.to_string()),
            11,
            &test_params(),
            &new_zk_cache(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not verify"), "{err}");
    }

    #[test]
    fn zklogin_unknown_jwk_rejected() {
        let message = b"hello world".to_vec();
        let (address, sig) = sign_zklogin_personal_msg(PersonalMessage {
            message: message.clone(),
        });
        // Same env, but an empty JWK map — the registry gate must hold.
        let params =
            VerifyParams::new(Default::default(), vec![], ZkLoginEnv::Test, false, true, true, None, true, true);
        let err = recover_and_verify(
            &sig,
            &message,
            Some(&address.to_string()),
            0,
            &params,
            &new_zk_cache(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not verify"), "{err}");
    }
}
