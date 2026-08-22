//! Signature-verification front door for the login handler (SO-423).
//!
//! Composes the pure verifier (`sui_sig`) with the chain-sourced zkLogin
//! inputs (`zk_inputs`): classic schemes verify standalone; zkLogin pulls
//! the cached (JWK registry, epoch) pair, and on failure refreshes the
//! inputs once and retries — covering the minutes-wide window where a
//! provider has rotated a key but our cache predates it landing on 0x7.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use fastcrypto_zkp::bn254::zk_login_api::ZkLoginEnv;
use sui_types::signature::VerifyParams;
use tracing::info;

use crate::sui_sig::{self, ZkCache};
use crate::zk_inputs::ZkInputs;

pub struct SigVerifier {
    /// `None` ⇒ zkLogin disabled (dev: no `sui_graphql_url`); classic
    /// schemes still verify, fully offline.
    zk_inputs: Option<Arc<ZkInputs>>,
    cache: ZkCache,
}

impl SigVerifier {
    pub fn new(sui_graphql_url: Option<&str>) -> Self {
        Self {
            zk_inputs: sui_graphql_url.map(|u| Arc::new(ZkInputs::new(u))),
            cache: sui_sig::new_zk_cache(),
        }
    }

    /// Verify a login signature and return the canonical signer address.
    pub async fn verify(
        &self,
        signature_b64: &str,
        message: &[u8],
        claimed_address: Option<&str>,
    ) -> Result<String> {
        let sig = sui_sig::parse(signature_b64)?;

        if !sig.is_zklogin() {
            // No epoch bound, no external inputs — pure local verification.
            let scheme = if sig.is_upgraded_multisig() {
                "multisig"
            } else if sig.is_passkey() {
                "passkey"
            } else {
                "simple"
            };
            metrics::counter!("auth_login_signatures_total", "scheme" => scheme).increment(1);
            return sui_sig::recover_and_verify(
                &sig,
                message,
                claimed_address,
                0,
                &VerifyParams::default(),
                &self.cache,
            );
        }

        metrics::counter!("auth_login_signatures_total", "scheme" => "zklogin").increment(1);
        let Some(zk) = &self.zk_inputs else {
            return Err(anyhow!("zkLogin logins are not enabled in this environment"));
        };

        let inputs = zk.current().await?;
        let first = sui_sig::recover_and_verify(
            &sig,
            message,
            claimed_address,
            inputs.epoch,
            &self.params(inputs.jwks),
            &self.cache,
        );
        let Err(first_err) = first else {
            return first;
        };

        // Retry once on fresh inputs — a rotated JWK (or an epoch tick) may
        // simply not have been in our cache yet. Rate-floored inside.
        let inputs = zk.force_refresh().await?;
        info!(error = %first_err, "zkLogin verify failed; retried on fresh chain inputs");
        sui_sig::recover_and_verify(
            &sig,
            message,
            claimed_address,
            inputs.epoch,
            &self.params(inputs.jwks),
            &self.cache,
        )
    }

    fn params(
        &self,
        jwks: im::HashMap<
            fastcrypto_zkp::bn254::zk_login::JwkId,
            fastcrypto_zkp::bn254::zk_login::JWK,
        >,
    ) -> VerifyParams {
        VerifyParams::new(
            jwks,
            // Empty ⇒ no provider restriction: the chain's JWK registry IS
            // the effective gate (exact validator parity).
            vec![],
            // Testnet and mainnet both verify against the production
            // Groth16 key. (Localnet would need Test, but dev environments
            // run with zkLogin disabled entirely.)
            ZkLoginEnv::Prod,
            /* verify_legacy_zklogin_address */ false,
            /* accept_zklogin_in_multisig */ true,
            /* accept_passkey_in_multisig */ true,
            /* zklogin_max_epoch_upper_bound_delta — bounds how far ahead a
             * wallet may set max_epoch when SIGNING; irrelevant when only
             * verifying, where current ≤ max_epoch is the check. */
            None,
            /* additional_multisig_checks */ true,
            /* validate_zklogin_public_identifier */ true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use shared_crypto::intent::PersonalMessage;
    use sui_types::crypto::ToFromBytes;
    use sui_types::utils::sign_zklogin_personal_msg;

    /// Full glue path against LIVE testnet inputs: parse → fetch the real
    /// (JWK registry, epoch) → verify → forced-refresh retry. The fixture
    /// proof is for the Test verifying key, so production params must
    /// REJECT it — what this asserts is that the whole pipeline executes
    /// and fails closed, not that the fixture passes. Network-bound:
    /// `cargo test -p auth-service -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_zklogin_pipeline_fails_closed_on_test_proof() {
        let message = b"hello world".to_vec();
        let (address, sig) = sign_zklogin_personal_msg(PersonalMessage {
            message: message.clone(),
        });
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.as_bytes());

        let v = SigVerifier::new(Some("https://graphql.testnet.sui.io/graphql"));
        let err = v
            .verify(&sig_b64, &message, Some(&address.to_string()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not verify"), "{err}");
    }

    #[tokio::test]
    async fn zklogin_disabled_without_graphql_url() {
        let message = b"hello world".to_vec();
        let (address, sig) = sign_zklogin_personal_msg(PersonalMessage {
            message: message.clone(),
        });
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.as_bytes());

        let v = SigVerifier::new(None);
        let err = v
            .verify(&sig_b64, &message, Some(&address.to_string()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not enabled"), "{err}");
    }
}
